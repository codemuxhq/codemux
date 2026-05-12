//! `codemuxd` — host-side daemon (`codemuxd`) that owns a PTY across
//! SSH disconnects.
//!
//! Stage 0 (commit `8dbf805`): walking skeleton — listens on a unix
//! socket, accepts one client, spawns a child in a PTY, shuttles bytes.
//! Stage 1 (commit `1452c4f`): wire protocol with `Hello`/`HelloAck`.
//! Stage 2 (this commit): canonical filesystem layout
//! ([`fs_layout::Layout`]), exclusive pid file acquisition,
//! socket-mode 0600, and tracing redirected to a log file when not
//! `--foreground`.
//!
//! Library surface exists so future integration tests in `apps/tui` can
//! spawn the supervisor in-process without forking a subprocess. Stage 4
//! also re-uses [`fs_layout::Layout`] from the bootstrap to derive the
//! exact paths it then passes back to this binary as `--socket`,
//! `--pid-file`, `--log-file`.

pub mod bootstrap;
pub mod cli;
pub mod conn;
pub mod error;
pub mod fs_layout;
pub mod pty;
pub mod session;
pub mod supervisor;

pub use bootstrap::{DaemonResources, bring_up};
pub use cli::Cli;
pub use error::Error;
pub use fs_layout::Layout;
pub use supervisor::Supervisor;
