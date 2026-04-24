//! `codemux-daemon` — host-side daemon (`codemuxd`) that owns a PTY across
//! SSH disconnects.
//!
//! Stage 0 (this commit): walking skeleton. The daemon listens on a unix
//! socket, accepts one client at a time, spawns a child process inside a
//! PTY (default `claude`), and shuttles raw bytes between the socket and
//! the PTY. No protocol framing yet — that arrives in Stage 1 alongside
//! `codemux-wire`. No SSH integration yet — Stage 4 wires the TUI to it.
//!
//! Library surface exists so future integration tests in `apps/tui` can
//! spawn the supervisor in-process without forking a subprocess.

pub mod cli;
pub mod conn;
pub mod error;
pub mod pty;
pub mod session;
pub mod supervisor;

pub use cli::Cli;
pub use error::Error;
pub use supervisor::Supervisor;
