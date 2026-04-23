//! `AgentRepo` backed by a single `SQLite` file at
//! `$XDG_STATE_HOME/codemux/state.db` (AD-7). Schema migrations via
//! `rusqlite_migration`.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::Connection;

use codemux_shared_kernel::{AgentId, HostId};

use crate::domain::{Agent, AgentStatus, Host};
use crate::error::Error;
use crate::ports::AgentRepo;

pub struct SqliteStore {
    #[allow(dead_code)]
    conn: Mutex<Connection>,
}

impl SqliteStore {
    /// Open or create the `SQLite` file at `path` and run pending migrations.
    pub fn open(_path: &Path) -> Result<Self, Error> {
        Err(Error::NotImplemented("SqliteStore::open"))
    }
}

impl AgentRepo for SqliteStore {
    fn insert_host(&self, _host: &Host) -> Result<(), Error> {
        Err(Error::NotImplemented("SqliteStore::insert_host"))
    }

    fn get_host(&self, _id: &HostId) -> Result<Option<Host>, Error> {
        Err(Error::NotImplemented("SqliteStore::get_host"))
    }

    fn list_hosts(&self) -> Result<Vec<Host>, Error> {
        Err(Error::NotImplemented("SqliteStore::list_hosts"))
    }

    fn insert_agent(&self, _agent: &Agent) -> Result<(), Error> {
        Err(Error::NotImplemented("SqliteStore::insert_agent"))
    }

    fn get_agent(&self, _id: &AgentId) -> Result<Option<Agent>, Error> {
        Err(Error::NotImplemented("SqliteStore::get_agent"))
    }

    fn list_agents(&self) -> Result<Vec<Agent>, Error> {
        Err(Error::NotImplemented("SqliteStore::list_agents"))
    }

    fn update_agent_status(&self, _id: &AgentId, _status: AgentStatus) -> Result<(), Error> {
        Err(Error::NotImplemented("SqliteStore::update_agent_status"))
    }

    fn delete_agent(&self, _id: &AgentId) -> Result<(), Error> {
        Err(Error::NotImplemented("SqliteStore::delete_agent"))
    }
}
