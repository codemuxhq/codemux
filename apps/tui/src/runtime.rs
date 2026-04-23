use color_eyre::Result;
use color_eyre::eyre::bail;

pub fn run() -> Result<()> {
    tracing::info!("codemux starting");
    // TODO(P1): initialize ratatui terminal, instantiate adapters
    // (SqliteStore, LocalPtyTransport, SshPtyTransport), construct
    // SessionService, run the event loop until the user exits.
    bail!("runtime::run not yet implemented");
}
