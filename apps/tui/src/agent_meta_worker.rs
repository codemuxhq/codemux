//! Background worker that surfaces "what model + effort is the focused
//! agent running, and what git branch is its cwd on" to the status bar.
//!
//! ## Why a background worker
//!
//! Both lookups touch the filesystem — the branch lookup reads
//! `<cwd>/.git/HEAD` (cheap, but still I/O), and the model lookup
//! reads `~/.claude/settings.json` (small JSON config). Doing either
//! inline on the render hot path would risk a stutter. We poll on a
//! 2 s cadence — slow enough to be invisible to top, fast enough that
//! a `/model` change in claude is reflected before the user has time
//! to be confused about it.
//!
//! ## Single coordinator, focused-agent only
//!
//! One worker thread, not one per agent — mirrors the
//! [`crate::index_manager`] pattern. The runtime calls
//! [`AgentMetaWorker::set_target`] when focus changes; the worker
//! tracks the latest target and polls just that one. SSH agents are
//! not handled in v1 (the worker silently ignores them): the branch
//! lookup needs a local cwd, and the model/effort lookup reads the
//! *local* user's settings, which may not match the remote claude
//! instance's state.
//!
//! ## AD-1 carve-out
//!
//! Reading `~/.claude/settings.json` is the single sanctioned
//! exception to AD-1's "never semantically parse Claude Code" rule.
//! We only read one specific file (the user's global claude config),
//! only extract two fields (`model`, `effortLevel`), only for the
//! focused local agent. The previous approach tailed the per-session
//! JSONL transcript for the model; that was dropped because the
//! "newest jsonl by mtime" heuristic was fragile when multiple
//! sessions shared a project directory (the wrong session's transcript
//! could win the mtime race, masking `/model` changes in the active
//! agent). settings.json is a single-writer file that updates
//! immediately on `/model`, so the bug class disappears. See AD-1's
//! amended prose in `docs/architecture.md`.

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
/// which calls into [`git_branch`] and reads `~/.claude/settings.json`;
/// tests use a scripted impl so the worker thread can be exercised
/// without touching the real filesystem and without sleeping.
///
/// `Send + Sync + 'static` because the worker thread captures it
/// behind `Box<dyn MetaProbe>` and reads through it concurrently with
/// the runtime potentially holding additional handles.
pub trait MetaProbe: Send + Sync + 'static {
    /// Read the git branch for `cwd`. `None` outside a git repo or on
    /// an unreadable HEAD.
    fn read_branch(&self, cwd: &Path) -> Option<String>;
    /// Read the user's currently-active claude model (alias) and
    /// reasoning effort level from `~/.claude/settings.json`. Returns
    /// `None` when the file can't be read or has no `model` field.
    /// The effort level is a separate `Option` because it may be
    /// absent from the file (older claude versions, default value
    /// not yet customised).
    fn read_model_effort(&self) -> Option<ModelEffort>;
}

/// Pair returned by [`MetaProbe::read_model_effort`]. The model alias
/// is required (no point reporting "we read the file but no model");
/// effort is optional (older settings.json files may not have it, and
/// claude only writes the field when it's been customised).
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ModelEffort {
    /// Raw alias as it appears in `~/.claude/settings.json`'s `model`
    /// field — `"opus[1m]"`, `"sonnet"`, `"claude-opus-4-7[1m]"`, etc.
    /// The status-bar segment shortens this for display; the worker
    /// passes it through verbatim.
    pub model: String,
    /// Reasoning effort level from `effortLevel` — `"low"`, `"medium"`,
    /// `"high"`, `"xhigh"`. `None` when the field is absent. The
    /// segment hides the effort badge for the default value.
    pub effort: Option<String>,
}

/// Production [`MetaProbe`]: forwards to the real filesystem readers.
/// Stateless; spawn one per worker. Tests substitute a scripted
/// implementation that records calls and returns canned values.
pub struct RealProbe;

impl MetaProbe for RealProbe {
    fn read_branch(&self, cwd: &Path) -> Option<String> {
        git_branch::resolve_local(cwd)
    }

    fn read_model_effort(&self) -> Option<ModelEffort> {
        current_model_and_effort()
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
    /// New model + effort reading. `value = None` means
    /// `~/.claude/settings.json` couldn't be read (no HOME, file
    /// missing, malformed) or has no `model` field. Carries both
    /// fields together because they live in the same file and read
    /// atomically — splitting them into two events would risk a frame
    /// where the user briefly sees a model with the wrong effort.
    Model {
        agent_id: AgentId,
        value: Option<ModelEffort>,
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
    let mut last_model_effort: Option<ModelEffort> = None;

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
            // Model + effort lookup: read the global claude
            // settings.json, paired together so the segment never
            // shows model-without-effort or vice versa for one frame.
            let model_effort = probe.read_model_effort();
            if model_effort != last_model_effort {
                last_model_effort.clone_from(&model_effort);
                if events_tx
                    .send(MetaEvent::Model {
                        agent_id: agent_id.clone(),
                        value: model_effort,
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
                    apply_control(msg, &mut target, &mut last_branch, &mut last_model_effort);
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        } else {
            // Idle: block until the runtime hands us a target.
            match control_rx.recv() {
                Ok(msg) => {
                    apply_control(msg, &mut target, &mut last_branch, &mut last_model_effort);
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
    last_model_effort: &mut Option<ModelEffort>,
) {
    match msg {
        Control::SetTarget { agent_id, cwd } => {
            let same_target = target.as_ref().is_some_and(|(id, _)| id == &agent_id);
            if !same_target {
                // Focus moved; flush cache so the new agent gets a
                // fresh poll instead of inheriting the previous
                // agent's last reading.
                *last_branch = None;
                *last_model_effort = None;
            }
            *target = Some((agent_id, cwd));
        }
        Control::ClearTarget => {
            *target = None;
            *last_branch = None;
            *last_model_effort = None;
        }
    }
}

/// Read the user's currently-active claude model and effort level
/// from `~/.claude/settings.json`.
///
/// Returns `None` for any failure (no `$HOME`, settings.json missing,
/// malformed JSON, no `model` field). The model field is required to
/// return `Some` — without it there's nothing to display, and pairing
/// an effort with no model would render a stray bracket on the bar.
///
/// The `model` field is the alias the user picked from `/model`
/// (e.g. `"opus[1m]"`, `"sonnet"`). The status-bar segment shortens
/// it for display; we pass it through verbatim.
///
/// Why settings.json instead of the per-session JSONL transcript:
/// the previous tailing approach picked the newest `.jsonl` by mtime
/// in the project directory, which raced when multiple sessions
/// shared a project dir (host vs. test instance vs. subagent
/// transcripts) — a `/model` switch in one agent could appear to do
/// nothing because the worker was scanning a different session's
/// transcript. settings.json is a single-writer global file that
/// updates immediately on `/model`. See AD-1 in `docs/architecture.md`.
#[must_use]
pub fn current_model_and_effort() -> Option<ModelEffort> {
    let home = std::env::var_os("HOME")?;
    let path = PathBuf::from(home).join(".claude").join("settings.json");
    read_model_effort_from(&path)
}

/// Parse `path` as claude's settings.json and pull out `model` +
/// `effortLevel`. Split out from [`current_model_and_effort`] so a
/// test can drive it against a tempfile without monkey-patching
/// `$HOME`.
#[must_use]
pub fn read_model_effort_from(path: &Path) -> Option<ModelEffort> {
    #[derive(serde::Deserialize)]
    struct Partial {
        model: Option<String>,
        #[serde(rename = "effortLevel")]
        effort_level: Option<String>,
    }
    let bytes = std::fs::read(path).ok()?;
    let parsed: Partial = serde_json::from_slice(&bytes).ok()?;
    let model = parsed.model?;
    Some(ModelEffort {
        model,
        effort: parsed.effort_level,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;
    use std::time::Instant;
    use tempfile::TempDir;

    // ─── read_model_effort_from ────────────────────────────────────

    /// Helper to write a settings.json into a tempdir and return the
    /// path. Tests use this instead of monkey-patching `$HOME` so the
    /// real `current_model_and_effort()` path can stay simple and we
    /// still exercise the full read+parse path.
    fn write_settings(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("settings.json");
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn read_model_effort_returns_both_fields_when_present() {
        // The shape we read is the production claude code settings.json
        // file shape (verified live): a top-level `model` alias plus
        // an `effortLevel`. Other fields exist (env vars, hooks, etc.)
        // and must be ignored without failing the parse.
        let tmp = TempDir::new().unwrap();
        let path = write_settings(
            tmp.path(),
            r#"{"model":"opus[1m]","effortLevel":"xhigh","unrelated":42}"#,
        );
        let got = read_model_effort_from(&path).unwrap();
        assert_eq!(got.model, "opus[1m]");
        assert_eq!(got.effort.as_deref(), Some("xhigh"));
    }

    #[test]
    fn read_model_effort_returns_some_with_no_effort_when_field_absent() {
        // Older claude versions wrote settings.json without
        // `effortLevel`; the segment treats missing-effort the same
        // as default-effort (no badge shown). Here we pin that the
        // parse path doesn't fail just because the optional field is
        // missing.
        let tmp = TempDir::new().unwrap();
        let path = write_settings(tmp.path(), r#"{"model":"sonnet"}"#);
        let got = read_model_effort_from(&path).unwrap();
        assert_eq!(got.model, "sonnet");
        assert!(got.effort.is_none());
    }

    #[test]
    fn read_model_effort_returns_none_when_model_field_missing() {
        // Without a model alias there's nothing to display. Returning
        // None (vs Some with empty model) means the segment slot
        // collapses cleanly instead of rendering a stray bracket.
        let tmp = TempDir::new().unwrap();
        let path = write_settings(tmp.path(), r#"{"effortLevel":"high"}"#);
        assert!(read_model_effort_from(&path).is_none());
    }

    #[test]
    fn read_model_effort_returns_none_for_missing_file() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("does-not-exist.json");
        assert!(read_model_effort_from(&missing).is_none());
    }

    #[test]
    fn read_model_effort_returns_none_for_malformed_json() {
        // Half-written file mid-flush from claude code's writer.
        // `read_model_effort_from` must swallow the error and return
        // None — the next poll will see the completed file.
        let tmp = TempDir::new().unwrap();
        let path = write_settings(tmp.path(), r#"{"model":"opus[1m]","#);
        assert!(read_model_effort_from(&path).is_none());
    }

    // ─── apply_control ─────────────────────────────────────────────

    #[test]
    fn apply_control_set_target_when_idle_caches_nothing_yet() {
        let mut target: Option<(AgentId, PathBuf)> = None;
        let mut last_branch: Option<String> = None;
        let mut last_model_effort: Option<ModelEffort> = None;
        apply_control(
            Control::SetTarget {
                agent_id: AgentId::new("a"),
                cwd: PathBuf::from("/tmp/a"),
            },
            &mut target,
            &mut last_branch,
            &mut last_model_effort,
        );
        assert_eq!(target, Some((AgentId::new("a"), PathBuf::from("/tmp/a"))),);
        assert!(last_branch.is_none());
        assert!(last_model_effort.is_none());
    }

    #[test]
    fn apply_control_set_target_to_different_agent_flushes_cache() {
        // Switching focus must drop the previous agent's cached
        // values so the new agent's first poll posts a fresh
        // event — otherwise the user sees the previous agent's
        // model/branch flash for ~2s after switching. With model+
        // effort now coming from a global file the per-agent flush
        // is technically redundant for the model side (everyone
        // reads the same value), but keeping it symmetric with the
        // branch side avoids a special case in the cache logic.
        let mut target: Option<(AgentId, PathBuf)> =
            Some((AgentId::new("a"), PathBuf::from("/tmp/a")));
        let mut last_branch = Some("main".to_string());
        let mut last_model_effort = Some(ModelEffort {
            model: "opus[1m]".into(),
            effort: Some("xhigh".into()),
        });
        apply_control(
            Control::SetTarget {
                agent_id: AgentId::new("b"),
                cwd: PathBuf::from("/tmp/b"),
            },
            &mut target,
            &mut last_branch,
            &mut last_model_effort,
        );
        assert_eq!(target.as_ref().unwrap().0, AgentId::new("b"));
        assert!(last_branch.is_none(), "cache must flush on agent change");
        assert!(
            last_model_effort.is_none(),
            "cache must flush on agent change"
        );
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
        let mut last_model_effort = Some(ModelEffort {
            model: "opus[1m]".into(),
            effort: None,
        });
        apply_control(
            Control::SetTarget {
                agent_id: AgentId::new("a"),
                cwd: PathBuf::from("/tmp/a"),
            },
            &mut target,
            &mut last_branch,
            &mut last_model_effort,
        );
        assert_eq!(last_branch.as_deref(), Some("main"));
        assert_eq!(
            last_model_effort.as_ref().map(|m| m.model.as_str()),
            Some("opus[1m]")
        );
    }

    #[test]
    fn apply_control_clear_target_drops_target_and_cache() {
        let mut target: Option<(AgentId, PathBuf)> =
            Some((AgentId::new("a"), PathBuf::from("/tmp/a")));
        let mut last_branch = Some("main".to_string());
        let mut last_model_effort = Some(ModelEffort {
            model: "opus[1m]".into(),
            effort: Some("xhigh".into()),
        });
        apply_control(
            Control::ClearTarget,
            &mut target,
            &mut last_branch,
            &mut last_model_effort,
        );
        assert!(target.is_none());
        assert!(last_branch.is_none());
        assert!(last_model_effort.is_none());
    }

    // ─── worker integration tests ──────────────────────────────────
    //
    // Drive the real worker thread with a scripted [`MetaProbe`] and
    // a 50 ms poll interval so we can observe end-to-end behavior
    // (set_target → poll → emit → drain) without sleeping for the
    // production 2 s cadence and without touching the real filesystem.

    /// `MetaProbe` whose return values for each `read_branch` /
    /// `read_model_effort` call are scripted in advance. Records
    /// every call (path arg, count) so a test can assert the worker
    /// queried the right agent. When the script runs out, the **last**
    /// scripted value repeats forever — that mirrors a stable file
    /// state and keeps the "no-change → no-emit" tests deterministic
    /// across extra polls a slow CI box might race in.
    struct ScriptedProbe {
        branch_script: Mutex<Vec<Option<String>>>,
        model_script: Mutex<Vec<Option<ModelEffort>>>,
        branch_calls: Mutex<Vec<PathBuf>>,
        model_calls: Mutex<u64>,
    }

    impl ScriptedProbe {
        fn new(branches: Vec<Option<String>>, models: Vec<Option<ModelEffort>>) -> Self {
            Self {
                branch_script: Mutex::new(branches.into_iter().rev().collect()),
                model_script: Mutex::new(models.into_iter().rev().collect()),
                branch_calls: Mutex::new(Vec::new()),
                model_calls: Mutex::new(0),
            }
        }
    }

    /// Pop the next scripted value, leaving the last one in place so
    /// further calls keep returning it. `None`-valued scripts behave
    /// the same as a real probe that consistently can't read the file.
    fn pop_or_repeat<T: Clone>(script: &Mutex<Vec<Option<T>>>) -> Option<T> {
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

        fn read_model_effort(&self) -> Option<ModelEffort> {
            *self.model_calls.lock().unwrap() += 1;
            pop_or_repeat(&self.model_script)
        }
    }

    /// Wait up to a deadline for the worker to emit `expected_count`
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
            vec![Some(ModelEffort {
                model: "opus[1m]".into(),
                effort: Some("xhigh".into()),
            })],
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
                value: Some(ModelEffort {
                    model: "opus[1m]".into(),
                    effort: Some("xhigh".into()),
                }),
            }),
            "expected Model event in {events:?}",
        );
    }

    #[test]
    fn worker_does_not_re_emit_when_value_unchanged() {
        // Probe returns the same branch + model+effort every call.
        // The worker must emit ONCE per value across many polls —
        // re-emitting would flood the runtime with redundant change
        // notifications.
        let me = ModelEffort {
            model: "opus[1m]".into(),
            effort: Some("xhigh".into()),
        };
        let probe = Box::new(ScriptedProbe::new(
            vec![
                Some("main".to_string()),
                Some("main".to_string()),
                Some("main".to_string()),
            ],
            vec![Some(me.clone()), Some(me.clone()), Some(me.clone())],
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
    fn worker_emits_update_when_model_or_effort_changes_mid_session() {
        // Simulates `/model` mid-session: poll 1 sees `opus[1m]`+xhigh,
        // poll 2 sees `sonnet` with no effort. Both transitions
        // (model alias change AND effort drop) must produce a fresh
        // event so the segment renders the new pair.
        let probe = Box::new(ScriptedProbe::new(
            vec![None, None],
            vec![
                Some(ModelEffort {
                    model: "opus[1m]".into(),
                    effort: Some("xhigh".into()),
                }),
                Some(ModelEffort {
                    model: "sonnet".into(),
                    effort: None,
                }),
            ],
        ));
        let worker = AgentMetaWorker::start_with(probe, Duration::from_millis(50));
        worker.set_target(AgentId::new("a"), PathBuf::from("/tmp/a"));

        // Wait for both Model events.
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut models: Vec<Option<ModelEffort>> = Vec::new();
        while Instant::now() < deadline && models.len() < 2 {
            for ev in worker.drain() {
                if let MetaEvent::Model { value, .. } = ev {
                    models.push(value);
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            models,
            vec![
                Some(ModelEffort {
                    model: "opus[1m]".into(),
                    effort: Some("xhigh".into()),
                }),
                Some(ModelEffort {
                    model: "sonnet".into(),
                    effort: None,
                }),
            ],
            "expected sequential Model updates",
        );
    }

    #[test]
    fn worker_clears_target_and_stops_emitting() {
        // After ClearTarget, the worker stops polling and emits no
        // further events for the cleared agent. Drain after a brief
        // wait should be empty.
        let probe = Box::new(ScriptedProbe::new(
            vec![Some("main".to_string())],
            vec![Some(ModelEffort {
                model: "opus[1m]".into(),
                effort: None,
            })],
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

    // ─── RealProbe ─────────────────────────────────────────────────

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
    fn real_probe_read_model_effort_does_not_panic() {
        // The shim just delegates to current_model_and_effort, which
        // is in turn covered by read_model_effort_from tests against
        // tempfiles. We can't override $HOME from a parallel test
        // without a global lock, so the smoke check here just pins
        // that the production wiring runs end-to-end.
        let probe = RealProbe;
        let _ = probe.read_model_effort();
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
}
