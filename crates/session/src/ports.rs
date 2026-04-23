//! Traits that session use cases depend on.
//!
//! Per AD-20, these are the ports. Infrastructure implements them; use cases
//! consume them as generic bounds. The binary (`apps/tui/runtime.rs`) is the
//! only place where concrete adapters are instantiated and injected.

use std::path::Path;

use codemux_shared_kernel::{AgentId, HostId};

use crate::domain::{Agent, AgentStatus, Host};
use crate::error::Error;

/// Persistence port. Implementations must be safe to call from background
/// threads so blocking I/O stays off the TUI render loop.
pub trait AgentRepo: Send + Sync {
    fn insert_host(&self, host: &Host) -> Result<(), Error>;
    fn get_host(&self, id: &HostId) -> Result<Option<Host>, Error>;
    fn list_hosts(&self) -> Result<Vec<Host>, Error>;

    fn insert_agent(&self, agent: &Agent) -> Result<(), Error>;
    fn get_agent(&self, id: &AgentId) -> Result<Option<Agent>, Error>;
    fn list_agents(&self) -> Result<Vec<Agent>, Error>;
    fn update_agent_status(&self, id: &AgentId, status: AgentStatus) -> Result<(), Error>;
    fn delete_agent(&self, id: &AgentId) -> Result<(), Error>;
}

/// PTY transport port. One implementation per transport kind (local fork,
/// SSH subprocess). `spawn` returns a handle that outlives attach and detach;
/// the session engine owns the handle's lifecycle.
pub trait PtyTransport: Send + Sync {
    fn spawn(&self, cwd: &Path, resume_session_id: Option<&str>) -> Result<PtyHandle, Error>;
}

/// Opaque handle to a live PTY. Fields are private: readers, writers, the
/// child-process handle, and the kill switch land in P1 implementation.
/// Callers access the owning agent via the `agent_id()` accessor.
#[derive(Debug)]
pub struct PtyHandle {
    agent_id: AgentId,
}

impl PtyHandle {
    #[must_use]
    pub fn new(agent_id: AgentId) -> Self {
        Self { agent_id }
    }

    #[must_use]
    pub fn agent_id(&self) -> &AgentId {
        &self.agent_id
    }
}
