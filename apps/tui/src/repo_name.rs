//! Resolve a human-friendly "where is this agent" label from a
//! filesystem path. Used by the navigator to show `codemux: <title>`
//! instead of `~/Workbench/repositories/codemux/apps/tui: <title>`.
//!
//! Local resolution walks up from the given cwd looking for a `.git`
//! directory; the parent's basename is the repo name (handles working
//! inside `apps/tui/` of a repo whose root is `codemux/`). When the
//! cwd is not inside a git repo, falls back to the cwd basename so
//! agents spawned in `~/scratch` show as `scratch`, not the empty
//! string.
//!
//! Remote-side resolution is basename-only: probing the remote
//! filesystem would mean a second ssh round-trip, and the user-typed
//! cwd is good enough for the navigator label. The common case is
//! spawning at the repo root, where basename and repo name match.

use std::path::Path;

/// Resolve a label for a *local* path: git repo root basename if the
/// path is inside a git repo, otherwise the path's own basename.
/// Returns `None` only when both lookups fail (e.g. the path is `/`
/// or a relative path with no parent components); callers fall back
/// to whatever static label they already had.
pub fn resolve_local(cwd: &Path) -> Option<String> {
    git_root_name(cwd).or_else(|| basename(cwd))
}

/// Resolve a label for a *remote* path string (the user-typed cwd
/// the spawn modal handed to the bootstrap worker). We can't probe
/// the remote filesystem here without a second ssh round-trip, so
/// this is just basename extraction. Empty input → `None`.
pub fn resolve_remote(cwd: &str) -> Option<String> {
    let trimmed = cwd.trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    let basename = trimmed.rsplit_once('/').map_or(trimmed, |(_, tail)| tail);
    Some(basename.to_string())
}

/// Walk upward from `start` looking for a `.git` entry (file *or*
/// directory — git worktrees use a regular file at `.git`). Returns
/// the basename of the directory that contains it.
fn git_root_name(start: &Path) -> Option<String> {
    start
        .ancestors()
        .find(|dir| dir.join(".git").exists())
        .and_then(basename)
}

fn basename(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|s| s.to_str())
        .map(str::to_string)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn resolve_local_finds_git_root_from_subdir() {
        let tmp = TempDir::new().expect("tempdir");
        let repo = tmp.path().join("myrepo");
        let nested = repo.join("apps").join("tui");
        fs::create_dir_all(&nested).expect("create nested");
        fs::create_dir(repo.join(".git")).expect("create .git");

        assert_eq!(resolve_local(&nested), Some("myrepo".to_string()));
    }

    #[test]
    fn resolve_local_finds_git_root_at_self() {
        let tmp = TempDir::new().expect("tempdir");
        let repo = tmp.path().join("self-repo");
        fs::create_dir(&repo).expect("create repo");
        fs::create_dir(repo.join(".git")).expect("create .git");

        assert_eq!(resolve_local(&repo), Some("self-repo".to_string()));
    }

    #[test]
    fn resolve_local_treats_git_file_as_repo() {
        // git worktrees use a `.git` file (not a directory) pointing
        // back to the main repo. Same labelling story.
        let tmp = TempDir::new().expect("tempdir");
        let worktree = tmp.path().join("worktree-foo");
        fs::create_dir(&worktree).expect("create worktree");
        fs::write(worktree.join(".git"), "gitdir: ../main/.git").expect("write .git file");

        assert_eq!(resolve_local(&worktree), Some("worktree-foo".to_string()));
    }

    #[test]
    fn resolve_local_falls_back_to_basename_when_no_git() {
        let tmp = TempDir::new().expect("tempdir");
        let dir = tmp.path().join("scratch");
        fs::create_dir(&dir).expect("create scratch");

        assert_eq!(resolve_local(&dir), Some("scratch".to_string()));
    }

    #[test]
    fn resolve_remote_extracts_basename() {
        assert_eq!(
            resolve_remote("/home/user/code/api"),
            Some("api".to_string())
        );
    }

    #[test]
    fn resolve_remote_handles_trailing_slash() {
        assert_eq!(
            resolve_remote("/home/user/code/api/"),
            Some("api".to_string())
        );
    }

    #[test]
    fn resolve_remote_handles_tilde_path() {
        // The spawn modal passes `~`-relative paths verbatim to the
        // remote daemon; the basename logic stays valid.
        assert_eq!(resolve_remote("~/code/api"), Some("api".to_string()));
    }

    #[test]
    fn resolve_remote_empty_returns_none() {
        assert_eq!(resolve_remote(""), None);
        assert_eq!(resolve_remote("/"), None);
    }
}
