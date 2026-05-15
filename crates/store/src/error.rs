//! Errors raised by the persistent-state adapter.
//!
//! Per AD-17, each component crate owns its own `thiserror` enum, marked
//! `#[non_exhaustive]` so variants can be added without breaking downstream
//! `match` arms. This crate is the SQLite-backed driven adapter behind the
//! (future, step 3) repository ports in `crates/session`; its failure
//! vocabulary is intentionally narrow — open/migrate/IO — because that is
//! the entire surface area this step introduces.

use std::path::PathBuf;

use thiserror::Error;

/// Errors raised while opening or migrating the codemux state database.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum StoreError {
    /// `$XDG_STATE_HOME` and `$HOME` were both unset, so the default DB
    /// path could not be resolved. Extremely unusual in interactive
    /// shells; codemux surfaces it rather than guessing a path. Callers
    /// that hit this case should pass an explicit path via
    /// [`open`](crate::open).
    #[error("cannot resolve default state directory: neither XDG_STATE_HOME nor HOME is set")]
    StateDirUnresolved,

    /// Failed to create the parent directory of the database file. The
    /// inner [`std::io::Error`] carries the OS-level detail (permission
    /// denied, read-only filesystem, etc.).
    #[error("failed to create state directory {path}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Failed to open the `SQLite` database file. Wraps the underlying
    /// `rusqlite::Error` (file locked, corrupt header, etc.).
    #[error("failed to open database at {path}")]
    OpenDatabase {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    /// Failed to apply a `PRAGMA` after opening the database (`foreign_keys`
    /// or `journal_mode`). Surfaced separately from migration errors so the
    /// caller can tell "the file opened but is broken" from "schema is
    /// stale".
    #[error("failed to apply pragma {pragma}")]
    Pragma {
        pragma: &'static str,
        #[source]
        source: rusqlite::Error,
    },

    /// Schema migration failed. The inner [`rusqlite_migration::Error`]
    /// distinguishes "definition error" (a bug in this crate) from
    /// "runtime error" (a real failure against the file at hand).
    #[error("failed to apply schema migrations")]
    Migrate {
        #[source]
        source: rusqlite_migration::Error,
    },
}
