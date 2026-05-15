//! Persistence wiring for the TUI runtime ([AD-7]).
//!
//! This module is the seam between the runtime's in-memory `RuntimeAgent`
//! shape and the domain `Agent` / `Host` rows owned by the SQLite-backed
//! adapter in `crates/store`. The runtime calls into a [`Persistence`]
//! value at the state-mutation seam (post-spawn, status transitions,
//! focus events, dismissals) and the adapter writes through.
//!
//! ## What does NOT live here
//!
//! - The strings `--session-id` / `--resume`. Per the 2026-05-15
//!   persistence spike, knowledge of Claude's CLI surface stays in the
//!   spawn-argv builders (`runtime::build_claude_args`). The `session_id`
//!   field carried through this module is opaque text the runtime
//!   neither generates nor consumes today (step 5 introduces that).
//! - Render decisions. This module only writes; what the user sees for
//!   a loaded-Dead agent is the runtime's job.
//!
//! ## Failure shape
//!
//! - **Startup load failures** (path resolve, open, migration, `load_all`)
//!   are fatal: they propagate out of [`Persistence::open`] /
//!   [`Persistence::load_snapshot`] as [`PersistenceError`] and `main`
//!   prints + exits before raw mode is entered.
//! - **Mutation-time write failures** are best-effort: every `record_*`
//!   method logs at `error!` and continues. We'd rather lose a row than
//!   take down the user's session.
//!
//! [AD-7]: ../../../docs/004--architecture.md

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use codemux_session::domain::{Agent, AgentStatus, Host, HostKind};
use codemux_session::repository::{AgentRepository, HostRepository};
use codemux_shared_kernel::{AgentId, HostId};
use codemux_store::{SqliteStore, StoreError};
use thiserror::Error;

/// Stable [`HostId`] used for every local agent. Local hosts have no
/// natural identifier (the machine has one name from the user's
/// perspective); fixing it as a constant keeps the `hosts` table
/// single-row for local-only setups and makes the join from an
/// `agents.host_id` row predictable.
const LOCAL_HOST_ID: &str = "local";

/// Display name for the local host row. Mirrors the `kind = local`
/// row's `name` column so the navigator's eventual "where is this
/// agent running?" affordance can read it back without inventing
/// strings.
const LOCAL_HOST_NAME: &str = "local";

/// Errors raised while initialising persistence.
///
/// Per AD-17 each component crate carries its own `thiserror` enum,
/// `#[non_exhaustive]` so additive variants don't break downstream
/// `match` arms. Mutation-time failures do NOT flow through this
/// enum — they are swallowed and logged inside [`Persistence`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum PersistenceError {
    /// Opening the `SQLite` file or applying migrations failed. Wraps
    /// the underlying [`StoreError`] so callers can render a
    /// `Caused by:` chain.
    #[error("open state database")]
    OpenStore(#[source] StoreError),

    /// `load_all` on one of the repository ports failed at startup.
    /// We surface the kind (`"host"`, `"agent"`) so the user sees
    /// which load went wrong.
    #[error("load persisted {kind}")]
    Load {
        kind: &'static str,
        #[source]
        source: codemux_session::repository::RepositoryError,
    },
}

/// Owned access to the codemux state database, plus the helpers the
/// runtime needs to write through agent / host mutations.
///
/// Construction goes through [`Persistence::open`] so the file open
/// and migration steps are encapsulated. The internal [`SqliteStore`]
/// is wrapped in an [`Arc`] so a future async / worker-thread reach
/// (e.g. background persistence for the focus-touch debounce in a
/// later step) can clone it without a re-open.
pub struct Persistence {
    store: Arc<SqliteStore>,
}

impl Persistence {
    /// Open the codemux state database at `path` and run pending
    /// migrations.
    ///
    /// Wraps the lower-level `codemux_store::open` call and packages
    /// the result inside an [`Arc<SqliteStore>`]. Callers in `main`
    /// invoke this BEFORE raw-mode entry; a failure here exits
    /// non-zero with the path and underlying cause printed.
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError::OpenStore`] when the file cannot
    /// be opened or migrations fail.
    pub fn open(path: &Path) -> Result<Self, PersistenceError> {
        let conn = codemux_store::open(path).map_err(PersistenceError::OpenStore)?;
        Ok(Self {
            store: Arc::new(SqliteStore::new(conn)),
        })
    }

    /// Load every persisted host + agent from disk, returning them
    /// as a [`Snapshot`]. Called once at startup so the runtime can
    /// re-populate the navigator with previously-known agents.
    ///
    /// Agents come back with whatever status was last persisted; the
    /// caller is responsible for stamping them as `Dead` at the
    /// load boundary (the PTYs are not running, regardless of what
    /// the DB says).
    ///
    /// # Errors
    ///
    /// Returns [`PersistenceError::Load`] on any underlying repository
    /// failure. The `kind` field tells the caller which load failed.
    pub fn load_snapshot(&self) -> Result<Snapshot, PersistenceError> {
        let hosts = HostRepository::load_all(self.store.as_ref()).map_err(|source| {
            PersistenceError::Load {
                kind: "host",
                source,
            }
        })?;
        let agents = AgentRepository::load_all(self.store.as_ref()).map_err(|source| {
            PersistenceError::Load {
                kind: "agent",
                source,
            }
        })?;
        Ok(Snapshot { hosts, agents })
    }

    /// Insert-or-replace the local-host row. Idempotent.
    ///
    /// Best-effort: any storage failure is logged and swallowed.
    pub fn record_local_host(&self) {
        let host = Host {
            id: HostId::new(LOCAL_HOST_ID),
            name: LOCAL_HOST_NAME.to_string(),
            kind: HostKind::Local,
            last_seen: Some(SystemTime::now()),
        };
        self.save_host(&host);
    }

    /// Insert-or-replace an SSH host row keyed by `target` (the
    /// `user@host` string codemux uses everywhere else for SSH).
    /// Idempotent — re-spawning to the same target updates only
    /// `last_seen`.
    ///
    /// Best-effort: any storage failure is logged and swallowed.
    pub fn record_ssh_host(&self, target: &str) {
        let host = Host {
            id: ssh_host_id(target),
            name: target.to_string(),
            kind: HostKind::Ssh {
                target: target.to_string(),
            },
            last_seen: Some(SystemTime::now()),
        };
        self.save_host(&host);
    }

    /// Insert-or-replace an agent row. The caller supplies the full
    /// domain shape so this module does not need to know about the
    /// runtime's `RuntimeAgent` internals.
    ///
    /// Best-effort: any storage failure is logged and swallowed.
    pub fn save_agent(&self, agent: &Agent) {
        if let Err(err) = AgentRepository::save(self.store.as_ref(), agent) {
            tracing::error!(?err, agent_id = %agent.id, "failed to persist agent");
        }
    }

    /// Delete an agent row. The schema's `ON DELETE CASCADE`
    /// removes any `agent_groups` edges.
    ///
    /// Best-effort: a missing row (`NotFound`) is treated as success
    /// — the caller's intent (the row should not exist) has been
    /// achieved regardless of who removed it. Other storage failures
    /// are logged and swallowed.
    pub fn delete_agent(&self, id: &AgentId) {
        match AgentRepository::delete(self.store.as_ref(), id) {
            Ok(()) | Err(codemux_session::repository::RepositoryError::NotFound { .. }) => {}
            Err(err) => {
                tracing::error!(?err, agent_id = %id, "failed to delete persisted agent");
            }
        }
    }

    fn save_host(&self, host: &Host) {
        if let Err(err) = HostRepository::save(self.store.as_ref(), host) {
            tracing::error!(?err, host_id = %host.id, "failed to persist host");
        }
    }
}

/// Snapshot of persisted state returned by [`Persistence::load_snapshot`].
///
/// `hosts` and `agents` are returned in their on-disk order (sorted
/// by id) so the navigator's tab order across restarts is stable.
#[derive(Debug)]
pub struct Snapshot {
    pub hosts: Vec<Host>,
    pub agents: Vec<Agent>,
}

/// Build the [`HostId`] for an SSH host keyed by its `user@host`
/// target. Centralised so the spawn-time `record_ssh_host` and the
/// future resume-on-focus lookup agree on the same id-shape.
#[must_use]
pub fn ssh_host_id(target: &str) -> HostId {
    HostId::new(format!("ssh:{target}"))
}

/// Build the [`HostId`] for a local-backed agent.
#[must_use]
pub fn local_host_id() -> HostId {
    HostId::new(LOCAL_HOST_ID)
}

/// Plain-data inputs for [`build_agent_row`]. Bundled as a struct so
/// the call sites stay readable — the runtime mutation seams pass
/// each of these as separate values, and listing seven positional
/// arguments at every site would obscure which one is `cwd` vs
/// `host_id`.
pub struct AgentRow<'a> {
    pub id: AgentId,
    pub host_id: HostId,
    pub label: String,
    pub cwd: &'a Path,
    pub session_id: Option<String>,
    pub status: AgentStatus,
    pub last_attached_at: Option<SystemTime>,
}

/// Project an [`AgentRow`] into a domain [`Agent`] suitable for
/// `AgentRepository::save`. `group_ids` is always empty in this step;
/// the runtime does not surface group membership yet, and forcing
/// the persistence call to invent group ids would silently delete
/// any existing edges via the reconcile logic in the adapter.
#[must_use]
pub fn build_agent_row(row: AgentRow<'_>) -> Agent {
    let AgentRow {
        id,
        host_id,
        label,
        cwd,
        session_id,
        status,
        last_attached_at,
    } = row;
    Agent {
        id,
        host_id,
        label,
        cwd: cwd.to_path_buf(),
        group_ids: Vec::new(),
        session_id,
        status,
        last_attached_at,
    }
}

/// Resolve the host the on-disk database lives in. Mirrors
/// [`codemux_store::default_db_path`] but returns the wrapped
/// [`PersistenceError`] so the caller has a single error type to
/// match on across the open path.
///
/// # Errors
///
/// Returns [`PersistenceError::OpenStore`] when neither
/// `XDG_STATE_HOME` nor `HOME` is set.
pub fn default_db_path() -> Result<PathBuf, PersistenceError> {
    codemux_store::default_db_path().map_err(PersistenceError::OpenStore)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn ssh_host_id_is_namespaced() {
        let id = ssh_host_id("user@devpod");
        assert_eq!(id.as_str(), "ssh:user@devpod");
    }

    #[test]
    fn local_host_id_is_stable() {
        assert_eq!(local_host_id().as_str(), LOCAL_HOST_ID);
    }

    #[test]
    fn build_agent_row_projects_fields() {
        let agent = build_agent_row(AgentRow {
            id: AgentId::new("a1"),
            host_id: local_host_id(),
            label: "label".to_string(),
            cwd: Path::new("/work/repo"),
            session_id: None,
            status: AgentStatus::Running,
            last_attached_at: None,
        });
        assert_eq!(agent.id.as_str(), "a1");
        assert_eq!(agent.label, "label");
        assert_eq!(agent.cwd, PathBuf::from("/work/repo"));
        assert!(agent.group_ids.is_empty());
        assert_eq!(agent.status, AgentStatus::Running);
    }

    #[test]
    fn open_creates_database_in_tempdir() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join("state.db");
        let persistence = Persistence::open(&path).unwrap();
        // load_snapshot against a brand-new file returns empty rows
        // — no panics, no errors.
        let snap = persistence.load_snapshot().unwrap();
        assert!(snap.hosts.is_empty());
        assert!(snap.agents.is_empty());
        assert!(path.exists(), "open must create the database file");
    }

    #[test]
    fn save_then_load_round_trip_through_persistence() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.db");
        let persistence = Persistence::open(&path).unwrap();
        persistence.record_local_host();
        let agent = build_agent_row(AgentRow {
            id: AgentId::new("a1"),
            host_id: local_host_id(),
            label: "label".to_string(),
            cwd: Path::new("/work/repo"),
            session_id: None,
            status: AgentStatus::Running,
            last_attached_at: None,
        });
        persistence.save_agent(&agent);

        let snap = persistence.load_snapshot().unwrap();
        assert_eq!(snap.hosts.len(), 1);
        assert_eq!(snap.hosts[0].id.as_str(), LOCAL_HOST_ID);
        assert_eq!(snap.agents.len(), 1);
        assert_eq!(snap.agents[0].id.as_str(), "a1");
    }

    #[test]
    fn delete_missing_agent_is_a_silent_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.db");
        let persistence = Persistence::open(&path).unwrap();
        // Deleting an id that was never inserted must not panic and
        // must not return an error — the caller's invariant (the row
        // does not exist) is already true.
        persistence.delete_agent(&AgentId::new("ghost"));
    }
}
