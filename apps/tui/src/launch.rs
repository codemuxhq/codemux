//! Launch-mode resolution for the codemux CLI.
//!
//! `codemux` supports three launch behaviors, modelled on tmux/zellij:
//!
//! - **bare `codemux`** — spawn a fresh agent in the current cwd. Do
//!   NOT auto-hydrate any persisted agents into the tab list. The
//!   fresh agent still gets written through to the DB, so it is
//!   available for a later `--continue` / `--resume`.
//! - **`codemux --continue`** — pick the most-recently-attached
//!   persisted agent and resume it. No fresh agent, no tab list of
//!   other persisted rows. If no persisted agents exist, falls back
//!   silently to the bare behavior.
//! - **`codemux --resume`** — print a numbered picker on stdout, read
//!   the user's selection from stdin, then resume the chosen agent.
//!   When no rows exist, prints a one-line message to stderr and
//!   exits non-zero.
//!
//! All picker IO and DB queries here run BEFORE
//! `crossterm::terminal::enable_raw_mode`. Any error in this module
//! surfaces as a normal stderr message + non-zero exit, with the
//! terminal still in cooked mode.
//!
//! ## Architectural boundary
//!
//! The strings `--session-id` and `--resume` (as passed to `claude`)
//! continue to live ONLY in `runtime::build_claude_args` and the
//! daemon's `build_child_args`. The codemux-facing flags
//! `--continue` / `--resume` are an entirely separate surface — this
//! module never speaks Claude's CLI dialect.

use std::fmt::Write as _;
use std::io::{self, BufRead, Write};
use std::time::{Duration, SystemTime};

/// Maximum garbage-input retries the `--resume` picker accepts before
/// giving up and exiting non-zero. Set up here as a module-level
/// constant so the prompt-loop body keeps to one screen and the
/// "items after statements" lint stays quiet.
const PICKER_MAX_TRIES: usize = 3;

use codemux_session::domain::Agent;

/// Resolved launch behavior after CLI parsing + (for `--resume`)
/// interactive selection.
///
/// Wrapping the selection in a value type keeps `main.rs` free of any
/// branching beyond "ask the user, then call `runtime::run` with this."
/// The selected agent (when present) is the full domain row — the
/// runtime hydrates it directly, no second `load_all` needed.
#[derive(Debug)]
pub enum LaunchMode {
    /// Bare `codemux`: spawn a fresh agent, do not hydrate any
    /// persisted row into the tab list.
    Fresh,
    /// `--continue` or `--resume`: hydrate this single agent as the
    /// only tab and resume its `session_id` (if present) via the
    /// existing AD-2 wiring. No fresh agent in addition.
    SelectedAgent(Agent),
}

/// Sort `agents` in place by `last_attached_at_unix DESC`, treating
/// `None` as the smallest value so never-attached rows land at the
/// end. Stable so an agent's relative position on a tie (same epoch
/// second, two rows attached in the same wall clock second) follows
/// the load order — which itself is `ORDER BY id` from the `SQLite`
/// adapter, giving a deterministic UX.
pub fn sort_by_recency_desc(agents: &mut [Agent]) {
    agents.sort_by(|a, b| match (a.last_attached_at, b.last_attached_at) {
        (Some(left), Some(right)) => right.cmp(&left),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
}

/// Pick the most-recently-attached agent, if any.
///
/// Used by `--continue`. Equivalent to "sort by recency, take first."
#[must_use]
pub fn pick_most_recent(mut agents: Vec<Agent>) -> Option<Agent> {
    sort_by_recency_desc(&mut agents);
    agents.into_iter().next()
}

/// Outcome of parsing a single line of user input from the picker.
///
/// Models the three things the prompt can produce: a valid number
/// inside the displayed range, a quit signal, or an error message
/// fit for re-prompt.
#[derive(Debug, Eq, PartialEq)]
pub enum PickerSelection {
    /// 1-based number the user typed, already validated to be inside
    /// `1..=count`. Converted to a 0-based index by the caller.
    Picked(usize),
    /// User hit empty enter, `q`, or `quit`. Caller exits silently
    /// with a non-zero status.
    Quit,
    /// Garbage input (non-numeric, out-of-range, etc.). The message
    /// is human-readable and ready to print before the re-prompt.
    Invalid(String),
}

/// Parse one line of user input against a picker of size `count`.
///
/// `count` must be `>= 1` — callers guard against the empty case
/// before reaching this function (an empty picker exits before any
/// prompt fires).
pub fn parse_picker_input(line: &str, count: usize) -> PickerSelection {
    let trimmed = line.trim();
    if trimmed.is_empty()
        || trimmed.eq_ignore_ascii_case("q")
        || trimmed.eq_ignore_ascii_case("quit")
    {
        return PickerSelection::Quit;
    }
    match trimmed.parse::<usize>() {
        Ok(n) if (1..=count).contains(&n) => PickerSelection::Picked(n),
        Ok(n) => PickerSelection::Invalid(format!(
            "{n} is out of range (pick 1-{count}, or q to quit)"
        )),
        Err(_) => PickerSelection::Invalid(format!(
            "couldn't parse `{trimmed}` (pick 1-{count}, or q to quit)"
        )),
    }
}

/// Format a `SystemTime` as a coarse English relative duration:
/// "just now", "1 minute ago", "3 hours ago", "2 weeks ago", etc.
/// Never-attached rows (i.e. `None`) return `"(never)"`.
///
/// Resolution caps at "weeks" because anything older isn't actionable
/// at picker speed — the user is going to skim, not parse a date. The
/// thresholds are the boring obvious ones (60s, 60min, 24h, 7d).
#[must_use]
pub fn relative_time(now: SystemTime, then: Option<SystemTime>) -> String {
    let Some(then) = then else {
        return "(never)".to_string();
    };
    // If a row claims to be from the future (clock skew, manually-edited
    // DB), don't render a negative duration — render "just now."
    let delta = now.duration_since(then).unwrap_or(Duration::ZERO);
    let secs = delta.as_secs();
    if secs < 5 {
        return "just now".to_string();
    }
    if secs < 60 {
        return format!("{secs} seconds ago");
    }
    let minutes = secs / 60;
    if minutes < 60 {
        return pluralize(minutes, "minute");
    }
    let hours = minutes / 60;
    if hours < 24 {
        return pluralize(hours, "hour");
    }
    let days = hours / 24;
    if days < 7 {
        return pluralize(days, "day");
    }
    let weeks = days / 7;
    pluralize(weeks, "week")
}

fn pluralize(n: u64, unit: &str) -> String {
    if n == 1 {
        format!("1 {unit} ago")
    } else {
        format!("{n} {unit}s ago")
    }
}

/// Display string for an agent's host: "local" for the local-host
/// row, the SSH target for SSH rows, or the raw `host_id` for anything
/// unexpected (defensive — current schema only emits two variants).
#[must_use]
pub fn host_label(host_id: &str) -> String {
    if host_id == "local" {
        "local".to_string()
    } else if let Some(target) = host_id.strip_prefix("ssh:") {
        target.to_string()
    } else {
        host_id.to_string()
    }
}

/// Display path for the agent's cwd. Empty paths render as `?` so
/// the picker columns never collapse. We do NOT contract `$HOME` to
/// `~`: the picker is glance-friendly, not a shell prompt, and
/// surfacing the literal path matches what the user pasted in.
#[must_use]
pub fn cwd_label(agent: &Agent) -> String {
    let s = agent.cwd.display().to_string();
    if s.is_empty() { "?".to_string() } else { s }
}

/// Render the picker block (header + numbered rows + prompt) as a
/// string. Pure function so the caller's IO is a single
/// `write_all` and tests can assert on the formatted output.
///
/// `now` is injected (rather than read inside) so tests have a
/// stable wall-clock reference.
#[must_use]
pub fn format_picker(agents: &[Agent], now: SystemTime) -> String {
    let mut out = String::new();
    out.push_str("Saved sessions (most recent first):\n");
    let width = (agents.len()).to_string().len();
    for (i, agent) in agents.iter().enumerate() {
        let n = i + 1;
        let id = agent.id.as_str();
        let cwd = cwd_label(agent);
        let host = host_label(agent.host_id.as_str());
        let when = relative_time(now, agent.last_attached_at);
        // Single-line, tab-separated. Tabs let the user's terminal
        // align columns regardless of cwd length — much friendlier
        // than hand-padding when one path is `/tmp` and another is
        // 80 chars of absolute monorepo. `write!` instead of
        // `push_str(&format!(...))` keeps the format-into-existing-
        // String lint quiet AND avoids the intermediate allocation.
        let _ = writeln!(out, "{n:>width$}) {id}\t{cwd}\t{host}\t{when}");
    }
    out
}

/// Drive the `--resume` interactive picker against the provided
/// reader / writers. Returns the selected [`Agent`] on success, or
/// an error message ready to be printed to stderr.
///
/// Splits stdout from stderr so the prompts and the "no saved
/// sessions" exit message land in the right channels. The reader is
/// expected to deliver one line per `read_line` (matching `stdin().lock()`).
///
/// # Errors
///
/// - Returns `LaunchError::NoSavedSessions` when `agents` is empty.
/// - Returns `LaunchError::Aborted` when the user picks `q` / `quit`
///   / empty line, or when retries are exhausted (3 invalid inputs
///   in a row).
/// - Returns `LaunchError::Io` on a stdin / stdout / stderr error.
pub fn run_picker<R: BufRead, W: Write, E: Write>(
    mut agents: Vec<Agent>,
    now: SystemTime,
    stdin: &mut R,
    stdout: &mut W,
    stderr: &mut E,
) -> Result<Agent, LaunchError> {
    if agents.is_empty() {
        return Err(LaunchError::NoSavedSessions);
    }
    sort_by_recency_desc(&mut agents);
    let block = format_picker(&agents, now);
    stdout
        .write_all(block.as_bytes())
        .map_err(LaunchError::Io)?;

    let count = agents.len();
    for attempt in 1..=PICKER_MAX_TRIES {
        write!(stdout, "pick (1-{count}, q to quit): ").map_err(LaunchError::Io)?;
        stdout.flush().map_err(LaunchError::Io)?;
        let mut line = String::new();
        let read = stdin.read_line(&mut line).map_err(LaunchError::Io)?;
        // `read_line` returning Ok(0) is EOF: treat like quit so the
        // user can pipe `echo "" | codemux --resume` and have it
        // abort cleanly.
        if read == 0 {
            return Err(LaunchError::Aborted);
        }
        match parse_picker_input(&line, count) {
            PickerSelection::Picked(n) => {
                let agent = agents.remove(n - 1);
                return Ok(agent);
            }
            PickerSelection::Quit => return Err(LaunchError::Aborted),
            PickerSelection::Invalid(msg) => {
                writeln!(stderr, "{msg}").map_err(LaunchError::Io)?;
                if attempt == PICKER_MAX_TRIES {
                    writeln!(stderr, "giving up after {PICKER_MAX_TRIES} invalid inputs.")
                        .map_err(LaunchError::Io)?;
                    return Err(LaunchError::Aborted);
                }
            }
        }
    }
    // Unreachable: the loop returns in every branch by attempt == PICKER_MAX_TRIES.
    Err(LaunchError::Aborted)
}

/// Failure modes for the launch-mode resolver. Modelled with
/// `thiserror` per AD-17 so the binary's `color-eyre` edge prints a
/// clean cause chain.
///
/// `Aborted` is not really an "error" in the failure sense — it's the
/// user politely declining to pick a session. The caller maps it to
/// a quiet non-zero exit (no stack trace). Kept in the same enum so
/// the caller's `match` is exhaustive.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LaunchError {
    /// `--resume` was invoked against an empty DB. Prints a single
    /// friendly line; no resume to attempt.
    #[error("no saved sessions; run `codemux` to start fresh")]
    NoSavedSessions,
    /// User typed `q` / `quit` / empty / EOF, or exhausted retries.
    /// Quiet non-zero exit; no diagnostic noise.
    #[error("aborted")]
    Aborted,
    /// stdin / stdout / stderr blew up while running the picker. We
    /// surface this so the caller can choose to retry (probably
    /// not — the terminal is in a weird state).
    #[error("picker io error")]
    Io(#[source] io::Error),
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::io::Cursor;
    use std::path::PathBuf;
    use std::time::{Duration, UNIX_EPOCH};

    use codemux_session::domain::{Agent, AgentStatus};
    use codemux_shared_kernel::{AgentId, HostId};

    use super::*;

    fn agent(id: &str, last: Option<SystemTime>) -> Agent {
        Agent {
            id: AgentId::new(id),
            host_id: HostId::new("local"),
            label: id.to_string(),
            cwd: PathBuf::from("/work/repo"),
            group_ids: Vec::new(),
            session_id: Some(format!("{id}-uuid")),
            status: AgentStatus::Dead,
            last_attached_at: last,
        }
    }

    #[test]
    fn relative_time_within_five_seconds_is_just_now() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert_eq!(relative_time(now, Some(now)), "just now");
        assert_eq!(
            relative_time(now, Some(now - Duration::from_secs(4))),
            "just now",
        );
    }

    #[test]
    fn relative_time_seconds_and_minutes() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert_eq!(
            relative_time(now, Some(now - Duration::from_secs(30))),
            "30 seconds ago",
        );
        assert_eq!(
            relative_time(now, Some(now - Duration::from_secs(60))),
            "1 minute ago",
        );
        assert_eq!(
            relative_time(now, Some(now - Duration::from_secs(120))),
            "2 minutes ago",
        );
    }

    #[test]
    fn relative_time_hours_days_weeks() {
        let now = UNIX_EPOCH + Duration::from_secs(2_000_000);
        assert_eq!(
            relative_time(now, Some(now - Duration::from_secs(3 * 60 * 60))),
            "3 hours ago",
        );
        assert_eq!(
            relative_time(now, Some(now - Duration::from_secs(24 * 60 * 60))),
            "1 day ago",
        );
        assert_eq!(
            relative_time(now, Some(now - Duration::from_secs(2 * 7 * 24 * 60 * 60))),
            "2 weeks ago",
        );
    }

    #[test]
    fn relative_time_none_is_never() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000);
        assert_eq!(relative_time(now, None), "(never)");
    }

    #[test]
    fn relative_time_future_is_just_now() {
        // Clock skew: persisted timestamp newer than `now`. We must
        // not panic on the negative duration; render "just now".
        let now = UNIX_EPOCH + Duration::from_secs(100);
        let future = now + Duration::from_secs(5);
        assert_eq!(relative_time(now, Some(future)), "just now");
    }

    #[test]
    fn parse_picker_accepts_valid_number() {
        assert_eq!(parse_picker_input("2", 3), PickerSelection::Picked(2));
        assert_eq!(parse_picker_input("  1  ", 3), PickerSelection::Picked(1));
    }

    #[test]
    fn parse_picker_rejects_out_of_range() {
        match parse_picker_input("0", 3) {
            PickerSelection::Invalid(msg) => assert!(msg.contains("0 is out of range")),
            other => panic!("expected Invalid, got {other:?}"),
        }
        match parse_picker_input("4", 3) {
            PickerSelection::Invalid(msg) => assert!(msg.contains("4 is out of range")),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn parse_picker_accepts_quit_variants() {
        assert_eq!(parse_picker_input("", 3), PickerSelection::Quit);
        assert_eq!(parse_picker_input("q", 3), PickerSelection::Quit);
        assert_eq!(parse_picker_input("Q\n", 3), PickerSelection::Quit);
        assert_eq!(parse_picker_input("quit", 3), PickerSelection::Quit);
        assert_eq!(parse_picker_input(" QUIT ", 3), PickerSelection::Quit);
    }

    #[test]
    fn parse_picker_rejects_garbage() {
        match parse_picker_input("abc", 3) {
            PickerSelection::Invalid(msg) => assert!(msg.contains("couldn't parse")),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn sort_by_recency_desc_nulls_last() {
        let mut v = vec![
            agent("never", None),
            agent("old", Some(UNIX_EPOCH + Duration::from_secs(100))),
            agent("new", Some(UNIX_EPOCH + Duration::from_secs(500))),
        ];
        sort_by_recency_desc(&mut v);
        let ids: Vec<&str> = v.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, vec!["new", "old", "never"]);
    }

    #[test]
    fn pick_most_recent_returns_newest_or_none() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000);
        let chosen = pick_most_recent(vec![
            agent("a", Some(now - Duration::from_secs(60))),
            agent("b", Some(now)),
            agent("c", None),
        ]);
        assert_eq!(chosen.map(|a| a.id.as_str().to_string()), Some("b".into()));

        assert!(pick_most_recent(Vec::new()).is_none());
    }

    #[test]
    fn format_picker_renders_all_rows() {
        let now = UNIX_EPOCH + Duration::from_secs(10_000);
        let rows = vec![
            agent("a1", Some(now - Duration::from_secs(60 * 60))),
            agent("a2", None),
        ];
        let out = format_picker(&rows, now);
        assert!(out.contains("Saved sessions"));
        assert!(out.contains("1) a1"));
        assert!(out.contains("2) a2"));
        assert!(out.contains("1 hour ago"));
        assert!(out.contains("(never)"));
    }

    #[test]
    fn run_picker_returns_no_saved_sessions_when_empty() {
        let mut stdin = Cursor::new(Vec::new());
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let err = run_picker(
            Vec::new(),
            SystemTime::UNIX_EPOCH,
            &mut stdin,
            &mut stdout,
            &mut stderr,
        )
        .unwrap_err();
        assert!(matches!(err, LaunchError::NoSavedSessions));
        // No prompts written before the early-exit.
        assert!(stdout.is_empty());
    }

    #[test]
    fn run_picker_selects_valid_number_on_first_try() {
        let now = UNIX_EPOCH + Duration::from_secs(10_000);
        let rows = vec![
            agent("recent", Some(now)),
            agent("older", Some(now - Duration::from_secs(60))),
        ];
        let mut stdin = Cursor::new(b"2\n".to_vec());
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let picked = run_picker(rows, now, &mut stdin, &mut stdout, &mut stderr).unwrap();
        // The recency sort puts `recent` first; index 2 is `older`.
        assert_eq!(picked.id.as_str(), "older");
    }

    #[test]
    fn run_picker_quit_aborts_silently() {
        let now = UNIX_EPOCH + Duration::from_secs(10_000);
        let rows = vec![agent("only", Some(now))];
        let mut stdin = Cursor::new(b"q\n".to_vec());
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let err = run_picker(rows, now, &mut stdin, &mut stdout, &mut stderr).unwrap_err();
        assert!(matches!(err, LaunchError::Aborted));
    }

    #[test]
    fn run_picker_reprompts_on_garbage_then_succeeds() {
        let now = UNIX_EPOCH + Duration::from_secs(10_000);
        let rows = vec![agent("only", Some(now))];
        let mut stdin = Cursor::new(b"abc\n1\n".to_vec());
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let picked = run_picker(rows, now, &mut stdin, &mut stdout, &mut stderr).unwrap();
        assert_eq!(picked.id.as_str(), "only");
        let stderr_text = String::from_utf8(stderr).unwrap();
        assert!(stderr_text.contains("couldn't parse"));
    }

    #[test]
    fn run_picker_gives_up_after_three_invalid_inputs() {
        let now = UNIX_EPOCH + Duration::from_secs(10_000);
        let rows = vec![agent("only", Some(now))];
        let mut stdin = Cursor::new(b"x\ny\nz\n".to_vec());
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let err = run_picker(rows, now, &mut stdin, &mut stdout, &mut stderr).unwrap_err();
        assert!(matches!(err, LaunchError::Aborted));
        let stderr_text = String::from_utf8(stderr).unwrap();
        assert!(stderr_text.contains("giving up"));
    }

    #[test]
    fn host_label_renders_local_and_ssh() {
        assert_eq!(host_label("local"), "local");
        assert_eq!(host_label("ssh:user@host"), "user@host");
        assert_eq!(host_label("anything-else"), "anything-else");
    }
}
