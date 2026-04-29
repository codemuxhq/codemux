use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use clap::Parser;
use color_eyre::Result;
use color_eyre::eyre::WrapErr;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};

mod agent_meta_worker;
mod bootstrap_worker;
mod config;
mod git_branch;
mod host_title;
mod index_cache;
mod index_manager;
mod index_worker;
mod keymap;
mod log_tail;
mod pty_title;
mod repo_name;
mod runtime;
mod spawn;
mod ssh_config;
mod status_bar;
use runtime::NavStyle;

#[derive(Debug, Parser)]
#[command(name = "codemux", version, about)]
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
}

fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();
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
    runtime::run(cli.nav, &config, &initial_cwd, cli.log.then_some(&tail))
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
        .unwrap_or_else(|_| EnvFilter::new("codemux=info,codemux_tui=info,warn"));
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
}
