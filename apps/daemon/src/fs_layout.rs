//! Filesystem layout for `codemuxd`.
//!
//! Per AD-3, the daemon owns a tree under `~/.cache/codemuxd/`:
//!
//! ```text
//! ~/.cache/codemuxd/
//!   bin/codemuxd
//!   sockets/{agent-id}.sock     (mode 0600)
//!   pids/{agent-id}.pid         (exclusive create)
//!   logs/{agent-id}.log
//!   src/codemuxd-{version}.tar.gz
//!   agent.version               (text: "codemuxd-{cargo-pkg-version}")
//! ```
//!
//! This module is **pure path resolution**: no IO, no globals, no env
//! reads at module load. Callers — Stage 4's bootstrap on the local side
//! and the daemon itself when validating CLI-supplied paths — construct a
//! [`Layout`] from either `$HOME` or an explicit override and ask it for
//! resolved paths. The HOME override exists so tests can redirect the
//! whole tree to a tempdir without mutating process env (which would race
//! across parallel test runs).

use std::path::{Path, PathBuf};

/// Resolved filesystem layout rooted at `<home>/.cache/codemuxd/`.
#[derive(Debug, Clone)]
pub struct Layout {
    root: PathBuf,
}

impl Layout {
    /// Build from `$HOME/.cache/codemuxd`. Returns `None` if `$HOME` is
    /// unset — production callers turn this into an error with their own
    /// context, and tests don't go through this path at all.
    #[must_use]
    pub fn from_home_env() -> Option<Self> {
        std::env::var_os("HOME").map(|h| Self::from_home(Path::new(&h)))
    }

    /// Build from an explicit home directory. Tests pass a tempdir here
    /// to keep the layout fully under their control.
    #[must_use]
    pub fn from_home(home: &Path) -> Self {
        Self {
            root: home.join(".cache").join("codemuxd"),
        }
    }

    /// Root of the layout (`<home>/.cache/codemuxd`).
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<root>/sockets/<agent-id>.sock`.
    #[must_use]
    pub fn socket_path(&self, agent_id: &str) -> PathBuf {
        self.root.join("sockets").join(format!("{agent_id}.sock"))
    }

    /// `<root>/pids/<agent-id>.pid`.
    #[must_use]
    pub fn pid_path(&self, agent_id: &str) -> PathBuf {
        self.root.join("pids").join(format!("{agent_id}.pid"))
    }

    /// `<root>/logs/<agent-id>.log`.
    #[must_use]
    pub fn log_path(&self, agent_id: &str) -> PathBuf {
        self.root.join("logs").join(format!("{agent_id}.log"))
    }

    /// `<root>/agent.version` — single file recording the installed daemon
    /// version. Stage 4's bootstrap reads this to decide whether a rebuild
    /// is needed.
    #[must_use]
    pub fn agent_version_file(&self) -> PathBuf {
        self.root.join("agent.version")
    }

    /// `<root>/bin` — directory the bootstrap drops the built `codemuxd`
    /// binary into.
    #[must_use]
    pub fn bin_dir(&self) -> PathBuf {
        self.root.join("bin")
    }

    /// `<root>/src` — directory the bootstrap stages the tarball under
    /// before building.
    #[must_use]
    pub fn src_dir(&self) -> PathBuf {
        self.root.join("src")
    }
}

/// Ensure the parent directory of `path` exists, creating it (and any
/// missing ancestors) on demand. Idempotent. Does nothing if `path` has
/// no parent (e.g. a bare filename in the cwd).
pub fn ensure_parent(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `from_home` plants the layout root at the canonical location and
    /// each accessor concatenates the right segment. Verifies the full
    /// per-agent path shape end-to-end.
    #[test]
    fn from_home_resolves_canonical_paths() {
        let home = Path::new("/fake/home");
        let layout = Layout::from_home(home);
        assert_eq!(layout.root(), Path::new("/fake/home/.cache/codemuxd"));
        assert_eq!(
            layout.socket_path("alpha"),
            Path::new("/fake/home/.cache/codemuxd/sockets/alpha.sock"),
        );
        assert_eq!(
            layout.pid_path("alpha"),
            Path::new("/fake/home/.cache/codemuxd/pids/alpha.pid"),
        );
        assert_eq!(
            layout.log_path("alpha"),
            Path::new("/fake/home/.cache/codemuxd/logs/alpha.log"),
        );
        assert_eq!(
            layout.agent_version_file(),
            Path::new("/fake/home/.cache/codemuxd/agent.version"),
        );
        assert_eq!(
            layout.bin_dir(),
            Path::new("/fake/home/.cache/codemuxd/bin")
        );
        assert_eq!(
            layout.src_dir(),
            Path::new("/fake/home/.cache/codemuxd/src")
        );
    }

    /// Different agent ids produce non-overlapping paths within the same
    /// layout — the per-agent suffix is the only varying part.
    #[test]
    fn different_agent_ids_do_not_collide() {
        let layout = Layout::from_home(Path::new("/h"));
        assert_ne!(layout.socket_path("a"), layout.socket_path("b"));
        assert_ne!(layout.pid_path("a"), layout.pid_path("b"));
        assert_ne!(layout.log_path("a"), layout.log_path("b"));
    }

    /// HOME-override via [`Layout::from_home`] redirects the whole tree
    /// without touching process env. Tests rely on this to operate in a
    /// tempdir while running in parallel.
    #[test]
    fn from_home_with_tempdir_isolates_layout() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let layout = Layout::from_home(dir.path());
        assert!(layout.root().starts_with(dir.path()));
        assert!(layout.socket_path("test").starts_with(dir.path()));
        Ok(())
    }

    /// `ensure_parent` creates the missing parent directory of a path
    /// that lives several levels deeper than the tempdir root.
    #[test]
    fn ensure_parent_creates_missing_dirs() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("a").join("b").join("c").join("file.txt");
        let Some(parent_before) = path.parent() else {
            panic!("path must have a parent");
        };
        assert!(!parent_before.exists());
        ensure_parent(&path)?;
        let Some(parent) = path.parent() else {
            panic!("path must have a parent");
        };
        assert!(parent.exists(), "parent should exist after ensure_parent");
        Ok(())
    }

    /// `ensure_parent` is idempotent — calling it twice on the same path
    /// must not error.
    #[test]
    fn ensure_parent_is_idempotent() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("nested").join("file");
        ensure_parent(&path)?;
        ensure_parent(&path)?;
        Ok(())
    }

    /// `ensure_parent` on a bare filename (no parent component) is a
    /// no-op — it must not try to `mkdir ""` and fail.
    #[test]
    fn ensure_parent_on_bare_filename_is_noop() -> Result<(), Box<dyn std::error::Error>> {
        ensure_parent(Path::new("just-a-name"))?;
        Ok(())
    }

    /// `from_home_env` reads `$HOME` and roots the layout under
    /// `<home>/.cache/codemuxd`. The test asserts the relationship rather
    /// than a hardcoded path so it works on any developer machine.
    #[test]
    fn from_home_env_resolves_relative_to_home() {
        let Some(layout) = Layout::from_home_env() else {
            // Test environments without $HOME are vanishingly rare; skip
            // rather than panic if the assumption doesn't hold.
            return;
        };
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        let expected = Path::new(&home).join(".cache").join("codemuxd");
        assert_eq!(layout.root(), expected);
    }
}
