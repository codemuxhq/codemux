//! Use cases. `SessionService` orchestrates the session bounded context
//! against injected ports.
//!
//! The service owns its `AgentRepo` and `PtyTransport` and exposes one method
//! per use case. Tests substitute in-memory adapters; no method touches the
//! filesystem, database, or network directly.

use std::path::Path;

use codemux_shared_kernel::{AgentId, HostId};

use crate::domain::Agent;
use crate::error::Error;
use crate::ports::{AgentRepo, PtyTransport};

pub struct SpawnRequest<'a> {
    pub host_id: &'a HostId,
    pub label: String,
    pub cwd: &'a Path,
}

pub struct SessionService<R, T>
where
    R: AgentRepo,
    T: PtyTransport,
{
    repo: R,
    transport: T,
}

impl<R, T> SessionService<R, T>
where
    R: AgentRepo,
    T: PtyTransport,
{
    #[must_use]
    pub fn new(repo: R, transport: T) -> Self {
        Self { repo, transport }
    }

    /// Create a new agent and start its PTY.
    pub fn spawn_agent(&self, _request: SpawnRequest<'_>) -> Result<Agent, Error> {
        let _ = (&self.repo, &self.transport);
        Err(Error::NotImplemented("SessionService::spawn_agent"))
    }

    /// Ensure the agent's PTY is alive (resuming via `claude --resume` if
    /// needed) and mark it as focused.
    pub fn focus_agent(&self, _id: &AgentId) -> Result<(), Error> {
        let _ = (&self.repo, &self.transport);
        Err(Error::NotImplemented("SessionService::focus_agent"))
    }

    /// Detach the TUI from the agent without killing it. PTY stays alive.
    pub fn detach_agent(&self, _id: &AgentId) -> Result<(), Error> {
        let _ = &self.repo;
        Err(Error::NotImplemented("SessionService::detach_agent"))
    }

    /// Kill the agent's PTY and mark it dead in the repo. Record persists.
    pub fn kill_agent(&self, _id: &AgentId) -> Result<(), Error> {
        let _ = &self.repo;
        Err(Error::NotImplemented("SessionService::kill_agent"))
    }
}
