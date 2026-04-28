//! Disk-backed persistence for the fuzzy directory index.
//!
//! The session-only worker in [`crate::index_worker`] is fast on
//! cold-cache walks (sub-second to a few seconds for a typical home),
//! but the SWR contract is "every modal open shows results
//! immediately." That contract has to survive a codemux restart, so
//! this module persists the index to disk between sessions:
//!
//! - **Local:** `~/.cache/codemux/index.json` — read at session start,
//!   written after every successful walk. Hydrates the
//!   [`IndexCatalog`](crate::index_worker::IndexCatalog) so the very
//!   first spawn-modal open already has results.
//! - **Remote (SSH):** the same JSON shape, but stored *on the remote
//!   host itself* at `~/.cache/codemux/index.json`. Read/written
//!   through the existing ssh `ControlMaster` socket. Storing the
//!   cache remote-side means a brand-new local codemux session
//!   benefits from a previous client's walk — and there's no per-host
//!   on-disk state to manage on the local machine.
//!
//! Cache invalidation: the cache file embeds the search-root list it
//! was built from. On load, we compare against the *current* roots
//! (root config changed → return `None` so the caller falls back to a
//! cold build). Verbatim comparison rather than hashing keeps the
//! schema dependency-free.
//!
//! The I/O surface is split into two layers:
//! - Pure path-driven helpers ([`load_at_path`], [`save_at_path`])
//!   which the unit tests exercise against a tmpdir.
//! - Thin wrappers ([`load_local`], [`save_local`]) that resolve
//!   `$HOME` for the production call sites.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::index_worker::IndexedDir;

/// Schema version. Bumped on any incompatible change to
/// [`CacheFile`]. Older versions are rejected as `None` (treated like
/// a cold cache) so a stale layout never deserializes into a wrong
/// shape.
const SCHEMA_VERSION: u32 = 1;

/// Relative path from `$HOME` to the cache file. Mirrors the existing
/// `~/.cache/codemux/logs/codemux.log` convention; the parent
/// directory is created on demand at write time.
const CACHE_RELATIVE: &str = ".cache/codemux/index.json";

/// On-disk cache shape. Owns the schema (no nested types); kept flat
/// so a stray `cat` of the file is human-readable.
#[derive(Debug, Serialize, Deserialize)]
struct CacheFile {
    /// See [`SCHEMA_VERSION`].
    version: u32,
    /// The search roots the cache was built from, verbatim. Used for
    /// invalidation: a load with a different `roots` list returns
    /// `None` so the modal falls back to a cold build.
    roots: Vec<String>,
    /// The indexed directory list. Order is preserved (the worker
    /// sorts ascending by path before writing).
    dirs: Vec<IndexedDir>,
}

/// Resolve `~/.cache/codemux/index.json` against `$HOME`. Returns
/// `None` if `$HOME` is unset (test envs, exotic init systems) so the
/// caller treats the cache as missing rather than panicking.
#[must_use]
pub fn local_cache_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(CACHE_RELATIVE))
}

/// Read the on-disk cache at `path` and return its `dirs` if the file
/// exists, parses cleanly, the version matches, and the embedded
/// roots match the supplied current roots. Any failure returns `None`
/// — the caller treats it as a cache miss.
///
/// Logging policy: missing file → trace (expected on first run);
/// mismatch / parse failure → debug (worth knowing during dev, but
/// not user-actionable).
#[must_use]
pub fn load_at_path(path: &Path, current_roots: &[String]) -> Option<Vec<IndexedDir>> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            tracing::trace!(path = %path.display(), "fuzzy index cache: miss (no file)");
            return None;
        }
        Err(e) => {
            tracing::debug!(path = %path.display(), error = %e, "fuzzy index cache: read failed");
            return None;
        }
    };
    parse_and_validate(&bytes, current_roots, &path.display().to_string())
}

/// Write the index to `path`. Creates the parent directory if missing.
/// Errors are returned but production callers typically log-and-ignore
/// (a failed write means the next session pays a cold build, no other
/// consequence).
///
/// Atomicity: writes to a sibling `*.tmp` file first, then renames
/// over the target. `std::fs::rename` is atomic on the same
/// filesystem, so a process killed mid-write either leaves the prior
/// cache intact (rename never happened) or has the new file fully
/// formed — never a half-written JSON that fails to parse on the next
/// session.
pub fn save_at_path(path: &Path, current_roots: &[String], dirs: &[IndexedDir]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let bytes = serialize_cache(current_roots, dirs)?;
    let tmp = tmp_sibling(path);
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    tracing::debug!(path = %path.display(), n = dirs.len(), "fuzzy index cache: wrote");
    Ok(())
}

/// Build the path to a sibling temp file by appending `.tmp` to the
/// final component (e.g. `index.json` → `index.json.tmp`). Same
/// directory so the subsequent `rename` is on a single filesystem.
fn tmp_sibling(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(".tmp");
    PathBuf::from(s)
}

/// Convenience wrapper that resolves the local cache path via `$HOME`
/// and delegates to [`load_at_path`]. Returns `None` on a `$HOME`-
/// unset environment so the caller falls back to a cold build.
#[must_use]
pub fn load_local(current_roots: &[String]) -> Option<Vec<IndexedDir>> {
    let path = local_cache_path()?;
    load_at_path(&path, current_roots)
}

/// Convenience wrapper that resolves the local cache path via `$HOME`
/// and delegates to [`save_at_path`].
pub fn save_local(current_roots: &[String], dirs: &[IndexedDir]) -> io::Result<()> {
    let path =
        local_cache_path().ok_or_else(|| io::Error::other("HOME unset; cannot resolve cache"))?;
    save_at_path(&path, current_roots, dirs)
}

/// Read the SSH-side disk cache by `cat`-ing it through the existing
/// ssh `ControlMaster` socket. Same envelope as [`load_local`]: any
/// failure returns `None`.
///
/// We rely on the master being already up — `RemoteFs::open` was
/// called during the prepare phase. If the master died, `ssh -S
/// socket cat` returns non-zero and we treat it as a miss; the next
/// successful walk will rewrite the file.
#[must_use]
pub fn load_remote(host: &str, socket: &Path, current_roots: &[String]) -> Option<Vec<IndexedDir>> {
    let socket_str = socket.to_string_lossy();
    let output = Command::new("ssh")
        .arg("-S")
        .arg(socket_str.as_ref())
        .arg("-o")
        .arg("BatchMode=yes")
        .arg(host)
        .arg("--")
        .arg("cat ~/.cache/codemux/index.json 2>/dev/null")
        .output()
        .ok()?;
    if !output.status.success() {
        tracing::trace!(
            host,
            status = ?output.status.code(),
            "fuzzy index cache: remote miss (cat failed or no file)",
        );
        return None;
    }
    parse_and_validate(&output.stdout, current_roots, host)
}

/// Write the index to the SSH-side disk cache by piping JSON through
/// `ssh -S socket -- 'mkdir -p ~/.cache/codemux && tee >/dev/null && mv'`.
/// The `mkdir -p` makes the call idempotent on a fresh host.
///
/// Atomicity: same write-then-rename pattern as [`save_at_path`].
/// Tee writes to `index.json.tmp`; a successful `mv` makes the swap
/// atomic. A killed pipe leaves the old cache intact rather than a
/// half-written JSON that the next session would reject as invalid.
///
/// **Stdin handling**: we `take()` the child's stdin before writing
/// and drop it explicitly before `wait()`. Without the drop, ssh's
/// `tee` would block reading because the writer side of the pipe
/// stays open as long as the `Child` holds it.
pub fn save_remote(
    host: &str,
    socket: &Path,
    current_roots: &[String],
    dirs: &[IndexedDir],
) -> io::Result<()> {
    let bytes = serialize_cache(current_roots, dirs)?;
    let socket_str = socket.to_string_lossy();
    let mut child = Command::new("ssh")
        .arg("-S")
        .arg(socket_str.as_ref())
        .arg("-o")
        .arg("BatchMode=yes")
        .arg(host)
        .arg("--")
        .arg(
            "mkdir -p ~/.cache/codemux \
             && tee ~/.cache/codemux/index.json.tmp >/dev/null \
             && mv ~/.cache/codemux/index.json.tmp ~/.cache/codemux/index.json",
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("ssh tee: stdin missing"))?;
    stdin.write_all(&bytes)?;
    drop(stdin);
    let status = child.wait()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "ssh tee for {host} failed: {:?}",
            status.code()
        )));
    }
    tracing::debug!(host, n = dirs.len(), "fuzzy index cache: wrote remote");
    Ok(())
}

/// Decode bytes as a [`CacheFile`] and validate version + roots
/// match. Returns `Some(dirs)` only on a clean hit; logs and returns
/// `None` for every miss reason so the caller has one branch.
fn parse_and_validate(
    bytes: &[u8],
    current_roots: &[String],
    source: &str,
) -> Option<Vec<IndexedDir>> {
    let file: CacheFile = match serde_json::from_slice(bytes) {
        Ok(f) => f,
        Err(e) => {
            tracing::debug!(source, error = %e, "fuzzy index cache: parse failed");
            return None;
        }
    };
    if file.version != SCHEMA_VERSION {
        tracing::debug!(
            source,
            file_version = file.version,
            expected = SCHEMA_VERSION,
            "fuzzy index cache: version mismatch",
        );
        return None;
    }
    if file.roots != current_roots {
        tracing::debug!(
            source,
            "fuzzy index cache: roots mismatch (config changed since last build)",
        );
        return None;
    }
    Some(file.dirs)
}

/// Encode the cache payload. Pretty-printed so a `cat` of the file is
/// readable; the size overhead is small relative to the path strings.
fn serialize_cache(current_roots: &[String], dirs: &[IndexedDir]) -> io::Result<Vec<u8>> {
    let file = CacheFile {
        version: SCHEMA_VERSION,
        roots: current_roots.to_vec(),
        dirs: dirs.to_vec(),
    };
    serde_json::to_vec_pretty(&file).map_err(io::Error::other)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::index_worker::ProjectKind;
    use tempfile::TempDir;

    fn dirs_fixture() -> Vec<IndexedDir> {
        vec![
            IndexedDir {
                path: PathBuf::from("/home/u/repo-a"),
                kind: ProjectKind::Git,
            },
            IndexedDir {
                path: PathBuf::from("/home/u/scratch"),
                kind: ProjectKind::Plain,
            },
        ]
    }

    // ── parse_and_validate (the validation core) ─────────────────
    //
    // Every field that participates in invalidation gets one
    // happy-path and one mismatch case. The on-disk path code is
    // tested through `*_at_path` helpers below; the parser is tested
    // here directly so its rules are pinned independent of any I/O.

    #[test]
    fn roundtrip_preserves_dirs() {
        let roots = vec!["~".to_string()];
        let bytes = serialize_cache(&roots, &dirs_fixture()).unwrap();
        let parsed = parse_and_validate(&bytes, &roots, "test").unwrap();
        assert_eq!(parsed, dirs_fixture());
    }

    #[test]
    fn roots_mismatch_returns_none() {
        // The cache was built with one set of roots; loading with a
        // different set must invalidate so we don't show stale results
        // from a stale config.
        let bytes = serialize_cache(&["~".to_string()], &dirs_fixture()).unwrap();
        let parsed = parse_and_validate(&bytes, &["/srv".to_string()], "test");
        assert!(parsed.is_none());
    }

    #[test]
    fn invalid_json_returns_none() {
        let parsed = parse_and_validate(b"{not valid json", &[], "test");
        assert!(parsed.is_none());
    }

    #[test]
    fn truncated_json_returns_none() {
        // Half-written cache file (e.g. crash mid-write). Must not
        // panic, must not return Some — caller falls back to cold build.
        let bytes = serialize_cache(&[], &dirs_fixture()).unwrap();
        let truncated = &bytes[..bytes.len() / 2];
        let parsed = parse_and_validate(truncated, &[], "test");
        assert!(parsed.is_none());
    }

    #[test]
    fn version_mismatch_returns_none() {
        // Hand-roll a cache file with a wrong version field. A future
        // bump must not silently deserialize old shapes.
        let payload = br#"{"version":99,"roots":[],"dirs":[]}"#;
        let parsed = parse_and_validate(payload, &[], "test");
        assert!(parsed.is_none());
    }

    #[test]
    fn empty_dirs_round_trips() {
        // The "successful walk found nothing" case is real (e.g. a
        // search root that exists but has no readable subdirs). Round
        // trip must not lose `Some(empty)` semantics.
        let roots = vec!["~/empty".to_string()];
        let bytes = serialize_cache(&roots, &[]).unwrap();
        let parsed = parse_and_validate(&bytes, &roots, "test").unwrap();
        assert!(parsed.is_empty());
    }

    // ── load_at_path / save_at_path (path-driven I/O) ────────────
    //
    // The local path API takes an explicit path so tests don't have
    // to stomp `$HOME` (which would be unsafe under the workspace
    // `unsafe_code = forbid` lint AND would race other tests). The
    // production wrapper `load_local` is a one-line `$HOME` resolver
    // tested implicitly via integration runs.

    #[test]
    fn save_then_load_round_trips_through_disk() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("idx.json");
        let roots = vec!["~/code".to_string()];
        save_at_path(&path, &roots, &dirs_fixture()).unwrap();
        let parsed = load_at_path(&path, &roots).unwrap();
        assert_eq!(parsed, dirs_fixture());
    }

    #[test]
    fn load_returns_none_when_file_missing() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("does-not-exist.json");
        // Fresh tmpdir — no cache file present. Must be silent miss,
        // not an error path.
        assert!(load_at_path(&missing, &["~".to_string()]).is_none());
    }

    #[test]
    fn save_creates_parent_directory() {
        // The cache lives several dirs deep under `$HOME`. The save
        // path must mkdir -p them or the first session of every
        // user's life would silently fail to persist.
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path().join("nested/deep/idx.json");
        save_at_path(&nested, &[], &dirs_fixture()).unwrap();
        assert!(nested.exists());
    }

    #[test]
    fn load_invalidates_on_root_change() {
        // The whole point of `roots` in the cache file: if the user
        // edits their config to point at different search roots, the
        // old cache is meaningless. Invalidation must reach the load
        // path through the on-disk file, not just the parser.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("idx.json");
        save_at_path(&path, &["~/old".to_string()], &dirs_fixture()).unwrap();
        let parsed = load_at_path(&path, &["~/new".to_string()]);
        assert!(parsed.is_none());
    }

    #[test]
    fn save_at_path_does_not_leave_tmp_file_after_success() {
        // The atomic-write pattern (write .tmp, rename) must clean
        // up after itself. A leaked .tmp would accumulate across
        // sessions and waste disk; if a future session mistakes a
        // stale .tmp for the real cache the parse will fail (different
        // path) but it's still cruft.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("idx.json");
        save_at_path(&path, &[], &dirs_fixture()).unwrap();
        assert!(path.exists(), "final cache file must exist");
        let leftover = tmp.path().join("idx.json.tmp");
        assert!(
            !leftover.exists(),
            "tmp sibling should have been renamed away",
        );
    }

    #[test]
    fn save_at_path_overwrites_existing_cache_atomically() {
        // Round-trip a second save against the same path. The
        // rename should atomically replace the prior contents — no
        // half-merged file, no error from "destination exists".
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("idx.json");
        save_at_path(&path, &[], &dirs_fixture()).unwrap();
        let updated = vec![IndexedDir {
            path: PathBuf::from("/new/entry"),
            kind: ProjectKind::Plain,
        }];
        save_at_path(&path, &[], &updated).unwrap();
        let parsed = load_at_path(&path, &[]).unwrap();
        assert_eq!(parsed, updated, "second save must replace first");
    }
}
