//! A `Session` owns a PTY child **independently of any client connection**.
//!
//! Clients attach to the session, exchange bytes, detach. The session and
//! its child persist across attaches — that's the whole reason the daemon
//! exists (session continuity across SSH disconnects). Coupling the PTY
//! lifecycle to the connection lifecycle would defeat the daemon's purpose
//! and bake in an architectural assumption that fights every later stage.
//!
//! Stage 0: at most one session per daemon, lazily spawned on first
//! accept. Stage 2 will key sessions by agent id.
//!
//! Drop kills the child on daemon shutdown. Without that, the child would
//! become a zombie outliving the daemon — exactly the opposite of what
//! we want.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::Path;

use crossbeam_channel::Receiver;
use portable_pty::{Child, MasterPty};

use crate::conn;
use crate::error::Error;
use crate::pty::{self, PtyChild};

pub struct Session {
    child: Box<dyn Child + Send + Sync>,
    writer: Box<dyn Write + Send>,
    rx: Receiver<Vec<u8>>,
    // Held to keep the master fd open across attaches. Closing it would
    // make the child see EOF on stdin and exit, breaking continuity.
    // Stage 4: also used by `conn` to apply inbound Resize frames; the
    // underscore prefix is gone now that we read it.
    master: Box<dyn MasterPty + Send>,
}

impl Session {
    pub fn spawn(
        command: &str,
        args: &[String],
        cwd: Option<&Path>,
        rows: u16,
        cols: u16,
    ) -> Result<Self, Error> {
        let PtyChild {
            master,
            writer,
            child,
            rx,
        } = pty::spawn(command, args, cwd, rows, cols)?;
        Ok(Self {
            child,
            writer,
            rx,
            master,
        })
    }

    /// Attach `stream` to this session's PTY. Returns when the client
    /// disconnects, the child exits, or an I/O error occurs. **The PTY
    /// child survives** — call again with a fresh stream to re-attach.
    pub fn attach(&mut self, stream: UnixStream) -> Result<(), Error> {
        conn::run(
            stream,
            &mut *self.writer,
            &self.rx,
            &mut *self.master,
            &mut *self.child,
        )
    }

    /// True if the child has exited. Best-effort: an I/O error from
    /// `try_wait` is treated as "still running" (the supervisor will
    /// recheck on the next accept).
    #[must_use]
    pub fn child_exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Best-effort cleanup. The child may already be dead from EOF or
        // a signal; portable_pty handles `kill`/`wait` on dead children
        // gracefully. We log at debug so a stuck-zombie investigation
        // has a breadcrumb without polluting normal logs.
        if let Err(e) = self.child.kill() {
            tracing::debug!("session drop: child.kill failed: {e}");
        }
        if let Err(e) = self.child.wait() {
            tracing::debug!("session drop: child.wait failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;

    /// `child_exited` flips to true once the child has actually exited.
    /// `true` exits immediately; on a fast laptop this is sub-millisecond,
    /// but we give it 2s on slower CI.
    #[test]
    fn child_exited_returns_true_after_natural_exit() -> Result<(), Box<dyn std::error::Error>> {
        let mut session = Session::spawn("true", &[], None, 24, 80)?;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if session.child_exited() {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(20));
        }
        panic!("child `true` did not exit within 2s");
    }

    /// `child_exited` is false while the child is still running.
    #[test]
    fn child_exited_returns_false_for_running_child() -> Result<(), Box<dyn std::error::Error>> {
        // `cat` with no input source blocks waiting on stdin (the PTY).
        let mut session = Session::spawn("cat", &[], None, 24, 80)?;
        assert!(
            !session.child_exited(),
            "freshly-spawned cat should still be running",
        );
        Ok(())
    }
}
