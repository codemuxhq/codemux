//! Resolve the current git branch name from a filesystem path. Used by
//! the status-bar's `BranchSegment` to surface the worktree's branch
//! alongside the repo name (e.g. `codemux:main`,
//! `worktree-foo:feat/bar`).
//!
//! Hand-rolled (no `git2` / `gix` dep) to keep the dependency surface
//! small — same reasoning as [`crate::repo_name`]. We only handle the
//! shapes that show up in normal day-to-day work:
//!
//! 1. `<cwd>/.git/` directory + `<cwd>/.git/HEAD` — the standard repo
//! 2. `<cwd>/.git` is a *file* containing `gitdir: <path>` — used by
//!    git worktrees and submodules; we follow the pointer and read
//!    `<gitdir>/HEAD`
//! 3. `HEAD` content `ref: refs/heads/<name>\n` → return `<name>`
//! 4. `HEAD` content is a 40-char hex SHA (detached HEAD) → return the
//!    7-char short SHA
//!
//! Anything else (bare repo, packed-refs only, malformed HEAD, missing
//! `.git`, walks-up-too-far) returns `None`. The status segment then
//! renders nothing — we don't want to lie about state we can't read
//! cheaply.

use std::path::{Path, PathBuf};

/// Resolve the branch (or short detached SHA) for a *local* path. Walks
/// upward looking for `.git`, then dispatches on whether `.git` is a
/// directory (regular repo) or a file (worktree pointer).
///
/// Errors are deliberately collapsed to `None` (not propagated as a
/// `Result`) — the caller is the status-bar `BranchSegment`, which
/// either renders the branch or skips the segment. A noisy log on
/// every status-bar refresh outside a git repo would drown out real
/// signal, so individual `read_to_string` / `metadata` failures emit
/// at `trace!` level. To see them, run with
/// `RUST_LOG=codemux::git_branch=trace`.
#[must_use]
pub fn resolve_local(cwd: &Path) -> Option<String> {
    let dot_git = find_dot_git(cwd)?;
    let head_path = head_path_for(&dot_git)?;
    let raw = std::fs::read_to_string(&head_path)
        .map_err(|e| {
            tracing::trace!(path = %head_path.display(), error = %e, "git_branch: HEAD read failed");
        })
        .ok()?;
    parse_head(&raw)
}

/// Walk upward from `start` looking for a `.git` entry (file or
/// directory). Returns the path of the `.git` entry itself, not its
/// parent — so callers can branch on metadata to decide whether to
/// follow a `gitdir:` pointer.
fn find_dot_git(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .map(|dir| dir.join(".git"))
        .find(|p| p.exists())
}

/// Given a `.git` path, resolve the location of the `HEAD` file.
///
/// - When `.git` is a directory: `<dir>/HEAD`.
/// - When `.git` is a file (worktree / submodule): read the `gitdir:`
///   pointer and append `/HEAD`. The pointer is interpreted relative
///   to the directory that *contains* the `.git` file — that's the
///   convention `git` itself uses, and it lets `gitdir: ../foo/.git`
///   work when the worktree sits next to the main repo.
fn head_path_for(dot_git: &Path) -> Option<PathBuf> {
    let meta = std::fs::metadata(dot_git).ok()?;
    if meta.is_dir() {
        return Some(dot_git.join("HEAD"));
    }
    if meta.is_file() {
        let content = std::fs::read_to_string(dot_git).ok()?;
        let pointer = content.lines().find_map(|l| l.strip_prefix("gitdir:"))?;
        let pointer = pointer.trim();
        let gitdir = if Path::new(pointer).is_absolute() {
            PathBuf::from(pointer)
        } else {
            // Relative pointers anchor on the `.git` file's parent —
            // that's where `git` resolves them from.
            dot_git.parent()?.join(pointer)
        };
        return Some(gitdir.join("HEAD"));
    }
    None
}

/// Parse the contents of a `HEAD` file. Two shapes are accepted:
///
/// - Symbolic ref: `ref: refs/heads/<name>` (branch name returned)
/// - Detached HEAD: 40 hex chars (short 7-char SHA returned)
///
/// Anything else returns `None`. Trailing whitespace / newlines are
/// trimmed before matching.
fn parse_head(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if let Some(refname) = trimmed.strip_prefix("ref:") {
        let refname = refname.trim();
        // We render only the part after `refs/heads/` so the segment
        // shows `main` rather than `refs/heads/main`. A ref outside
        // `refs/heads/` (e.g. `refs/remotes/origin/main`) renders the
        // full ref so the user can still tell what's checked out.
        let branch = refname.strip_prefix("refs/heads/").unwrap_or(refname);
        return (!branch.is_empty()).then(|| branch.to_string());
    }
    if trimmed.len() == 40 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        return Some(trimmed[..7].to_string());
    }
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn head_with_symbolic_ref_returns_branch_name() {
        assert_eq!(parse_head("ref: refs/heads/main\n"), Some("main".into()));
        assert_eq!(
            parse_head("ref: refs/heads/feat/bar\n"),
            Some("feat/bar".into()),
        );
    }

    #[test]
    fn head_with_detached_sha_returns_short_sha() {
        // 40 hex chars; we render the first 7.
        let sha = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(parse_head(sha), Some("0123456".into()));
        assert_eq!(parse_head(&format!("{sha}\n")), Some("0123456".into()));
    }

    #[test]
    fn head_with_unrecognized_content_returns_none() {
        assert_eq!(parse_head(""), None);
        // Mid-rebase / odd state: not a ref, not 40 hex chars.
        assert_eq!(parse_head("ref:\n"), None);
        assert_eq!(parse_head("abcdef\n"), None);
        // 40 chars but non-hex — must fall through to None.
        let not_hex = "z".repeat(40);
        assert_eq!(parse_head(&not_hex), None);
    }

    #[test]
    fn head_with_non_branch_ref_returns_full_ref() {
        // Detached-on-tag is rare but not impossible; show the user
        // what's actually checked out rather than nothing.
        assert_eq!(
            parse_head("ref: refs/tags/v1.0\n"),
            Some("refs/tags/v1.0".into()),
        );
    }

    #[test]
    fn resolve_local_reads_branch_from_git_dir() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("myrepo");
        fs::create_dir(&repo).unwrap();
        fs::create_dir(repo.join(".git")).unwrap();
        fs::write(repo.join(".git").join("HEAD"), "ref: refs/heads/main\n").unwrap();

        assert_eq!(resolve_local(&repo), Some("main".into()));
    }

    #[test]
    fn resolve_local_walks_upward_from_subdir() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("myrepo");
        let nested = repo.join("apps").join("tui");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir(repo.join(".git")).unwrap();
        fs::write(repo.join(".git").join("HEAD"), "ref: refs/heads/dev\n").unwrap();

        assert_eq!(resolve_local(&nested), Some("dev".into()));
    }

    #[test]
    fn resolve_local_follows_worktree_gitdir_pointer() {
        // Worktree layout: the worktree's `.git` is a *file* whose
        // contents are `gitdir: <relative-or-absolute-path>` pointing
        // to the per-worktree subdir under the main repo's `.git/worktrees/<name>`.
        let tmp = TempDir::new().unwrap();
        let main_repo = tmp.path().join("main");
        let worktree_meta = main_repo.join(".git").join("worktrees").join("feature-x");
        fs::create_dir_all(&worktree_meta).unwrap();
        fs::write(worktree_meta.join("HEAD"), "ref: refs/heads/feat/x\n").unwrap();

        let worktree = tmp.path().join("feature-x");
        fs::create_dir(&worktree).unwrap();
        // Pointer is relative to the worktree dir (where the `.git`
        // file lives) — exactly how `git worktree add` writes it.
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", worktree_meta.display()),
        )
        .unwrap();

        assert_eq!(resolve_local(&worktree), Some("feat/x".into()));
    }

    #[test]
    fn resolve_local_returns_none_outside_a_git_repo() {
        let tmp = TempDir::new().unwrap();
        let scratch = tmp.path().join("scratch");
        fs::create_dir(&scratch).unwrap();
        assert_eq!(resolve_local(&scratch), None);
    }

    #[test]
    fn resolve_local_returns_none_when_head_is_unparseable() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("broken");
        fs::create_dir(&repo).unwrap();
        fs::create_dir(repo.join(".git")).unwrap();
        // Mid-rebase or interrupted clone: HEAD content we don't
        // understand. Better to render nothing than guess wrong.
        fs::write(repo.join(".git").join("HEAD"), "garbage\n").unwrap();

        assert_eq!(resolve_local(&repo), None);
    }

    #[test]
    fn resolve_local_returns_none_when_head_file_is_missing() {
        // `.git/` exists but `HEAD` doesn't (interrupted `git init` or
        // a corrupted repo). The std::fs::read_to_string `?` early
        // return must propagate as None — never panic.
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("no-head");
        fs::create_dir(&repo).unwrap();
        fs::create_dir(repo.join(".git")).unwrap();
        assert_eq!(resolve_local(&repo), None);
    }

    #[test]
    fn resolve_local_returns_none_when_git_file_has_no_gitdir_pointer() {
        // `.git` is a regular file but its content doesn't include a
        // `gitdir:` line — happens with corrupted submodule pointers
        // or a stray file someone named `.git`. head_path_for must
        // bail with None rather than fall through to a wrong path.
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("no-pointer");
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join(".git"), "this is not a real gitfile\n").unwrap();
        assert_eq!(resolve_local(&dir), None);
    }
}
