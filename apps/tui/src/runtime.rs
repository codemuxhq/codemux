use color_eyre::Result;
use color_eyre::eyre::bail;

pub fn run() -> Result<()> {
    tracing::info!("codemux starting");
    // TODO(P0): set up the ratatui terminal, spawn `claude` via portable-pty,
    // pump bytes through vt100 + tui-term into a single ratatui Rect, forward
    // input keys to the PTY, and exit cleanly on Ctrl-C.
    bail!("runtime::run not yet implemented");
}
