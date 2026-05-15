//! Schema definition for the codemux state database.
//!
//! V1 ships four tables: `hosts`, `groups`, `agents`, `agent_groups`. They
//! map directly onto the domain types in `crates/session/src/domain.rs`
//! (`Host`, `Agent`, `AgentStatus`) plus the membership edge needed for
//! `Agent::group_ids`. The `session_id` column is opaque `TEXT` — per the
//! 2026-05-15 spike, `crates/store` knows nothing about Claude's
//! `--session-id` / `--resume` CLI surface; that knowledge stays in the
//! spawn-argv builders inside `apps/tui` and `apps/daemon`.
//!
//! Conventions:
//!
//! - Timestamps are Unix seconds (`INTEGER`), nullable where the domain
//!   type uses `Option<SystemTime>`. `SQLite` has no native time type and
//!   `rusqlite` does not carry `SystemTime`; Unix-seconds is the smallest
//!   round-trippable shape.
//! - String enums (`hosts.kind`, `agents.status`) are stored as `TEXT`
//!   without an `IN (...)` CHECK. The Rust mapper layer is the single
//!   writer and already enforces the variant set; `SQLite` cannot
//!   `ALTER TABLE ... DROP CONSTRAINT`, so a column-level CHECK would
//!   force the table-rebuild dance on every future variant rename or
//!   addition. The cross-column invariant on `hosts` (`local` ↔ no
//!   `ssh_target`, `ssh` ↔ has `ssh_target`) is kept as a CHECK because
//!   it's tedious to enforce purely in the mapper.
//! - Foreign keys are declared on the schema but only enforced when the
//!   connection has `PRAGMA foreign_keys = ON`, which [`crate::open`]
//!   sets per connection (rusqlite defaults to OFF).
//!
//! Migrations use a `&'static [M<'_>]` slice and
//! [`rusqlite_migration::Migrations::from_slice`], which keeps the
//! migration list const and avoids re-allocating on every `open`.

use rusqlite_migration::{M, Migrations};

/// SQL applied at schema version 1. Single statement string so it runs
/// atomically inside one migration step (`rusqlite_migration` wraps each
/// `M::up` in a transaction).
const V1_UP: &str = "\
CREATE TABLE hosts (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    kind TEXT NOT NULL,
    ssh_target TEXT,
    last_seen_unix INTEGER,
    CHECK (
        (kind = 'local' AND ssh_target IS NULL)
        OR (kind = 'ssh' AND ssh_target IS NOT NULL)
    )
);

CREATE TABLE groups (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL
);

CREATE TABLE agents (
    id TEXT PRIMARY KEY,
    host_id TEXT NOT NULL REFERENCES hosts(id) ON DELETE CASCADE,
    label TEXT NOT NULL,
    cwd TEXT NOT NULL,
    session_id TEXT,
    status TEXT NOT NULL,
    last_attached_at_unix INTEGER
);

CREATE TABLE agent_groups (
    agent_id TEXT NOT NULL REFERENCES agents(id) ON DELETE CASCADE,
    group_id TEXT NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    PRIMARY KEY (agent_id, group_id)
);
";

/// All schema migrations in order. Append-only: never edit V1 once it has
/// shipped to a real machine — add `M::up("...")` for V2 instead.
const MIGRATIONS_SLICE: &[M<'_>] = &[M::up(V1_UP)];

/// Construct the migration set. Cheap (const slice under the hood) and
/// callable from both [`crate::open`] and the in-memory tests.
#[must_use]
pub(crate) fn migrations() -> Migrations<'static> {
    Migrations::from_slice(MIGRATIONS_SLICE)
}
