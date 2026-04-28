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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::thread;
use std::time::Instant;

use crossbeam_channel::{Receiver, Sender, unbounded};
use ignore::WalkBuilder;
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
    /// Running count of directories discovered so far. Emitted every
    /// [`PROGRESS_INTERVAL`] entries during the walk. The wildmenu
    /// renders this as `"indexing… {count} dirs"`.
    Progress(usize),
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexedDir {
    pub path: PathBuf,
    pub kind: ProjectKind,
}

/// What kind of project (if any) a directory looks like, for fuzzy
/// score boosting. Order is significant — `Git` outranks `Marker`
/// outranks `Plain`. A directory can match more than one signal at
/// once; we record the strongest.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

/// Lifecycle of the session-long fuzzy directory index. The runtime
/// holds `Option<IndexState>`: `None` means the user never entered
/// fuzzy mode (or the runtime hasn't kicked off the build yet).
pub enum IndexState {
    /// Index is being built. The runtime drains events and transitions
    /// to `Ready` / `Failed` on `Done`.
    Building {
        handle: IndexHandle,
        /// Running count from the most recent `Progress` event, or 0
        /// if none has arrived yet. Surfaced to the modal as the
        /// "indexing… N dirs" counter.
        count: usize,
    },
    /// Index is ready for queries. Sorted ascending by path so display
    /// order is stable and binary search is available if needed.
    Ready { dirs: Vec<IndexedDir> },
    /// Last build attempt failed. Pre-formatted message so the render
    /// path doesn't re-format on every frame.
    Failed { message: String },
}

/// Handle to an in-flight or completed index walk.
///
/// `Drop` cooperatively signals the worker to exit at the next entry
/// boundary. The worker thread is detached at `thread::spawn` time —
/// the TUI never joins on it.
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
    let mut dir_paths: Vec<PathBuf> = Vec::new();
    let mut marker_dirs: HashSet<PathBuf> = HashSet::new();
    let mut failures: Vec<RootFailure> = Vec::new();
    let mut succeeded_any = false;

    'roots: for root in roots {
        if cancel.load(Relaxed) {
            break;
        }
        if !root.exists() {
            failures.push(RootFailure {
                path: root.clone(),
                reason: "path does not exist",
            });
            continue;
        }
        tracing::debug!(root = %root.display(), "fuzzy index: starting walk");
        // Default `WalkBuilder` already enables `git_ignore`,
        // `git_global`, `git_exclude`, `parents`, and skips hidden
        // entries. We rely on those defaults — explicit calls would
        // just repeat them.
        let walker = WalkBuilder::new(&root).build();

        let mut root_succeeded = false;
        for entry in walker {
            if cancel.load(Relaxed) {
                break 'roots;
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
                dir_paths.push(entry.into_path());
                root_succeeded = true;
                if dir_paths.len() >= MAX_INDEX_ENTRIES {
                    tracing::warn!(
                        cap = MAX_INDEX_ENTRIES,
                        "fuzzy index: hit entry cap; truncating",
                    );
                    break 'roots;
                }
                if dir_paths.len().is_multiple_of(PROGRESS_INTERVAL) {
                    tx.send(IndexEvent::Progress(dir_paths.len())).ok();
                }
            } else if let Some(name) = entry.path().file_name().and_then(|n| n.to_str())
                && marker_set.contains(name)
                && let Some(parent) = entry.path().parent()
            {
                // The marker file flags its parent dir as a project.
                marker_dirs.insert(parent.to_path_buf());
            }
        }
        if root_succeeded {
            succeeded_any = true;
        } else {
            failures.push(RootFailure {
                path: root.clone(),
                reason: "no readable entries",
            });
        }
    }

    let was_canceled = cancel.load(Relaxed);
    // Cancel always produces `Ok` (possibly partial / empty) so the
    // runtime can choose to keep what was built; treating it as an
    // error would force a useless rebuild on every cancel.
    let result = if was_canceled || succeeded_any {
        let mut dirs: Vec<IndexedDir> = dir_paths
            .into_iter()
            .map(|path| {
                let kind = classify_dir(&path, &marker_dirs);
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
        Err(IndexError::NoRoots(failures))
    };
    tx.send(IndexEvent::Done(result)).ok();
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
                Ok(IndexEvent::Progress(n)) => progress.push(n),
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
}
