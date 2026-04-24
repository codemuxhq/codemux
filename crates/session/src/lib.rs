//! Bounded context: agent lifecycle.
//!
//! P0 surface: domain types only. The agent lifecycle service and
//! persistence land alongside their first real caller — see
//! `docs/architecture.md` "Deferred ideas" for the planned shape.
//!
//! Stage 3 of the codemuxd build-out adds [`AgentTransport`], the seam
//! between the runtime and the per-agent PTY. The local variant ports
//! the prior inline `RuntimeAgent` PTY shape; the SSH variant lights up
//! in Stage 4 via [`bootstrap`].

pub mod bootstrap;
pub mod domain;
pub mod error;
pub mod transport;

pub use bootstrap::{CommandRunner, RealRunner, default_local_socket_dir};
pub use error::{BootstrapStage, Error};
pub use transport::{AgentTransport, LocalPty, SshDaemonPty};
