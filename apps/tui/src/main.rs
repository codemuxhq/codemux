use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use clap::Parser;
use color_eyre::Result;
use color_eyre::eyre::WrapErr;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};

mod bootstrap_worker;
mod config;
mod keymap;
mod log_tail;
mod runtime;
mod spawn;
mod ssh_config;
use runtime::NavStyle;

#[derive(Debug, Parser)]
#[command(name = "codemux", version, about)]
struct Cli {
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
    runtime::run(cli.nav, &config, cli.log.then_some(&tail))
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
