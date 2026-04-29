//! Owns the per-host fuzzy directory index lifecycle for the runtime.
//!
//! Before this module existed, `runtime.rs` carried the catalog
//! handle, the disk hydration, the SWR trigger logic for local AND
//! every SSH host, and the multi-stage drain that turns
//! [`IndexEvent`]s into state transitions plus disk-save dispatches.
//! The previous architecture review (NLM session) flagged this as an
//! event-loop "god object" smell — UI routing was sharing space with
//! infrastructure orchestration.
//!
//! This module is the extracted infrastructure half. The runtime
//! keeps its keymap-routing role and forwards lifecycle requests:
//! `request_local_swr` on session start and modal open,
//! `request_remote_swr` after the SSH prepare succeeds,
//! `force_rebuild_*` from the `RefreshIndex` outcome, [`Self::tick`] once
//! per frame, and `state_for(host)` whenever the modal needs to
//! render the current index.
//!
//! Everything I/O-flavored (disk reads, ssh subprocess spawns, save
//! dispatch in a detached thread) lives here. The catalog's state
//! machine stays in [`crate::index_worker`]; this module is the
//! coordination layer between the catalog and the runtime.

use std::path::Path;

use crate::index_cache;
use crate::index_worker::{
    HostIndex, IndexCatalog, IndexEvent, IndexSaveCtx, IndexState, IndexedDir, RefreshOutcome,
    expand_remote_roots, expand_search_roots, start_index, start_index_remote,
};
use crate::spawn::HOST_PLACEHOLDER;

/// Coordinator for the per-host fuzzy directory index. Owns the
/// [`IndexCatalog`]; turns runtime-shaped requests
/// (`request_local_swr`, `request_remote_swr`, `force_rebuild_*`)
/// into catalog mutations + walker spawns + disk hydration; drains
/// [`IndexEvent`]s once per frame in [`Self::tick`] and dispatches
/// the disk-save side effect for any completed walks.
pub struct IndexManager {
    catalog: IndexCatalog,
}

impl Default for IndexManager {
    fn default() -> Self {
        Self::new()
    }
}

impl IndexManager {
    #[must_use]
    pub fn new() -> Self {
        Self {
            catalog: IndexCatalog::new(),
        }
    }

    /// SWR trigger for the local index. Hydrates from disk if no
    /// state exists yet (first run or freshly toggled to fuzzy
    /// mid-session) and then starts a background walk under the SWR
    /// contract — cached results stay queryable while the walker
    /// runs. Idempotent: repeated calls while a walker is in flight
    /// are no-ops via [`IndexCatalog::start_refresh`]'s skip path.
    ///
    /// Returns the [`RefreshOutcome`] for telemetry / debug logging
    /// at the call site.
    pub fn request_local_swr(&mut self, roots: &[String], markers: &[String]) -> RefreshOutcome {
        let ctx = IndexSaveCtx::Local {
            roots: roots.to_vec(),
        };
        if self.catalog.state_for(HOST_PLACEHOLDER).is_none()
            && let Some(cached) = index_cache::load_local(roots)
        {
            tracing::debug!(
                n = cached.len(),
                "fuzzy index: hydrated local from disk cache",
            );
            self.catalog
                .hydrate(HOST_PLACEHOLDER.to_string(), ctx.clone(), cached);
        }
        let expanded = expand_search_roots(roots);
        let markers = markers.to_vec();
        self.catalog
            .start_refresh(HOST_PLACEHOLDER.to_string(), ctx, || {
                start_index(expanded, markers)
            })
    }

    /// SWR trigger for an SSH host, called after the prepare phase
    /// returns a live `RemoteFs`. Hydrates from the remote disk
    /// cache (read over the existing `ControlMaster` socket) and then
    /// starts a background `find` walk over the same socket. Same
    /// SWR semantics as the local case.
    pub fn request_remote_swr(
        &mut self,
        host: &str,
        socket: &Path,
        remote_home: &Path,
        roots: &[String],
        markers: &[String],
    ) -> RefreshOutcome {
        let ctx = IndexSaveCtx::Remote {
            host: host.to_string(),
            socket: socket.to_path_buf(),
            roots: roots.to_vec(),
        };
        if let Some(cached) = index_cache::load_remote(host, socket, roots) {
            tracing::debug!(
                host,
                n = cached.len(),
                "fuzzy index: hydrated remote from disk cache",
            );
            self.catalog.hydrate(host.to_string(), ctx.clone(), cached);
        }
        let expanded = expand_remote_roots(roots, remote_home);
        let markers = markers.to_vec();
        let host_owned = host.to_string();
        let socket_owned = socket.to_path_buf();
        self.catalog.start_refresh(host.to_string(), ctx, || {
            start_index_remote(host_owned, socket_owned, expanded, markers)
        })
    }

    /// User-triggered (Ctrl-R) force-rebuild for the local host.
    /// Always cancels any in-flight worker and starts a fresh one;
    /// cached results survive as `Refreshing { dirs, .. }` so the
    /// wildmenu doesn't blank during the rebuild.
    pub fn force_rebuild_local(&mut self, roots: &[String], markers: &[String]) {
        let ctx = IndexSaveCtx::Local {
            roots: roots.to_vec(),
        };
        let expanded = expand_search_roots(roots);
        let markers = markers.to_vec();
        self.catalog
            .force_rebuild(HOST_PLACEHOLDER.to_string(), ctx, || {
                start_index(expanded, markers)
            });
    }

    /// User-triggered force-rebuild for an SSH host. Mirrors
    /// [`Self::force_rebuild_local`] but routes through the
    /// remote walker.
    pub fn force_rebuild_remote(
        &mut self,
        host: &str,
        socket: &Path,
        remote_home: &Path,
        roots: &[String],
        markers: &[String],
    ) {
        let ctx = IndexSaveCtx::Remote {
            host: host.to_string(),
            socket: socket.to_path_buf(),
            roots: roots.to_vec(),
        };
        let expanded = expand_remote_roots(roots, remote_home);
        let markers = markers.to_vec();
        let host_owned = host.to_string();
        let socket_owned = socket.to_path_buf();
        self.catalog.force_rebuild(host.to_string(), ctx, || {
            start_index_remote(host_owned, socket_owned, expanded, markers)
        });
    }

    /// Per-frame drain: walk every in-flight host, consume queued
    /// [`IndexEvent`]s, compute state transitions, and dispatch any
    /// resulting disk save in a detached thread. Returns nothing —
    /// the runtime queries [`Self::state_for`] separately when it
    /// needs to render or notify the modal.
    ///
    /// The two-phase pattern (drain → re-bind state to take stale
    /// dirs out of `Refreshing`) is required by the borrow checker:
    /// the drain holds `&mut handle/count`, and we can't reach back
    /// into the parent enum's other fields until that borrow ends.
    pub fn tick(&mut self) {
        let mut transitions: Vec<(String, IndexState, Option<Vec<IndexedDir>>, IndexSaveCtx)> =
            Vec::new();
        for (host, slot) in self.catalog.iter_in_flight_mut() {
            let Some(transition) = drain_one_host(host, slot) else {
                continue;
            };
            transitions.push(transition);
        }
        for (host, new_state, completed_dirs, save_ctx) in transitions {
            self.catalog.set_state(&host, new_state);
            // Persist on success. The save runs detached so this
            // loop is not blocked on remote `tee` (which over a slow
            // connection can take a couple hundred ms).
            if let Some(dirs) = completed_dirs {
                save_index_async(save_ctx, dirs);
            }
        }
    }

    /// Borrow the [`IndexState`] for `host`, if any. Used by the
    /// modal's `notify_index_state` and the wildmenu render path.
    /// Returns `None` for a host that has never been touched in
    /// this session and has no disk cache.
    #[must_use]
    pub fn state_for(&self, host: &str) -> Option<&IndexState> {
        self.catalog.state_for(host)
    }
}

/// Internal: drain one host's in-flight events and compute the next
/// transition. Returns `None` when the worker hasn't sent a terminal
/// event yet (just `Progress` so far, or nothing).
///
/// While draining, `Progress` events with a non-empty batch are
/// folded into the live state: `Building.dirs` accumulates so the
/// fuzzy matcher can search the partial index, while `Refreshing`
/// drops the batch (the cached `dirs` field is the user-visible
/// result and stays stable until the terminating `Done(Ok)` swaps
/// in the freshly-classified version).
///
/// The shape of the tuple — `(host, next_state, completed_dirs?,
/// save_ctx)` — matches what the [`IndexManager::tick`] post-loop
/// expects so the borrow on the catalog can be released before the
/// detached save thread is spawned.
fn drain_one_host(
    host: &str,
    slot: &mut HostIndex,
) -> Option<(String, IndexState, Option<Vec<IndexedDir>>, IndexSaveCtx)> {
    #[derive(Debug)]
    enum Term {
        Ok(Vec<IndexedDir>),
        Err(String),
    }
    let term: Option<Term> = match &mut slot.state {
        IndexState::Building {
            handle,
            count,
            dirs,
        } => {
            let mut t = None;
            while let Some(event) = handle.try_recv() {
                match event {
                    IndexEvent::Progress { count: c, batch } => {
                        *count = c;
                        // Append the new-since-last-event batch so
                        // the partial index grows monotonically.
                        // Classification stays Plain; the final
                        // `Done(Ok)` swaps the entire dirs vec for
                        // the fully-classified version.
                        dirs.extend(batch);
                    }
                    IndexEvent::Done(Ok(d)) => {
                        t = Some(Term::Ok(d));
                        break;
                    }
                    IndexEvent::Done(Err(e)) => {
                        t = Some(Term::Err(format!("{e}")));
                        break;
                    }
                }
            }
            t
        }
        IndexState::Refreshing { handle, count, .. } => {
            let mut t = None;
            while let Some(event) = handle.try_recv() {
                match event {
                    IndexEvent::Progress { count: c, .. } => {
                        // Cache is what's displayed during a refresh;
                        // dropping the batch keeps the SWR contract
                        // (stale-but-stable until Done).
                        *count = c;
                    }
                    IndexEvent::Done(Ok(d)) => {
                        t = Some(Term::Ok(d));
                        break;
                    }
                    IndexEvent::Done(Err(e)) => {
                        t = Some(Term::Err(format!("{e}")));
                        break;
                    }
                }
            }
            t
        }
        _ => None,
    };
    let term = term?;
    // Stale dirs from the prior `Refreshing` carrier. `take` empties
    // the vec in place so the next state owns a fresh allocation;
    // `Building` has nothing to keep so this returns `None`.
    let stale_dirs: Option<Vec<IndexedDir>> = match &mut slot.state {
        IndexState::Refreshing { dirs, .. } => Some(std::mem::take(dirs)),
        _ => None,
    };
    let (next_state, completed_dirs) = match term {
        Term::Ok(dirs) => {
            tracing::info!(host, n = dirs.len(), "fuzzy index: build complete");
            (IndexState::Ready { dirs: dirs.clone() }, Some(dirs))
        }
        Term::Err(msg) => {
            tracing::warn!(host, "fuzzy index: build failed: {msg}");
            // Refreshing → keep cached results so the user never
            // sees a populated wildmenu go empty because of a
            // transient walker failure. Building had nothing to
            // keep.
            let next = match stale_dirs {
                Some(dirs) => IndexState::Ready { dirs },
                None => IndexState::Failed { message: msg },
            };
            (next, None)
        }
    };
    Some((
        host.to_string(),
        next_state,
        completed_dirs,
        slot.save_ctx.clone(),
    ))
}

/// Spawn a detached thread to write `dirs` to the appropriate disk
/// cache (local or remote). Errors are logged at warn — a failed
/// cache write means the next session pays a cold build, which is
/// noisy enough to surface but not fatal enough to propagate.
fn save_index_async(ctx: IndexSaveCtx, dirs: Vec<IndexedDir>) {
    std::thread::spawn(move || {
        let result = match &ctx {
            IndexSaveCtx::Local { roots } => index_cache::save_local(roots, &dirs),
            IndexSaveCtx::Remote {
                host,
                socket,
                roots,
            } => index_cache::save_remote(host, socket, roots, &dirs),
        };
        if let Err(e) = result {
            tracing::warn!(error = %e, "fuzzy index cache: save failed");
        }
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    //! `IndexManager` is mostly orchestration glue between
    //! [`IndexCatalog`] (already exhaustively tested in
    //! `index_worker::tests`), the disk cache (tested in
    //! `index_cache::tests`), and the walker spawns (impure I/O).
    //! These tests cover the small slice of pure logic the manager
    //! adds: the post-drain transition tuple shape produced by
    //! `drain_one_host`.

    use super::*;
    use crate::index_worker;
    use crate::index_worker::{IndexError, IndexedDir, ProjectKind};
    use std::path::{Path, PathBuf};

    fn ctx() -> IndexSaveCtx {
        IndexSaveCtx::Local { roots: Vec::new() }
    }

    fn ready_slot(paths: &[&str]) -> HostIndex {
        HostIndex {
            state: IndexState::Ready {
                dirs: paths
                    .iter()
                    .map(|p| IndexedDir {
                        path: PathBuf::from(p),
                        kind: ProjectKind::Plain,
                    })
                    .collect(),
            },
            save_ctx: ctx(),
        }
    }

    #[test]
    fn drain_returns_none_for_settled_state() {
        // A `Ready` slot has no in-flight handle, so the drain
        // helper has nothing to do — it must return `None` rather
        // than synthesize a transition.
        let mut slot = ready_slot(&["/x"]);
        let result = drain_one_host("local", &mut slot);
        assert!(result.is_none());
    }

    #[test]
    fn state_for_returns_none_when_host_unknown() {
        let mgr = IndexManager::new();
        assert!(mgr.state_for("local").is_none());
    }

    #[test]
    fn state_for_returns_hydrated_dirs() {
        // Smoke test that the manager's read API surfaces what was
        // hydrated. The catalog itself is exhaustively tested
        // elsewhere; this exists so a future refactor that
        // accidentally bypasses `state_for` would break here.
        let mut mgr = IndexManager::new();
        let dirs = vec![IndexedDir {
            path: PathBuf::from("/h"),
            kind: ProjectKind::Plain,
        }];
        mgr.catalog.hydrate("local".to_string(), ctx(), dirs);
        match mgr.state_for("local") {
            Some(IndexState::Ready { dirs }) => assert_eq!(dirs.len(), 1),
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn drain_on_refreshing_with_no_events_returns_none() {
        // Inert handle (no live worker) → no events, so the drain
        // produces no transition. Stale dirs must not be consumed
        // until a terminal event arrives.
        let mut slot = HostIndex {
            state: IndexState::Refreshing {
                dirs: vec![IndexedDir {
                    path: PathBuf::from("/cached"),
                    kind: ProjectKind::Git,
                }],
                handle: index_worker::inert_handle_for_test(),
                count: 0,
            },
            save_ctx: ctx(),
        };
        assert!(drain_one_host("local", &mut slot).is_none());
        match &slot.state {
            IndexState::Refreshing { dirs, .. } => assert_eq!(dirs.len(), 1),
            _ => panic!("Refreshing state must be preserved"),
        }
    }

    /// A `Progress { batch }` event arriving on a `Building` slot
    /// must extend the in-flight `dirs` accumulator and update
    /// `count`, then return `None` (no terminal event yet) so the
    /// caller leaves the host in `Building`. This is the pivotal
    /// behavior behind "search the partial index while the walk
    /// runs."
    #[test]
    fn drain_on_building_appends_progress_batch_to_dirs() {
        let (handle, tx) = index_worker::handle_with_sender_for_test();
        tx.send(IndexEvent::Progress {
            count: 2,
            batch: vec![
                IndexedDir {
                    path: PathBuf::from("/p1"),
                    kind: ProjectKind::Plain,
                },
                IndexedDir {
                    path: PathBuf::from("/p2"),
                    kind: ProjectKind::Plain,
                },
            ],
        })
        .unwrap();
        let mut slot = HostIndex {
            state: IndexState::Building {
                handle,
                count: 0,
                dirs: Vec::new(),
            },
            save_ctx: ctx(),
        };
        assert!(drain_one_host("local", &mut slot).is_none());
        match &slot.state {
            IndexState::Building { dirs, count, .. } => {
                assert_eq!(*count, 2);
                assert_eq!(dirs.len(), 2);
                assert_eq!(dirs[0].path, PathBuf::from("/p1"));
                assert_eq!(dirs[1].path, PathBuf::from("/p2"));
            }
            other => panic!("expected Building, got {other:?}"),
        }
    }

    /// `Progress` on `Refreshing` updates the count but discards the
    /// batch (the cached `dirs` field is the user-visible snapshot
    /// during SWR — mixing in partial new entries would surface
    /// duplicates and ghosts during the transition).
    #[test]
    fn drain_on_refreshing_with_progress_drops_batch_and_preserves_cache() {
        let (handle, tx) = index_worker::handle_with_sender_for_test();
        tx.send(IndexEvent::Progress {
            count: 5,
            batch: vec![IndexedDir {
                path: PathBuf::from("/new"),
                kind: ProjectKind::Plain,
            }],
        })
        .unwrap();
        let mut slot = HostIndex {
            state: IndexState::Refreshing {
                dirs: vec![IndexedDir {
                    path: PathBuf::from("/cached"),
                    kind: ProjectKind::Git,
                }],
                handle,
                count: 0,
            },
            save_ctx: ctx(),
        };
        assert!(drain_one_host("local", &mut slot).is_none());
        match &slot.state {
            IndexState::Refreshing { dirs, count, .. } => {
                assert_eq!(*count, 5);
                assert_eq!(dirs.len(), 1, "cache must stay stable until Done");
                assert_eq!(dirs[0].path, PathBuf::from("/cached"));
            }
            other => panic!("expected Refreshing, got {other:?}"),
        }
    }

    /// `Done(Ok)` on a `Building` slot transitions to `Ready` with
    /// the fully-classified dirs from the event (replacing the
    /// partial accumulator) and surfaces `completed_dirs` so the
    /// runtime can persist them to disk.
    #[test]
    fn drain_on_building_done_ok_transitions_to_ready_and_returns_dirs() {
        let (handle, tx) = index_worker::handle_with_sender_for_test();
        tx.send(IndexEvent::Done(Ok(vec![IndexedDir {
            path: PathBuf::from("/final"),
            kind: ProjectKind::Git,
        }])))
        .unwrap();
        let mut slot = HostIndex {
            state: IndexState::Building {
                handle,
                count: 0,
                dirs: vec![IndexedDir {
                    path: PathBuf::from("/partial"),
                    kind: ProjectKind::Plain,
                }],
            },
            save_ctx: ctx(),
        };
        let (host, next, completed, _) = drain_one_host("local", &mut slot).unwrap();
        assert_eq!(host, "local");
        match next {
            IndexState::Ready { dirs } => {
                assert_eq!(dirs.len(), 1);
                assert_eq!(dirs[0].path, PathBuf::from("/final"));
                assert_eq!(dirs[0].kind, ProjectKind::Git);
            }
            other => panic!("expected Ready, got {other:?}"),
        }
        assert!(
            completed.is_some_and(|d| d[0].path == Path::new("/final")),
            "completed_dirs must surface the fully-classified list for persistence",
        );
    }

    /// `Done(Err)` on a `Refreshing` slot keeps the cached dirs as
    /// `Ready` (don't blank the wildmenu on a transient walker
    /// failure) and returns `None` for `completed_dirs` (nothing
    /// new to persist).
    #[test]
    fn drain_on_refreshing_done_err_falls_back_to_cached_ready() {
        let (handle, tx) = index_worker::handle_with_sender_for_test();
        tx.send(IndexEvent::Done(Err(IndexError::NoRoots(Vec::new()))))
            .unwrap();
        let mut slot = HostIndex {
            state: IndexState::Refreshing {
                dirs: vec![IndexedDir {
                    path: PathBuf::from("/cached"),
                    kind: ProjectKind::Git,
                }],
                handle,
                count: 0,
            },
            save_ctx: ctx(),
        };
        let (_, next, completed, _) = drain_one_host("local", &mut slot).unwrap();
        match next {
            IndexState::Ready { dirs } => {
                assert_eq!(dirs.len(), 1);
                assert_eq!(dirs[0].path, PathBuf::from("/cached"));
            }
            other => panic!("expected Ready (fallback to cache), got {other:?}"),
        }
        assert!(
            completed.is_none(),
            "no fresh dirs to persist on Done(Err) fallback",
        );
    }

    /// `Done(Err)` on a `Building` slot (no cache to fall back to)
    /// transitions to `Failed` so the wildmenu can surface the
    /// error sentinel instead of pretending nothing went wrong.
    #[test]
    fn drain_on_building_done_err_transitions_to_failed() {
        let (handle, tx) = index_worker::handle_with_sender_for_test();
        tx.send(IndexEvent::Done(Err(IndexError::NoRoots(Vec::new()))))
            .unwrap();
        let mut slot = HostIndex {
            state: IndexState::Building {
                handle,
                count: 0,
                dirs: Vec::new(),
            },
            save_ctx: ctx(),
        };
        let (_, next, completed, _) = drain_one_host("local", &mut slot).unwrap();
        assert!(matches!(next, IndexState::Failed { .. }));
        assert!(completed.is_none());
    }
}
