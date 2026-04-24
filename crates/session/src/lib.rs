//! Bounded context: agent lifecycle.
//!
//! P0 surface: domain types only. The agent lifecycle service and
//! persistence land alongside their first real caller — see
//! `docs/architecture.md` "Deferred ideas" for the planned shape.
//!
//! Stage 3 of the codemuxd build-out adds [`AgentTransport`], the seam
//! between the runtime and the per-agent PTY. The local variant ports
//! the prior inline `RuntimeAgent` PTY shape; the SSH variant is a
//! stub that Stage 4 lights up.

pub mod domain;
pub mod error;
pub mod transport;

pub use error::Error;
pub use transport::{AgentTransport, LocalPty, SshDaemonPty};
