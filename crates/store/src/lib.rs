//! codemux persistent state: schema + migrations over `rusqlite` ([AD-7]).
//!
//! This crate is the storage substrate behind the (forthcoming, step 3)
//! repository ports defined in `crates/session`. It owns SQLite-specific
//! concerns — connection setup, pragmas, schema migrations — and nothing
//! else. In particular, per the 2026-05-15 persistence spike, the strings
//! `--session-id` and `--resume` MUST NOT appear here: that knowledge of
//! Claude's CLI surface stays in the spawn-argv builders inside
//! `apps/tui` and `apps/daemon`. The `session_id` column is opaque text.
//!
//! Surface area (V1):
//!
//! - [`open`] — opens (creating parent dirs if needed) and migrates a
//!   `SQLite` file, returning a ready-to-use [`rusqlite::Connection`].
//! - [`default_db_path`] — resolves the platform-conventional location
//!   `$XDG_STATE_HOME/codemux/state.db`, falling back to
//!   `~/.local/state/codemux/state.db`.
//! - [`StoreError`] — component-local `thiserror` enum, `#[non_exhaustive]`
//!   per AD-17.
//!
//! [AD-7]: ../../../../docs/004--architecture.md

mod error;
mod schema;

use std::path::{Path, PathBuf};

use rusqlite::Connection;

pub use crate::error::StoreError;

/// Subdirectory inside the state root that holds codemux's database.
const STATE_SUBDIR: &str = "codemux";

/// Filename of the `SQLite` database under the state subdirectory.
const DB_FILENAME: &str = "state.db";

/// Open the codemux state database at `path`, creating the file (and any
/// missing parent directories) if needed, and apply all pending schema
/// migrations.
///
/// The returned connection has `foreign_keys = ON` and `journal_mode = WAL`.
/// Calling `open` again on the same path is idempotent: migrations track
/// their own state in `SQLite`'s `user_version` pragma, so a second call
/// applies zero migrations and returns a connection against the existing
/// schema.
///
/// # Errors
///
/// - [`StoreError::CreateDir`] if the parent directory cannot be created.
/// - [`StoreError::OpenDatabase`] if `SQLite` cannot open the file (locked,
///   corrupt, permission denied, …).
/// - [`StoreError::Pragma`] if `foreign_keys` or `journal_mode` cannot be
///   applied.
/// - [`StoreError::Migrate`] if a migration fails (definition bug or
///   runtime conflict against an existing schema).
pub fn open(path: &Path) -> Result<Connection, StoreError> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|source| StoreError::CreateDir {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    let mut conn = Connection::open(path).map_err(|source| StoreError::OpenDatabase {
        path: path.to_path_buf(),
        source,
    })?;

    // Enable foreign-key enforcement. rusqlite (libsqlite default) leaves
    // this OFF, which silently makes our `REFERENCES` clauses advisory.
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(|source| StoreError::Pragma {
            pragma: "foreign_keys",
            source,
        })?;

    // WAL: cheap concurrency + crash-safety win for the personal-tool
    // workload. `pragma_update_and_check` would also work, but we don't
    // need the readback — failure surfaces as an `Err` either way.
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|source| StoreError::Pragma {
            pragma: "journal_mode",
            source,
        })?;

    schema::migrations()
        .to_latest(&mut conn)
        .map_err(|source| StoreError::Migrate { source })?;

    Ok(conn)
}

/// Resolve codemux's default state-database path.
///
/// Looks up `$XDG_STATE_HOME` first; if unset, falls back to
/// `$HOME/.local/state`. In either case the final path is
/// `<state-root>/codemux/state.db`.
///
/// This intentionally avoids the `dirs` / `directories` crates: AD-7's
/// XDG fallback is a four-line rule, and the rest of the workspace
/// already declines those crates for the same reason.
///
/// # Errors
///
/// Returns [`StoreError::StateDirUnresolved`] if both `XDG_STATE_HOME`
/// and `HOME` are unset — extremely unusual in interactive shells; we
/// surface it rather than guessing a path. Callers that hit this case
/// should pass an explicit path to [`open`].
pub fn default_db_path() -> Result<PathBuf, StoreError> {
    resolve_default_db_path(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
}

/// Pure variant of [`default_db_path`] that takes the env values as
/// arguments. Exists so the resolution rule can be tested without
/// `unsafe { set_var(...) }` — the workspace's `unsafe_code = "forbid"`
/// (AD-21) makes mutating `std::env` in tests a non-starter.
fn resolve_default_db_path(
    xdg_state_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> Result<PathBuf, StoreError> {
    if let Some(xdg) = xdg_state_home
        && !xdg.is_empty()
    {
        return Ok(PathBuf::from(xdg).join(STATE_SUBDIR).join(DB_FILENAME));
    }

    if let Some(home) = home
        && !home.is_empty()
    {
        return Ok(PathBuf::from(home)
            .join(".local")
            .join("state")
            .join(STATE_SUBDIR)
            .join(DB_FILENAME));
    }

    Err(StoreError::StateDirUnresolved)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    //! Tests run against on-disk temp files rather than `:memory:` because
    //! `open` itself takes a path (and the on-disk path exercises
    //! `create_dir_all` and the WAL pragma against a real file, which is
    //! the realistic failure surface). `:memory:` is used in one focused
    //! test to confirm the migration set applies cleanly without I/O.

    use std::collections::BTreeSet;

    use rusqlite::Connection;

    use super::*;

    fn collect_tables(conn: &Connection) -> BTreeSet<String> {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
            .unwrap();
        stmt.query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<BTreeSet<_>, _>>()
            .unwrap()
    }

    fn collect_columns(conn: &Connection, table: &str) -> BTreeSet<String> {
        // `PRAGMA table_info(<name>)` is the canonical way to enumerate
        // columns. We feed the table name through `pragma_query` rather
        // than interpolating it into SQL so the call stays free of any
        // injection footgun even in tests.
        let mut cols = BTreeSet::new();
        conn.pragma(None, "table_info", table, |row| {
            cols.insert(row.get::<_, String>(1)?);
            Ok(())
        })
        .unwrap();
        cols
    }

    /// Confirms the migration set itself is well-formed and applies
    /// against a brand-new in-memory connection.
    #[test]
    fn migrations_apply_against_in_memory() {
        let mut conn = Connection::open_in_memory().unwrap();
        schema::migrations().to_latest(&mut conn).unwrap();

        let tables = collect_tables(&conn);
        // `sqlite_sequence` would only appear if we used AUTOINCREMENT,
        // which we don't — primary keys are TEXT UUIDs from the domain.
        assert!(tables.contains("hosts"));
        assert!(tables.contains("groups"));
        assert!(tables.contains("agents"));
        assert!(tables.contains("agent_groups"));
    }

    /// Confirms the on-disk path is created and the expected tables exist.
    #[test]
    fn open_creates_file_with_expected_tables() {
        let tmp = tempfile::tempdir().unwrap();
        // Force `open` through the `create_dir_all` branch by nesting one
        // level deeper than the tempdir root.
        let path = tmp.path().join("sub").join("state.db");
        let conn = open(&path).unwrap();
        assert!(path.exists(), "open() must create the database file");

        let tables = collect_tables(&conn);
        let expected: BTreeSet<String> = ["agent_groups", "agents", "groups", "hosts"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        assert!(
            expected.is_subset(&tables),
            "expected tables {expected:?} not all present in {tables:?}",
        );
    }

    /// Confirms calling `open` twice on the same path is idempotent and
    /// preserves data inserted between calls (i.e. the second `open` does
    /// not blow away the schema or any rows).
    #[test]
    fn open_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state.db");

        {
            let conn = open(&path).unwrap();
            conn.execute(
                "INSERT INTO hosts (id, name, kind) VALUES (?1, ?2, 'local')",
                ["host-1", "laptop"],
            )
            .unwrap();
        }

        let conn = open(&path).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM hosts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1, "row inserted before second open must survive");
    }

    /// Confirms the columns we care about for step 3's repository layer
    /// are present on each table.
    #[test]
    fn schema_columns_match_domain_shape() {
        let mut conn = Connection::open_in_memory().unwrap();
        schema::migrations().to_latest(&mut conn).unwrap();

        let host_cols = collect_columns(&conn, "hosts");
        for c in ["id", "name", "kind", "ssh_target", "last_seen_unix"] {
            assert!(host_cols.contains(c), "hosts missing column {c}");
        }

        let agent_cols = collect_columns(&conn, "agents");
        for c in [
            "id",
            "host_id",
            "label",
            "cwd",
            "session_id",
            "status",
            "last_attached_at_unix",
        ] {
            assert!(agent_cols.contains(c), "agents missing column {c}");
        }

        let group_cols = collect_columns(&conn, "groups");
        for c in ["id", "name"] {
            assert!(group_cols.contains(c), "groups missing column {c}");
        }

        let edge_cols = collect_columns(&conn, "agent_groups");
        for c in ["agent_id", "group_id"] {
            assert!(edge_cols.contains(c), "agent_groups missing column {c}");
        }
    }

    /// Confirms the compound CHECK on `hosts` rejects rows where
    /// `kind`/`ssh_target` disagree: `local` with a non-null `ssh_target`
    /// and `ssh` with a null `ssh_target`. This is the smallest test
    /// that proves the schema, not just the table names, made it into
    /// the database. There is no separate enum CHECK on `kind` — the
    /// mapper layer is the writer and `SQLite` cannot drop column-level
    /// CHECKs without a table rebuild.
    #[test]
    fn host_kind_target_check_constraint_is_enforced() {
        let mut conn = Connection::open_in_memory().unwrap();
        schema::migrations().to_latest(&mut conn).unwrap();

        // Valid: local with no ssh_target.
        conn.execute(
            "INSERT INTO hosts (id, name, kind) VALUES ('h-local', 'laptop', 'local')",
            [],
        )
        .unwrap();

        // Valid: ssh with a target.
        conn.execute(
            "INSERT INTO hosts (id, name, kind, ssh_target) \
             VALUES ('h-ssh', 'devpod', 'ssh', 'user@devpod')",
            [],
        )
        .unwrap();

        // Invalid: local with a target.
        let err = conn
            .execute(
                "INSERT INTO hosts (id, name, kind, ssh_target) \
                 VALUES ('h-bad-1', 'x', 'local', 'oops')",
                [],
            )
            .unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("check"),
            "expected CHECK constraint error, got {err}",
        );

        // Invalid: ssh with no target.
        let err = conn
            .execute(
                "INSERT INTO hosts (id, name, kind) VALUES ('h-bad-2', 'x', 'ssh')",
                [],
            )
            .unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("check"),
            "expected CHECK constraint error, got {err}",
        );
    }

    /// `default_db_path` should prefer `XDG_STATE_HOME` when set, fall back
    /// to `$HOME/.local/state`, and fail cleanly when both are unset.
    /// Driven against the pure [`resolve_default_db_path`] helper so the
    /// test stays free of `unsafe { set_var(...) }` — the workspace
    /// forbids `unsafe_code` (AD-21), and mutating `std::env` from a
    /// parallel test runner is unsound on edition 2024 anyway.
    #[test]
    fn default_db_path_prefers_xdg_state_home() {
        use std::ffi::OsString;

        let xdg = Some(OsString::from("/tmp/xdg-state-test"));
        let home = Some(OsString::from("/home/should-not-be-used"));
        let p = resolve_default_db_path(xdg, home.clone()).unwrap();
        assert_eq!(p, PathBuf::from("/tmp/xdg-state-test/codemux/state.db"));

        let p = resolve_default_db_path(None, home).unwrap();
        assert_eq!(
            p,
            PathBuf::from("/home/should-not-be-used/.local/state/codemux/state.db"),
        );

        // Empty XDG_STATE_HOME is treated as "unset" so a stray
        // `XDG_STATE_HOME=` in the shell environment doesn't pin the DB
        // at `/codemux/state.db`.
        let p = resolve_default_db_path(Some(OsString::new()), Some(OsString::from("/h"))).unwrap();
        assert_eq!(p, PathBuf::from("/h/.local/state/codemux/state.db"));

        assert!(matches!(
            resolve_default_db_path(None, None),
            Err(StoreError::StateDirUnresolved),
        ));
    }
}
