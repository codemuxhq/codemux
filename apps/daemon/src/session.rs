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

use std::io::{ErrorKind, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, Mutex};

use codemux_wire::Message;
use crossbeam_channel::Receiver;
use portable_pty::{Child, MasterPty, PtySize};
use vt100::Parser;

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
    // Daemon-side mirror of the child's screen, fed by the PTY reader
    // thread (see `pty::spawn_reader_thread`). Persists across attaches
    // so a reattaching client can be served the current screen as its
    // first PtyData frame, instead of waiting for the (often-idle)
    // child to redraw on its own. On respawn after `child_exited`, the
    // supervisor drops this Session and the new one gets a fresh
    // parser — no carryover from the dead session.
    screen: Arc<Mutex<Parser>>,
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
            screen,
        } = pty::spawn(command, args, cwd, rows, cols)?;
        Ok(Self {
            child,
            writer,
            rx,
            master,
            screen,
        })
    }

    /// Attach `stream` to this session's PTY. Returns when the client
    /// disconnects, the child exits, or an I/O error occurs. **The PTY
    /// child survives** — call again with a fresh stream to re-attach.
    ///
    /// The attach lifecycle splits cleanly across two responsibilities:
    /// `Session` owns the *domain* (handshake outcome, parser state,
    /// snapshot encoding); `conn` owns the *transport* (inbound /
    /// outbound loops over the socket). Keeping the snapshot encoding
    /// here means `conn` never needs to know about `vt100` or the
    /// `?1049h` alt-screen toggle — it just gets opaque bytes to write.
    pub fn attach(&mut self, stream: UnixStream) -> Result<(), Error> {
        let mut handshake_buf = Vec::with_capacity(256);
        let hello = conn::perform_handshake(&stream, &mut handshake_buf).inspect_err(|_| {
            shutdown_best_effort(&stream, "handshake-failure shutdown failed");
        })?;
        tracing::info!(
            protocol_version = hello.protocol_version,
            rows = hello.rows,
            cols = hello.cols,
            agent_id = %hello.agent_id,
            "handshake complete",
        );

        // Resize the master PTY to match the client's geometry. This
        // sits OUTSIDE the parser lock — `master.resize` is a
        // `TIOCSWINSZ` ioctl that may deliver `SIGWINCH` to the child
        // and unblock its next read; doing it under the parser lock
        // would needlessly stall the reader thread on the resize
        // syscall. Best-effort: a failed resize means the child sees
        // a stale size until the next resize, a harmless cosmetic
        // glitch.
        if let Err(e) = self.master.resize(PtySize {
            rows: hello.rows,
            cols: hello.cols,
            pixel_width: 0,
            pixel_height: 0,
        }) {
            tracing::warn!("initial resize from Hello geometry failed: {e}");
        }

        let snapshot = self.take_snapshot(hello.rows, hello.cols);
        let frame = Message::PtyData(snapshot).encode().inspect_err(|_| {
            shutdown_best_effort(&stream, "snapshot-encode shutdown failed");
        })?;
        let mut writer_ref = &stream;
        if let Err(e) = writer_ref.write_all(&frame) {
            tracing::debug!("snapshot frame write failed: {e}");
            shutdown_best_effort(&stream, "post-snapshot-failure shutdown failed");
            return Ok(());
        }
        if let Err(e) = writer_ref.flush() {
            tracing::debug!("snapshot frame flush failed: {e}");
        }

        conn::run_io_loops(
            stream,
            handshake_buf,
            &mut *self.writer,
            &self.rx,
            &mut *self.master,
            &mut *self.child,
        )
    }

    /// Build the snapshot payload and prepare the parser+rx for the new
    /// attach atomically.
    ///
    /// Held under a single parser lock so that the reader thread (which
    /// feeds the same parser atomically with `rx` sends, see
    /// `pty::spawn_reader_thread`) can't slip a chunk in between the
    /// steps below. The atomicity invariant we rely on: any chunk in
    /// `rx` is also in the parser, and vice versa. Drain + snapshot is
    /// therefore lossless and dedup-free.
    ///
    /// Steps under lock:
    /// 1. Resize the parser to the client's geometry. The master is
    ///    already resized (see `attach`); both must agree or
    ///    `state_formatted` would encode for the wrong grid and the
    ///    client would see a wrapped/clipped replay.
    /// 2. Drain `rx` of anything queued while the previous client was
    ///    disconnected — those bytes are already in the parser via the
    ///    reader thread, so the snapshot covers them. Forwarding them
    ///    again would double-paint.
    /// 3. Encode the snapshot bytes (alt-screen prefix + state).
    ///
    /// The `?1049h` prefix is required when the child is in xterm
    /// alt-screen mode. `Screen::state_formatted` writes the contents
    /// of the *active* screen but does NOT toggle which screen the
    /// receiver should be on — without the prefix the client would
    /// clear and paint the alt content into its primary buffer,
    /// displaying the wrong half on every reattach to a session that
    /// uses alt-screen (Claude, vim, less, etc.). We deliberately do
    /// NOT emit `?1049l` for primary-mode sessions: the client parser
    /// starts in primary, so a no-op toggle just wastes bytes.
    fn take_snapshot(&self, rows: u16, cols: u16) -> Vec<u8> {
        let mut parser = self
            .screen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        parser.screen_mut().set_size(rows, cols);
        while self.rx.try_recv().is_ok() {}
        let screen = parser.screen();
        let mut buf = Vec::new();
        if screen.alternate_screen() {
            buf.extend_from_slice(b"\x1b[?1049h");
        }
        buf.extend_from_slice(&screen.state_formatted());
        buf
    }

    /// True if the child has exited. Best-effort: an I/O error from
    /// `try_wait` is treated as "still running" (the supervisor will
    /// recheck on the next accept).
    #[must_use]
    pub fn child_exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }
}

/// Best-effort socket shutdown shared by every attach-failure branch.
/// `NotConnected` is expected when the peer already closed; everything
/// else logs at debug so a stuck-attach investigation has a breadcrumb
/// without polluting normal logs.
fn shutdown_best_effort(stream: &UnixStream, context: &'static str) {
    if let Err(e) = stream.shutdown(Shutdown::Both)
        && e.kind() != ErrorKind::NotConnected
    {
        tracing::debug!("{context}: {e}");
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
#[allow(clippy::unwrap_used)]
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

    /// Primary-screen snapshot must NOT include the `?1049h` toggle —
    /// the client parser starts in primary mode, so a no-op switch is
    /// just byte waste. The body is `state_formatted`, which begins
    /// with `\x1b[H\x1b[J` (cursor home + clear).
    #[test]
    fn take_snapshot_primary_screen_omits_alt_screen_toggle() {
        let session = Session::spawn("cat", &[], None, 24, 80).unwrap();
        // Drive the parser directly so we have a known screen state
        // without needing the cat child to actually echo anything (the
        // PTY reader race is the supervisor tests' problem, not ours).
        session.screen.lock().unwrap().process(b"primary-content");
        let frame = session.take_snapshot(24, 80);
        assert!(
            !frame.starts_with(b"\x1b[?1049h"),
            "primary-mode snapshot should not toggle alt-screen, got prefix {:?}",
            &frame[..frame.len().min(16)],
        );
        assert!(
            frame
                .windows(b"primary-content".len())
                .any(|w| w == b"primary-content"),
            "snapshot body should contain `primary-content`, got {:?}",
            String::from_utf8_lossy(&frame),
        );
    }

    /// Alt-screen snapshot MUST emit `?1049h` first so the client
    /// parser switches buffers before consuming the contents. Without
    /// the prefix, the alt content would be applied to the client's
    /// primary screen — wrong half of the buffer, persists when the
    /// child later toggles back to primary.
    #[test]
    fn take_snapshot_alt_screen_includes_alt_screen_toggle() {
        let session = Session::spawn("cat", &[], None, 24, 80).unwrap();
        // `\x1b[?1049h` enters alt-screen + saves cursor + clears;
        // subsequent text lands in the alt buffer.
        session
            .screen
            .lock()
            .unwrap()
            .process(b"\x1b[?1049halt-screen-content");
        assert!(
            session.screen.lock().unwrap().screen().alternate_screen(),
            "test setup: parser should be in alt-screen mode after `?1049h`",
        );
        let frame = session.take_snapshot(24, 80);
        assert!(
            frame.starts_with(b"\x1b[?1049h"),
            "alt-mode snapshot must lead with `?1049h`, got prefix {:?}",
            &frame[..frame.len().min(16)],
        );
        assert!(
            frame
                .windows(b"alt-screen-content".len())
                .any(|w| w == b"alt-screen-content"),
            "snapshot body should contain alt-screen content, got {:?}",
            String::from_utf8_lossy(&frame),
        );
    }
}
