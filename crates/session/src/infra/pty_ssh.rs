//! `PtyTransport` backed by
//! `ssh -tt <target> -- tmux new -A -s ccmux-<id> -- claude` (AD-3).
//!
//! A dropped SSH connection leaves `claude` running on the remote inside the
//! `ccmux-<id>` tmux session, ready to be reattached on the next focus.

use std::path::Path;

use crate::error::Error;
use crate::ports::{PtyHandle, PtyTransport};

pub struct SshPtyTransport {
    target: String,
}

impl SshPtyTransport {
    #[must_use]
    pub fn new(target: impl Into<String>) -> Self {
        Self { target: target.into() }
    }

    #[must_use]
    pub fn target(&self) -> &str {
        &self.target
    }
}

impl PtyTransport for SshPtyTransport {
    fn spawn(&self, _cwd: &Path, _resume_session_id: Option<&str>) -> Result<PtyHandle, Error> {
        Err(Error::NotImplemented("SshPtyTransport::spawn"))
    }
}
