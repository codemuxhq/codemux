//! Bounded context: agent lifecycle.
//!
//! P0 surface: domain types only. The agent lifecycle service and
//! persistence land alongside their first real caller — see
//! `docs/004--architecture.md` "Deferred ideas" for the planned shape.
//!
//! Stage 3 of the codemuxd build-out adds [`AgentTransport`], the seam
//! between the runtime and the per-agent PTY. The local variant ports
//! the prior inline `RuntimeAgent` PTY shape; the SSH variant lights
//! up in Stage 4 — its bootstrap orchestration lives in the
//! `codemuxd-bootstrap` adapter crate, which constructs
//! [`AgentTransport::SshDaemon`] via [`SshDaemonPty::attach`].

pub mod domain;
pub mod error;
pub mod repository;
pub mod spawner;
pub mod transport;

pub use error::Error;
pub use repository::{AgentRepository, GroupRepository, HostRepository, RepositoryError};
pub use spawner::{AgentSpawner, BinaryAgentSpawner, SpawnRequest};
pub use transport::{AgentTransport, LocalPty, SshDaemonPty};
