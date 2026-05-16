use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use clap::{ArgGroup, Parser, Subcommand};
use codemux_session::BinaryAgentSpawner;
use color_eyre::Result;
use color_eyre::eyre::WrapErr;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};

mod agent_meta_worker;
mod bootstrap_worker;
mod config;
mod fuzzy_worker;
mod git_branch;
mod host_title;
mod index_cache;
mod index_manager;
mod index_worker;
mod keymap;
mod launch;
mod log_tail;
mod persistence;
mod pty_title;
mod repo_name;
mod runtime;
mod spawn;
mod ssh_config;
mod status_bar;
mod statusline_ipc;
mod toast;
mod url_scan;
use launch::{LaunchError, LaunchMode};
use runtime::{NavStyle, TestSeams};

#[derive(Debug, Parser)]
#[command(name = "codemux", version, about)]
#[command(args_conflicts_with_subcommands = true)]
// `--continue` and `--resume` are mutually exclusive: each is a
// terminal launch-mode decision, and combining them would mean
// "auto-pick the most recent AND ask the user to pick" — which is
// nonsense. Modeling as a clap `ArgGroup` lets clap reject the
// combination at parse time with a clean error instead of leaving
// us to write a runtime check that fires after the terminal has
// already been touched.
#[command(group(
    ArgGroup::new("launch-mode")
        .args(["resume_continue", "resume_pick"])
        .multiple(false)
))]
struct Cli {
    /// Working directory for the initial agent. Omit to inherit the
    /// shell's current pwd (the common case); pass a path to spawn the
    /// agent there instead. Relative paths are resolved against the
    /// shell's pwd. The path is validated up-front — a missing or
    /// non-directory path exits non-zero before the terminal switches
    /// to raw mode.
    #[arg(value_name = "PATH")]
    path: Option<PathBuf>,

    /// Initial navigator style. Toggle at runtime with the prefix-key + v.
    #[arg(long, value_enum, env = "CODEMUX_NAV", default_value = "popup")]
    nav: NavStyle,

    /// Enable an in-TUI log strip at the bottom of the screen showing
    /// the most recent log line. Logs always also go to
    /// `~/.cache/codemux/logs/codemux.log`; this flag controls
    /// whether they're additionally surfaced in-band. Off by default
    /// so the TUI never gets contaminated by tracing output during
    /// normal use.
    #[arg(long, short = 'l', env = "CODEMUX_LOG")]
    log: bool,

    /// Path to the agent binary. Defaults to `claude` (resolved via
    /// `$PATH`). Override with `--agent-bin` or `CODEMUX_AGENT_BIN`;
    /// the env var is the seam the E2E harness uses to point codemux at
    /// the in-tree `fake_agent` instead of the real `claude`. Read at
    /// the clap parse boundary only — production code never reaches
    /// for `std::env::var`.
    #[arg(
        long,
        value_name = "BIN",
        env = "CODEMUX_AGENT_BIN",
        default_value = "claude"
    )]
    agent_bin: PathBuf,

    /// E2E test seam: panic on the main thread after this many
    /// milliseconds, AFTER raw mode has been entered. Pins AC-038 (a
    /// panic restores the terminal before the report is printed); the
    /// harness boots codemux, the runtime arms a deadline at the top
    /// of the event loop, and the panic fires from the next tick that
    /// observes the deadline expired. Hidden from `--help` because it
    /// is only useful for the slow-tier PTY tests.
    #[arg(long, value_name = "MS", hide = true)]
    panic_after: Option<u64>,

    /// E2E test seam: when set, replaces the production `OsUrlOpener`
    /// with a recording opener that appends each opened URL on its
    /// own line to this file. Pins AC-041 (Ctrl+click hands the URL
    /// to the OS opener) without actually spawning a browser. Hidden
    /// from `--help` for the same reason as `--panic-after`.
    #[arg(long, value_name = "PATH", hide = true)]
    record_opens_to: Option<PathBuf>,

    /// Path to the codemux state database. Defaults to
    /// `$XDG_STATE_HOME/codemux/state.db` (or
    /// `$HOME/.local/state/codemux/state.db` if XDG is unset). The
    /// file and any missing parent directories are created on first
    /// run; subsequent runs reuse the same DB so persisted sessions
    /// are available to `--continue` and `--resume`. Useful in
    /// tests to redirect persistence at a tempfile, and for the rare
    /// user who wants a non-XDG location.
    #[arg(long, value_name = "PATH", env = "CODEMUX_STATE_DB")]
    state_db: Option<PathBuf>,

    /// Launch the most-recently-attached persisted session instead
    /// of spawning a fresh agent. Mirrors `tmux attach` semantics: no
    /// new tab, no other persisted rows hydrated into the navigator —
    /// just the one previous session, resumed via
    /// `claude --resume <session_id>`. If no persisted sessions
    /// exist, falls back silently to the bare-`codemux` fresh-spawn
    /// behavior. Mutually exclusive with `--resume`.
    #[arg(long = "continue", short = 'c', group = "launch-mode")]
    resume_continue: bool,

    /// Print a numbered picker of every persisted session on stdout
    /// and resume the one the user picks. Reads the selection from
    /// stdin BEFORE the terminal enters raw mode, so the prompt
    /// integrates with the user's normal shell scrollback. Exits
    /// non-zero with a one-line "no saved sessions" message when the
    /// database is empty. Mutually exclusive with `--continue`.
    #[arg(long = "resume", short = 'r', group = "launch-mode")]
    resume_pick: bool,

    /// Hidden IPC subcommands. The default `codemux [PATH]` invocation
    /// stays a positional-only path; subcommands kick in only when
    /// explicitly named. `args_conflicts_with_subcommands` keeps clap
    /// from rejecting the no-subcommand path when no positional was
    /// passed.
    #[command(subcommand)]
    cmd: Option<Command>,
}

/// Hidden IPC subcommands. Not surfaced in `--help` because they are
/// internal plumbing — codemux invokes them on itself, not the user.
#[derive(Debug, Subcommand)]
enum Command {
    /// Tee a Claude Code statusLine JSON snapshot to a file.
    ///
    /// Reads stdin to EOF (Claude Code pipes the JSON in), atomically
    /// writes to `--out`, exits silently with no stdout. Wired into
    /// each spawned agent's `--settings '{statusLine.command:...}'`
    /// override by [`runtime::spawn_local_agent`]; the per-agent
    /// metadata worker reads the resulting file each tick to surface
    /// token usage on the codemux status bar.
    ///
    /// See `apps/tui/src/statusline_ipc.rs` for the on-disk layout
    /// (`$XDG_RUNTIME_DIR/codemux/agents/<id>.json`).
    #[command(hide = true)]
    StatuslineTee {
        /// Absolute path to write the snapshot to. The parent
        /// directory is created on demand.
        #[arg(long, value_name = "PATH")]
        out: PathBuf,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // Subcommand fast-path: short-circuit before color-eyre, tracing,
    // or config load. The tee subcommand is invoked by Claude Code as
    // a child process on every assistant turn; it must stay cheap and
    // must not contaminate `~/.cache/codemux/logs/codemux.log` (tens
    // of writes per session would drown out actual TUI logging).
    if let Some(Command::StatuslineTee { out }) = &cli.cmd {
        return statusline_ipc::run_tee(out)
            .map_err(|e| color_eyre::eyre::eyre!("statusline-tee {}: {e}", out.display()));
    }

    color_eyre::install()?;
    // Build the in-memory tail buffer up-front so the tracing
    // subscriber and the runtime see the same Arc<Mutex<...>>. When
    // `--log` is off, the runtime keeps the buffer but never reads
    // it (the bottom strip render is gated on the flag) — keeping
    // both paths producing the same value avoids a cfg-style branch
    // in the subscriber init.
    let tail = log_tail::LogTail::new();
    init_tracing(&tail).wrap_err("init tracing")?;
    // Load config (or defaults if missing) before touching the terminal so a
    // malformed config file fails loud instead of corrupting raw mode.
    let config = config::load()?;
    // Resolve and validate the cwd before raw mode for the same
    // reason: a typo'd path should print a clean error, not corrupt
    // the user's terminal mid-init. When the user omits `[PATH]` we
    // capture `std::env::current_dir()` ourselves rather than passing
    // `None` through to `portable-pty` — the latter is documented to
    // inherit the parent's cwd, but in practice the `claude` agent
    // reports `~` as its working directory unless we set it
    // explicitly.
    let initial_cwd = match cli.path.as_deref() {
        Some(p) => resolve_cwd(p)?,
        None => std::env::current_dir().wrap_err("read current working directory")?,
    };
    // Construct the production spawner once, here at the composition
    // root, so the runtime depends on the `AgentSpawner` port rather
    // than on `BinaryAgentSpawner` directly. Tests substitute their
    // own binary via `CODEMUX_AGENT_BIN`, which clap resolved into
    // `cli.agent_bin` above — `BinaryAgentSpawner` then invokes
    // whatever the test pointed at.
    let agent_spawner = BinaryAgentSpawner::new(cli.agent_bin.clone());
    let seams = TestSeams {
        panic_after: cli.panic_after.map(std::time::Duration::from_millis),
        record_opens_to: cli.record_opens_to.clone(),
    };
    // Open the state DB and load whatever rows are on disk BEFORE
    // raw-mode entry. AD-7's failure-mode rule mirrors config: a
    // missing file is fine (we just write a fresh DB on first save),
    // but anything else (path resolution, file open, migration,
    // initial `load_all`) must surface a readable error and exit
    // non-zero before the terminal switches to alt-screen + raw mode.
    let state_db_path = match cli.state_db.clone() {
        Some(p) => p,
        None => persistence::default_db_path().wrap_err("resolve state database path")?,
    };
    let persistence = persistence::Persistence::open(&state_db_path)
        .wrap_err_with(|| format!("open state database at {}", state_db_path.display()))?;
    let snapshot = persistence
        .load_snapshot()
        .wrap_err("load persisted state")?;
    // Resolve the launch mode (Fresh / continue-most-recent /
    // interactive-picker) BEFORE raw-mode entry. The picker reads
    // from stdin and writes to stdout/stderr; raw mode and the
    // alt-screen would garble both. Any error here flows back through
    // color-eyre and exits non-zero with a readable message,
    // terminal still in cooked mode.
    let launch_mode = match resolve_launch_mode(&cli, snapshot.agents.clone()) {
        Ok(mode) => mode,
        // `Aborted` is the user politely declining — quiet exit, no
        // backtrace. Anything else flows through color-eyre.
        Err(LaunchError::Aborted) => {
            std::process::exit(1);
        }
        Err(LaunchError::NoSavedSessions) => {
            eprintln!("no saved sessions; run `codemux` to start fresh");
            std::process::exit(1);
        }
        Err(err) => return Err(color_eyre::eyre::eyre!(err)),
    };
    runtime::run(
        cli.nav,
        &config,
        &initial_cwd,
        cli.log.then_some(&tail),
        &agent_spawner,
        seams,
        &persistence,
        &snapshot,
        &launch_mode,
    )
}

/// Resolve the user's launch-mode intent into a [`LaunchMode`].
///
/// - Bare `codemux` → `LaunchMode::Fresh`.
/// - `--continue` against an empty DB → also `LaunchMode::Fresh`
///   (silent fallback; matches `tmux new-session -A` semantics).
/// - `--continue` against a non-empty DB → `LaunchMode::SelectedAgent`
///   for the most-recently-attached row.
/// - `--resume` against an empty DB → `LaunchError::NoSavedSessions`,
///   caller exits non-zero with a one-line stderr message.
/// - `--resume` against a non-empty DB → interactive picker on
///   stdout/stdin, returning the chosen `LaunchMode::SelectedAgent`
///   or `LaunchError::Aborted` if the user backs out.
fn resolve_launch_mode(
    cli: &Cli,
    agents: Vec<codemux_session::domain::Agent>,
) -> std::result::Result<LaunchMode, LaunchError> {
    if cli.resume_continue {
        return Ok(
            launch::pick_most_recent(agents).map_or(LaunchMode::Fresh, LaunchMode::SelectedAgent)
        );
    }
    if cli.resume_pick {
        let stdin = std::io::stdin();
        let mut stdin = stdin.lock();
        let stdout = std::io::stdout();
        let mut stdout = stdout.lock();
        let stderr = std::io::stderr();
        let mut stderr = stderr.lock();
        let picked = launch::run_picker(
            agents,
            std::time::SystemTime::now(),
            &mut stdin,
            &mut stdout,
            &mut stderr,
        )?;
        return Ok(LaunchMode::SelectedAgent(picked));
    }
    Ok(LaunchMode::Fresh)
}

/// Canonicalize `path` and verify it's a directory. Returns a clean
/// `eyre` chain on failure so the user sees `<path>: <reason>` rather
/// than a bare `io::Error`.
fn resolve_cwd(path: &Path) -> Result<PathBuf> {
    let resolved =
        fs::canonicalize(path).wrap_err_with(|| format!("invalid path `{}`", path.display()))?;
    if !resolved.is_dir() {
        return Err(color_eyre::eyre::eyre!(
            "`{}` is not a directory",
            resolved.display()
        ));
    }
    Ok(resolved)
}

/// Default log file path. Created on first run via `init_tracing`.
fn default_log_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| color_eyre::eyre::eyre!("HOME unset; can't derive log file path"))?;
    Ok(PathBuf::from(home)
        .join(".cache")
        .join("codemux")
        .join("logs")
        .join("codemux.log"))
}

/// Wire tracing through two layers:
///   1. **File appender** — always on, writes to
///      `~/.cache/codemux/logs/codemux.log` with no ANSI escapes
///      (the file gets cat'd by humans). Append mode so successive
///      runs accumulate; cleanup is the user's job (or a future
///      log-rotation task).
///   2. **In-memory tail** — captures the same formatted lines into
///      [`log_tail::LogTail`] for the runtime to render in its
///      bottom strip when `--log` is on. Keeping this layer always
///      attached (regardless of `--log`) is wasteful by ~one mutex
///      lock per log event, but it keeps the subscriber init free of
///      conditional branches and the cost is invisible at our log
///      volume (~1 line/sec at info, ~10 lines/sec at debug).
///
/// stderr is **not** a sink. The TUI's alt-screen does not protect
/// against bare stderr writes, so previously a `tracing::info!` line
/// would land at the cursor position in the middle of the agent
/// pane — visible as garbled characters around the placeholder text.
fn init_tracing(tail: &log_tail::LogTail) -> Result<()> {
    let path = default_log_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .wrap_err_with(|| format!("create log dir {}", parent.display()))?;
    }
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .wrap_err_with(|| format!("open log file {}", path.display()))?;
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("codemux=info,codemux_cli=info,warn"));
    let file_layer = fmt::layer()
        .with_writer(Mutex::new(file))
        .with_ansi(false)
        .with_target(false);
    let tail_layer = tail.layer();
    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(tail_layer)
        .try_init()
        .map_err(|e| color_eyre::eyre::eyre!("tracing init failed: {e}"))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn resolve_cwd_returns_canonicalized_directory() {
        let dir = tempfile::tempdir().unwrap();
        let resolved = resolve_cwd(dir.path()).unwrap();
        assert_eq!(resolved, dir.path().canonicalize().unwrap());
        assert!(resolved.is_absolute());
    }

    #[test]
    fn resolve_cwd_errors_when_path_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let err = resolve_cwd(&missing).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("invalid path"),
            "expected `invalid path` in error, got: {msg}",
        );
    }

    #[test]
    fn resolve_cwd_errors_when_path_is_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-dir");
        fs::write(&file, b"").unwrap();
        let err = resolve_cwd(&file).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("is not a directory"),
            "expected `is not a directory` in error, got: {msg}",
        );
    }

    // ── launch-mode resolution ──────────────────────────────────
    //
    // These tests pin the tmux-style pivot behavior end-to-end at
    // the CLI seam: parse argv into a `Cli`, hand it to
    // `resolve_launch_mode` against a synthetic agent list, assert
    // the resulting `LaunchMode`. The structural guarantee is that
    // bare-`codemux` never reaches into the persisted rows even
    // when they exist — which previously hydrated them as Dead
    // tabs sitting behind the fresh agent.

    use codemux_session::domain::{Agent, AgentStatus};
    use codemux_shared_kernel::{AgentId, HostId};
    use std::time::{Duration, UNIX_EPOCH};

    fn persisted_agent(id: &str, last_attached: Option<std::time::SystemTime>) -> Agent {
        Agent {
            id: AgentId::new(id),
            host_id: HostId::new("local"),
            label: id.to_string(),
            cwd: PathBuf::from("/work/repo"),
            group_ids: Vec::new(),
            session_id: Some(format!("{id}-uuid")),
            status: AgentStatus::Dead,
            last_attached_at: last_attached,
        }
    }

    fn parse_cli(argv: &[&str]) -> Cli {
        use clap::Parser;
        Cli::try_parse_from(argv).unwrap()
    }

    #[test]
    fn bare_codemux_resolves_to_fresh_even_with_persisted_rows() {
        // The whole point of the tmux pivot: bare `codemux` MUST
        // NOT auto-hydrate. Persisted rows are reachable only
        // through `--continue` / `--resume`.
        let cli = parse_cli(&["codemux"]);
        let agents = vec![
            persisted_agent("a1", Some(UNIX_EPOCH + Duration::from_secs(100))),
            persisted_agent("a2", Some(UNIX_EPOCH + Duration::from_secs(200))),
        ];
        let mode = resolve_launch_mode(&cli, agents).unwrap();
        assert!(
            matches!(mode, LaunchMode::Fresh),
            "bare codemux must resolve to Fresh, got {mode:?}",
        );
    }

    #[test]
    fn continue_against_empty_db_falls_back_to_fresh() {
        // `--continue` matches `tmux new-session -A` semantics:
        // resume if available, otherwise quietly fresh-spawn.
        let cli = parse_cli(&["codemux", "--continue"]);
        let mode = resolve_launch_mode(&cli, Vec::new()).unwrap();
        assert!(
            matches!(mode, LaunchMode::Fresh),
            "--continue with no persisted rows must fall back to Fresh",
        );
    }

    #[test]
    fn continue_picks_the_most_recent_attached_agent() {
        let cli = parse_cli(&["codemux", "--continue"]);
        let agents = vec![
            persisted_agent("old", Some(UNIX_EPOCH + Duration::from_secs(100))),
            persisted_agent("newer", Some(UNIX_EPOCH + Duration::from_secs(500))),
            persisted_agent("never", None),
        ];
        let mode = resolve_launch_mode(&cli, agents).unwrap();
        match mode {
            LaunchMode::SelectedAgent(agent) => assert_eq!(agent.id.as_str(), "newer"),
            LaunchMode::Fresh => panic!("expected SelectedAgent(newer), got Fresh"),
        }
    }

    #[test]
    fn resume_against_empty_db_returns_no_saved_sessions() {
        let cli = parse_cli(&["codemux", "--resume"]);
        let err = resolve_launch_mode(&cli, Vec::new()).unwrap_err();
        assert!(matches!(err, LaunchError::NoSavedSessions));
    }

    #[test]
    fn continue_and_resume_are_mutually_exclusive_at_parse_time() {
        // Mutual exclusion is enforced by clap's ArgGroup so we
        // don't have to add runtime defensive code. Pin the
        // contract here so a future refactor that drops the group
        // can't silently re-enable the nonsense combination.
        use clap::Parser;
        let err = Cli::try_parse_from(["codemux", "--continue", "--resume"]).unwrap_err();
        let rendered = err.to_string();
        assert!(
            rendered.contains("cannot be used with"),
            "expected mutual-exclusion error, got: {rendered}",
        );
    }
}
