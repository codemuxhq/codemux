//! Plumbing for codemux's piggyback on Claude Code's `statusLine`
//! callback contract.
//!
//! ## How the data flows
//!
//! 1. [`spawn_local_agent`](crate::runtime) injects `--settings '<json>'`
//!    when launching `claude` for a local agent. The injected JSON
//!    overrides only the `statusLine.command` field; the user's existing
//!    settings (managed → user → project → local) layer normally.
//! 2. After every assistant turn (and on `/compact`, mode changes;
//!    debounced 300 ms by Claude Code itself), Claude Code spawns the
//!    configured statusLine command and pipes a JSON snapshot of the
//!    session state to its stdin.
//! 3. The injected command is `codemux statusline-tee --out <path>`
//!    (the hidden subcommand wired in [`crate::main`]). It runs
//!    [`run_tee`] below: read stdin, atomically write to `<path>`,
//!    exit silently with no stdout. The stdout silence is deliberate —
//!    Claude renders whatever the command prints into its own in-pane
//!    status row, and the design choice is "codemux's chrome is the
//!    single source of truth for token usage."
//! 4. [`crate::agent_meta_worker`] reads the per-agent file on its
//!    poll cycle (same cadence as the model+branch lookup) and pushes
//!    a [`crate::agent_meta_worker::TokenUsage`] into the runtime,
//!    which threads it into [`crate::status_bar::SegmentCtx`] so
//!    [`crate::status_bar::segments::TokenSegment`] can render it.
//!
//! ## AD-1
//!
//! The data we consume is Claude Code's documented `statusLine`
//! callback contract — a stable, versioned API surface, not parsed
//! TUI output. Same architectural footing as the existing AD-1
//! carve-out in `agent_meta_worker.rs` for `~/.claude/settings.json`.

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use codemux_shared_kernel::AgentId;

/// Subdirectory under the runtime root where per-agent statusLine
/// snapshots live. Joined with the resolved runtime root by
/// [`statusline_dir`].
const SUBDIR: &str = "codemux";

/// Per-agent statusLine snapshot path. Layout:
///
/// - `$XDG_RUNTIME_DIR/codemux/agents/<id>.json` when `XDG_RUNTIME_DIR`
///   is set (Linux, most modern desktop environments).
/// - `$TMPDIR/codemux/agents/<id>.json` otherwise (macOS, BSDs, and
///   any environment without XDG). Falls back to `/tmp` when `TMPDIR`
///   is unset.
///
/// The parent directory is created on demand by [`ensure_parent`] so
/// the spawn path doesn't have to special-case first-run vs. warm.
#[must_use]
pub fn statusline_path_for(id: &AgentId) -> PathBuf {
    statusline_path_with(id, &resolve_runtime_root())
}

/// Test seam for [`statusline_path_for`] that takes the resolved
/// runtime root explicitly. Production code calls
/// [`statusline_path_for`]; tests use this to assert the path layout
/// without mutating process env (which would race under the default
/// parallel test runner).
fn statusline_path_with(id: &AgentId, root: &Path) -> PathBuf {
    let mut p = root.join(SUBDIR);
    p.push("agents");
    p.push(format!("{}.json", id.as_str()));
    p
}

/// Resolve the runtime root directory: `$XDG_RUNTIME_DIR` if set,
/// `$TMPDIR` if not, `/tmp` as a last resort. Pulled out so the
/// production path stays a one-liner and tests can drive
/// [`statusline_path_with`] directly.
fn resolve_runtime_root() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(xdg);
    }
    if let Some(tmp) = std::env::var_os("TMPDIR") {
        return PathBuf::from(tmp);
    }
    PathBuf::from("/tmp")
}

/// Ensure the parent directory of `path` exists. Mode bits are left to
/// the OS (umask-derived) — the JSON contains nothing more sensitive
/// than what Claude Code already passes through unprivileged
/// subprocess args.
///
/// # Errors
/// Returns the underlying `io::Error` when the directory can't be
/// created.
pub fn ensure_parent(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    Ok(())
}

/// Atomically write `bytes` to `path` via tempfile-in-same-dir + rename.
/// The same-dir constraint is what guarantees the rename is atomic on
/// every Unix filesystem we run on (cross-filesystem rename falls back
/// to copy+unlink and is not atomic).
///
/// # Errors
/// Surfaces any I/O error from create/write/rename. The tempfile is
/// best-effort cleaned up on failure; a leftover `.<id>.json.tmp.<pid>`
/// is harmless and gets overwritten on the next run.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    ensure_parent(path)?;
    let tmp = tmp_path_for(path);
    {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        // Best-effort tempfile cleanup; if rename failed the tempfile
        // is the orphan we just made. `.ok()` swallows the result the
        // way the project's `let_underscore_must_use` lint expects.
        std::fs::remove_file(&tmp).ok();
        return Err(e);
    }
    Ok(())
}

/// Build a same-dir temp path that won't collide with concurrent
/// invocations on the same agent. Two concurrent statusLine commands
/// for the same agent should not happen in practice (Claude Code
/// debounces and cancels in-flight invocations), but if they did,
/// PID-suffixing keeps them from clobbering each other's tempfiles.
#[must_use]
fn tmp_path_for(final_path: &Path) -> PathBuf {
    let parent = final_path.parent().unwrap_or_else(|| Path::new("."));
    let name = final_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("statusline.json");
    let pid = std::process::id();
    parent.join(format!(".{name}.tmp.{pid}"))
}

/// Build the JSON value for `--settings` that wires `statusLine.command`
/// at `<codemux_bin> statusline-tee --out <out_path>`. `refresh_interval_secs`
/// is forwarded to Claude Code as the statusLine `refreshInterval` (in
/// seconds) when set; omitted otherwise so Claude stays on its
/// event-driven cadence.
///
/// The returned string is suitable to pass directly as the value of
/// the `--settings` argument (`claude --settings '<json>'`). Path
/// components inside the `command` string are POSIX-shell-quoted via
/// [`sh_quote`] because Claude Code runs the command through a shell
/// on its side (the `command` field is a shell command string, not an
/// argv list).
#[must_use]
pub fn build_settings_json(
    codemux_bin: &Path,
    out_path: &Path,
    refresh_interval_secs: Option<u32>,
) -> String {
    let bin = sh_quote(&codemux_bin.to_string_lossy());
    let out = sh_quote(&out_path.to_string_lossy());
    let cmd = format!("{bin} statusline-tee --out {out}");
    let payload = SettingsOverlay {
        status_line: StatusLineOverlay {
            kind: "command",
            command: &cmd,
            refresh_interval: refresh_interval_secs,
        },
    };
    // Cannot fail in practice (the type is fully serializable); fall
    // back to a minimal literal so a serde regression doesn't kill
    // the spawn path entirely.
    serde_json::to_string(&payload)
        .unwrap_or_else(|_| format!(r#"{{"statusLine":{{"type":"command","command":"{cmd}"}}}}"#))
}

/// Outer envelope for [`build_settings_json`]'s payload. Only the
/// `statusLine` field is serialized so Claude Code's settings layering
/// (managed → user → project → local) merges everything else from the
/// existing files unchanged.
#[derive(Debug, serde::Serialize)]
struct SettingsOverlay<'a> {
    #[serde(rename = "statusLine")]
    status_line: StatusLineOverlay<'a>,
}

/// Inner shape for the `statusLine` overlay — `type`, `command`, and
/// optionally `refreshInterval` to mirror Claude Code's documented
/// statusLine schema. The `refreshInterval` field is opt-in
/// (`skip_serializing_if`) so the JSON stays minimal when the user
/// hasn't asked for time-based refresh.
#[derive(Debug, serde::Serialize)]
struct StatusLineOverlay<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    command: &'a str,
    #[serde(rename = "refreshInterval", skip_serializing_if = "Option::is_none")]
    refresh_interval: Option<u32>,
}

/// POSIX-shell-quote `s` by single-quoting and replacing each interior
/// `'` with `'\''`. Idempotent on strings with no special chars
/// (modulo the wrapping quotes) and survives any byte sequence a
/// filesystem path might legitimately contain.
#[must_use]
fn sh_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str(r"'\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Run the `statusline-tee` subcommand: read stdin to EOF, atomically
/// write to `out_path`, exit. Designed to fail-open: any error is
/// logged to stderr (which Claude Code discards) and signaled via the
/// `Err` return. Claude then renders whatever string we wrote to
/// stdout — which is empty, by design.
///
/// # Errors
/// Returns the underlying `io::Error` when stdin can't be read or the
/// snapshot can't be written.
pub fn run_tee(out_path: &Path) -> io::Result<()> {
    let mut buf = Vec::with_capacity(4096);
    io::stdin().read_to_end(&mut buf)?;
    write_atomic(out_path, &buf)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn statusline_path_layout_is_root_codemux_agents_id_json() {
        // Pin the on-disk layout exactly. Tests that mutated
        // $XDG_RUNTIME_DIR / $TMPDIR would race under the default
        // parallel test runner; the test seam takes the resolved root
        // explicitly so the assertion is hermetic.
        let id = AgentId::new("abc");
        let path = statusline_path_with(&id, Path::new("/run/user/1000"));
        assert_eq!(
            path,
            PathBuf::from("/run/user/1000/codemux/agents/abc.json"),
        );
    }

    #[test]
    fn statusline_path_includes_full_agent_id_in_filename() {
        // AgentIds with dashes / dots / Arc<str>-y bytes must round-trip
        // through the path unchanged — the file is later read back by
        // path, so any mangling here would silently break per-agent
        // isolation.
        let id = AgentId::new("agent-1.local");
        let path = statusline_path_with(&id, Path::new("/x"));
        assert_eq!(path, PathBuf::from("/x/codemux/agents/agent-1.local.json"));
    }

    #[test]
    fn write_atomic_creates_parent_and_writes_bytes() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("nested/dir/file.json");
        write_atomic(&path, b"hello").unwrap();
        let got = std::fs::read(&path).unwrap();
        assert_eq!(got, b"hello");
    }

    #[test]
    fn write_atomic_overwrites_existing_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("file.json");
        write_atomic(&path, b"first").unwrap();
        write_atomic(&path, b"second").unwrap();
        let got = std::fs::read(&path).unwrap();
        assert_eq!(got, b"second");
    }

    #[test]
    fn write_atomic_leaves_no_tempfile_on_success() {
        // The tempfile-then-rename dance must not leave the .tmp
        // sibling around — otherwise repeated runs would accumulate
        // garbage in $XDG_RUNTIME_DIR.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("file.json");
        write_atomic(&path, b"x").unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(std::result::Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".file.json.tmp.")
            })
            .collect();
        assert!(
            leftovers.is_empty(),
            "expected no .tmp.<pid> sibling, got {leftovers:?}",
        );
    }

    // ─── build_settings_json ──────────────────────────────────────

    #[test]
    fn build_settings_json_emits_command_string_with_tee_invocation() {
        // The injected JSON must point Claude Code at exactly the
        // tee subcommand we wired up in main.rs. A drift between
        // the binary path here and the subcommand name there would
        // silently break per-turn token reporting.
        let bin = Path::new("/usr/local/bin/codemux");
        let out = Path::new("/tmp/codemux/agents/abc.json");
        let json = build_settings_json(bin, out, None);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["statusLine"]["type"], "command");
        let cmd = parsed["statusLine"]["command"].as_str().unwrap();
        assert!(
            cmd.contains("codemux"),
            "command must invoke the codemux binary; got {cmd:?}",
        );
        assert!(
            cmd.contains("statusline-tee"),
            "command must invoke the statusline-tee subcommand; got {cmd:?}",
        );
        assert!(
            cmd.contains("--out"),
            "command must pass --out; got {cmd:?}",
        );
        assert!(
            parsed["statusLine"].get("refreshInterval").is_none(),
            "refreshInterval must be omitted when not configured",
        );
    }

    #[test]
    fn build_settings_json_includes_refresh_interval_when_set() {
        // Configurable knob: when the user sets
        // [ui.segments.tokens] refresh_interval_secs, the value must
        // round-trip into Claude Code's `refreshInterval` field.
        let bin = Path::new("/x/codemux");
        let out = Path::new("/tmp/o.json");
        let json = build_settings_json(bin, out, Some(7));
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["statusLine"]["refreshInterval"], 7);
    }

    #[test]
    fn build_settings_json_quotes_paths_with_spaces() {
        // Spaces in the binary or out path (common on macOS) must be
        // POSIX-shell-quoted because Claude Code shells the
        // statusLine command out — without quoting, "/Users/Foo Bar/codemux"
        // would split into two argv tokens and the spawn would fail
        // with "command not found: /Users/Foo".
        let bin = Path::new("/Users/Foo Bar/codemux");
        let out = Path::new("/tmp space/agents/x.json");
        let json = build_settings_json(bin, out, None);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let cmd = parsed["statusLine"]["command"].as_str().unwrap();
        assert!(
            cmd.contains("'/Users/Foo Bar/codemux'"),
            "binary path must be single-quoted; got {cmd:?}",
        );
        assert!(
            cmd.contains("'/tmp space/agents/x.json'"),
            "out path must be single-quoted; got {cmd:?}",
        );
    }

    #[test]
    fn build_settings_json_escapes_single_quotes_in_paths() {
        // The POSIX `'\''` escape — exotic, but a path under
        // ~/Library/Application Support/Don't Panic/ would otherwise
        // produce broken shell. Pin the escape sequence so a future
        // simplification of `sh_quote` doesn't silently regress.
        let bin = Path::new("/x/it's/codemux");
        let out = Path::new("/tmp/x.json");
        let json = build_settings_json(bin, out, None);
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let cmd = parsed["statusLine"]["command"].as_str().unwrap();
        assert!(
            cmd.contains(r"'/x/it'\''s/codemux'"),
            "single quote inside path must be POSIX-escaped; got {cmd:?}",
        );
    }
}
