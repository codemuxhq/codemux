use clap::Parser;
use color_eyre::Result;
use color_eyre::eyre::WrapErr;
use tracing_subscriber::{EnvFilter, fmt};

use codemuxd::{Cli, Supervisor, bootstrap, fs_layout};

fn main() -> Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();
    init_tracing(&cli)?;
    let resources = bootstrap::bring_up(&cli).wrap_err("bring up daemon")?;
    let mut supervisor = Supervisor::new(resources);
    supervisor.serve().wrap_err("serve")?;
    Ok(())
}

/// Configure `tracing_subscriber`. Foreground mode keeps Stage 0
/// behaviour: target-less stderr formatter for ergonomic `cargo run`.
/// Daemon mode (default) routes through the `--log-file` so the
/// originating SSH session that spawned the daemon can be torn down
/// without losing diagnostics — `setsid -f` (Stage 4) detaches us from
/// the controlling terminal and stderr is no longer reachable.
///
/// The default filter covers both the binary and the library, both of
/// which share the `codemuxd` target name post-AD-31 crate rename.
fn init_tracing(cli: &Cli) -> Result<()> {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("codemuxd=info,warn"));

    if cli.foreground {
        fmt().with_env_filter(filter).with_target(false).init();
        return Ok(());
    }

    let Some(log_path) = cli.log_file.as_deref() else {
        unreachable!(
            "clap's required_unless_present guarantees --log-file when --foreground is unset"
        );
    };
    fs_layout::ensure_parent(log_path)
        .wrap_err_with(|| format!("create log dir for {}", log_path.display()))?;
    let file = std::fs::File::create(log_path)
        .wrap_err_with(|| format!("create log file {}", log_path.display()))?;
    // `Mutex<File>` impls `MakeWriter`; the formatter locks per write.
    // The volume here is structured tracing fields, not raw stdout —
    // contention is negligible.
    let writer = std::sync::Mutex::new(file);
    fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(false)
        .init();
    Ok(())
}
