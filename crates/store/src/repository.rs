//! SQLite-backed adapter for the [`codemux_session::repository`] ports.
//!
//! This is the driven adapter behind the port traits defined in
//! `crates/session`. It owns SQLite-specific concerns (row mapping,
//! transactions, enum-text codecs) and translates them to/from the
//! infrastructure-free domain types.
//!
//! Per the 2026-05-15 persistence spike, the strings `--session-id` and
//! `--resume` MUST NOT appear in this crate — that knowledge of Claude's
//! CLI surface lives in the spawn-argv builders inside `apps/tui` and
//! `apps/daemon`. The `session_id` column is opaque text and the
//! row-mapper treats it as such.
//!
//! Concurrency model: a single [`rusqlite::Connection`] guarded by a
//! [`Mutex`]. The connection is `!Sync` and the port methods take
//! `&self`, so interior mutability is required. For a single-user,
//! single-writer personal tool a `Mutex` is the simple, correct choice;
//! `SQLite`'s own WAL gives us crash-safety on top.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, params};

use codemux_session::domain::{Agent, AgentStatus, Host, HostKind};
use codemux_session::repository::{
    AgentRepository, GroupRepository, HostRepository, RepositoryError,
};
use codemux_shared_kernel::{AgentId, GroupId, HostId};

/// Text encoding of [`AgentStatus`] as it lives in the `agents.status`
/// column. The mapping is owned here, not on the enum, so the domain
/// crate stays free of storage concerns.
mod status_text {
    pub(super) const STARTING: &str = "starting";
    pub(super) const RUNNING: &str = "running";
    pub(super) const IDLE: &str = "idle";
    pub(super) const NEEDS_INPUT: &str = "needs_input";
    pub(super) const DEAD: &str = "dead";
}

/// Text encoding of [`HostKind`] as it lives in the `hosts.kind` column.
mod host_kind_text {
    pub(super) const LOCAL: &str = "local";
    pub(super) const SSH: &str = "ssh";
}

fn agent_status_to_text(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Starting => status_text::STARTING,
        AgentStatus::Running => status_text::RUNNING,
        AgentStatus::Idle => status_text::IDLE,
        AgentStatus::NeedsInput => status_text::NEEDS_INPUT,
        AgentStatus::Dead => status_text::DEAD,
    }
}

fn agent_status_from_text(text: &str) -> Result<AgentStatus, RepositoryError> {
    match text {
        status_text::STARTING => Ok(AgentStatus::Starting),
        status_text::RUNNING => Ok(AgentStatus::Running),
        status_text::IDLE => Ok(AgentStatus::Idle),
        status_text::NEEDS_INPUT => Ok(AgentStatus::NeedsInput),
        status_text::DEAD => Ok(AgentStatus::Dead),
        other => Err(storage_err(format!(
            "unknown agent status text in database: {other:?}"
        ))),
    }
}

fn host_kind_to_text(kind: &HostKind) -> (&'static str, Option<&str>) {
    match kind {
        HostKind::Local => (host_kind_text::LOCAL, None),
        HostKind::Ssh { target } => (host_kind_text::SSH, Some(target.as_str())),
    }
}

fn host_kind_from_text(kind: &str, target: Option<String>) -> Result<HostKind, RepositoryError> {
    match (kind, target) {
        (host_kind_text::LOCAL, None) => Ok(HostKind::Local),
        (host_kind_text::SSH, Some(target)) => Ok(HostKind::Ssh { target }),
        (host_kind_text::LOCAL, Some(_)) => Err(storage_err(
            "hosts row has kind='local' but non-null ssh_target".to_string(),
        )),
        (host_kind_text::SSH, None) => Err(storage_err(
            "hosts row has kind='ssh' but null ssh_target".to_string(),
        )),
        (other, _) => Err(storage_err(format!(
            "unknown host kind text in database: {other:?}"
        ))),
    }
}

fn time_to_unix(value: Option<SystemTime>) -> Result<Option<i64>, RepositoryError> {
    let Some(t) = value else {
        return Ok(None);
    };
    let duration = t
        .duration_since(UNIX_EPOCH)
        .map_err(|err| storage_err(format!("pre-epoch SystemTime: {err}")))?;
    // i64::MAX seconds is ~292 billion years; the `as i64` cast cannot
    // overflow in this universe, but cap it with try_from to keep the
    // pedantic lint happy without an opinionated panic.
    let secs = i64::try_from(duration.as_secs())
        .map_err(|err| storage_err(format!("timestamp seconds overflow i64: {err}")))?;
    Ok(Some(secs))
}

fn time_from_unix(value: Option<i64>) -> Result<Option<SystemTime>, RepositoryError> {
    let Some(secs) = value else {
        return Ok(None);
    };
    // Mirror `agent_status_from_text`: a value in the column that the
    // Rust mapper would never have written signals upstream corruption,
    // not absence. `Option::None` already models "row had a NULL here";
    // negative seconds are a third state we must surface so an
    // operator can find the bad row.
    let unsigned = u64::try_from(secs)
        .map_err(|_| storage_err(format!("negative unix seconds in timestamp column: {secs}")))?;
    Ok(Some(UNIX_EPOCH + std::time::Duration::from_secs(unsigned)))
}

fn cwd_to_text(cwd: &Path) -> Result<&str, RepositoryError> {
    cwd.to_str().ok_or_else(|| {
        storage_err(format!(
            "agent cwd is not valid UTF-8 and cannot be persisted: {}",
            cwd.display()
        ))
    })
}

fn storage_err(message: String) -> RepositoryError {
    RepositoryError::Storage {
        source: Box::new(StorageMessage(message)),
    }
}

fn storage_from_rusqlite(err: rusqlite::Error) -> RepositoryError {
    RepositoryError::Storage {
        source: Box::new(err),
    }
}

/// Newtype wrapper so adapter-internal messages flow through the
/// `Box<dyn Error>` carried by [`RepositoryError::Storage`] without
/// reaching for `std::io::Error::other` (which would be misleading).
#[derive(Debug)]
struct StorageMessage(String);

impl std::fmt::Display for StorageMessage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StorageMessage {}

/// SQLite-backed adapter implementing the session-crate repository ports.
///
/// Construct with [`SqliteStore::new`], passing a [`Connection`] obtained
/// from [`crate::open`]. The store owns the connection for its lifetime;
/// `&self` access is serialised through an internal [`Mutex`].
pub struct SqliteStore {
    conn: Mutex<Connection>,
}

impl SqliteStore {
    /// Wrap a freshly-opened connection. The connection should already
    /// have had migrations applied (i.e. it came out of [`crate::open`]).
    #[must_use]
    pub fn new(conn: Connection) -> Self {
        Self {
            conn: Mutex::new(conn),
        }
    }

    fn with_conn<R>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<R, RepositoryError>,
    ) -> Result<R, RepositoryError> {
        // SAFETY-RATIONALE: a `PoisonError` here means a previous holder
        // of this lock panicked mid-transaction. The connection's
        // internal state (in-flight tx, prepared-statement cache) is
        // potentially torn, so masking the panic as a routine
        // `Storage` error would hide a real bug. Fail-fast instead —
        // codemux is a single-user TUI; a panic from the persistence
        // path is already fatal at the binary edge via `color-eyre`.
        #[allow(clippy::expect_used)] // see SAFETY-RATIONALE above
        let mut guard = self
            .conn
            .lock()
            .expect("connection mutex poisoned — another thread panicked mid-write");
        f(&mut guard)
    }
}

impl HostRepository for SqliteStore {
    fn load_all(&self) -> Result<Vec<Host>, RepositoryError> {
        self.with_conn(|conn| {
            let mut stmt = conn
                .prepare("SELECT id, name, kind, ssh_target, last_seen_unix FROM hosts ORDER BY id")
                .map_err(storage_from_rusqlite)?;
            let rows = stmt
                .query_map([], |row| {
                    let id: String = row.get(0)?;
                    let name: String = row.get(1)?;
                    let kind: String = row.get(2)?;
                    let ssh_target: Option<String> = row.get(3)?;
                    let last_seen_unix: Option<i64> = row.get(4)?;
                    Ok((id, name, kind, ssh_target, last_seen_unix))
                })
                .map_err(storage_from_rusqlite)?;

            let mut out = Vec::new();
            for row in rows {
                let (id, name, kind, ssh_target, last_seen_unix) =
                    row.map_err(storage_from_rusqlite)?;
                let kind = host_kind_from_text(&kind, ssh_target)?;
                let last_seen = time_from_unix(last_seen_unix)?;
                out.push(Host {
                    id: HostId::new(id),
                    name,
                    kind,
                    last_seen,
                });
            }
            Ok(out)
        })
    }

    fn save(&self, host: &Host) -> Result<(), RepositoryError> {
        let (kind_text, ssh_target) = host_kind_to_text(&host.kind);
        let last_seen_unix = time_to_unix(host.last_seen)?;
        self.with_conn(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO hosts (id, name, kind, ssh_target, last_seen_unix) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    host.id.as_str(),
                    host.name,
                    kind_text,
                    ssh_target,
                    last_seen_unix,
                ],
            )
            .map_err(storage_from_rusqlite)?;
            Ok(())
        })
    }

    fn delete(&self, id: &HostId) -> Result<(), RepositoryError> {
        let affected = self.with_conn(|conn| {
            conn.execute("DELETE FROM hosts WHERE id = ?1", params![id.as_str()])
                .map_err(storage_from_rusqlite)
        })?;
        if affected == 0 {
            return Err(RepositoryError::NotFound {
                kind: "host",
                id: id.as_str().to_string(),
            });
        }
        Ok(())
    }
}

impl AgentRepository for SqliteStore {
    fn load_all(&self) -> Result<Vec<Agent>, RepositoryError> {
        self.with_conn(|conn| {
            let mut stmt = conn
                .prepare(
                    "SELECT id, host_id, label, cwd, session_id, status, last_attached_at_unix \
                     FROM agents ORDER BY id",
                )
                .map_err(storage_from_rusqlite)?;
            let rows = stmt
                .query_map([], |row| {
                    let id: String = row.get(0)?;
                    let host_id: String = row.get(1)?;
                    let label: String = row.get(2)?;
                    let cwd: String = row.get(3)?;
                    let session_id: Option<String> = row.get(4)?;
                    let status: String = row.get(5)?;
                    let last_attached_unix: Option<i64> = row.get(6)?;
                    Ok((
                        id,
                        host_id,
                        label,
                        cwd,
                        session_id,
                        status,
                        last_attached_unix,
                    ))
                })
                .map_err(storage_from_rusqlite)?;

            let mut agents: Vec<Agent> = Vec::new();
            for row in rows {
                let (id, host_id, label, cwd, session_id, status, last_attached_unix) =
                    row.map_err(storage_from_rusqlite)?;
                let status = agent_status_from_text(&status)?;
                let last_attached_at = time_from_unix(last_attached_unix)?;
                agents.push(Agent {
                    id: AgentId::new(id),
                    host_id: HostId::new(host_id),
                    label,
                    cwd: PathBuf::from(cwd),
                    group_ids: Vec::new(),
                    session_id,
                    status,
                    last_attached_at,
                });
            }

            // Hydrate `group_ids` for every agent in one query rather than
            // N. Order by `agent_id, group_id` so the resulting Vec on
            // each Agent is stable across reads.
            let mut stmt = conn
                .prepare("SELECT agent_id, group_id FROM agent_groups ORDER BY agent_id, group_id")
                .map_err(storage_from_rusqlite)?;
            let edges = stmt
                .query_map([], |row| {
                    let agent_id: String = row.get(0)?;
                    let group_id: String = row.get(1)?;
                    Ok((agent_id, group_id))
                })
                .map_err(storage_from_rusqlite)?;

            for edge in edges {
                let (agent_id, group_id) = edge.map_err(storage_from_rusqlite)?;
                if let Some(agent) = agents.iter_mut().find(|a| a.id.as_str() == agent_id) {
                    agent.group_ids.push(GroupId::new(group_id));
                }
                // Orphan edges (no matching agent) are impossible under
                // the schema's FK + CASCADE — and would be a corruption
                // signal worth ignoring rather than failing the read.
            }

            Ok(agents)
        })
    }

    fn save(&self, agent: &Agent) -> Result<(), RepositoryError> {
        let cwd_text = cwd_to_text(&agent.cwd)?;
        let status_text = agent_status_to_text(agent.status);
        let last_attached_unix = time_to_unix(agent.last_attached_at)?;

        self.with_conn(|conn| {
            let tx = conn.transaction().map_err(storage_from_rusqlite)?;

            tx.execute(
                "INSERT OR REPLACE INTO agents \
                 (id, host_id, label, cwd, session_id, status, last_attached_at_unix) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    agent.id.as_str(),
                    agent.host_id.as_str(),
                    agent.label,
                    cwd_text,
                    agent.session_id,
                    status_text,
                    last_attached_unix,
                ],
            )
            .map_err(storage_from_rusqlite)?;

            // Reconcile join-table membership against `agent.group_ids`.
            //
            // Strategy: DELETE-diff + INSERT-diff. Read the current edges
            // for this agent, compute the set difference against the
            // desired set, and apply only the deltas. This keeps the
            // write footprint minimal (no churn on unchanged rows) and
            // makes the resulting SQL trace readable.
            let mut current: Vec<String> = {
                let mut stmt = tx
                    .prepare("SELECT group_id FROM agent_groups WHERE agent_id = ?1")
                    .map_err(storage_from_rusqlite)?;
                let rows = stmt
                    .query_map(params![agent.id.as_str()], |row| row.get::<_, String>(0))
                    .map_err(storage_from_rusqlite)?;
                let mut acc = Vec::new();
                for row in rows {
                    acc.push(row.map_err(storage_from_rusqlite)?);
                }
                acc
            };
            current.sort();
            current.dedup();

            let mut desired: Vec<String> = agent
                .group_ids
                .iter()
                .map(|g| g.as_str().to_string())
                .collect();
            desired.sort();
            desired.dedup();

            // To delete: in `current`, not in `desired`.
            for group_id in current.iter().filter(|g| !desired.contains(g)) {
                tx.execute(
                    "DELETE FROM agent_groups WHERE agent_id = ?1 AND group_id = ?2",
                    params![agent.id.as_str(), group_id],
                )
                .map_err(storage_from_rusqlite)?;
            }

            // To insert: in `desired`, not in `current`.
            for group_id in desired.iter().filter(|g| !current.contains(g)) {
                tx.execute(
                    "INSERT INTO agent_groups (agent_id, group_id) VALUES (?1, ?2)",
                    params![agent.id.as_str(), group_id],
                )
                .map_err(storage_from_rusqlite)?;
            }

            tx.commit().map_err(storage_from_rusqlite)?;
            Ok(())
        })
    }

    fn delete(&self, id: &AgentId) -> Result<(), RepositoryError> {
        let affected = self.with_conn(|conn| {
            conn.execute("DELETE FROM agents WHERE id = ?1", params![id.as_str()])
                .map_err(storage_from_rusqlite)
        })?;
        if affected == 0 {
            return Err(RepositoryError::NotFound {
                kind: "agent",
                id: id.as_str().to_string(),
            });
        }
        Ok(())
    }
}

impl GroupRepository for SqliteStore {
    fn load_all(&self) -> Result<Vec<(GroupId, String)>, RepositoryError> {
        self.with_conn(|conn| {
            let mut stmt = conn
                .prepare("SELECT id, name FROM groups ORDER BY id")
                .map_err(storage_from_rusqlite)?;
            let rows = stmt
                .query_map([], |row| {
                    let id: String = row.get(0)?;
                    let name: String = row.get(1)?;
                    Ok((id, name))
                })
                .map_err(storage_from_rusqlite)?;

            let mut out = Vec::new();
            for row in rows {
                let (id, name) = row.map_err(storage_from_rusqlite)?;
                out.push((GroupId::new(id), name));
            }
            Ok(out)
        })
    }

    fn save(&self, id: &GroupId, name: &str) -> Result<(), RepositoryError> {
        self.with_conn(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO groups (id, name) VALUES (?1, ?2)",
                params![id.as_str(), name],
            )
            .map_err(storage_from_rusqlite)?;
            Ok(())
        })
    }

    fn delete(&self, id: &GroupId) -> Result<(), RepositoryError> {
        let affected = self.with_conn(|conn| {
            conn.execute("DELETE FROM groups WHERE id = ?1", params![id.as_str()])
                .map_err(storage_from_rusqlite)
        })?;
        if affected == 0 {
            return Err(RepositoryError::NotFound {
                kind: "group",
                id: id.as_str().to_string(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    //! Tests run against an in-memory connection: every test owns its
    //! own DB, no `tempfile` overhead, and the migration set is exercised
    //! from a fresh schema each time. The `open()` helper documented in
    //! `lib.rs` is also fine to use; we prefer in-memory here to keep
    //! the test suite fast.
    //!
    //! No coverage for the mutex-poisoning path in `with_conn`: poisoning
    //! requires a panic inside the locked closure, and the `.expect()`
    //! there is fail-fast by design (see the SAFETY-RATIONALE comment).
    //! A test that deliberately panics inside the closure would have to
    //! catch the unwind across thread boundaries and assert on
    //! aborts-vs-panics; the ratio of harness complexity to bug-finding
    //! value is poor for a single-user TUI.

    use std::path::PathBuf;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use rusqlite::Connection;

    use codemux_session::domain::{Agent, AgentStatus, Host, HostKind};
    use codemux_session::repository::{
        AgentRepository, GroupRepository, HostRepository, RepositoryError,
    };
    use codemux_shared_kernel::{AgentId, GroupId, HostId};

    use super::SqliteStore;
    use crate::schema;

    fn fresh_store() -> SqliteStore {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        schema::migrations().to_latest(&mut conn).unwrap();
        SqliteStore::new(conn)
    }

    fn make_host(id: &str, kind: HostKind) -> Host {
        Host {
            id: HostId::new(id),
            name: format!("name-of-{id}"),
            kind,
            last_seen: Some(UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
        }
    }

    fn seed_host(store: &SqliteStore, id: &str) {
        HostRepository::save(store, &make_host(id, HostKind::Local)).unwrap();
    }

    fn seed_group(store: &SqliteStore, id: &str) {
        GroupRepository::save(store, &GroupId::new(id), &format!("name-of-{id}")).unwrap();
    }

    fn make_agent(id: &str, host: &str) -> Agent {
        Agent {
            id: AgentId::new(id),
            host_id: HostId::new(host),
            label: format!("label-{id}"),
            cwd: PathBuf::from("/work/repo"),
            group_ids: Vec::new(),
            session_id: None,
            status: AgentStatus::Running,
            last_attached_at: None,
        }
    }

    #[test]
    fn round_trip_local_host() {
        let store = fresh_store();
        let host = make_host("h-local", HostKind::Local);
        HostRepository::save(&store, &host).unwrap();

        let loaded = HostRepository::load_all(&store).unwrap();
        assert_eq!(loaded, vec![host]);
    }

    #[test]
    fn round_trip_ssh_host() {
        let store = fresh_store();
        let host = make_host(
            "h-ssh",
            HostKind::Ssh {
                target: "user@devpod".to_string(),
            },
        );
        HostRepository::save(&store, &host).unwrap();

        let loaded = HostRepository::load_all(&store).unwrap();
        assert_eq!(loaded, vec![host]);
    }

    #[test]
    fn host_save_is_idempotent() {
        let store = fresh_store();
        let host = make_host("h-local", HostKind::Local);
        HostRepository::save(&store, &host).unwrap();
        HostRepository::save(&store, &host).unwrap();

        let loaded = HostRepository::load_all(&store).unwrap();
        assert_eq!(loaded.len(), 1, "saving twice must not duplicate the row");
        assert_eq!(loaded[0], host);
    }

    #[test]
    fn host_delete_missing_returns_not_found() {
        let store = fresh_store();
        let err = HostRepository::delete(&store, &HostId::new("ghost")).unwrap_err();
        match err {
            RepositoryError::NotFound { kind, id } => {
                assert_eq!(kind, "host");
                assert_eq!(id, "ghost");
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn host_last_seen_round_trips_nullable() {
        let store = fresh_store();
        let host = Host {
            id: HostId::new("h-none"),
            name: "ephemeral".into(),
            kind: HostKind::Local,
            last_seen: None,
        };
        HostRepository::save(&store, &host).unwrap();

        let loaded = HostRepository::load_all(&store).unwrap();
        assert_eq!(loaded, vec![host]);
    }

    #[test]
    fn round_trip_agent_without_session_or_groups() {
        let store = fresh_store();
        seed_host(&store, "h1");
        let agent = make_agent("a1", "h1");
        AgentRepository::save(&store, &agent).unwrap();

        let loaded = AgentRepository::load_all(&store).unwrap();
        assert_eq!(loaded, vec![agent]);
    }

    #[test]
    fn round_trip_agent_with_session_id_and_groups() {
        let store = fresh_store();
        seed_host(&store, "h1");
        seed_group(&store, "g-a");
        seed_group(&store, "g-b");

        let mut agent = make_agent("a1", "h1");
        agent.session_id = Some("8e3c7632-f5ad-4e8c-bcbf-960c4a7d7c7d".into());
        agent.group_ids = vec![GroupId::new("g-a"), GroupId::new("g-b")];
        agent.status = AgentStatus::NeedsInput;
        agent.last_attached_at = Some(SystemTime::UNIX_EPOCH + Duration::from_secs(42));
        AgentRepository::save(&store, &agent).unwrap();

        let mut loaded = AgentRepository::load_all(&store).unwrap();
        // load_all orders edges by group_id; mirror that here so the
        // equality check doesn't trip on Vec ordering.
        assert_eq!(loaded.len(), 1);
        loaded[0]
            .group_ids
            .sort_by(|l, r| l.as_str().cmp(r.as_str()));
        let mut expected = agent;
        expected
            .group_ids
            .sort_by(|l, r| l.as_str().cmp(r.as_str()));
        assert_eq!(loaded, vec![expected]);
    }

    #[test]
    fn agent_save_is_idempotent_and_does_not_dupe_join_rows() {
        let store = fresh_store();
        seed_host(&store, "h1");
        seed_group(&store, "g-a");

        let mut agent = make_agent("a1", "h1");
        agent.group_ids = vec![GroupId::new("g-a")];

        AgentRepository::save(&store, &agent).unwrap();
        AgentRepository::save(&store, &agent).unwrap();

        let loaded = AgentRepository::load_all(&store).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].group_ids, vec![GroupId::new("g-a")]);

        // Confirm at the raw-SQL level too — the loader dedupes by
        // construction, the join table itself must not have grown.
        let conn = store.conn.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM agent_groups WHERE agent_id = 'a1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn agent_group_membership_reconciles_on_resave() {
        let store = fresh_store();
        seed_host(&store, "h1");
        seed_group(&store, "g-a");
        seed_group(&store, "g-b");
        seed_group(&store, "g-c");

        let mut agent = make_agent("a1", "h1");

        agent.group_ids = vec![GroupId::new("g-a"), GroupId::new("g-b")];
        AgentRepository::save(&store, &agent).unwrap();

        agent.group_ids = vec![GroupId::new("g-b"), GroupId::new("g-c")];
        AgentRepository::save(&store, &agent).unwrap();

        let conn = store.conn.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT group_id FROM agent_groups WHERE agent_id = 'a1' ORDER BY group_id")
            .unwrap();
        let groups: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(groups, vec!["g-b".to_string(), "g-c".to_string()]);
    }

    #[test]
    fn agent_delete_missing_returns_not_found() {
        let store = fresh_store();
        let err = AgentRepository::delete(&store, &AgentId::new("ghost")).unwrap_err();
        match err {
            RepositoryError::NotFound { kind, id } => {
                assert_eq!(kind, "agent");
                assert_eq!(id, "ghost");
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn agent_delete_cascades_join_table() {
        let store = fresh_store();
        seed_host(&store, "h1");
        seed_group(&store, "g-a");

        let mut agent = make_agent("a1", "h1");
        agent.group_ids = vec![GroupId::new("g-a")];
        AgentRepository::save(&store, &agent).unwrap();

        AgentRepository::delete(&store, &AgentId::new("a1")).unwrap();

        let conn = store.conn.lock().unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM agent_groups WHERE agent_id = 'a1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "ON DELETE CASCADE should have removed the edge");
    }

    #[test]
    fn group_round_trip_and_idempotent_save() {
        let store = fresh_store();
        GroupRepository::save(&store, &GroupId::new("g-a"), "alpha").unwrap();
        GroupRepository::save(&store, &GroupId::new("g-a"), "alpha-renamed").unwrap();
        GroupRepository::save(&store, &GroupId::new("g-b"), "beta").unwrap();

        let loaded = GroupRepository::load_all(&store).unwrap();
        assert_eq!(
            loaded,
            vec![
                (GroupId::new("g-a"), "alpha-renamed".to_string()),
                (GroupId::new("g-b"), "beta".to_string()),
            ]
        );
    }

    #[test]
    fn group_delete_missing_returns_not_found() {
        let store = fresh_store();
        let err = GroupRepository::delete(&store, &GroupId::new("ghost")).unwrap_err();
        match err {
            RepositoryError::NotFound { kind, id } => {
                assert_eq!(kind, "group");
                assert_eq!(id, "ghost");
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn unknown_status_text_surfaces_as_storage_error() {
        let store = fresh_store();
        seed_host(&store, "h1");
        // Inject a row with a status string that the Rust mapper would
        // never write. The load path must surface a Storage error
        // rather than panicking or silently mapping to Dead.
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, host_id, label, cwd, status) \
                 VALUES ('a-bad', 'h1', 'x', '/x', 'haunted')",
                [],
            )
            .unwrap();
        }

        let err = AgentRepository::load_all(&store).unwrap_err();
        match err {
            RepositoryError::Storage { source } => {
                assert!(
                    source.to_string().contains("haunted"),
                    "storage error message should mention the offending text, got {source}",
                );
            }
            other => panic!("expected Storage, got {other:?}"),
        }
    }

    /// Negative `last_attached_at_unix` (or `last_seen_unix`) is a value
    /// the Rust mapper never writes — `time_to_unix` rejects pre-epoch
    /// `SystemTime` on the write path. If we ever see one on the read
    /// path it's upstream corruption, and `Option::None` would mask the
    /// bug. The load path must surface `Storage` with the offending
    /// value in the message so an operator can find the row.
    #[test]
    fn negative_timestamp_surfaces_as_storage_error() {
        let store = fresh_store();
        seed_host(&store, "h1");
        {
            let conn = store.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO agents (id, host_id, label, cwd, status, last_attached_at_unix) \
                 VALUES ('a-bad', 'h1', 'x', '/x', 'running', -42)",
                [],
            )
            .unwrap();
        }

        let err = AgentRepository::load_all(&store).unwrap_err();
        match err {
            RepositoryError::Storage { source } => {
                let msg = source.to_string();
                assert!(
                    msg.contains("-42"),
                    "storage error message should mention the offending value, got {msg}",
                );
                assert!(
                    msg.contains("negative unix seconds"),
                    "storage error message should describe the corruption mode, got {msg}",
                );
            }
            other => panic!("expected Storage, got {other:?}"),
        }
    }
}
