//! SSH config host loader.
//!
//! Reads `~/.ssh/config`, recursively follows any `Include` directives, and
//! returns the union of `Host` entries (wildcards `*`, `?`, `!` skipped).
//! The output feeds the spawn modal's host autocompletion (see
//! `apps/tui/src/spawn.rs`).
//!
//! ## Why a separate module
//!
//! Per the architecture-guide review (NLM 2026-04-24), file-system access
//! and config parsing are *secondary (driven) adapter* concerns. Cohabiting
//! them with the spawn modal's *primary (driving) adapter* responsibilities
//! (keystroke handling, rendering) lowered the cohesion of `spawn.rs`. The
//! split keeps the UI module focused and gives the loader a clean home for
//! its growing test surface — and a natural place to land if SSH transport
//! (P1.4+) needs richer host metadata than just the alias name.
//!
//! The only public surface is [`load_ssh_hosts`]; everything else is a
//! private helper. Tests in this module exercise the recursive walker
//! against `tempfile::TempDir` scratch dirs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Read `~/.ssh/config`, recursively follow any `Include` directives, and
/// return the union of `Host` entries (wildcards `*`, `?`, `!` skipped).
/// Returns an empty Vec if the root file is missing or unreadable; the
/// caller falls back to free-text input in that case.
///
/// Missing root config is normal (a fresh user account has no
/// `~/.ssh/config`), so failures degrade quietly to "empty list" but emit a
/// `tracing::debug!` event for `RUST_LOG=codemux=debug` debugging.
///
/// Include resolution covers the dominant real-world layouts:
/// - `Include config.d/*` — relative paths resolve against `~/.ssh/` per
///   `man ssh_config`. Glob patterns are expanded.
/// - `Include ~/.orbstack/ssh/config` — `~/` is expanded to `$HOME`.
/// - `Include /etc/ssh/extra-config` — absolute paths are honored as-is.
///
/// We DO NOT support `~user/foo` (other-user expansion) or quoted paths
/// (`Include "path with spaces"`). Both are vanishingly rare in real
/// configs; if a user hits one they'll see a `read failed` debug log.
#[must_use]
pub fn load_ssh_hosts() -> Vec<String> {
    let Ok(home) = std::env::var("HOME") else {
        tracing::debug!("HOME unset; SSH host autocomplete disabled");
        return Vec::new();
    };
    let home = PathBuf::from(home);
    let root = home.join(".ssh/config");
    load_ssh_hosts_from(&root, &home)
}

/// Cap on `Include` recursion depth. Cycles are caught by the `visited` set,
/// but a pathological "include chain" (a → b → c → ...) without cycles
/// would still pin the render thread to disk; cap defends against that.
const MAX_INCLUDE_DEPTH: usize = 16;

/// Filesystem-driven entry point used by both production and integration
/// tests. Production callers pass `~/.ssh/config` and `$HOME`; tests pass a
/// scratch root from `tempfile::TempDir`.
fn load_ssh_hosts_from(root: &Path, home: &Path) -> Vec<String> {
    let mut hosts = Vec::new();
    let mut visited = HashSet::new();
    collect_from_file(root, home, &mut hosts, &mut visited, 0);
    hosts.sort();
    hosts.dedup();
    hosts
}

/// Recursive walker. Reads `path`, harvests its `Host` entries into `out`,
/// then resolves each `Include` directive (glob + tilde + relative-to-`~/.ssh/`
/// expansion) and recurses on every matched file.
///
/// Cycle protection: a canonicalized form of `path` is inserted into
/// `visited`; revisits are skipped. `canonicalize` requires the file to
/// exist — when it fails (broken include) we fall back to the raw path,
/// and `read_to_string` below bails naturally with a debug log on the root
/// call only (logging on every missing nested include would be too noisy).
fn collect_from_file(
    path: &Path,
    home: &Path,
    out: &mut Vec<String>,
    visited: &mut HashSet<PathBuf>,
    depth: usize,
) {
    if depth > MAX_INCLUDE_DEPTH {
        tracing::debug!(
            "ssh config Include depth cap ({MAX_INCLUDE_DEPTH}) reached at {}",
            path.display(),
        );
        return;
    }
    let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(key) {
        return;
    }
    let Ok(content) = std::fs::read_to_string(path) else {
        if depth == 0 {
            tracing::debug!(
                "read {} failed; SSH host autocomplete disabled",
                path.display(),
            );
        }
        return;
    };

    out.extend(parse_ssh_hosts(&content));

    for include in parse_includes(&content) {
        let expanded = expand_include_path(&include, home);
        for resolved in expand_glob(&expanded) {
            collect_from_file(&resolved, home, out, visited, depth + 1);
        }
    }
}

/// Pure single-file `Host` parser. Returns the (sorted, deduped) list of
/// `Host` entries from a single SSH config file's contents, with wildcards
/// (`*`, `?`, `!`) skipped.
///
/// `Include` directives are intentionally ignored here — Include resolution
/// requires a `$HOME` value and a visited set for cycle protection, which
/// belongs at the file-walker layer (`collect_from_file`). Use
/// `load_ssh_hosts` (or `load_ssh_hosts_from` for tests) to get the union of
/// hosts across the full Include graph.
fn parse_ssh_hosts(content: &str) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let mut parts = trimmed.splitn(2, char::is_whitespace);
        let Some(keyword) = parts.next() else {
            continue;
        };
        if !keyword.eq_ignore_ascii_case("host") {
            continue;
        }
        let Some(rest) = parts.next() else {
            continue;
        };
        for entry in rest.split_whitespace() {
            // Wildcards (`Host *`, `Host *.foo`, `Host !bar`) are too generic
            // for autocomplete — skip them rather than offering them as
            // candidates the user cannot actually SSH to.
            if entry.contains('*') || entry.contains('?') || entry.contains('!') {
                continue;
            }
            hosts.push(entry.to_string());
        }
    }
    hosts.sort();
    hosts.dedup();
    hosts
}

/// Yield the raw path patterns from each `Include` line in `content`.
/// Multiple paths per line (`Include a b c`) are split on whitespace.
///
/// Quoted paths (`Include "path with spaces"`) are not handled; the quote
/// characters end up in the resulting pattern, which will then fail to
/// match anything on disk. Real configs use unquoted paths; revisit if
/// this assumption ever bites.
fn parse_includes(content: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let mut parts = trimmed.splitn(2, char::is_whitespace);
        let Some(keyword) = parts.next() else {
            continue;
        };
        if !keyword.eq_ignore_ascii_case("include") {
            continue;
        }
        let Some(rest) = parts.next() else {
            continue;
        };
        for entry in rest.split_whitespace() {
            paths.push(entry.to_string());
        }
    }
    paths
}

/// Resolve an `Include` pattern relative to `home`, per `man ssh_config`:
/// - `~` alone → `$HOME`.
/// - `~/foo` → `$HOME/foo`.
/// - Absolute path → unchanged.
/// - Anything else → resolved against `~/.ssh/` (the user-config directory).
///
/// Other-user expansion (`~alice/foo`) is intentionally not supported — it
/// is vanishingly rare and would pull in `shellexpand` for one corner case.
/// Such patterns degrade to a literal "file not found" silently.
fn expand_include_path(pattern: &str, home: &Path) -> PathBuf {
    if pattern == "~" {
        return home.to_path_buf();
    }
    if let Some(stripped) = pattern.strip_prefix("~/") {
        return home.join(stripped);
    }
    let p = PathBuf::from(pattern);
    if p.is_absolute() {
        return p;
    }
    home.join(".ssh").join(p)
}

/// Glob-expand `pattern` and return matched paths sorted lexicographically.
/// Returns empty on syntax errors or when nothing matches; both cases are
/// handled the same way upstream (no files to read = no hosts).
fn expand_glob(pattern: &Path) -> Vec<PathBuf> {
    let pat = pattern.to_string_lossy();
    let Ok(iter) = glob::glob(&pat) else {
        tracing::debug!("invalid glob pattern in ssh include: {pat}");
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = iter.filter_map(Result::ok).collect();
    paths.sort();
    paths
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Write `content` to `path`, creating parent directories as needed.
    /// Used by the tempdir-based integration tests below.
    fn write_file(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn parse_ssh_hosts_returns_empty_for_empty_file() {
        assert!(parse_ssh_hosts("").is_empty());
    }

    #[test]
    fn parse_ssh_hosts_returns_a_single_named_host() {
        assert_eq!(parse_ssh_hosts("Host foo"), vec!["foo".to_string()]);
    }

    #[test]
    fn parse_ssh_hosts_returns_multiple_hosts_per_line() {
        let mut got = parse_ssh_hosts("Host alpha bravo charlie");
        got.sort();
        assert_eq!(got, vec!["alpha", "bravo", "charlie"]);
    }

    #[test]
    fn parse_ssh_hosts_skips_wildcards() {
        let got = parse_ssh_hosts(
            "Host *\nHost *.uber.com\nHost real-host\nHost !excluded\nHost q?stion",
        );
        assert_eq!(got, vec!["real-host".to_string()]);
    }

    #[test]
    fn parse_ssh_hosts_skips_comments_and_blank_lines() {
        let got = parse_ssh_hosts("# comment\n\n  Host  foo  \n");
        assert_eq!(got, vec!["foo".to_string()]);
    }

    #[test]
    fn parse_ssh_hosts_is_case_insensitive_on_keyword() {
        // `host`, `Host`, `HOST` all valid per the SSH config grammar.
        assert_eq!(parse_ssh_hosts("host foo"), vec!["foo".to_string()]);
        assert_eq!(parse_ssh_hosts("HOST bar"), vec!["bar".to_string()]);
    }

    #[test]
    fn parse_ssh_hosts_ignores_other_directives() {
        let got = parse_ssh_hosts(
            "User daniel\nHostName example.com\nHost actual\nIdentityFile ~/.ssh/id_rsa",
        );
        assert_eq!(got, vec!["actual".to_string()]);
    }

    #[test]
    fn parse_ssh_hosts_dedups() {
        let got = parse_ssh_hosts("Host foo\nHost foo\nHost bar");
        assert_eq!(got, vec!["bar".to_string(), "foo".to_string()]);
    }

    #[test]
    fn expand_include_path_handles_bare_tilde() {
        let home = PathBuf::from("/Users/x");
        assert_eq!(expand_include_path("~", &home), PathBuf::from("/Users/x"));
    }

    #[test]
    fn expand_include_path_expands_tilde_slash_prefix() {
        let home = PathBuf::from("/Users/x");
        assert_eq!(
            expand_include_path("~/.orbstack/ssh/config", &home),
            PathBuf::from("/Users/x/.orbstack/ssh/config"),
        );
    }

    #[test]
    fn expand_include_path_passes_absolute_paths_through() {
        let home = PathBuf::from("/Users/x");
        assert_eq!(
            expand_include_path("/etc/ssh/extra", &home),
            PathBuf::from("/etc/ssh/extra"),
        );
    }

    #[test]
    fn expand_include_path_resolves_relative_against_dot_ssh() {
        // Per ssh_config(5), bare relative paths in user config resolve
        // against `~/.ssh/`, not the CWD or the including file's parent.
        let home = PathBuf::from("/Users/x");
        assert_eq!(
            expand_include_path("config.d/uber", &home),
            PathBuf::from("/Users/x/.ssh/config.d/uber"),
        );
    }

    #[test]
    fn expand_include_path_does_not_misinterpret_tilde_user() {
        // `~alice/foo` is not supported — we treat it as a literal relative
        // path, which will then fail to read. This documents the limitation.
        let home = PathBuf::from("/Users/x");
        assert_eq!(
            expand_include_path("~alice/foo", &home),
            PathBuf::from("/Users/x/.ssh/~alice/foo"),
        );
    }

    #[test]
    fn parse_ssh_hosts_ignores_include_directives() {
        // Include resolution lives in the file walker, not the line parser.
        // If this ever started returning include paths as if they were hosts
        // it would corrupt the wildmenu.
        let got = parse_ssh_hosts("Include config.d/*\nHost real");
        assert_eq!(got, vec!["real".to_string()]);
    }

    #[test]
    fn load_walks_a_simple_include_directive() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        write_file(&home.join(".ssh/config"), "Include extra\n");
        write_file(&home.join(".ssh/extra"), "Host devpod-go\n");
        let got = load_ssh_hosts_from(&home.join(".ssh/config"), home);
        assert_eq!(got, vec!["devpod-go".to_string()]);
    }

    /// The exact layout that triggered the bug report: a near-empty root
    /// config that just `Include`s a glob-expanded directory.
    #[test]
    fn load_walks_uber_style_include_glob() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        write_file(&home.join(".ssh/config"), "Include config.d/*\n");
        write_file(&home.join(".ssh/config.d/common"), "Host *\n");
        write_file(
            &home.join(".ssh/config.d/uber"),
            "Host devpod-go\nHost devpod-web\n",
        );
        write_file(
            &home.join(".ssh/config.d/orbstack"),
            "Host orbstack-ubuntu\n",
        );
        let mut got = load_ssh_hosts_from(&home.join(".ssh/config"), home);
        got.sort();
        assert_eq!(got, vec!["devpod-go", "devpod-web", "orbstack-ubuntu"]);
    }

    #[test]
    fn load_expands_tilde_slash_in_include() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        write_file(
            &home.join(".ssh/config"),
            "Include ~/.orbstack/ssh/config\n",
        );
        write_file(&home.join(".orbstack/ssh/config"), "Host orb-container\n");
        let got = load_ssh_hosts_from(&home.join(".ssh/config"), home);
        assert_eq!(got, vec!["orb-container".to_string()]);
    }

    #[test]
    fn load_handles_nested_includes() {
        // a → b → c, hosts at every level.
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        write_file(
            &home.join(".ssh/config"),
            "Host root-host\nInclude level-b\n",
        );
        write_file(
            &home.join(".ssh/level-b"),
            "Host mid-host\nInclude level-c\n",
        );
        write_file(&home.join(".ssh/level-c"), "Host leaf-host\n");
        let mut got = load_ssh_hosts_from(&home.join(".ssh/config"), home);
        got.sort();
        assert_eq!(got, vec!["leaf-host", "mid-host", "root-host"]);
    }

    #[test]
    fn load_breaks_include_cycles() {
        // a → b → a. Without a visited set this stack-overflows.
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        write_file(&home.join(".ssh/config"), "Host a-host\nInclude b\n");
        write_file(&home.join(".ssh/b"), "Host b-host\nInclude config\n");
        let mut got = load_ssh_hosts_from(&home.join(".ssh/config"), home);
        got.sort();
        assert_eq!(got, vec!["a-host", "b-host"]);
    }

    #[test]
    fn load_skips_missing_included_files_silently() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        write_file(
            &home.join(".ssh/config"),
            "Host real-host\nInclude /no/such/file\nInclude does-not-exist\n",
        );
        let got = load_ssh_hosts_from(&home.join(".ssh/config"), home);
        assert_eq!(got, vec!["real-host".to_string()]);
    }

    #[test]
    fn load_returns_empty_when_root_config_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        // Don't write any config file — the root path doesn't exist.
        let got = load_ssh_hosts_from(&home.join(".ssh/config"), home);
        assert!(got.is_empty());
    }

    #[test]
    fn load_dedups_hosts_seen_via_multiple_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        write_file(&home.join(".ssh/config"), "Host shared\nInclude extra\n");
        write_file(&home.join(".ssh/extra"), "Host shared\nHost only-extra\n");
        let mut got = load_ssh_hosts_from(&home.join(".ssh/config"), home);
        got.sort();
        assert_eq!(got, vec!["only-extra", "shared"]);
    }

    #[test]
    fn collect_from_file_respects_max_include_depth() {
        // Build a non-cyclic include chain deeper than MAX_INCLUDE_DEPTH and
        // verify the cap kicks in. The chain numbering is by the *level
        // file* (level-N), not by the depth value the walker sees: depth 0
        // is the root, depth 1 is level-0, ..., depth MAX_INCLUDE_DEPTH is
        // level-(MAX_INCLUDE_DEPTH-1). The next level would be at depth
        // MAX_INCLUDE_DEPTH+1, which trips the guard before being read.
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        write_file(&home.join(".ssh/config"), "Host root\nInclude level-0\n");
        let chain_len = MAX_INCLUDE_DEPTH + 4;
        for i in 0..chain_len {
            let next = i + 1;
            write_file(
                &home.join(format!(".ssh/level-{i}")),
                &format!("Host h-{i}\nInclude level-{next}\n"),
            );
        }
        write_file(
            &home.join(format!(".ssh/level-{chain_len}")),
            &format!("Host h-{chain_len}\n"),
        );
        let got = load_ssh_hosts_from(&home.join(".ssh/config"), home);
        let last_reachable = MAX_INCLUDE_DEPTH - 1;
        let first_cut = MAX_INCLUDE_DEPTH;
        assert!(got.contains(&"root".into()));
        assert!(got.contains(&format!("h-{last_reachable}")));
        assert!(
            !got.contains(&format!("h-{first_cut}")),
            "depth cap should have stopped before h-{first_cut}",
        );
    }

    #[test]
    fn expand_glob_returns_empty_on_invalid_pattern() {
        // Unclosed `[` is a glob syntax error; the helper logs and falls
        // through to an empty Vec rather than propagating the error.
        let got = expand_glob(Path::new("/tmp/[unclosed"));
        assert!(got.is_empty());
    }
}
