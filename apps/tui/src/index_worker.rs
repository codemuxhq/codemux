//! Off-thread directory index for fuzzy spawn-modal navigation.
//!
//! Walks the configured `[spawn].search_roots` (default `["~"]`)
//! respecting `.gitignore` / `.ignore` (via the `ignore` crate). The
//! result is a flat `Vec<PathBuf>` of every directory under the roots,
//! capped at [`MAX_INDEX_ENTRIES`] to bound memory on unorganized
//! homes.
//!
//! Mirrors the worker pattern in `bootstrap_worker.rs`: a channel of
//! events, a `Drop`-cancellable handle, a detached worker thread. The
//! TUI never blocks on the walk — cold-cache index of `~` is hundreds
//! of ms on a typical dev machine, but the runtime polls the channel
//! every frame and updates the wildmenu live.
//!
//! Index lifetime is **session-long** by design: built lazily on first
//! fuzzy-mode entry, surviving across modal open/close cycles. Manual
//! rebuild via the `RefreshIndex` keybind (default `ctrl+r`) when the
//! user creates a new directory mid-session.

use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::thread;
use std::time::Instant;

use crossbeam_channel::{Receiver, Sender, unbounded};
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Hard cap on indexed directories. Protects memory on machines with an
/// unorganized home (`~/Library`, `~/Downloads`, etc. without
/// `.gitignore`). At ~hundreds of bytes per `PathBuf`, 100k entries is
/// ~tens of MB. If you hit the cap, add a `.gitignore` to the noisy
/// subtree — the wildmenu is more useful with curated roots anyway.
const MAX_INDEX_ENTRIES: usize = 100_000;

/// How often the worker emits a `Progress(count)` event during the
/// walk. Tuned so the wildmenu's "indexing… N dirs" counter ticks at a
/// human-perceptible cadence without overwhelming the channel — ~50
/// progress events on a typical 50k-entry home is one tick per modal
/// frame at most.
const PROGRESS_INTERVAL: usize = 1_000;

/// Stream of events emitted by an [`IndexHandle`]'s worker thread.
/// Modeled as one channel so the receiver can't see `Done` before the
/// final `Progress`. Always terminates with exactly one `Done`; the
/// channel is silent after.
#[derive(Debug)]
pub enum IndexEvent {
    /// Running count of directories discovered so far AND the new
    /// batch of dirs since the last progress event. Emitted every
    /// [`PROGRESS_INTERVAL`] entries during the walk plus a final
    /// flush at the end of each root. The wildmenu renders the count
    /// as `"indexing… {count} dirs"`; the manager appends `batch`
    /// to the in-flight `Building` state's dir list so the user can
    /// fuzzy-search the partial index while the walk continues.
    ///
    /// `batch` entries are classified as [`ProjectKind::Plain`] —
    /// per-dir Git/marker classification only completes at the end
    /// of the walk (markers are discovered alongside dirs). The final
    /// `Done(Ok)` carries the fully-classified version which replaces
    /// the partial accumulated list.
    Progress {
        count: usize,
        batch: Vec<IndexedDir>,
    },
    /// Walk finished. `Ok` carries the discovered directory list with
    /// per-entry project classification (possibly partial if cancel
    /// arrived mid-walk). `Err` is reserved for the all-roots-failed
    /// case; per-entry walker errors are silently skipped.
    Done(Result<Vec<IndexedDir>, IndexError>),
}

/// One indexed directory and the project signals attached to it.
/// `kind` lets the fuzzy matcher boost git repos and project-marker
/// directories above plain ones — the boost magnitudes live in
/// `spawn.rs` next to the matcher itself.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct IndexedDir {
    pub path: PathBuf,
    pub kind: ProjectKind,
}

/// What kind of project (if any) a directory looks like, for fuzzy
/// score boosting. Order is significant — `Git` outranks `Marker`
/// outranks `Plain`. A directory can match more than one signal at
/// once; we record the strongest.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ProjectKind {
    /// Has a `.git` child (a git repo root, including submodules).
    /// Strongest signal — manually-cloned projects are almost always
    /// what the user wants to spawn into.
    Git,
    /// Contains one of the configured `project_markers` filenames
    /// (`Cargo.toml`, `package.json`, `go.mod`, etc.). Indicates a
    /// non-git project root or the same git repo seen via its build
    /// file (still useful — some users disable git for monorepo subs).
    Marker,
    /// No project signal detected — just a directory.
    Plain,
}

/// Errors the indexer can produce. Currently single-variant but kept as
/// an enum so future per-root errors can be added without breaking the
/// caller's match.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum IndexError {
    /// Every configured root failed to open (e.g. nonexistent
    /// directories, permission denied). Carries one entry per failed
    /// root with a human-readable cause.
    #[error("no usable search roots ({} failures)", _0.len())]
    NoRoots(Vec<RootFailure>),
}

/// One root that the walker couldn't usefully process. Named struct
/// instead of `(PathBuf, &'static str)` so the fields are
/// self-documenting at every call site.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RootFailure {
    pub path: PathBuf,
    /// Always a static literal in current code; widened type would be
    /// `Cow<'static, str>` if a dynamic reason ever lands.
    pub reason: &'static str,
}

/// Per-host metadata the runtime needs to persist a completed walk
/// to disk. Stored alongside [`IndexState`] inside [`HostIndex`] so
/// the runtime never has to keep two parallel maps in sync — every
/// SWR trigger sets state and ctx atomically through the catalog API.
///
/// The actual save dispatch (calling into [`crate::index_cache`])
/// happens in the runtime layer; this enum only carries the inputs.
#[derive(Clone, Debug)]
pub enum IndexSaveCtx {
    Local {
        /// Search roots verbatim (the cache file embeds them for
        /// invalidation; passing a `Vec<String>` rather than the
        /// expanded `Vec<PathBuf>` keeps the on-disk shape stable
        /// across machines with different `$HOME`).
        roots: Vec<String>,
    },
    Remote {
        /// SSH host name, used as the `ssh ...` argument.
        host: String,
        /// Cloned from `RemoteFs::socket_path()`. The socket file
        /// is shared — multiple ssh subprocesses can multiplex over
        /// it concurrently — so handing a clone to the save thread
        /// is safe.
        socket: PathBuf,
        /// Search roots verbatim, same role as the local case.
        roots: Vec<String>,
    },
}

/// Per-host bundle: the live walker state plus the persistence
/// metadata the runtime needs to write the disk cache when the
/// walker reports `Done(Ok)`. Bundling these resolves a previously
/// flagged "parallel state" smell where the runtime carried two
/// maps (`HashMap<String, IndexState>` + `HashMap<String,
/// IndexSaveCtx>`) keyed by the same host string and synchronized
/// at every insertion site.
pub struct HostIndex {
    pub state: IndexState,
    pub save_ctx: IndexSaveCtx,
}

/// Lifecycle of the session-long fuzzy directory index. The runtime
/// holds an [`IndexCatalog`] keyed by host so each host (local + each
/// SSH host the user has spawned to) tracks its own state independently.
#[derive(Debug)]
pub enum IndexState {
    /// Index is being built and no prior cached results are available
    /// (first run, or the previous run failed). The runtime drains
    /// events and transitions to `Ready` / `Failed` on `Done`. The
    /// wildmenu shows "indexing… N dirs" until the user types a
    /// query, at which point it scores the partial `dirs` list.
    ///
    /// `dirs` accumulates from the worker's `Progress` batches as the
    /// walk runs, so the user can fuzzy-search what has been
    /// discovered so far instead of waiting for the full walk. Entries
    /// are classified as [`ProjectKind::Plain`] until the terminating
    /// `Done(Ok)` arrives with the fully-classified version.
    Building {
        handle: IndexHandle,
        /// Running count from the most recent `Progress` event, or 0
        /// if none has arrived yet. Surfaced to the modal as the
        /// "indexing… N dirs" counter. Equals `dirs.len()` at the
        /// moment the event is processed.
        count: usize,
        /// Partial dir list accumulated from `Progress` batches. The
        /// fuzzy matcher reads this via [`Self::cached_dirs`] so the
        /// user can search the in-progress index. Replaced atomically
        /// with the fully-classified list on `Done(Ok)`.
        dirs: Vec<IndexedDir>,
    },
    /// Cached results are usable AND a fresh build is in flight. The
    /// modal renders `dirs` immediately (zero-latency open) while the
    /// background walk produces a refreshed snapshot. On the rebuild's
    /// `Done(Ok)` we transition to `Ready { dirs: new_dirs }`; on
    /// `Done(Err)` we fall back to `Ready { dirs }` (keep stale,
    /// log) so the user never sees results disappear because of a
    /// transient walker failure.
    Refreshing {
        dirs: Vec<IndexedDir>,
        handle: IndexHandle,
        /// Same semantics as the `Building` counter — wildmenu does
        /// not surface it (results are usable so no "indexing…"
        /// sentinel) but we keep it for parity / debug logging.
        count: usize,
    },
    /// Index is ready for queries. Sorted ascending by path so display
    /// order is stable and binary search is available if needed.
    Ready { dirs: Vec<IndexedDir> },
    /// Last build attempt failed. Pre-formatted message so the render
    /// path doesn't re-format on every frame.
    Failed { message: String },
}

impl IndexState {
    /// Borrow the cached directory list if any is available.
    /// `Ready` and `Refreshing` carry settled / stale-but-usable
    /// results; `Building` carries the partial accumulator from the
    /// in-flight walker. The modal's fuzzy matcher reads through this
    /// rather than discriminating on the variant itself, so a partial
    /// index is searchable just like a settled one.
    #[must_use]
    pub fn cached_dirs(&self) -> Option<&[IndexedDir]> {
        match self {
            Self::Ready { dirs } | Self::Refreshing { dirs, .. } | Self::Building { dirs, .. } => {
                Some(dirs)
            }
            Self::Failed { .. } => None,
        }
    }

    /// `true` when a worker is currently walking. Used by the runtime
    /// to skip a "rebuild on open" SWR trigger when one is already in
    /// flight (don't double-rebuild).
    #[must_use]
    pub fn is_in_flight(&self) -> bool {
        matches!(self, Self::Building { .. } | Self::Refreshing { .. })
    }
}

/// Handle to an in-flight or completed index walk.
///
/// `Drop` cooperatively signals the worker to exit at the next entry
/// boundary. The worker thread is detached at `thread::spawn` time —
/// the TUI never joins on it.
#[derive(Debug)]
pub struct IndexHandle {
    cancel: Arc<AtomicBool>,
    rx: Receiver<IndexEvent>,
}

impl IndexHandle {
    /// Non-blocking poll for the worker's next event.
    #[must_use]
    pub fn try_recv(&self) -> Option<IndexEvent> {
        self.rx.try_recv().ok()
    }

    /// Signal the worker to stop at the next entry boundary. Idempotent.
    /// Production cancels via `Drop` (a fresh `IndexState` replacement
    /// cancels the previous one); this method exists so tests can
    /// verify cancellation without dropping the receiver.
    #[cfg(test)]
    pub fn cancel(&self) {
        self.cancel.store(true, Relaxed);
    }
}

impl Drop for IndexHandle {
    fn drop(&mut self) {
        self.cancel.store(true, Relaxed);
    }
}

/// Spawn a worker thread that walks `roots` and reports progress via
/// [`IndexHandle`]. Roots must already be tilde-expanded (use
/// [`expand_search_roots`] first). `markers` is the set of project-
/// marker filenames the walker uses to classify each indexed
/// directory — typically `spawn_config.project_markers`.
///
/// **Hold the returned handle.** Dropping it cancels the worker
/// thread immediately (via the `Drop` impl) — `let _ = start_index(…)`
/// would kill the build before it sends a single event.
#[must_use]
pub fn start_index(roots: Vec<PathBuf>, markers: Vec<String>) -> IndexHandle {
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = unbounded();
    let cancel_for_thread = Arc::clone(&cancel);
    thread::spawn(move || {
        run_index_walk(roots, markers, &tx, &cancel_for_thread);
    });
    IndexHandle { cancel, rx }
}

/// Worker body — extracted so tests can drive the walk synchronously
/// without `thread::spawn`. Sends `Progress` events during the walk and
/// exactly one terminating `Done` event.
///
/// Project classification:
///  * `Git` — directory has a `.git` child (separate per-dir stat
///    because `.git` is hidden and the walker skips hidden entries).
///  * `Marker` — directory contains any file whose name matches the
///    `markers` set. Detected during the walk's file iteration, so
///    the cost is only the `HashSet` lookup per file (basically free).
///  * `Plain` — neither.
///
/// `Git` outranks `Marker` when both signals are present.
fn run_index_walk(
    roots: Vec<PathBuf>,
    markers: Vec<String>,
    tx: &Sender<IndexEvent>,
    cancel: &AtomicBool,
) {
    let start = Instant::now();
    let marker_set: HashSet<String> = markers.into_iter().collect();
    let mut state = LocalWalkState::default();

    'roots: for root in roots {
        if cancel.load(Relaxed) {
            break;
        }
        if !root.exists() {
            state.failures.push(RootFailure {
                path: root.clone(),
                reason: "path does not exist",
            });
            continue;
        }
        match walk_one_local_root(&root, &marker_set, tx, cancel, &mut state) {
            LocalRootOutcome::Succeeded => state.succeeded_any = true,
            LocalRootOutcome::FailedWithReason(reason) => {
                state.failures.push(RootFailure {
                    path: root.clone(),
                    reason,
                });
            }
            LocalRootOutcome::HitCap | LocalRootOutcome::Canceled => break 'roots,
        }
    }

    let was_canceled = cancel.load(Relaxed);
    // Cancel always produces `Ok` (possibly partial / empty) so the
    // runtime can choose to keep what was built; treating it as an
    // error would force a useless rebuild on every cancel.
    let result = if was_canceled || state.succeeded_any {
        let mut dirs: Vec<IndexedDir> = state
            .dir_paths
            .into_iter()
            .map(|path| {
                let kind = classify_dir(&path, &state.marker_dirs);
                IndexedDir { path, kind }
            })
            .collect();
        dirs.sort_by(|a, b| a.path.cmp(&b.path));
        let git_count = dirs.iter().filter(|d| d.kind == ProjectKind::Git).count();
        let marker_count = dirs
            .iter()
            .filter(|d| d.kind == ProjectKind::Marker)
            .count();
        tracing::debug!(
            count = dirs.len(),
            git = git_count,
            marker = marker_count,
            elapsed_ms = start.elapsed().as_millis(),
            canceled = was_canceled,
            "fuzzy index: walk complete",
        );
        Ok(dirs)
    } else {
        Err(IndexError::NoRoots(state.failures))
    };
    tx.send(IndexEvent::Done(result)).ok();
}

/// Mutable state accumulated across roots in a single local walk.
/// Bundled so the per-root helper takes one `&mut` rather than five.
/// Mirrors [`RemoteWalkState`] for the remote walker — the two stay
/// in lockstep so the only differences between the loops are the
/// data source (local `WalkBuilder` vs remote `find` lines) and the
/// classification timing.
#[derive(Default)]
struct LocalWalkState {
    dir_paths: Vec<PathBuf>,
    marker_dirs: HashSet<PathBuf>,
    failures: Vec<RootFailure>,
    succeeded_any: bool,
    /// Index up to which `dir_paths` has been streamed via a
    /// `Progress` batch. The slice from here to the current end is
    /// the new-since-last-batch view.
    last_batch_end: usize,
}

/// Per-root completion outcome. `HitCap` and `Canceled` short-circuit
/// the outer `'roots` loop so we don't keep walking once we've
/// truncated the index or the user cancelled the build.
enum LocalRootOutcome {
    Succeeded,
    FailedWithReason(&'static str),
    HitCap,
    Canceled,
}

/// Walk one local root: iterate the `WalkBuilder` entries, partition
/// into directories vs. marker files, accumulate progress batches,
/// and update `state` in place. Returns the per-root outcome for the
/// caller's success/failure tally.
fn walk_one_local_root(
    root: &Path,
    marker_set: &HashSet<String>,
    tx: &Sender<IndexEvent>,
    cancel: &AtomicBool,
    state: &mut LocalWalkState,
) -> LocalRootOutcome {
    tracing::debug!(root = %root.display(), "fuzzy index: starting walk");
    // Default `WalkBuilder` already enables `git_ignore`,
    // `git_global`, `git_exclude`, `parents`, and skips hidden
    // entries. We rely on those defaults — explicit calls would
    // just repeat them.
    let walker = WalkBuilder::new(root).build();
    let mut root_succeeded = false;
    for entry in walker {
        if cancel.load(Relaxed) {
            return LocalRootOutcome::Canceled;
        }
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::trace!(error = %e, "fuzzy index: walker entry error (skipped)");
                continue;
            }
        };
        let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
        if is_dir {
            state.dir_paths.push(entry.into_path());
            root_succeeded = true;
            if state.dir_paths.len() >= MAX_INDEX_ENTRIES {
                tracing::warn!(
                    cap = MAX_INDEX_ENTRIES,
                    "fuzzy index: hit entry cap; truncating",
                );
                return LocalRootOutcome::HitCap;
            }
            if state.dir_paths.len().is_multiple_of(PROGRESS_INTERVAL) {
                let batch = batch_as_plain(&state.dir_paths[state.last_batch_end..]);
                state.last_batch_end = state.dir_paths.len();
                tx.send(IndexEvent::Progress {
                    count: state.dir_paths.len(),
                    batch,
                })
                .ok();
            }
        } else if let Some(name) = entry.path().file_name().and_then(|n| n.to_str())
            && marker_set.contains(name)
            && let Some(parent) = entry.path().parent()
        {
            // The marker file flags its parent dir as a project.
            state.marker_dirs.insert(parent.to_path_buf());
        }
    }
    // Per-root flush so small roots (< PROGRESS_INTERVAL) still
    // give the user a partial searchable index instead of waiting
    // for the terminating `Done`.
    if state.dir_paths.len() > state.last_batch_end {
        let batch = batch_as_plain(&state.dir_paths[state.last_batch_end..]);
        state.last_batch_end = state.dir_paths.len();
        tx.send(IndexEvent::Progress {
            count: state.dir_paths.len(),
            batch,
        })
        .ok();
    }
    if root_succeeded {
        LocalRootOutcome::Succeeded
    } else {
        LocalRootOutcome::FailedWithReason("no readable entries")
    }
}

/// Classify a single directory against the discovered marker-dir set
/// and a per-dir `.git` stat. `Git` outranks `Marker` outranks `Plain`.
#[must_use]
fn classify_dir(path: &Path, marker_dirs: &HashSet<PathBuf>) -> ProjectKind {
    if path.join(".git").exists() {
        ProjectKind::Git
    } else if marker_dirs.contains(path) {
        ProjectKind::Marker
    } else {
        ProjectKind::Plain
    }
}

/// Wrap a slice of newly-discovered paths as `Plain`-classified
/// [`IndexedDir`] entries for an in-flight `Progress` batch. Final
/// per-dir classification (Git / Marker) only completes when the
/// walk finishes — markers are discovered alongside dirs, so a
/// directory streamed in the first batch may have its marker file
/// found in a later batch. Sending `Plain` here keeps the partial
/// index searchable with the worst-case classification; the
/// terminating `Done(Ok)` carries the corrected version.
#[must_use]
fn batch_as_plain(paths: &[PathBuf]) -> Vec<IndexedDir> {
    paths
        .iter()
        .map(|p| IndexedDir {
            path: p.clone(),
            kind: ProjectKind::Plain,
        })
        .collect()
}

/// Tilde-expand each root entry using `$HOME`. Roots starting with `~`
/// are joined with the user's home directory; absolute / relative paths
/// pass through unchanged. If `$HOME` is unset, tilde-prefixed entries
/// are dropped with a warning rather than panicking.
#[must_use]
pub fn expand_search_roots(roots: &[String]) -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    roots
        .iter()
        .filter_map(|r| expand_one(r, home.as_deref()))
        .collect()
}

fn expand_one(root: &str, home: Option<&Path>) -> Option<PathBuf> {
    if root == "~" {
        let Some(h) = home else {
            tracing::warn!("fuzzy index: $HOME unset; skipping ~");
            return None;
        };
        return Some(h.to_path_buf());
    }
    if let Some(rest) = root.strip_prefix("~/") {
        let Some(h) = home else {
            tracing::warn!(root = %root, "fuzzy index: $HOME unset; skipping ~");
            return None;
        };
        return Some(h.join(rest));
    }
    Some(PathBuf::from(root))
}

/// Tilde-expand each root entry against `remote_home` (the SSH host's
/// `$HOME`, captured during prepare). Mirrors [`expand_search_roots`]
/// for the remote case where local `$HOME` is irrelevant. Roots
/// without a leading `~` pass through as absolute remote paths.
#[must_use]
pub fn expand_remote_roots(roots: &[String], remote_home: &Path) -> Vec<PathBuf> {
    roots
        .iter()
        .filter_map(|r| {
            if r == "~" {
                Some(remote_home.to_path_buf())
            } else if let Some(rest) = r.strip_prefix("~/") {
                Some(remote_home.join(rest))
            } else if r.starts_with('/') {
                Some(PathBuf::from(r))
            } else {
                tracing::warn!(root = %r, "remote fuzzy index: dropping non-absolute root");
                None
            }
        })
        .collect()
}

/// Spawn a worker thread that runs `find` over the supplied SSH
/// `ControlMaster` socket, streams its stdout, and produces the same
/// [`IndexEvent`] shape as the local walker. The roots must already
/// be tilde-expanded against the remote `$HOME` (use
/// [`expand_remote_roots`]).
///
/// Cancellation is best-effort and **coarse**: the worker checks the
/// cancel flag between roots, but the find process itself runs to
/// completion (we don't kill it mid-walk in v1). Most users have a
/// single root, so this is rarely user-visible. A force-rebuild
/// during a walk leaves the old worker running until find finishes;
/// its Done event lands on a dropped receiver and is discarded.
///
/// **Hold the returned handle.** Dropping it sets the cancel flag,
/// which the worker honors at root boundaries.
#[must_use]
pub fn start_index_remote(
    host: String,
    socket: PathBuf,
    roots: Vec<PathBuf>,
    markers: Vec<String>,
) -> IndexHandle {
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = unbounded();
    let cancel_for_thread = Arc::clone(&cancel);
    thread::spawn(move || {
        run_remote_index_walk(&host, &socket, roots, &markers, &tx, &cancel_for_thread);
    });
    IndexHandle { cancel, rx }
}

/// Worker body for the remote walk. Extracted so its happy-path can
/// run on a real subprocess (no test mock — `ssh` would be required)
/// while the line parser is exercised independently via
/// [`classify_remote_line`] tests.
///
/// One `find` invocation per root. The expression prints:
///   * every `.git` directory (so [`classify_remote_line`] can flag
///     its parent as Git, and `.git` itself is not descended into),
///   * every other non-hidden directory (the index entries),
///   * every marker file (so its parent is flagged as Marker).
///
/// Hidden directories other than `.git` are pruned to match the
/// local walker's `ignore`-crate default of skipping hidden files.
fn run_remote_index_walk(
    host: &str,
    socket: &Path,
    roots: Vec<PathBuf>,
    markers: &[String],
    tx: &Sender<IndexEvent>,
    cancel: &AtomicBool,
) {
    let start = Instant::now();
    let marker_set: HashSet<String> = markers.iter().cloned().collect();
    let mut state = RemoteWalkState::default();

    'roots: for root in roots {
        if cancel.load(Relaxed) {
            break;
        }
        let walk_outcome =
            walk_one_remote_root(host, socket, &root, markers, &marker_set, tx, &mut state);
        match walk_outcome {
            RootOutcome::Succeeded => {
                state.succeeded_any = true;
            }
            RootOutcome::FailedWithReason(reason) => {
                state.failures.push(RootFailure {
                    path: root.clone(),
                    reason,
                });
            }
            RootOutcome::HitCap => break 'roots,
        }
    }

    let was_canceled = cancel.load(Relaxed);
    let result = if was_canceled || state.succeeded_any {
        let dirs = finalize_remote_dirs(state.dir_paths, &state.git_dirs, &state.marker_dirs);
        let git_count = dirs.iter().filter(|d| d.kind == ProjectKind::Git).count();
        let marker_count = dirs
            .iter()
            .filter(|d| d.kind == ProjectKind::Marker)
            .count();
        tracing::debug!(
            host = %host,
            count = dirs.len(),
            git = git_count,
            marker = marker_count,
            elapsed_ms = start.elapsed().as_millis(),
            canceled = was_canceled,
            "remote fuzzy index: walk complete",
        );
        Ok(dirs)
    } else {
        Err(IndexError::NoRoots(state.failures))
    };
    tx.send(IndexEvent::Done(result)).ok();
}

/// Mutable state accumulated across roots in a single remote walk.
/// Bundled so the helper functions below take one `&mut` rather than
/// five.
#[derive(Default)]
struct RemoteWalkState {
    dir_paths: Vec<PathBuf>,
    git_dirs: HashSet<PathBuf>,
    marker_dirs: HashSet<PathBuf>,
    failures: Vec<RootFailure>,
    succeeded_any: bool,
    /// Index up to which `dir_paths` has been streamed via a
    /// `Progress` batch. Same role as the local walker's local
    /// counterpart — the slice from here to the current end is the
    /// new-since-last-batch view.
    last_batch_end: usize,
}

/// Per-root completion outcome. `HitCap` short-circuits the outer
/// `'roots` loop so we don't keep walking after we've already
/// truncated the index at [`MAX_INDEX_ENTRIES`].
enum RootOutcome {
    Succeeded,
    FailedWithReason(&'static str),
    HitCap,
}

/// Walk one remote root: spawn ssh+find, stream stdout, classify each
/// line, and update `state` in place. Returns the per-root outcome
/// for the caller's success/failure tally.
#[allow(clippy::too_many_arguments)]
fn walk_one_remote_root(
    host: &str,
    socket: &Path,
    root: &Path,
    markers: &[String],
    marker_set: &HashSet<String>,
    tx: &Sender<IndexEvent>,
    state: &mut RemoteWalkState,
) -> RootOutcome {
    let Some(find_cmd) = build_remote_find_cmd(root, markers) else {
        return RootOutcome::FailedWithReason("unsafe path or marker (rejected pre-shell)");
    };
    tracing::debug!(host, root = %root.display(), "remote fuzzy index: starting walk");
    let socket_str = socket.to_string_lossy().into_owned();
    let mut child = match Command::new("ssh")
        .args([
            "-S",
            &socket_str,
            "-o",
            "BatchMode=yes",
            host,
            "--",
            &find_cmd,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(error = %e, "remote fuzzy index: ssh spawn failed");
            return RootOutcome::FailedWithReason("ssh spawn failed");
        }
    };
    let Some(stdout) = child.stdout.take() else {
        return RootOutcome::FailedWithReason("ssh stdout missing");
    };
    let reader = BufReader::new(stdout);
    let mut root_succeeded = false;
    let mut hit_cap = false;
    for line in reader.lines() {
        let Ok(line) = line else {
            tracing::trace!("remote fuzzy index: stdout read error (skipped)");
            continue;
        };
        match classify_remote_line(&line, marker_set) {
            RemoteFindLine::GitMarker(parent) => {
                state.git_dirs.insert(parent);
            }
            RemoteFindLine::MarkerFile(parent) => {
                state.marker_dirs.insert(parent);
            }
            RemoteFindLine::Directory(path) => {
                state.dir_paths.push(path);
                root_succeeded = true;
                if state.dir_paths.len() >= MAX_INDEX_ENTRIES {
                    tracing::warn!(
                        cap = MAX_INDEX_ENTRIES,
                        host,
                        "remote fuzzy index: hit entry cap; truncating",
                    );
                    let _ = child.kill();
                    hit_cap = true;
                    break;
                }
                if state.dir_paths.len().is_multiple_of(PROGRESS_INTERVAL) {
                    let batch = batch_as_plain(&state.dir_paths[state.last_batch_end..]);
                    state.last_batch_end = state.dir_paths.len();
                    tx.send(IndexEvent::Progress {
                        count: state.dir_paths.len(),
                        batch,
                    })
                    .ok();
                }
            }
            RemoteFindLine::Skip => {}
        }
    }
    let status = child.wait();
    if !matches!(&status, Ok(s) if s.success()) {
        tracing::debug!(
            host,
            status = ?status,
            "remote fuzzy index: find exited non-zero (results so far kept)",
        );
    }
    // Per-root flush so small remote roots also produce a partial
    // searchable index without waiting for the terminating `Done`.
    if state.dir_paths.len() > state.last_batch_end {
        let batch = batch_as_plain(&state.dir_paths[state.last_batch_end..]);
        state.last_batch_end = state.dir_paths.len();
        tx.send(IndexEvent::Progress {
            count: state.dir_paths.len(),
            batch,
        })
        .ok();
    }
    if hit_cap {
        RootOutcome::HitCap
    } else if root_succeeded {
        RootOutcome::Succeeded
    } else {
        RootOutcome::FailedWithReason("no readable entries")
    }
}

/// Convert the accumulated path list into classified [`IndexedDir`]
/// entries, sorted ascending by path so display order is stable
/// across runs.
fn finalize_remote_dirs(
    dir_paths: Vec<PathBuf>,
    git_dirs: &HashSet<PathBuf>,
    marker_dirs: &HashSet<PathBuf>,
) -> Vec<IndexedDir> {
    let mut dirs: Vec<IndexedDir> = dir_paths
        .into_iter()
        .map(|path| {
            let kind = if git_dirs.contains(&path) {
                ProjectKind::Git
            } else if marker_dirs.contains(&path) {
                ProjectKind::Marker
            } else {
                ProjectKind::Plain
            };
            IndexedDir { path, kind }
        })
        .collect();
    dirs.sort_by(|a, b| a.path.cmp(&b.path));
    dirs
}

/// Classification result for a single line of `find` output. Pure on
/// the input so the parser is exercised in isolation by tests; the
/// worker just dispatches on the variant.
#[derive(Debug, Eq, PartialEq)]
enum RemoteFindLine {
    /// The line is a `.git` directory; flag the parent as Git and
    /// skip indexing the `.git` entry itself.
    GitMarker(PathBuf),
    /// The line is a configured marker file; flag the parent as
    /// Marker and skip indexing the file itself.
    MarkerFile(PathBuf),
    /// A normal directory entry to index.
    Directory(PathBuf),
    /// Empty line / no parent / non-UTF-8 — silently skip.
    Skip,
}

/// Pure parser: classify one line of remote `find` output against
/// the marker set. The find expression we ship guarantees the line
/// is either a directory path, a `.git` directory path, or a marker
/// file path. We branch on basename:
///
///   * basename `== ".git"` → [`RemoteFindLine::GitMarker`] of the
///     parent dir.
///   * basename in `markers` → [`RemoteFindLine::MarkerFile`] of the
///     parent dir.
///   * else → [`RemoteFindLine::Directory`] of the line itself.
fn classify_remote_line(line: &str, markers: &HashSet<String>) -> RemoteFindLine {
    let trimmed = line.trim_end_matches('\r');
    if trimmed.is_empty() {
        return RemoteFindLine::Skip;
    }
    let path = PathBuf::from(trimmed);
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return RemoteFindLine::Skip;
    };
    if name == ".git" {
        return parent_or_skip(&path).map_or(RemoteFindLine::Skip, RemoteFindLine::GitMarker);
    }
    if markers.contains(name) {
        return parent_or_skip(&path).map_or(RemoteFindLine::Skip, RemoteFindLine::MarkerFile);
    }
    RemoteFindLine::Directory(path)
}

/// Return the parent path only if it's a non-empty path. `Path::parent`
/// returns `Some("")` for a bare basename like `.git` (no `/` in the
/// input), which is not a usable directory — we treat it as Skip.
fn parent_or_skip(path: &Path) -> Option<PathBuf> {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
}

/// Build the remote-side `find` command string. Returns `None` if
/// the root path or any marker filename contains shell-unsafe
/// characters — a defensive validation that mirrors the
/// `RemoteFs::list_dir` policy of rejecting paths with `'`.
///
/// The `exec` prefix is intentional: when the local ssh subprocess
/// is killed (cancel-on-Drop or process exit) the remote sshd sends
/// SIGHUP to the spawned shell. With `exec`, find replaces the shell
/// in the same process, so SIGHUP terminates the find tree directly
/// instead of relying on bash's `huponexit` setting (which defaults
/// to off in non-interactive shells). Without it, we'd be guaranteed
/// to leak a remote `find` running for tens of seconds after every
/// SWR cancel — visible as remote CPU + IO that outlasts the user's
/// session.
///
/// The command's structure:
/// ```text
/// exec find 'ROOT' \(
///     -name .git -prune -print
///     -o -name '.*' -prune
///     -o -type d -print
///     -o -type f \( -name 'M1' -o -name 'M2' \) -print
/// \) 2>/dev/null
/// ```
///
/// `-prune` keeps `find` from descending into `.git` and other
/// hidden directories (matching the local `ignore`-crate default).
/// `2>/dev/null` swallows permission-denied noise so a single
/// unreadable subdir doesn't garble the line stream.
#[must_use]
fn build_remote_find_cmd(root: &Path, markers: &[String]) -> Option<String> {
    let root_str = root.to_str()?;
    if root_str.contains('\'') {
        return None;
    }
    for m in markers {
        if !m
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
        {
            tracing::debug!(marker = %m, "remote fuzzy index: rejecting unsafe marker");
            return None;
        }
    }
    let marker_branch = if markers.is_empty() {
        String::new()
    } else {
        let names = markers
            .iter()
            .map(|m| format!("-name '{m}'"))
            .collect::<Vec<_>>()
            .join(" -o ");
        format!(" -o -type f \\( {names} \\) -print")
    };
    Some(format!(
        "exec find '{root_str}' \\( -name .git -prune -print \
         -o -name '.*' -prune \
         -o -type d -print{marker_branch} \\) 2>/dev/null"
    ))
}

/// Per-host directory index store. Keyed by host string — local uses
/// the [`HOST_PLACEHOLDER`](crate::spawn::HOST_PLACEHOLDER) sentinel
/// (`"local"`), SSH uses the user-supplied host name. The runtime owns
/// one `IndexCatalog` for the whole session; per-host states track
/// independent walks so an in-flight SSH index can't preempt the
/// already-`Ready` local one.
///
/// All mutation goes through the helper methods so the SWR contract
/// (preserve `Ready { dirs }` cache when starting a refresh) is
/// enforced in one place.
#[derive(Default)]
pub struct IndexCatalog {
    hosts: std::collections::HashMap<String, HostIndex>,
}

impl IndexCatalog {
    /// Construct an empty catalog.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Convenience: borrow only the [`IndexState`] for `host`. The
    /// modal's `notify_index_state` and the wildmenu render path
    /// don't need the save context, just the state.
    #[must_use]
    pub fn state_for(&self, host: &str) -> Option<&IndexState> {
        self.hosts.get(host).map(|h| &h.state)
    }

    /// Iterate the in-flight (`Building` / `Refreshing`) entries
    /// mutably so the runtime can drain their event channels each
    /// frame.
    pub fn iter_in_flight_mut(&mut self) -> impl Iterator<Item = (&String, &mut HostIndex)> {
        self.hosts
            .iter_mut()
            .filter(|(_, h)| h.state.is_in_flight())
    }

    /// Replace the state for `host` while preserving its existing
    /// `save_ctx`. Used by the runtime when a `Done` event arrives
    /// (the drain code computes the next state and calls this).
    /// Returns the prior `HostIndex` if any, mostly for debug logging.
    pub fn set_state(&mut self, host: &str, state: IndexState) {
        if let Some(slot) = self.hosts.get_mut(host) {
            slot.state = state;
        }
    }

    /// Hydrate a host with `Ready { dirs }` from a disk cache. The
    /// runtime calls this at startup (local) or right after the SSH
    /// prepare succeeds (remote). `save_ctx` is set atomically with
    /// the state so the next `Done(Ok)` knows where to persist.
    pub fn hydrate(&mut self, host: String, save_ctx: IndexSaveCtx, dirs: Vec<IndexedDir>) {
        self.hosts.insert(
            host,
            HostIndex {
                state: IndexState::Ready { dirs },
                save_ctx,
            },
        );
    }

    /// SWR trigger: start a fresh walk for `host`, preserving any
    /// cached `Ready { dirs }` as the `Refreshing` carrier. Returns
    /// `Started` if a new worker was spawned, `Skipped` if the host
    /// was already in flight (Building or Refreshing). The caller
    /// supplies a thunk that produces the [`IndexHandle`] so the
    /// same code path drives both local and remote walkers, plus the
    /// `save_ctx` to remember for the persist-on-Done step.
    pub fn start_refresh<F>(
        &mut self,
        host: String,
        save_ctx: IndexSaveCtx,
        make_handle: F,
    ) -> RefreshOutcome
    where
        F: FnOnce() -> IndexHandle,
    {
        // Re-using the same exhaustive `remove` + classify pattern
        // as before; the bundled save_ctx threads through every
        // arm so a refresh always pairs the right ctx with the
        // host's new state.
        match self.hosts.remove(&host) {
            Some(HostIndex {
                state:
                    IndexState::Building {
                        handle,
                        count,
                        dirs,
                    },
                save_ctx: prior_ctx,
            }) => {
                self.hosts.insert(
                    host,
                    HostIndex {
                        state: IndexState::Building {
                            handle,
                            count,
                            dirs,
                        },
                        save_ctx: prior_ctx,
                    },
                );
                RefreshOutcome::Skipped
            }
            Some(HostIndex {
                state:
                    IndexState::Refreshing {
                        dirs,
                        handle,
                        count,
                    },
                save_ctx: prior_ctx,
            }) => {
                self.hosts.insert(
                    host,
                    HostIndex {
                        state: IndexState::Refreshing {
                            dirs,
                            handle,
                            count,
                        },
                        save_ctx: prior_ctx,
                    },
                );
                RefreshOutcome::Skipped
            }
            Some(HostIndex {
                state: IndexState::Ready { dirs },
                ..
            }) => {
                self.hosts.insert(
                    host,
                    HostIndex {
                        state: IndexState::Refreshing {
                            dirs,
                            handle: make_handle(),
                            count: 0,
                        },
                        save_ctx,
                    },
                );
                RefreshOutcome::Started
            }
            Some(HostIndex {
                state: IndexState::Failed { .. },
                ..
            })
            | None => {
                self.hosts.insert(
                    host,
                    HostIndex {
                        state: IndexState::Building {
                            handle: make_handle(),
                            count: 0,
                            dirs: Vec::new(),
                        },
                        save_ctx,
                    },
                );
                RefreshOutcome::Started
            }
        }
    }

    /// Force-rebuild trigger (Ctrl-R). Always cancels any in-flight
    /// build (via Drop on the replaced handle) and starts a fresh
    /// one. If a cached `Ready { dirs }` was present, it's preserved
    /// so the modal keeps rendering results during the rebuild.
    pub fn force_rebuild<F>(&mut self, host: String, save_ctx: IndexSaveCtx, make_handle: F)
    where
        F: FnOnce() -> IndexHandle,
    {
        let cached = match self.hosts.remove(&host) {
            Some(HostIndex {
                state: IndexState::Ready { dirs } | IndexState::Refreshing { dirs, .. },
                ..
            }) => Some(dirs),
            // A force-rebuild over a partial Building keeps the
            // partial accumulator as the visible cache via
            // `Refreshing` — the user requested a fresh walk, but
            // dropping the in-progress results would visibly empty
            // their wildmenu mid-search.
            Some(HostIndex {
                state: IndexState::Building { dirs, .. },
                ..
            }) if !dirs.is_empty() => Some(dirs),
            _ => None,
        };
        let new_state = match cached {
            Some(dirs) => IndexState::Refreshing {
                dirs,
                handle: make_handle(),
                count: 0,
            },
            None => IndexState::Building {
                handle: make_handle(),
                count: 0,
                dirs: Vec::new(),
            },
        };
        self.hosts.insert(
            host,
            HostIndex {
                state: new_state,
                save_ctx,
            },
        );
    }
}

/// Result of an SWR refresh attempt — drives logging at the call
/// site (no need to `match` on the catalog state directly).
#[derive(Debug, Eq, PartialEq)]
pub enum RefreshOutcome {
    /// A new worker was spawned. If a cache existed, the host moved to
    /// `Refreshing`; otherwise to `Building`.
    Started,
    /// A worker was already in flight; no-op.
    Skipped,
}

/// Construct an inert [`IndexHandle`] with no live worker. The
/// channel is empty (so `try_recv` always returns `None`) and the
/// cancel flag is wired up but no thread observes it. Useful in
/// tests that need to construct `IndexState::Building` /
/// `IndexState::Refreshing` without spawning real I/O.
#[cfg(test)]
#[must_use]
pub(crate) fn inert_handle_for_test() -> IndexHandle {
    let cancel = Arc::new(AtomicBool::new(false));
    let (_tx, rx) = unbounded::<IndexEvent>();
    IndexHandle { cancel, rx }
}

/// Construct an [`IndexHandle`] paired with its sender so tests can
/// push synthetic events directly. Used by `index_manager`'s drain
/// tests to exercise the Building / Refreshing Progress accumulation
/// paths without spinning up a real walker thread.
#[cfg(test)]
#[must_use]
pub(crate) fn handle_with_sender_for_test() -> (IndexHandle, Sender<IndexEvent>) {
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = unbounded::<IndexEvent>();
    (IndexHandle { cancel, rx }, tx)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;
    use tempfile::TempDir;

    /// Drain events until Done arrives (or a fixed timeout). Returns the
    /// terminating Done payload along with any Progress counts seen.
    fn drain_to_done(
        rx: &Receiver<IndexEvent>,
        timeout: Duration,
    ) -> (Vec<usize>, Result<Vec<IndexedDir>, IndexError>) {
        let deadline = Instant::now() + timeout;
        let mut progress = Vec::new();
        loop {
            assert!(
                Instant::now() <= deadline,
                "timed out waiting for IndexEvent::Done",
            );
            match rx.try_recv() {
                Ok(IndexEvent::Progress { count, .. }) => progress.push(count),
                Ok(IndexEvent::Done(result)) => return (progress, result),
                Err(crossbeam_channel::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    panic!("worker channel disconnected before Done");
                }
            }
        }
    }

    /// Default empty marker set — most tests don't care about
    /// classification, just that paths are present.
    fn no_markers() -> Vec<String> {
        Vec::new()
    }

    #[test]
    fn tmpdir_with_nested_dirs_yields_all_dirs() {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join("alpha")).unwrap();
        fs::create_dir(dir.path().join("beta")).unwrap();
        fs::create_dir(dir.path().join("alpha").join("gamma")).unwrap();

        let handle = start_index(vec![dir.path().to_path_buf()], no_markers());
        let (_progress, result) = drain_to_done(&handle.rx, Duration::from_secs(2));
        let dirs = result.unwrap();

        assert!(
            dirs.iter().any(|d| d.path.ends_with("alpha")),
            "alpha missing: {dirs:?}",
        );
        assert!(dirs.iter().any(|d| d.path.ends_with("beta")));
        assert!(dirs.iter().any(|d| d.path.ends_with("gamma")));
        // Without markers and without `.git`, every dir is Plain.
        assert!(dirs.iter().all(|d| d.kind == ProjectKind::Plain));
    }

    #[test]
    fn nonexistent_root_returns_no_roots_error() {
        let handle = start_index(
            vec![PathBuf::from(
                "/definitely/does/not/exist/codemux-fuzzy-test",
            )],
            no_markers(),
        );
        let (_progress, result) = drain_to_done(&handle.rx, Duration::from_secs(2));
        match result {
            Err(IndexError::NoRoots(failures)) => {
                assert_eq!(failures.len(), 1);
            }
            other => panic!("expected NoRoots, got {other:?}"),
        }
    }

    #[test]
    fn ignore_file_excludes_listed_dirs() {
        // The `ignore` crate respects `.ignore` files everywhere by
        // default — no git repo required. We use `.ignore` rather than
        // `.gitignore` so the test isn't sensitive to whether the
        // tmpdir is inside a git repo.
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join("kept")).unwrap();
        fs::create_dir(dir.path().join("vendor")).unwrap();
        fs::create_dir(dir.path().join("vendor").join("nested")).unwrap();
        fs::write(dir.path().join(".ignore"), "vendor\n").unwrap();

        let handle = start_index(vec![dir.path().to_path_buf()], no_markers());
        let (_progress, result) = drain_to_done(&handle.rx, Duration::from_secs(2));
        let dirs = result.unwrap();

        assert!(dirs.iter().any(|d| d.path.ends_with("kept")));
        assert!(
            !dirs.iter().any(|d| d.path.ends_with("vendor")),
            "vendor should be ignored: {dirs:?}",
        );
    }

    #[test]
    fn cancel_during_walk_terminates_quickly() {
        let dir = TempDir::new().unwrap();
        // Build a tree with enough entries that cancel has work to interrupt.
        for i in 0..200 {
            let sub = dir.path().join(format!("d{i:04}"));
            fs::create_dir(&sub).unwrap();
            for j in 0..10 {
                fs::create_dir(sub.join(format!("inner{j:02}"))).unwrap();
            }
        }
        let handle = start_index(vec![dir.path().to_path_buf()], no_markers());
        handle.cancel();
        // Done still arrives (worker emits it after the cancel
        // observation), within a generous bound.
        let _ = drain_to_done(&handle.rx, Duration::from_secs(5));
    }

    #[test]
    fn progress_events_emit_during_large_walk() {
        let dir = TempDir::new().unwrap();
        // Need >= PROGRESS_INTERVAL = 1000 entries to see at least one Progress.
        for i in 0..1100 {
            fs::create_dir(dir.path().join(format!("d{i:04}"))).unwrap();
        }
        let handle = start_index(vec![dir.path().to_path_buf()], no_markers());
        let (progress, result) = drain_to_done(&handle.rx, Duration::from_secs(5));
        result.unwrap();
        assert!(
            !progress.is_empty(),
            "expected at least one Progress event for 1100-entry walk",
        );
    }

    #[test]
    fn marker_files_classify_their_parent_as_marker_dir() {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join("rust_proj")).unwrap();
        fs::write(dir.path().join("rust_proj").join("Cargo.toml"), "").unwrap();
        fs::create_dir(dir.path().join("not_a_proj")).unwrap();
        fs::write(dir.path().join("not_a_proj").join("README.md"), "").unwrap();

        let handle = start_index(
            vec![dir.path().to_path_buf()],
            vec!["Cargo.toml".to_string()],
        );
        let (_progress, result) = drain_to_done(&handle.rx, Duration::from_secs(2));
        let dirs = result.unwrap();

        let rust = dirs.iter().find(|d| d.path.ends_with("rust_proj")).unwrap();
        assert_eq!(rust.kind, ProjectKind::Marker);

        let plain = dirs
            .iter()
            .find(|d| d.path.ends_with("not_a_proj"))
            .unwrap();
        assert_eq!(plain.kind, ProjectKind::Plain);
    }

    #[test]
    fn git_dir_outranks_marker_classification() {
        // A directory with both `.git` and `Cargo.toml` should be Git,
        // not Marker — Git is the strongest signal.
        let dir = TempDir::new().unwrap();
        let proj = dir.path().join("repo");
        fs::create_dir(&proj).unwrap();
        fs::create_dir(proj.join(".git")).unwrap();
        fs::write(proj.join("Cargo.toml"), "").unwrap();

        let handle = start_index(
            vec![dir.path().to_path_buf()],
            vec!["Cargo.toml".to_string()],
        );
        let (_progress, result) = drain_to_done(&handle.rx, Duration::from_secs(2));
        let dirs = result.unwrap();

        let proj_entry = dirs.iter().find(|d| d.path.ends_with("repo")).unwrap();
        assert_eq!(proj_entry.kind, ProjectKind::Git);
    }

    #[test]
    fn classify_dir_helper_picks_strongest_signal() {
        let tmp = TempDir::new().unwrap();
        let git_repo = tmp.path().join("git_repo");
        fs::create_dir(&git_repo).unwrap();
        fs::create_dir(git_repo.join(".git")).unwrap();

        let plain_dir = tmp.path().join("plain");
        fs::create_dir(&plain_dir).unwrap();

        let marker_dir = tmp.path().join("marker");
        fs::create_dir(&marker_dir).unwrap();

        let mut marker_set = HashSet::new();
        marker_set.insert(marker_dir.clone());

        assert_eq!(classify_dir(&git_repo, &marker_set), ProjectKind::Git);
        assert_eq!(classify_dir(&marker_dir, &marker_set), ProjectKind::Marker);
        assert_eq!(classify_dir(&plain_dir, &marker_set), ProjectKind::Plain);
    }

    #[test]
    fn expand_one_handles_tilde_absolute_and_missing_home() {
        let home = PathBuf::from("/home/test");
        assert_eq!(
            expand_one("~", Some(&home)),
            Some(PathBuf::from("/home/test")),
        );
        assert_eq!(
            expand_one("~/code", Some(&home)),
            Some(PathBuf::from("/home/test/code")),
        );
        assert_eq!(
            expand_one("/abs/path", Some(&home)),
            Some(PathBuf::from("/abs/path")),
        );
        assert_eq!(
            expand_one("/abs/path", None),
            Some(PathBuf::from("/abs/path")),
        );
        // HOME unset + ~ → dropped (warning logged, not asserted).
        assert_eq!(expand_one("~", None), None);
        assert_eq!(expand_one("~/code", None), None);
    }

    // ── IndexState helpers ───────────────────────────────────────
    //
    // `cached_dirs` and `is_in_flight` are the two read-side queries
    // every modal-render and every drain-loop iteration relies on.
    // They are exhaustive over the four-variant enum, so a future
    // variant addition should require updating both.

    fn ready_state(paths: &[&str]) -> IndexState {
        IndexState::Ready {
            dirs: paths
                .iter()
                .map(|p| IndexedDir {
                    path: PathBuf::from(p),
                    kind: ProjectKind::Plain,
                })
                .collect(),
        }
    }

    fn fresh_handle() -> IndexHandle {
        super::inert_handle_for_test()
    }

    #[test]
    fn cached_dirs_returns_some_for_ready_and_refreshing() {
        let ready = ready_state(&["/a"]);
        assert!(ready.cached_dirs().is_some_and(|d| d.len() == 1));

        let refreshing = IndexState::Refreshing {
            dirs: vec![IndexedDir {
                path: PathBuf::from("/b"),
                kind: ProjectKind::Plain,
            }],
            handle: fresh_handle(),
            count: 42,
        };
        assert!(refreshing.cached_dirs().is_some_and(|d| d.len() == 1));
    }

    #[test]
    fn cached_dirs_returns_partial_for_building() {
        // Building with a partial accumulator is still searchable —
        // the user can fuzzy-match against the in-progress index
        // instead of waiting for the walk to finish.
        let building = IndexState::Building {
            handle: fresh_handle(),
            count: 2,
            dirs: vec![
                IndexedDir {
                    path: PathBuf::from("/p1"),
                    kind: ProjectKind::Plain,
                },
                IndexedDir {
                    path: PathBuf::from("/p2"),
                    kind: ProjectKind::Plain,
                },
            ],
        };
        assert_eq!(building.cached_dirs().map(<[_]>::len), Some(2));
    }

    #[test]
    fn cached_dirs_returns_none_for_failed() {
        let failed = IndexState::Failed {
            message: "boom".into(),
        };
        assert!(failed.cached_dirs().is_none());
    }

    #[test]
    fn is_in_flight_true_for_building_and_refreshing_only() {
        assert!(
            IndexState::Building {
                handle: fresh_handle(),
                count: 0,
                dirs: Vec::new(),
            }
            .is_in_flight()
        );
        assert!(
            IndexState::Refreshing {
                dirs: Vec::new(),
                handle: fresh_handle(),
                count: 0,
            }
            .is_in_flight()
        );
        assert!(!ready_state(&["/a"]).is_in_flight());
        assert!(
            !IndexState::Failed {
                message: "x".into(),
            }
            .is_in_flight()
        );
    }

    // ── IndexCatalog ─────────────────────────────────────────────
    //
    // The catalog encapsulates the SWR contract — a refresh on a
    // `Ready` host preserves the cache as `Refreshing { dirs, .. }`,
    // and a refresh on an in-flight host is a no-op (no double-walk).
    // `save_ctx` rides along with each entry so the runtime never
    // needs a parallel sidecar map for persistence metadata.

    fn dummy_ctx() -> IndexSaveCtx {
        IndexSaveCtx::Local { roots: Vec::new() }
    }

    /// Wrap `set_state` plus an explicit hydrate so the prior tests
    /// (which constructed `IndexState::Building { ... }` directly)
    /// keep working without restating the bundled `HostIndex`
    /// boilerplate at every call site.
    fn install(cat: &mut IndexCatalog, host: &str, state: IndexState) {
        cat.hydrate(host.into(), dummy_ctx(), Vec::new());
        cat.set_state(host, state);
    }

    #[test]
    fn catalog_get_returns_none_for_unknown_host() {
        let cat = IndexCatalog::new();
        assert!(cat.state_for("local").is_none());
    }

    #[test]
    fn hydrate_inserts_ready_state() {
        let mut cat = IndexCatalog::new();
        cat.hydrate(
            "local".into(),
            dummy_ctx(),
            vec![IndexedDir {
                path: PathBuf::from("/x"),
                kind: ProjectKind::Plain,
            }],
        );
        match cat.state_for("local") {
            Some(IndexState::Ready { dirs }) => assert_eq!(dirs.len(), 1),
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn start_refresh_on_empty_starts_building() {
        let mut cat = IndexCatalog::new();
        let outcome = cat.start_refresh("local".into(), dummy_ctx(), fresh_handle);
        assert_eq!(outcome, RefreshOutcome::Started);
        assert!(matches!(
            cat.state_for("local"),
            Some(IndexState::Building { .. })
        ));
    }

    #[test]
    fn start_refresh_on_ready_preserves_cache_as_refreshing() {
        // The whole point of SWR: don't drop the cached results when a
        // background rebuild starts.
        let mut cat = IndexCatalog::new();
        cat.hydrate(
            "local".into(),
            dummy_ctx(),
            vec![IndexedDir {
                path: PathBuf::from("/cached"),
                kind: ProjectKind::Git,
            }],
        );
        let outcome = cat.start_refresh("local".into(), dummy_ctx(), fresh_handle);
        assert_eq!(outcome, RefreshOutcome::Started);
        match cat.state_for("local") {
            Some(IndexState::Refreshing { dirs, .. }) => {
                assert_eq!(dirs.len(), 1);
                assert_eq!(dirs[0].path, PathBuf::from("/cached"));
            }
            other => panic!("expected Refreshing, got {other:?}"),
        }
    }

    #[test]
    fn start_refresh_on_in_flight_is_skipped() {
        // No double-rebuild when one is already running. The make_handle
        // thunk must not be called on the skip path.
        let mut cat = IndexCatalog::new();
        install(
            &mut cat,
            "local",
            IndexState::Building {
                handle: fresh_handle(),
                count: 7,
                dirs: Vec::new(),
            },
        );
        let mut called = false;
        let outcome = cat.start_refresh("local".into(), dummy_ctx(), || {
            called = true;
            fresh_handle()
        });
        assert_eq!(outcome, RefreshOutcome::Skipped);
        assert!(!called, "make_handle must not be called when skipping");
        match cat.state_for("local") {
            Some(IndexState::Building { count, .. }) => assert_eq!(*count, 7),
            other => panic!("expected Building, got {other:?}"),
        }
    }

    #[test]
    fn start_refresh_on_refreshing_is_also_skipped() {
        // Same skip rule applies when the in-flight worker was a
        // background SWR refresh rather than a cold build.
        let mut cat = IndexCatalog::new();
        install(
            &mut cat,
            "local",
            IndexState::Refreshing {
                dirs: vec![IndexedDir {
                    path: PathBuf::from("/cached"),
                    kind: ProjectKind::Plain,
                }],
                handle: fresh_handle(),
                count: 12,
            },
        );
        let outcome = cat.start_refresh("local".into(), dummy_ctx(), fresh_handle);
        assert_eq!(outcome, RefreshOutcome::Skipped);
        match cat.state_for("local") {
            Some(IndexState::Refreshing { dirs, count, .. }) => {
                assert_eq!(dirs.len(), 1, "cached dirs preserved on skip");
                assert_eq!(*count, 12);
            }
            other => panic!("expected Refreshing, got {other:?}"),
        }
    }

    #[test]
    fn force_rebuild_on_ready_preserves_cache() {
        let mut cat = IndexCatalog::new();
        cat.hydrate(
            "local".into(),
            dummy_ctx(),
            vec![IndexedDir {
                path: PathBuf::from("/c"),
                kind: ProjectKind::Plain,
            }],
        );
        cat.force_rebuild("local".into(), dummy_ctx(), fresh_handle);
        match cat.state_for("local") {
            Some(IndexState::Refreshing { dirs, .. }) => assert_eq!(dirs.len(), 1),
            other => panic!("expected Refreshing, got {other:?}"),
        }
    }

    #[test]
    fn force_rebuild_on_in_flight_replaces_handle() {
        // Unlike start_refresh, force_rebuild always spawns a new
        // worker. The previous handle is dropped (cancels the prior
        // walk via the AtomicBool).
        let mut cat = IndexCatalog::new();
        install(
            &mut cat,
            "local",
            IndexState::Building {
                handle: fresh_handle(),
                count: 99,
                dirs: Vec::new(),
            },
        );
        cat.force_rebuild("local".into(), dummy_ctx(), fresh_handle);
        match cat.state_for("local") {
            Some(IndexState::Building { count, .. }) => {
                assert_eq!(*count, 0, "force_rebuild resets the progress counter");
            }
            other => panic!("expected Building, got {other:?}"),
        }
    }

    #[test]
    fn force_rebuild_on_failed_starts_fresh_building() {
        let mut cat = IndexCatalog::new();
        install(
            &mut cat,
            "local",
            IndexState::Failed {
                message: "prior".into(),
            },
        );
        cat.force_rebuild("local".into(), dummy_ctx(), fresh_handle);
        assert!(matches!(
            cat.state_for("local"),
            Some(IndexState::Building { .. })
        ));
    }

    #[test]
    fn iter_in_flight_skips_ready_and_failed() {
        let mut cat = IndexCatalog::new();
        cat.hydrate("local".into(), dummy_ctx(), Vec::new());
        install(
            &mut cat,
            "host-a",
            IndexState::Building {
                handle: fresh_handle(),
                count: 0,
                dirs: Vec::new(),
            },
        );
        install(
            &mut cat,
            "host-b",
            IndexState::Failed {
                message: "x".into(),
            },
        );
        let names: Vec<String> = cat.iter_in_flight_mut().map(|(h, _)| h.clone()).collect();
        assert_eq!(names.len(), 1);
        assert_eq!(names[0], "host-a");
    }

    // ── expand_remote_roots ──────────────────────────────────────
    //
    // Mirror the local expand_one rules but swap `$HOME` for the
    // captured remote `$HOME`. Non-absolute, non-tilde roots are
    // dropped (vs local where they're treated as relative-to-cwd) —
    // the remote shell's cwd is not meaningful from the client side.

    #[test]
    fn expand_remote_roots_handles_tilde_and_absolute() {
        let home = PathBuf::from("/home/df");
        let roots = vec![
            "~".to_string(),
            "~/code".to_string(),
            "/srv/projects".to_string(),
        ];
        let expanded = expand_remote_roots(&roots, &home);
        assert_eq!(
            expanded,
            vec![
                PathBuf::from("/home/df"),
                PathBuf::from("/home/df/code"),
                PathBuf::from("/srv/projects"),
            ],
        );
    }

    #[test]
    fn expand_remote_roots_drops_non_absolute_non_tilde() {
        // A relative path has no defined meaning on the remote side
        // (no client-side cwd to anchor against), so we skip it
        // rather than guess.
        let home = PathBuf::from("/h");
        let expanded = expand_remote_roots(&["code".to_string()], &home);
        assert!(expanded.is_empty());
    }

    // ── classify_remote_line ─────────────────────────────────────
    //
    // The pure parser that turns one line of `find` output into a
    // RemoteFindLine. Every branch (Git, Marker, Directory, Skip)
    // gets a positive case + at least one negative case for the
    // boundary that distinguishes it.

    fn marker_set(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn classify_dot_git_path_yields_git_marker_for_parent() {
        let line = "/home/u/proj/.git";
        let out = classify_remote_line(line, &marker_set(&[]));
        assert_eq!(
            out,
            RemoteFindLine::GitMarker(PathBuf::from("/home/u/proj"))
        );
    }

    #[test]
    fn classify_marker_file_yields_marker_file_for_parent() {
        let line = "/home/u/proj/Cargo.toml";
        let out = classify_remote_line(line, &marker_set(&["Cargo.toml", "package.json"]));
        assert_eq!(
            out,
            RemoteFindLine::MarkerFile(PathBuf::from("/home/u/proj"))
        );
    }

    #[test]
    fn classify_directory_path_yields_directory() {
        let line = "/home/u/proj";
        let out = classify_remote_line(line, &marker_set(&["Cargo.toml"]));
        assert_eq!(
            out,
            RemoteFindLine::Directory(PathBuf::from("/home/u/proj"))
        );
    }

    #[test]
    fn classify_empty_or_blank_line_skips() {
        assert_eq!(
            classify_remote_line("", &marker_set(&[])),
            RemoteFindLine::Skip
        );
        assert_eq!(
            classify_remote_line("\r", &marker_set(&[])),
            RemoteFindLine::Skip
        );
    }

    #[test]
    fn classify_strips_trailing_carriage_return_from_crlf_lines() {
        // Some find implementations / ssh transports may emit CRLF.
        // Without the trim, the basename would parse as "proj\r" and
        // the line would be misclassified as a directory rather than
        // as a git marker.
        let line = "/home/u/proj/.git\r";
        let out = classify_remote_line(line, &marker_set(&[]));
        assert_eq!(
            out,
            RemoteFindLine::GitMarker(PathBuf::from("/home/u/proj"))
        );
    }

    #[test]
    fn classify_root_path_with_no_parent_skips_for_git_marker() {
        // `.git` at the filesystem root has no parent — a degenerate
        // case but the classifier must not panic on it.
        let out = classify_remote_line("/.git", &marker_set(&[]));
        // Parent of "/.git" is "/" which IS a parent, so this should
        // yield GitMarker("/"). The actual edge case is a bare ".git"
        // string with no '/'.
        assert_eq!(out, RemoteFindLine::GitMarker(PathBuf::from("/")));
        let bare = classify_remote_line(".git", &marker_set(&[]));
        // Bare ".git" has no parent — skip rather than panic.
        assert_eq!(bare, RemoteFindLine::Skip);
    }

    // ── build_remote_find_cmd ────────────────────────────────────

    #[test]
    fn build_find_cmd_includes_root_and_default_branches() {
        let cmd = build_remote_find_cmd(Path::new("/home/u"), &[]).unwrap();
        assert!(cmd.contains("find '/home/u'"), "missing root: {cmd}");
        assert!(
            cmd.starts_with("exec "),
            "missing exec prefix (needed so SIGHUP propagates to find on cancel): {cmd}",
        );
        assert!(
            cmd.contains("-name .git -prune -print"),
            "missing git branch: {cmd}"
        );
        assert!(
            cmd.contains("-name '.*' -prune"),
            "missing hidden branch: {cmd}"
        );
        assert!(cmd.contains("-type d -print"), "missing dir branch: {cmd}");
        assert!(cmd.contains("2>/dev/null"), "missing stderr swallow: {cmd}");
    }

    #[test]
    fn build_find_cmd_includes_marker_branch_when_markers_present() {
        let cmd = build_remote_find_cmd(
            Path::new("/h"),
            &["Cargo.toml".to_string(), "package.json".to_string()],
        )
        .unwrap();
        assert!(cmd.contains("-name 'Cargo.toml'"));
        assert!(cmd.contains("-name 'package.json'"));
        assert!(cmd.contains("-type f"));
    }

    #[test]
    fn build_find_cmd_omits_marker_branch_when_empty() {
        let cmd = build_remote_find_cmd(Path::new("/h"), &[]).unwrap();
        // Without markers we must not emit `-type f \( ... \)` at all
        // (an empty marker list would produce malformed find syntax).
        assert!(!cmd.contains("-type f"), "spurious -type f branch: {cmd}");
    }

    #[test]
    fn build_find_cmd_rejects_quote_in_path() {
        // Defensive: a `'` in the root would break out of our shell
        // quoting and let an attacker inject commands. Reject early.
        let cmd = build_remote_find_cmd(Path::new("/home/o'malley"), &[]);
        assert!(cmd.is_none());
    }

    #[test]
    fn build_find_cmd_rejects_unsafe_marker() {
        // Same defense for markers — anything with a shell metachar
        // would let a malicious config inject. Allow [A-Za-z0-9._-]
        // only.
        let cmd = build_remote_find_cmd(Path::new("/h"), &["foo$(whoami)".to_string()]);
        assert!(cmd.is_none());
        let cmd_ok = build_remote_find_cmd(Path::new("/h"), &["valid-name.toml".to_string()]);
        assert!(cmd_ok.is_some());
    }

    // ── finalize_remote_dirs ─────────────────────────────────────
    //
    // The pure classifier that turns the accumulated find output
    // (raw dir paths + git-marker set + marker-file set) into the
    // final IndexedDir list. Git outranks Marker outranks Plain;
    // result is sorted ascending by path so display order is stable
    // across runs.

    #[test]
    fn finalize_assigns_git_when_path_in_git_set() {
        let dir_paths = vec![PathBuf::from("/h/a"), PathBuf::from("/h/b")];
        let git_dirs: HashSet<PathBuf> = [PathBuf::from("/h/a")].into_iter().collect();
        let marker_dirs = HashSet::new();
        let dirs = finalize_remote_dirs(dir_paths, &git_dirs, &marker_dirs);
        assert_eq!(dirs.len(), 2);
        assert_eq!(dirs[0].kind, ProjectKind::Git);
        assert_eq!(dirs[1].kind, ProjectKind::Plain);
    }

    #[test]
    fn finalize_git_outranks_marker_when_both_match() {
        // The strongest-signal-wins rule: a directory that's both a
        // git repo AND has a marker file is classified Git, not
        // Marker. Without this, the worker's classification would
        // diverge from the local walker's.
        let dir = PathBuf::from("/h/proj");
        let git_dirs: HashSet<PathBuf> = [dir.clone()].into_iter().collect();
        let marker_dirs: HashSet<PathBuf> = [dir.clone()].into_iter().collect();
        let dirs = finalize_remote_dirs(vec![dir], &git_dirs, &marker_dirs);
        assert_eq!(dirs[0].kind, ProjectKind::Git);
    }

    #[test]
    fn finalize_sorts_results_ascending_by_path() {
        // Stable display order: the matcher relies on ordered input
        // so a re-rendered wildmenu doesn't shuffle ties from one
        // frame to the next.
        let dir_paths = vec![
            PathBuf::from("/h/zeta"),
            PathBuf::from("/h/alpha"),
            PathBuf::from("/h/middle"),
        ];
        let dirs = finalize_remote_dirs(dir_paths, &HashSet::new(), &HashSet::new());
        let names: Vec<_> = dirs
            .iter()
            .map(|d| d.path.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["alpha", "middle", "zeta"]);
    }

    // ── IndexHandle::try_recv ────────────────────────────────────

    #[test]
    fn try_recv_on_inert_handle_returns_none() {
        // The runtime drain loop polls this every frame. Smoke check
        // that a handle with no live worker (the test fixture) is
        // silent — channel-disconnected behaves identically to
        // empty for `try_recv`.
        let handle = inert_handle_for_test();
        assert!(handle.try_recv().is_none());
    }
}
