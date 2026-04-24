use clap::Parser;
use color_eyre::Result;
use color_eyre::eyre::WrapErr;
use tracing_subscriber::{EnvFilter, fmt};

use codemux_daemon::{Cli, Supervisor};

fn main() -> Result<()> {
    color_eyre::install()?;
    init_tracing();
    let cli = Cli::parse();
    let mut supervisor = Supervisor::bind(&cli).wrap_err("bind supervisor")?;
    supervisor.serve().wrap_err("serve")?;
    Ok(())
}

fn init_tracing() {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("codemuxd=info,warn"));
    fmt().with_env_filter(filter).with_target(false).init();
}
