//! `PtyTransport` backed by `portable-pty` spawning `claude` directly on the
//! local host.

use std::path::Path;

use crate::error::Error;
use crate::ports::{PtyHandle, PtyTransport};

#[derive(Default)]
pub struct LocalPtyTransport;

impl LocalPtyTransport {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl PtyTransport for LocalPtyTransport {
    fn spawn(&self, _cwd: &Path, _resume_session_id: Option<&str>) -> Result<PtyHandle, Error> {
        Err(Error::NotImplemented("LocalPtyTransport::spawn"))
    }
}
