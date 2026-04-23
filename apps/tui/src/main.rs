use clap::Parser;
use color_eyre::Result;
use tracing_subscriber::{EnvFilter, fmt};

mod runtime;
mod ui;

#[derive(Debug, Parser)]
#[command(name = "codemux", version, about)]
struct Cli {}

fn main() -> Result<()> {
    color_eyre::install()?;
    init_tracing();
    let _cli = Cli::parse();
    runtime::run()
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("codemux=info,warn"));
    fmt().with_env_filter(filter).with_target(false).init();
}
