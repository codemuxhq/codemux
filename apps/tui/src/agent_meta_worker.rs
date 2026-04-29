//! Background worker that surfaces "what model is the focused agent
//! running, and what git branch is its cwd on" to the status bar.
//!
//! ## Why a background worker
//!
//! Both lookups touch the filesystem — the branch lookup reads
//! `<cwd>/.git/HEAD` (cheap, but still I/O), and the model lookup tails
//! `~/.claude/projects/<encoded-cwd>/*.jsonl` (potentially many MB).
//! Doing either inline on the render hot path would risk a stutter. We
//! poll on a 2 s cadence — slow enough to be invisible to top, fast
//! enough that a `/model` change in claude is reflected before the
//! user has time to be confused about it.
//!
//! ## Single coordinator, focused-agent only
//!
//! One worker thread, not one per agent — mirrors the
//! [`crate::index_manager`] pattern. The runtime calls
//! [`AgentMetaWorker::set_target`] when focus changes; the worker
//! tracks the latest target and polls just that one. SSH agents are
//! not handled in v1 (the worker silently ignores them); see the
//! plan file for the deferral rationale.
//!
//! ## AD-1 carve-out
//!
//! Reading `~/.claude/projects/<encoded-cwd>/*.jsonl` is the single
//! sanctioned exception to AD-1's "never semantically parse Claude
//! Code" rule. We only read one specific file shape (the per-session
//! transcript JSONL), only extract one specific field
//! (`message.model` from the most recent assistant turn), only for
//! the focused agent, and only for local agents in v1. See AD-1's
//! amended prose in `docs/architecture.md`.

use std::ffi::OsStr;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use codemux_shared_kernel::AgentId;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, unbounded};

use crate::git_branch;

/// How often the worker re-polls the focused agent's branch and model.
/// Two seconds keeps the poll cost negligible (a stat + a small file
/// read) while still surfacing a `/model` change before the user
/// finishes wondering why nothing happened.
const POLL_INTERVAL: Duration = Duration::from_millis(2_000);

/// Pluggable IO surface for the worker. Production uses [`RealProbe`]
/// which calls into [`git_branch`] and the JSONL tailer; tests use a
/// scripted impl so the worker thread can be exercised without
/// touching the real filesystem and without sleeping.
///
/// `Send + Sync + 'static` because the worker thread captures it
/// behind `Box<dyn MetaProbe>` and reads through it concurrently with
/// the runtime potentially holding additional handles.
pub trait MetaProbe: Send + Sync + 'static {
    /// Read the git branch for `cwd`. `None` outside a git repo or on
    /// an unreadable HEAD.
    fn read_branch(&self, cwd: &Path) -> Option<String>;
    /// Read the most-recent assistant model for `cwd`'s Claude session
    /// transcript. `None` when no transcript exists yet, no assistant
    /// turn yet, or any IO failure.
    fn read_model(&self, cwd: &Path) -> Option<String>;
}

/// Production [`MetaProbe`]: forwards to the real filesystem readers.
/// Stateless; spawn one per worker. Tests substitute a scripted
/// implementation that records calls and returns canned values.
pub struct RealProbe;

impl MetaProbe for RealProbe {
    fn read_branch(&self, cwd: &Path) -> Option<String> {
        git_branch::resolve_local(cwd)
    }

    fn read_model(&self, cwd: &Path) -> Option<String> {
        current_model_for_cwd(cwd)
    }
}

/// Update emitted by the worker. The runtime drains these once per
/// frame and applies them to the matching `RuntimeAgent` (resolved by
/// `agent_id` so a focus change or reorder mid-poll doesn't misroute).
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum MetaEvent {
    /// New branch reading. `value = None` means "no longer in a git
    /// repo" or "couldn't read HEAD" — the segment then renders
    /// nothing.
    Branch {
        agent_id: AgentId,
        value: Option<String>,
    },
    /// New model reading. `value = None` means "no JSONL found yet"
    /// (claude hasn't started writing the session file) or "no
    /// assistant turn yet."
    Model {
        agent_id: AgentId,
        value: Option<String>,
    },
}

/// Control message the runtime sends to the worker. Single-target —
/// the worker keeps the most recent and discards earlier ones if the
/// user mashes through agents fast.
enum Control {
    /// Focus is on this agent; poll its branch and model.
    SetTarget { agent_id: AgentId, cwd: PathBuf },
    /// Focus moved to an agent the worker shouldn't poll (SSH agent,
    /// failed agent, no agents at all). Stop polling until the next
    /// `SetTarget`.
    ClearTarget,
}

/// Runtime-side handle to the worker. Owns the control sender and the
/// event receiver. Drop signals the worker to exit at its next wakeup.
pub struct AgentMetaWorker {
    cancel: Arc<AtomicBool>,
    control_tx: Sender<Control>,
    events: Receiver<MetaEvent>,
}

impl AgentMetaWorker {
    /// Spawn the worker thread with the production [`RealProbe`] and
    /// the production [`POLL_INTERVAL`]. The thread sleeps until
    /// [`Self::set_target`] supplies a focused agent; then it polls
    /// every [`POLL_INTERVAL`] and emits [`MetaEvent`]s.
    #[must_use]
    pub fn start() -> Self {
        Self::start_with(Box::new(RealProbe), POLL_INTERVAL)
    }

    /// Test seam: spawn the worker with a custom [`MetaProbe`] and
    /// poll cadence. Used by the worker integration tests to drive
    /// scripted IO and a fast (~50 ms) poll loop without touching
    /// the real filesystem or sleeping for two seconds per cycle.
    #[must_use]
    pub fn start_with(probe: Box<dyn MetaProbe>, poll_interval: Duration) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let (control_tx, control_rx) = unbounded::<Control>();
        let (events_tx, events) = unbounded::<MetaEvent>();
        let cancel_for_thread = Arc::clone(&cancel);
        thread::spawn(move || {
            worker_loop(
                &cancel_for_thread,
                &control_rx,
                &events_tx,
                probe.as_ref(),
                poll_interval,
            );
        });
        Self {
            cancel,
            control_tx,
            events,
        }
    }

    /// Tell the worker to track this agent. Idempotent: repeated calls
    /// with the same `(agent_id, cwd)` simply reset the poll cadence.
    /// A different agent supersedes the previous target on the next
    /// poll boundary.
    pub fn set_target(&self, agent_id: AgentId, cwd: PathBuf) {
        let _ = self.control_tx.send(Control::SetTarget { agent_id, cwd });
    }

    /// Clear the worker's target — used when focus moves to an SSH
    /// agent (worker doesn't handle remote in v1) or to a Failed/no
    /// agent at all.
    pub fn clear_target(&self) {
        let _ = self.control_tx.send(Control::ClearTarget);
    }

    /// Drain pending events. Non-blocking; returns the events that
    /// were ready at the moment of the call. Call once per frame.
    #[must_use]
    pub fn drain(&self) -> Vec<MetaEvent> {
        let mut out = Vec::new();
        while let Ok(ev) = self.events.try_recv() {
            out.push(ev);
        }
        out
    }
}

impl Drop for AgentMetaWorker {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// Worker entry point. Runs until the cancel flag is set or the
/// control channel disconnects.
///
/// Polling discipline:
/// - With no target: block on `control_rx.recv()` until the runtime
///   wakes us. Idle CPU is zero.
/// - With a target: poll once via `probe`, then `recv_timeout(poll_interval)`.
///   If a control message arrives during that wait, handle it
///   immediately (avoids the stale-cache window when the user
///   switches focus).
fn worker_loop(
    cancel: &AtomicBool,
    control_rx: &Receiver<Control>,
    events_tx: &Sender<MetaEvent>,
    probe: &dyn MetaProbe,
    poll_interval: Duration,
) {
    let mut target: Option<(AgentId, PathBuf)> = None;
    let mut last_branch: Option<String> = None;
    let mut last_model: Option<String> = None;

    while !cancel.load(Ordering::Relaxed) {
        let has_target = target.is_some();
        if let Some((agent_id, cwd)) = &target {
            // Branch lookup: cheap. Only emit when the value changed
            // so we don't spam the runtime with no-op events.
            let branch = probe.read_branch(cwd);
            if branch != last_branch {
                last_branch.clone_from(&branch);
                if events_tx
                    .send(MetaEvent::Branch {
                        agent_id: agent_id.clone(),
                        value: branch,
                    })
                    .is_err()
                {
                    return;
                }
            }
            // Model lookup: find newest jsonl, scan tail-first.
            let model = probe.read_model(cwd);
            if model != last_model {
                last_model.clone_from(&model);
                if events_tx
                    .send(MetaEvent::Model {
                        agent_id: agent_id.clone(),
                        value: model,
                    })
                    .is_err()
                {
                    return;
                }
            }
        }
        // The immutable borrow of `target` ends here so apply_control
        // can take the &mut. Splitting the loop body this way avoids
        // cloning the (AgentId, PathBuf) every tick just to satisfy
        // the borrow checker — a real perf hit on a 50 ms test cycle.
        if has_target {
            // Wait up to poll_interval for the next control message
            // or the next poll cycle, whichever comes first.
            match control_rx.recv_timeout(poll_interval) {
                Ok(msg) => {
                    apply_control(msg, &mut target, &mut last_branch, &mut last_model);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        } else {
            // Idle: block until the runtime hands us a target.
            match control_rx.recv() {
                Ok(msg) => {
                    apply_control(msg, &mut target, &mut last_branch, &mut last_model);
                }
                Err(_) => return,
            }
        }
    }
}

/// Apply a single control message to the worker state, including the
/// "flush cache when target changes" rule so a focus change doesn't
/// inherit the previous agent's last reading.
fn apply_control(
    msg: Control,
    target: &mut Option<(AgentId, PathBuf)>,
    last_branch: &mut Option<String>,
    last_model: &mut Option<String>,
) {
    match msg {
        Control::SetTarget { agent_id, cwd } => {
            let same_target = target.as_ref().is_some_and(|(id, _)| id == &agent_id);
            if !same_target {
                // Focus moved; flush cache so the new agent gets a
                // fresh poll instead of inheriting the previous
                // agent's last reading.
                *last_branch = None;
                *last_model = None;
            }
            *target = Some((agent_id, cwd));
        }
        Control::ClearTarget => {
            *target = None;
            *last_branch = None;
            *last_model = None;
        }
    }
}

/// Resolve the most-recent Claude `model` for `cwd` by:
///
/// 1. Encoding `cwd` into the directory name Claude uses for its
///    project transcripts (every `/` and `.` becomes `-`, leading
///    `-` preserved).
/// 2. Locating the most-recently-modified `.jsonl` file in
///    `~/.claude/projects/<encoded>/`.
/// 3. Scanning that file from the end, stopping at the first
///    `{"type":"assistant","message":{"model":"...",...}}` line.
///
/// Returns `None` for any failure (no `$HOME`, no projects dir, no
/// jsonl, no assistant line yet, malformed file). Caller treats
/// `None` as "no model to display."
#[must_use]
pub fn current_model_for_cwd(cwd: &Path) -> Option<String> {
    let home = std::env::var_os("HOME")?;
    let encoded = encode_cwd(cwd)?;
    let project_dir = PathBuf::from(home)
        .join(".claude")
        .join("projects")
        .join(encoded);
    let newest = newest_jsonl_in(&project_dir)?;
    latest_model_in_file(&newest)
}

/// Encode an absolute path into Claude's project-dir naming. Every
/// `/` and `.` in the path becomes `-`. Verified against the live
/// filesystem layout (see the plan file for the verification command).
///
/// Returns `None` for non-UTF-8 paths (none of the rest of codemux
/// handles them either; this is a unix-only TUI).
#[must_use]
pub fn encode_cwd(cwd: &Path) -> Option<String> {
    let s = cwd.to_str()?;
    Some(
        s.chars()
            .map(|c| if c == '/' || c == '.' { '-' } else { c })
            .collect(),
    )
}

/// Find the most-recently-modified `.jsonl` file in `dir`. Returns
/// `None` if the directory doesn't exist, can't be read, or contains
/// no `.jsonl` files.
fn newest_jsonl_in(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension() != Some(OsStr::new("jsonl")) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(mtime) = meta.modified() else { continue };
        match &best {
            Some((current_mtime, _)) if mtime <= *current_mtime => {}
            _ => best = Some((mtime, path)),
        }
    }
    best.map(|(_, p)| p)
}

/// Scan `path` linearly for the most recent line that describes an
/// assistant turn with a `model` field. Returns the raw model
/// identifier (e.g. `"claude-opus-4-7"`).
///
/// Implementation: read forward, remember the last assistant line
/// seen. For the JSONL files claude writes (~hundreds of KB to a few
/// MB), this is fast enough — we'd only need a true reverse scan if
/// the files grew into the tens of MB. Polled every 2 s; even a 5 MB
/// file scans in single-digit ms on any modern disk.
#[must_use]
pub fn latest_model_in_file(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let reader = BufReader::new(file);
    let mut latest: Option<String> = None;
    for line in reader.lines().map_while(Result::ok) {
        if let Some(model) = extract_assistant_model(&line) {
            latest = Some(model);
        }
    }
    latest
}

/// Given a single JSONL line, return the `model` field iff this line
/// is an assistant turn (`type == "assistant"`) and `message.model`
/// is a string. `None` for any other shape (user turn, system event,
/// malformed JSON, etc.).
///
/// Implementation: a partial `serde_json` deserialise targeting only
/// the two fields we care about. Uses `Cow<'a, str>` so a value
/// without escapes borrows from the input (zero-alloc fast path)
/// while a value with `\"` escapes lands in an owned `String`. The
/// hand-rolled `find()` parser this replaced was flagged in code
/// review for being brittle around escapes — typed parsing is
/// shorter, correct, and not measurably slower at the per-poll
/// volumes Claude writes (a few thousand lines).
fn extract_assistant_model(line: &str) -> Option<String> {
    use std::borrow::Cow;

    #[derive(serde::Deserialize)]
    struct Partial<'a> {
        #[serde(rename = "type", borrow)]
        type_: Option<Cow<'a, str>>,
        #[serde(borrow)]
        message: Option<MessagePartial<'a>>,
    }
    #[derive(serde::Deserialize)]
    struct MessagePartial<'a> {
        #[serde(borrow)]
        model: Option<Cow<'a, str>>,
    }
    let parsed: Partial<'_> = serde_json::from_str(line).ok()?;
    if parsed.type_?.as_ref() != "assistant" {
        return None;
    }
    parsed.message?.model.map(std::borrow::Cow::into_owned)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // ─── encode_cwd ────────────────────────────────────────────────

    #[test]
    fn encode_cwd_replaces_slashes_with_dashes() {
        // Verified against the live filesystem:
        // /Users/x/Workbench/repositories/codemux
        //   → -Users-x-Workbench-repositories-codemux
        let encoded = encode_cwd(Path::new("/Users/x/Workbench/repositories/codemux")).unwrap();
        assert_eq!(encoded, "-Users-x-Workbench-repositories-codemux");
    }

    #[test]
    fn encode_cwd_replaces_dots_with_dashes() {
        // Hidden directories like `.dotfiles` produce a double-dash
        // in the encoded name. /Users/x/.dotfiles → -Users-x--dotfiles
        let encoded = encode_cwd(Path::new("/Users/x/.dotfiles")).unwrap();
        assert_eq!(encoded, "-Users-x--dotfiles");
    }

    #[test]
    fn encode_cwd_handles_root() {
        assert_eq!(encode_cwd(Path::new("/")).unwrap(), "-");
    }

    // ─── extract_assistant_model ───────────────────────────────────

    #[test]
    fn extract_returns_model_for_assistant_line() {
        let line =
            r#"{"type":"assistant","message":{"model":"claude-opus-4-7","role":"assistant"}}"#;
        assert_eq!(
            extract_assistant_model(line),
            Some("claude-opus-4-7".into())
        );
    }

    #[test]
    fn extract_returns_none_for_user_line() {
        let line = r#"{"type":"user","message":{"role":"user","content":"hi"}}"#;
        assert_eq!(extract_assistant_model(line), None);
    }

    #[test]
    fn extract_returns_none_for_system_line_without_model() {
        let line = r#"{"type":"system","subtype":"init"}"#;
        assert_eq!(extract_assistant_model(line), None);
    }

    #[test]
    fn extract_returns_none_for_malformed_line() {
        // Half a line — stream got truncated. Don't crash, don't lie.
        let line = r#"{"type":"assistant","message":{"mod"#;
        assert_eq!(extract_assistant_model(line), None);
    }

    #[test]
    fn extract_returns_none_for_empty_line() {
        assert_eq!(extract_assistant_model(""), None);
    }

    // ─── latest_model_in_file ──────────────────────────────────────

    #[test]
    fn latest_model_returns_most_recent_assistant_model() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("session.jsonl");
        fs::write(
            &path,
            concat!(
                r#"{"type":"user","message":{"role":"user"}}"#,
                "\n",
                r#"{"type":"assistant","message":{"model":"claude-sonnet-4-6"}}"#,
                "\n",
                r#"{"type":"user","message":{"role":"user"}}"#,
                "\n",
                r#"{"type":"assistant","message":{"model":"claude-opus-4-7"}}"#,
                "\n",
            ),
        )
        .unwrap();
        // The user mid-session ran `/model` and switched to opus —
        // the worker must report the latest, not the first.
        assert_eq!(latest_model_in_file(&path), Some("claude-opus-4-7".into()),);
    }

    #[test]
    fn latest_model_returns_none_for_empty_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("empty.jsonl");
        fs::write(&path, "").unwrap();
        assert_eq!(latest_model_in_file(&path), None);
    }

    #[test]
    fn latest_model_returns_none_for_missing_file() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("does-not-exist.jsonl");
        assert_eq!(latest_model_in_file(&missing), None);
    }

    #[test]
    fn latest_model_returns_none_when_no_assistant_lines_yet() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("user-only.jsonl");
        fs::write(
            &path,
            concat!(
                r#"{"type":"user","message":{"role":"user"}}"#,
                "\n",
                r#"{"type":"system","subtype":"init"}"#,
                "\n",
            ),
        )
        .unwrap();
        assert_eq!(latest_model_in_file(&path), None);
    }

    // ─── newest_jsonl_in ───────────────────────────────────────────

    #[test]
    fn newest_jsonl_in_returns_none_for_missing_directory() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("nope");
        assert_eq!(newest_jsonl_in(&missing), None);
    }

    #[test]
    fn newest_jsonl_in_returns_none_when_no_jsonl_files_present() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("README.md"), "").unwrap();
        assert_eq!(newest_jsonl_in(tmp.path()), None);
    }

    #[test]
    fn newest_jsonl_in_picks_most_recent_by_mtime() {
        // Two jsonl files written in order; the second one is newer
        // by writing it after the first. Sleep a tick between writes
        // to make sure even a 1-second-resolution FS sees a difference.
        let tmp = TempDir::new().unwrap();
        let older = tmp.path().join("a.jsonl");
        let newer = tmp.path().join("b.jsonl");
        fs::write(&older, "x").unwrap();
        thread::sleep(Duration::from_millis(1_100));
        fs::write(&newer, "y").unwrap();
        let picked = newest_jsonl_in(tmp.path()).unwrap();
        assert_eq!(picked, newer);
    }

    // ─── apply_control ─────────────────────────────────────────────

    #[test]
    fn apply_control_set_target_when_idle_caches_nothing_yet() {
        let mut target: Option<(AgentId, PathBuf)> = None;
        let mut last_branch: Option<String> = None;
        let mut last_model: Option<String> = None;
        apply_control(
            Control::SetTarget {
                agent_id: AgentId::new("a"),
                cwd: PathBuf::from("/tmp/a"),
            },
            &mut target,
            &mut last_branch,
            &mut last_model,
        );
        assert_eq!(target, Some((AgentId::new("a"), PathBuf::from("/tmp/a"))),);
        assert!(last_branch.is_none());
        assert!(last_model.is_none());
    }

    #[test]
    fn apply_control_set_target_to_different_agent_flushes_cache() {
        // Switching focus must drop the previous agent's cached
        // values so the new agent's first poll posts a fresh
        // event — otherwise the user sees the previous agent's
        // model/branch flash for ~2s after switching.
        let mut target: Option<(AgentId, PathBuf)> =
            Some((AgentId::new("a"), PathBuf::from("/tmp/a")));
        let mut last_branch = Some("main".to_string());
        let mut last_model = Some("claude-opus-4-7".to_string());
        apply_control(
            Control::SetTarget {
                agent_id: AgentId::new("b"),
                cwd: PathBuf::from("/tmp/b"),
            },
            &mut target,
            &mut last_branch,
            &mut last_model,
        );
        assert_eq!(target.as_ref().unwrap().0, AgentId::new("b"));
        assert!(last_branch.is_none(), "cache must flush on agent change");
        assert!(last_model.is_none(), "cache must flush on agent change");
    }

    #[test]
    fn apply_control_set_target_to_same_agent_preserves_cache() {
        // Repeated set_target with the same agent (same focus, just
        // a duplicate notification) must NOT flush the cache —
        // otherwise the very next poll re-emits the same value as a
        // "change," doubling traffic on the events channel.
        let mut target: Option<(AgentId, PathBuf)> =
            Some((AgentId::new("a"), PathBuf::from("/tmp/a")));
        let mut last_branch = Some("main".to_string());
        let mut last_model = Some("claude-opus-4-7".to_string());
        apply_control(
            Control::SetTarget {
                agent_id: AgentId::new("a"),
                cwd: PathBuf::from("/tmp/a"),
            },
            &mut target,
            &mut last_branch,
            &mut last_model,
        );
        assert_eq!(last_branch.as_deref(), Some("main"));
        assert_eq!(last_model.as_deref(), Some("claude-opus-4-7"));
    }

    #[test]
    fn apply_control_clear_target_drops_target_and_cache() {
        let mut target: Option<(AgentId, PathBuf)> =
            Some((AgentId::new("a"), PathBuf::from("/tmp/a")));
        let mut last_branch = Some("main".to_string());
        let mut last_model = Some("claude-opus-4-7".to_string());
        apply_control(
            Control::ClearTarget,
            &mut target,
            &mut last_branch,
            &mut last_model,
        );
        assert!(target.is_none());
        assert!(last_branch.is_none());
        assert!(last_model.is_none());
    }

    // ─── worker integration tests ──────────────────────────────────
    //
    // Drive the real worker thread with a scripted [`MetaProbe`] and a
    // 50 ms poll interval so we can observe end-to-end behavior
    // (set_target → poll → emit → drain) without sleeping for the
    // production 2 s cadence and without touching the real
    // filesystem. The probe records every call so we can also assert
    // that focus changes drive the expected re-poll pattern.

    use std::sync::Mutex;
    use std::time::Instant;

    /// `MetaProbe` whose return values for each `read_branch` /
    /// `read_model` call are scripted in advance. Records every call
    /// (path arg) so a test can assert the worker queried the right
    /// agent. When the script runs out, the **last** scripted value
    /// repeats forever — that mirrors a stable filesystem state and
    /// keeps the "no-change → no-emit" tests deterministic across
    /// extra polls a slow CI box might race in.
    struct ScriptedProbe {
        branch_script: Mutex<Vec<Option<String>>>,
        model_script: Mutex<Vec<Option<String>>>,
        branch_calls: Mutex<Vec<PathBuf>>,
        model_calls: Mutex<Vec<PathBuf>>,
    }

    impl ScriptedProbe {
        fn new(branches: Vec<Option<String>>, models: Vec<Option<String>>) -> Self {
            Self {
                branch_script: Mutex::new(branches.into_iter().rev().collect()),
                model_script: Mutex::new(models.into_iter().rev().collect()),
                branch_calls: Mutex::new(Vec::new()),
                model_calls: Mutex::new(Vec::new()),
            }
        }
    }

    /// Pop the next scripted value, leaving the last one in place so
    /// further calls keep returning it. `None`-valued scripts behave
    /// the same as a real probe that consistently can't read the file.
    fn pop_or_repeat(script: &Mutex<Vec<Option<String>>>) -> Option<String> {
        let mut s = script.lock().unwrap();
        if s.len() > 1 {
            s.pop().unwrap_or(None)
        } else {
            s.last().cloned().unwrap_or(None)
        }
    }

    impl MetaProbe for ScriptedProbe {
        fn read_branch(&self, cwd: &Path) -> Option<String> {
            self.branch_calls.lock().unwrap().push(cwd.to_path_buf());
            pop_or_repeat(&self.branch_script)
        }

        fn read_model(&self, cwd: &Path) -> Option<String> {
            self.model_calls.lock().unwrap().push(cwd.to_path_buf());
            pop_or_repeat(&self.model_script)
        }
    }

    /// Wait up to `deadline` for the worker to emit `expected_count`
    /// events, then drain and return them. The worker polls every
    /// 50 ms; the upper bound here is deliberately generous (2 s) so
    /// a slow CI box doesn't flake.
    fn drain_until(worker: &AgentMetaWorker, expected_count: usize) -> Vec<MetaEvent> {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut events: Vec<MetaEvent> = Vec::new();
        while Instant::now() < deadline {
            events.extend(worker.drain());
            if events.len() >= expected_count {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        events
    }

    #[test]
    fn worker_emits_branch_and_model_after_first_poll() {
        let probe = Box::new(ScriptedProbe::new(
            vec![Some("main".to_string())],
            vec![Some("claude-opus-4-7".to_string())],
        ));
        let worker = AgentMetaWorker::start_with(probe, Duration::from_millis(50));
        worker.set_target(AgentId::new("a"), PathBuf::from("/tmp/a"));

        let events = drain_until(&worker, 2);
        assert!(
            events.contains(&MetaEvent::Branch {
                agent_id: AgentId::new("a"),
                value: Some("main".into()),
            }),
            "expected Branch event in {events:?}",
        );
        assert!(
            events.contains(&MetaEvent::Model {
                agent_id: AgentId::new("a"),
                value: Some("claude-opus-4-7".into()),
            }),
            "expected Model event in {events:?}",
        );
    }

    #[test]
    fn worker_does_not_re_emit_when_value_unchanged() {
        // Probe returns the same branch + model every call. The worker
        // must emit ONCE per value across many polls — re-emitting
        // would flood the runtime with redundant change notifications.
        let probe = Box::new(ScriptedProbe::new(
            vec![
                Some("main".to_string()),
                Some("main".to_string()),
                Some("main".to_string()),
            ],
            vec![
                Some("claude-opus-4-7".to_string()),
                Some("claude-opus-4-7".to_string()),
                Some("claude-opus-4-7".to_string()),
            ],
        ));
        let worker = AgentMetaWorker::start_with(probe, Duration::from_millis(50));
        worker.set_target(AgentId::new("a"), PathBuf::from("/tmp/a"));

        // Wait long enough for ~3 poll cycles, then count.
        thread::sleep(Duration::from_millis(250));
        let events = worker.drain();
        let branches = events
            .iter()
            .filter(|e| matches!(e, MetaEvent::Branch { .. }))
            .count();
        let models = events
            .iter()
            .filter(|e| matches!(e, MetaEvent::Model { .. }))
            .count();
        assert_eq!(
            branches, 1,
            "expected exactly 1 Branch event, got {events:?}"
        );
        assert_eq!(models, 1, "expected exactly 1 Model event, got {events:?}");
    }

    #[test]
    fn worker_emits_update_when_branch_changes_mid_session() {
        // Simulates a `git checkout`: poll 1 sees `main`, poll 2 sees
        // `feature`. The worker must emit a second Branch event with
        // the new value.
        let probe = Box::new(ScriptedProbe::new(
            vec![Some("main".to_string()), Some("feature".to_string())],
            vec![None, None],
        ));
        let worker = AgentMetaWorker::start_with(probe, Duration::from_millis(50));
        worker.set_target(AgentId::new("a"), PathBuf::from("/tmp/a"));

        // Wait for both Branch events.
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut branches: Vec<Option<String>> = Vec::new();
        while Instant::now() < deadline && branches.len() < 2 {
            for ev in worker.drain() {
                if let MetaEvent::Branch { value, .. } = ev {
                    branches.push(value);
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            branches,
            vec![Some("main".into()), Some("feature".into())],
            "expected sequential Branch updates",
        );
    }

    #[test]
    fn worker_clears_target_and_stops_emitting() {
        // After ClearTarget, the worker stops polling and emits no
        // further events for the cleared agent. Drain after a brief
        // wait should be empty.
        let probe = Box::new(ScriptedProbe::new(
            vec![Some("main".to_string())],
            vec![Some("claude-opus-4-7".to_string())],
        ));
        let worker = AgentMetaWorker::start_with(probe, Duration::from_millis(50));
        worker.set_target(AgentId::new("a"), PathBuf::from("/tmp/a"));
        // Wait for the initial events, then clear.
        let _ = drain_until(&worker, 2);
        worker.clear_target();
        // Wait long enough for several would-be poll cycles, confirm
        // nothing new arrives (probe stays unread; events stays empty).
        thread::sleep(Duration::from_millis(250));
        let events = worker.drain();
        assert!(
            events.is_empty(),
            "expected no events post-clear, got {events:?}"
        );
    }

    #[test]
    fn worker_switching_agent_flushes_cache_and_re_emits() {
        // Focus moves from agent A to agent B. The worker must treat
        // B as a fresh target — emit a Branch event for B even though
        // A's last branch was the same value (cache flush on agent
        // change is what apply_control's same-target test pinned).
        let probe = Box::new(ScriptedProbe::new(
            vec![Some("main".to_string()), Some("main".to_string())],
            vec![None, None],
        ));
        let worker = AgentMetaWorker::start_with(probe, Duration::from_millis(50));
        worker.set_target(AgentId::new("a"), PathBuf::from("/tmp/a"));
        // Wait for A's Branch event.
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if !worker.drain().is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(20));
        }
        // Switch to B.
        worker.set_target(AgentId::new("b"), PathBuf::from("/tmp/b"));
        // Expect a Branch event for B with value "main" (cache was
        // flushed on the agent change so the same value re-emits).
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut found_b = false;
        while Instant::now() < deadline && !found_b {
            for ev in worker.drain() {
                if matches!(ev, MetaEvent::Branch { agent_id, value: Some(ref v) }
                    if agent_id == AgentId::new("b") && v == "main")
                {
                    found_b = true;
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(found_b, "worker should emit a fresh Branch event for B");
    }

    #[test]
    fn worker_drop_signals_cancel_and_thread_exits() {
        // Sanity-check the Drop impl: the worker thread should
        // observe the cancel flag at its next wakeup and stop.
        // We can't wait on thread::JoinHandle (start() detaches), but
        // we can verify Drop doesn't panic and that subsequent
        // `drain` on a fresh worker still works (i.e. dropping doesn't
        // poison anything global).
        let probe = Box::new(ScriptedProbe::new(vec![None], vec![None]));
        let worker = AgentMetaWorker::start_with(probe, Duration::from_millis(50));
        worker.set_target(AgentId::new("a"), PathBuf::from("/tmp/a"));
        thread::sleep(Duration::from_millis(80));
        drop(worker);

        // A fresh worker still spins up cleanly after the previous
        // thread exited via cancel.
        let probe = Box::new(ScriptedProbe::new(
            vec![Some("dev".to_string())],
            vec![None],
        ));
        let worker2 = AgentMetaWorker::start_with(probe, Duration::from_millis(50));
        worker2.set_target(AgentId::new("b"), PathBuf::from("/tmp/b"));
        let events = drain_until(&worker2, 1);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, MetaEvent::Branch { value: Some(v), .. } if v == "dev"))
        );
    }

    #[test]
    fn real_probe_delegates_to_git_branch_resolver() {
        // Spot-check that RealProbe is wired to git_branch::resolve_local —
        // build a tiny fake repo in a tempdir and confirm the probe
        // returns the same branch the resolver would.
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("rp");
        fs::create_dir(&repo).unwrap();
        fs::create_dir(repo.join(".git")).unwrap();
        fs::write(
            repo.join(".git").join("HEAD"),
            "ref: refs/heads/probe-branch\n",
        )
        .unwrap();
        let probe = RealProbe;
        assert_eq!(probe.read_branch(&repo), Some("probe-branch".into()));
    }

    #[test]
    fn real_probe_read_model_returns_none_for_path_with_no_transcript() {
        // RealProbe.read_model goes through current_model_for_cwd, which
        // hits HOME-resolution and the projects-dir lookup. For a path
        // that has no encoded directory under ~/.claude/projects/, the
        // result is None — verifies the probe doesn't panic.
        let probe = RealProbe;
        assert_eq!(
            probe.read_model(Path::new(
                "/this/path/has/no/claude/transcript/anywhere/xyzzy"
            )),
            None,
        );
    }

    #[test]
    fn start_constructs_a_worker_with_production_defaults() {
        // Sanity-check the public production constructor: it spins up
        // a worker without panicking and the handle is usable. The
        // worker thread itself does nothing observable until we send
        // a target, but that's the contract — Drop cleans up.
        let worker = AgentMetaWorker::start();
        // A drain immediately after construction should be empty.
        assert!(worker.drain().is_empty());
        drop(worker);
    }

    #[test]
    fn extract_returns_none_when_value_lacks_closing_quote() {
        // Truncated line — invalid JSON. The typed deserialiser
        // returns Err, the helper returns None. Pinning so a future
        // refactor can't silently start succeeding on partial frames.
        let line = r#"{"type":"assistant","model":"unterminated"#;
        assert_eq!(extract_assistant_model(line), None);
    }

    #[test]
    fn extract_returns_decoded_string_with_json_escape_sequences() {
        // Legal JSON with an escaped `"` inside the model value. The
        // typed serde_json parser must decode the escape correctly —
        // the previous hand-rolled string scanner stripped escapes
        // from the result, which was wrong (and a brittleness flagged
        // by the Rust style guide).
        let line = r#"{"type":"assistant","message":{"model":"weird\"name"}}"#;
        assert_eq!(extract_assistant_model(line), Some("weird\"name".into()));
    }

    #[test]
    fn extract_returns_none_for_completely_invalid_json() {
        // Garbage that doesn't even open a JSON object. serde returns
        // Err on the first character, helper returns None.
        let line = "\"type\":\"assistant\"\"model\":\"";
        assert_eq!(extract_assistant_model(line), None);
    }

    #[test]
    fn encode_cwd_returns_none_for_non_utf8_path() {
        // Non-UTF8 path. On Unix this is constructible via OsStr's
        // byte representation; the `to_str()?` early return guards
        // against passing garbage downstream.
        use std::os::unix::ffi::OsStrExt;
        let bad: &OsStr = OsStr::from_bytes(&[0x80, 0xff, 0xfe]);
        let path = Path::new(bad);
        assert_eq!(encode_cwd(path), None);
    }
}
