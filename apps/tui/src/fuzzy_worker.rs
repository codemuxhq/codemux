//! Background worker that owns fuzzy scoring for the spawn modal.
//!
//! ## Why a background worker
//!
//! [`crate::spawn::score_fuzzy`] runs `nucleo-matcher` over the entire
//! indexed dirs Vec — for the default search root (`~`) that's
//! commonly 30k–50k+ entries. Hundreds of milliseconds per call. The
//! previous design called it inline from the runtime's frame loop
//! (`runtime::notify_index_state` → `SpawnMinibuffer::refresh_fuzzy`),
//! so every keystroke blocked the next render until scoring finished
//! — typing visibly stalled, and an idle modal with a non-empty query
//! re-scored on every 50 ms tick for no reason.
//!
//! The worker moves scoring off the render thread and pairs with
//! per-host generation memoization in `runtime.rs` so a `SetIndex`
//! only fires when the underlying dirs Vec actually changed. Typing
//! echoes immediately; the wildmenu repaints a frame or two later
//! when the result lands.
//!
//! ## Single-host state, latest-wins drain
//!
//! The worker keeps **one** host's snapshot at a time — the spawn
//! modal only targets one host (local or one SSH alias). When the
//! runtime sends a `SetIndex` for a different host, the worker drops
//! the previous snapshot and flushes the cached query (a stale
//! query against a fresh-host index would score against the wrong
//! data).
//!
//! Each wake-up drains the entire control channel before scoring,
//! collapsing burst input (multiple keystrokes between wake-ups) to
//! a single score over the latest state. Mirrors
//! [`crate::agent_meta_worker`]'s focus-change collapse pattern.
//!
//! ## Race handling
//!
//! Results are tagged with the `(host, query)` they came from. The
//! consuming side ([`crate::spawn::SpawnMinibuffer::set_fuzzy_results`])
//! drops any [`FuzzyResult`] whose tag doesn't match the modal's
//! current state. This catches the natural race where the user types
//! `c` → result lands, then types `co` → another result lands: only
//! the `co` result is applied, and the in-flight `c` result is
//! discarded if it arrives between the two query dispatches.

use std::thread;

use crossbeam_channel::{Receiver, Sender, unbounded};

use crate::config::NamedProject;
use crate::index_worker::IndexedDir;
use crate::spawn::score_fuzzy;

/// Control messages the runtime sends to the worker.
///
/// `SetIndex` is sent only when the per-host generation
/// counter (`crate::index_manager::IndexManager::state_generation_for`)
/// changes — typically once per index transition (Building → Ready,
/// Ready → Refreshing → Ready, hydrate-from-disk, or a Building
/// Progress batch that grew the partial accumulator). `Query` fires
/// on every keystroke that mutates the modal's `fuzzy_query`.
pub enum FuzzyControl {
    /// Replace the cached snapshot for this host. The worker keeps
    /// at most one host's state at a time; sending `SetIndex` for a
    /// different host drops the prior snapshot and flushes any
    /// pending query (a query against the previous host would score
    /// against the wrong data).
    SetIndex {
        host: String,
        dirs: Vec<IndexedDir>,
        named: Vec<NamedProject>,
    },
    /// Update the latest query for this host. Dropped silently if
    /// the host doesn't match the worker's current snapshot — the
    /// runtime is expected to dispatch `SetIndex` before the first
    /// `Query` for any host (this is enforced by the gen-counter
    /// memoization layer in `runtime.rs`).
    Query { host: String, query: String },
}

/// Scored hits emitted by the worker. Tagged with the `(host, query)`
/// pair the result was computed against so the consuming modal can
/// reject stale results that arrived after a faster-typed superseding
/// query.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct FuzzyResult {
    pub host: String,
    pub query: String,
    pub hits: Vec<String>,
}

/// Runtime-side handle to the worker. Owns the control sender and the
/// event receiver. Dropping the handle disconnects `control_tx`, which
/// wakes the worker's blocking `recv()` with `Err` and triggers a
/// clean exit — no separate cancellation flag needed.
pub struct FuzzyWorker {
    control_tx: Sender<FuzzyControl>,
    events: Receiver<FuzzyResult>,
}

impl FuzzyWorker {
    /// Spawn the worker thread.
    #[must_use]
    pub fn start() -> Self {
        let (control_tx, control_rx) = unbounded::<FuzzyControl>();
        let (events_tx, events) = unbounded::<FuzzyResult>();
        thread::spawn(move || {
            worker_loop(&control_rx, &events_tx);
        });
        Self { control_tx, events }
    }

    /// Replace the worker's index snapshot for `host`. Fire-and-forget;
    /// the worker may not act on it for several ms (it's blocked on the
    /// next message recv or scoring the previous burst). The runtime
    /// guards re-sends with the per-host generation counter so this
    /// only fires when the underlying dirs Vec actually changed.
    pub fn set_index(&self, host: String, dirs: Vec<IndexedDir>, named: Vec<NamedProject>) {
        // Send-failure means the worker thread already exited (channel
        // disconnected). Nothing actionable from the runtime side.
        self.control_tx
            .send(FuzzyControl::SetIndex { host, dirs, named })
            .ok();
    }

    /// Update the latest query for `host`. Same fire-and-forget
    /// semantics as [`Self::set_index`].
    pub fn query(&self, host: String, query: String) {
        self.control_tx
            .send(FuzzyControl::Query { host, query })
            .ok();
    }

    /// Drain pending results. Non-blocking; returns whatever the
    /// worker has produced since the previous drain. Call once per
    /// frame, before dispatching new requests, so the modal applies
    /// the freshest result before the next query overwrites the
    /// worker's state.
    #[must_use]
    pub fn drain(&self) -> Vec<FuzzyResult> {
        self.events.try_iter().collect()
    }
}

/// Worker thread entry point. Blocks on the control channel,
/// drain-collapses bursts, scores against the latest snapshot, and
/// emits one [`FuzzyResult`] per scoring pass. Exits cleanly when the
/// runtime drops `control_tx` (recv returns `Err`).
fn worker_loop(control_rx: &Receiver<FuzzyControl>, events_tx: &Sender<FuzzyResult>) {
    let mut state = WorkerState::default();

    loop {
        // Block for the next message. `Err` here means `control_tx`
        // was dropped — the runtime exited or the worker is being
        // torn down. Either way: nothing more to do.
        let Ok(msg) = control_rx.recv() else {
            return;
        };
        state.apply(msg);
        // Drain any messages that piled up while we were blocked.
        // Latest of each kind wins (`apply` overwrites unconditionally).
        // This collapses fast typing bursts to a single score.
        for msg in control_rx.try_iter() {
            state.apply(msg);
        }
        // Score using the latest state. `score_now` returns `None`
        // when there's nothing to score (no host snapshot, or empty
        // query) — the runtime already short-circuits empty queries
        // so we never see them, but the guard here keeps the worker
        // honest if that contract ever drifts.
        let Some(result) = state.score_now() else {
            continue;
        };
        if events_tx.send(result).is_err() {
            // Receiver dropped → runtime exited mid-frame. Bail.
            return;
        }
    }
}

/// Internal worker state. Holds at most one host's snapshot plus the
/// latest query for it. A host change clears the query (a stale query
/// against a fresh-host index is meaningless). Default-constructed for
/// session start (no host targeted yet).
#[derive(Default)]
struct WorkerState {
    host: Option<String>,
    dirs: Vec<IndexedDir>,
    named: Vec<NamedProject>,
    query: Option<String>,
}

impl WorkerState {
    fn apply(&mut self, msg: FuzzyControl) {
        match msg {
            FuzzyControl::SetIndex { host, dirs, named } => {
                let host_changed = self.host.as_ref() != Some(&host);
                if host_changed {
                    // New host: any cached query was for the previous
                    // host's index. Drop it so the worker doesn't
                    // emit a result tagged with a stale (host, query)
                    // pair that the modal would just discard anyway.
                    self.query = None;
                }
                self.host = Some(host);
                self.dirs = dirs;
                self.named = named;
            }
            FuzzyControl::Query { host, query } => {
                // A query for a host the worker doesn't have an index
                // for is a contract violation (runtime should have
                // dispatched `SetIndex` first). Log and drop rather
                // than score against the wrong data.
                if self.host.as_ref() != Some(&host) {
                    tracing::debug!(
                        ?host,
                        worker_host = ?self.host,
                        "fuzzy worker: dropping query for unknown host",
                    );
                    return;
                }
                self.query = Some(query);
            }
        }
    }

    fn score_now(&self) -> Option<FuzzyResult> {
        let host = self.host.as_ref()?;
        let query = self.query.as_ref()?;
        // Empty query is meaningless for ranking; the runtime
        // short-circuits before dispatching, but guard here too.
        if query.is_empty() {
            return None;
        }
        let hits = score_fuzzy(query, &self.dirs, &self.named);
        Some(FuzzyResult {
            host: host.clone(),
            query: query.clone(),
            hits,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::index_worker::ProjectKind;

    fn dir(p: &str) -> IndexedDir {
        IndexedDir {
            path: PathBuf::from(p),
            kind: ProjectKind::Plain,
        }
    }

    /// Block until the worker emits a result or `timeout` elapses.
    /// Returns `None` on timeout. Used to keep tests deterministic
    /// without sleeping for fixed durations.
    fn await_result(worker: &FuzzyWorker, timeout: Duration) -> Option<FuzzyResult> {
        let deadline = Instant::now() + timeout;
        loop {
            let mut drained = worker.drain();
            if let Some(r) = drained.pop() {
                return Some(r);
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    /// Drain everything the worker has emitted, blocking briefly to
    /// let the worker thread catch up. Used by tests that need to
    /// verify "no other results were sent."
    fn drain_all(worker: &FuzzyWorker, settle: Duration) -> Vec<FuzzyResult> {
        thread::sleep(settle);
        worker.drain()
    }

    #[test]
    fn set_index_then_query_emits_matching_result() {
        let worker = FuzzyWorker::start();
        worker.set_index(
            "local".into(),
            vec![dir("/code"), dir("/notes")],
            Vec::new(),
        );
        worker.query("local".into(), "code".into());
        let result = await_result(&worker, Duration::from_millis(500)).unwrap();
        assert_eq!(result.host, "local");
        assert_eq!(result.query, "code");
        assert_eq!(result.hits, vec!["/code".to_string()]);
    }

    #[test]
    fn query_burst_collapses_to_latest() {
        // Send several queries back-to-back. Latest-wins drain should
        // collapse the burst so the worker emits *at most one* result
        // tagged with the final query. (Race-tolerant: the worker may
        // partially process the burst and emit one stale result, but
        // the assertion below tolerates that as long as the final
        // result wins.)
        let worker = FuzzyWorker::start();
        worker.set_index(
            "local".into(),
            vec![dir("/code"), dir("/codex"), dir("/notes")],
            Vec::new(),
        );
        worker.query("local".into(), "c".into());
        worker.query("local".into(), "co".into());
        worker.query("local".into(), "code".into());
        // Give the worker time to settle. Drain everything; the last
        // emitted result must be tagged "code".
        let results = drain_all(&worker, Duration::from_millis(100));
        let last = results.last().unwrap();
        assert_eq!(last.query, "code");
        assert_eq!(last.host, "local");
    }

    #[test]
    fn query_for_unknown_host_is_dropped() {
        let worker = FuzzyWorker::start();
        worker.set_index("local".into(), vec![dir("/code")], Vec::new());
        worker.query("ghost".into(), "code".into());
        let results = drain_all(&worker, Duration::from_millis(50));
        assert!(
            results.is_empty(),
            "query for unknown host must not emit a result, got {results:?}",
        );
    }

    #[test]
    fn set_index_for_new_host_clears_pending_query() {
        // Land a query for host A, then SetIndex to host B before the
        // worker has scored. The pending query must be flushed so
        // host B doesn't receive a score for host A's query.
        let worker = FuzzyWorker::start();
        worker.set_index("local".into(), vec![dir("/code")], Vec::new());
        worker.query("local".into(), "code".into());
        worker.set_index("remote".into(), vec![dir("/srv")], Vec::new());
        // Wait for any results. We expect either zero (worker hadn't
        // scored before the host swap) or one tagged "local"/"code"
        // (worker scored before the swap landed) — never one tagged
        // "remote"/"code", which would be the bug.
        let results = drain_all(&worker, Duration::from_millis(100));
        for r in &results {
            assert!(
                !(r.host == "remote" && r.query == "code"),
                "stale query must not score against new host's index, got {r:?}",
            );
        }
    }

    #[test]
    fn empty_query_is_a_no_op() {
        let worker = FuzzyWorker::start();
        worker.set_index("local".into(), vec![dir("/code")], Vec::new());
        worker.query("local".into(), String::new());
        let results = drain_all(&worker, Duration::from_millis(50));
        assert!(
            results.is_empty(),
            "empty query must not emit a result, got {results:?}",
        );
    }

    #[test]
    fn drop_disconnects_channel_and_exits_worker() {
        // Worker should exit cleanly when its handle is dropped:
        // dropping `control_tx` disconnects the channel, the
        // worker's blocking `recv()` returns `Err`, and the loop
        // exits. There's no direct observable for "thread exited,"
        // but a subsequent `set_index` after `drop` would also be
        // dropped silently (the worker's send-failure path covers
        // the inverse direction: events_tx.send failing on a dropped
        // receiver → return).
        let worker = FuzzyWorker::start();
        // Land at least one message so the worker has work to do
        // before tear-down — exercises the full drain path.
        worker.set_index("local".into(), Vec::new(), Vec::new());
        worker.query("local".into(), "x".into());
        drop(worker);
        // Give the worker thread a moment to exit. We can't observe
        // the exit directly without a JoinHandle, but if it didn't
        // exit it would leak the OS thread — `cargo test` would
        // still pass but the test suite's thread count would
        // monotonically grow.
        thread::sleep(std::time::Duration::from_millis(20));
    }
}
