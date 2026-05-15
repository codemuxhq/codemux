//! Repository ports for the session bounded context.
//!
//! Per Hexagonal Architecture, these are *ports* defined inside the
//! Application Core (this crate). Adapter implementations live in the
//! driven adapter — `crates/store` ships the SQLite-backed adapter today.
//!
//! Boundary invariants:
//!
//! - `crates/session` MUST NOT depend on `rusqlite`. Adapter-side failures
//!   are funnelled through [`RepositoryError::Storage`] as a boxed
//!   `dyn std::error::Error`, so the port surface is infrastructure-free.
//! - The `Agent::session_id` carried through these ports is opaque text.
//!   The strings `--session-id` and `--resume` MUST NOT appear here — that
//!   knowledge of Claude's CLI surface lives in the spawn-argv builders
//!   inside `apps/tui` and `apps/daemon` (see the 2026-05-15 spike).
//!
//! All trait methods take `&self`. Adapters that own a non-`Sync` handle
//! (e.g. `rusqlite::Connection`) wrap it in `std::sync::Mutex` to provide
//! interior mutability. For a single-user, single-writer personal tool
//! this is the simple, correct choice.

use std::error::Error as StdError;

use thiserror::Error;

use codemux_shared_kernel::{AgentId, GroupId, HostId};

use crate::domain::{Agent, Host};

/// Boxed source error for adapter-side failures. Adapters map their
/// tool-specific errors (`rusqlite::Error`, `std::io::Error`, ...) into
/// this form before crossing the port boundary, so `crates/session`
/// stays free of any specific storage vendor's types.
type BoxedSource = Box<dyn StdError + Send + Sync + 'static>;

/// Errors raised by repository ports.
///
/// Per AD-17, marked `#[non_exhaustive]` so new variants can be added
/// without breaking downstream `match` arms.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RepositoryError {
    /// The entity addressed by `id` was not present. Raised by `delete`
    /// of a missing row today; future query methods may reuse the same
    /// variant. `kind` is a static label (`"host"`, `"agent"`, `"group"`)
    /// so callers can branch on aggregate type without parsing the
    /// `Display` form.
    #[error("{kind} not found: {id}")]
    NotFound { kind: &'static str, id: String },

    /// Opaque adapter-side failure. Wraps anything the storage adapter
    /// surfaces (`SQLite` I/O error, unexpected enum text in the DB, etc.)
    /// so the application core does not depend on `rusqlite`.
    #[error("storage error")]
    Storage {
        #[source]
        source: BoxedSource,
    },
}

/// Port: persistence for [`Host`] aggregates.
///
/// Split per-aggregate (rather than one mega-`Repository`) so adapter
/// implementations can be focused and callers can take the narrowest
/// dependency they need.
pub trait HostRepository {
    /// Load every persisted host.
    ///
    /// # Errors
    /// Returns [`RepositoryError::Storage`] if the adapter fails to
    /// query or decode rows.
    fn load_all(&self) -> Result<Vec<Host>, RepositoryError>;

    /// Insert-or-replace the given host by primary key (`Host::id`).
    /// Idempotent: calling twice with the same value is a no-op on the
    /// row count.
    ///
    /// # Errors
    /// Returns [`RepositoryError::Storage`] if the adapter fails to
    /// write the row.
    fn save(&self, host: &Host) -> Result<(), RepositoryError>;

    /// Delete the host with the given id.
    ///
    /// # Errors
    /// Returns [`RepositoryError::NotFound`] if no row was deleted.
    /// Returns [`RepositoryError::Storage`] on adapter failure.
    fn delete(&self, id: &HostId) -> Result<(), RepositoryError>;
}

/// Port: persistence for [`Agent`] aggregates, including their
/// `group_ids` membership edges. `save` reconciles the join-table rows
/// atomically — callers do not interact with `agent_groups` directly.
pub trait AgentRepository {
    /// Load every persisted agent, with `group_ids` hydrated from the
    /// join table.
    ///
    /// # Errors
    /// Returns [`RepositoryError::Storage`] if the adapter fails to
    /// query or decode rows.
    fn load_all(&self) -> Result<Vec<Agent>, RepositoryError>;

    /// Insert-or-replace the given agent by primary key (`Agent::id`)
    /// and reconcile its `group_ids` against the join table inside a
    /// single transaction.
    ///
    /// # Errors
    /// Returns [`RepositoryError::Storage`] if the adapter fails to
    /// write the row or reconcile join-table membership.
    fn save(&self, agent: &Agent) -> Result<(), RepositoryError>;

    /// Delete the agent with the given id. The join-table rows referencing
    /// the agent are removed by the schema's `ON DELETE CASCADE`.
    ///
    /// # Errors
    /// Returns [`RepositoryError::NotFound`] if no row was deleted.
    /// Returns [`RepositoryError::Storage`] on adapter failure.
    fn delete(&self, id: &AgentId) -> Result<(), RepositoryError>;
}

/// Port: persistence for groups.
///
/// The session domain does not have a `Group` struct — a group is just
/// an identity plus a display name. The port reflects that shape rather
/// than inventing a one-field wrapper.
pub trait GroupRepository {
    /// Load every persisted group as `(id, name)` pairs.
    ///
    /// # Errors
    /// Returns [`RepositoryError::Storage`] if the adapter fails to
    /// query or decode rows.
    fn load_all(&self) -> Result<Vec<(GroupId, String)>, RepositoryError>;

    /// Insert-or-replace the given group by primary key.
    ///
    /// # Errors
    /// Returns [`RepositoryError::Storage`] if the adapter fails to
    /// write the row.
    fn save(&self, id: &GroupId, name: &str) -> Result<(), RepositoryError>;

    /// Delete the group with the given id. The join-table rows referencing
    /// the group are removed by the schema's `ON DELETE CASCADE`.
    ///
    /// # Errors
    /// Returns [`RepositoryError::NotFound`] if no row was deleted.
    /// Returns [`RepositoryError::Storage`] on adapter failure.
    fn delete(&self, id: &GroupId) -> Result<(), RepositoryError>;
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::error::Error as StdError;
    use std::io;

    use super::*;

    #[test]
    fn not_found_display_includes_kind_and_id() {
        let err = RepositoryError::NotFound {
            kind: "host",
            id: "laptop".into(),
        };
        assert_eq!(err.to_string(), "host not found: laptop");
    }

    #[test]
    fn storage_preserves_source_chain() {
        let inner = io::Error::other("disk on fire");
        let err = RepositoryError::Storage {
            source: Box::new(inner),
        };
        assert_eq!(err.to_string(), "storage error");
        let Some(source) = StdError::source(&err) else {
            unreachable!("Storage variant must carry a source")
        };
        assert_eq!(source.to_string(), "disk on fire");
    }
}
