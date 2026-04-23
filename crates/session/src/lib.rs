//! Bounded context: agent lifecycle.
//!
//! - `domain`: pure types (`Agent`, `Host`, `SessionState`).
//! - `use_cases`: spawn, focus, detach, kill — parameterized over ports.
//! - `ports`: traits that use cases depend on (`AgentRepo`, `PtyTransport`).
//! - `infra`: concrete adapters wiring ports to real tools (`SQLite`,
//!   `portable-pty`, ssh). Per AD-18, co-located with the component.

pub mod domain;
pub mod error;
pub mod infra;
pub mod ports;
pub mod use_cases;

pub use error::Error;
