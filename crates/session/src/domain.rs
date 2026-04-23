//! Domain types for the session bounded context. Pure Rust, no vendor deps.

use std::path::PathBuf;
use std::time::SystemTime;

use codemux_shared_kernel::{AgentId, GroupId, HostId};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Host {
    pub id: HostId,
    pub name: String,
    pub kind: HostKind,
    pub last_seen: Option<SystemTime>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostKind {
    Local,
    Ssh { target: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Agent {
    pub id: AgentId,
    pub host_id: HostId,
    pub label: String,
    pub cwd: PathBuf,
    pub group_ids: Vec<GroupId>,
    pub session_id: Option<String>,
    pub status: AgentStatus,
    pub last_attached_at: Option<SystemTime>,
}

/// Observable status from the outside. In P1, transitions are driven by PTY
/// liveness (running vs. dead). The `NeedsInput` variant is reserved for P2,
/// where heuristics over PTY output will detect approval prompts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentStatus {
    Starting,
    Running,
    Idle,
    NeedsInput,
    Dead,
}
