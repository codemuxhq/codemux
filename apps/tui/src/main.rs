use clap::Parser;
use color_eyre::Result;
use tracing_subscriber::{EnvFilter, fmt};

mod config;
mod keymap;
mod runtime;
mod spawn_modal;
use runtime::NavStyle;

#[derive(Debug, Parser)]
#[command(name = "codemux", version, about)]
struct Cli {
    /// Initial navigator style. Toggle at runtime with the prefix-key + v.
    #[arg(long, value_enum, env = "CODEMUX_NAV", default_value = "popup")]
    nav: NavStyle,
}

fn main() -> Result<()> {
    color_eyre::install()?;
    init_tracing();
    let cli = Cli::parse();
    // Load config (or defaults if missing) before touching the terminal so a
    // malformed config file fails loud instead of corrupting raw mode.
    let config = config::load()?;
    runtime::run(cli.nav, &config)
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("codemux=info,warn"));
    fmt().with_env_filter(filter).with_target(false).init();
}
