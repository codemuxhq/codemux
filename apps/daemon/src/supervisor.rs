//! Supervisor: owns the unix-socket listener and runs the accept loop.
//!
//! The supervisor's single responsibility is the runtime: accept a
//! client, attach it to the (lazily-spawned) [`Session`], handle a
//! disconnect, repeat. It owns nothing it didn't get handed by the
//! [`bootstrap`] module — [`Supervisor::new`] takes a
//! [`DaemonResources`] of already-bound listener, validated config, and
//! pid file guard.
//!
//! Sessions persist across client disconnects — the whole point of the
//! daemon is session continuity. If the child has exited between
//! attaches, the next accept respawns. Single-attach is implicit (the
//! accept loop is sequential, so a second client blocks in `connect`
//! until the first ends); a future stage may replace this with a
//! protocol-level `AlreadyAttached` rejection.
//!
//! Drop on the held [`Session`] kills the child; drop on the held
//! [`PidFile`] (inside [`DaemonResources`]) removes the pid file. Both
//! happen on a graceful supervisor shutdown.
//!
//! [`bootstrap`]: crate::bootstrap
//! [`DaemonResources`]: crate::bootstrap::DaemonResources
//! [`PidFile`]: crate::bootstrap::PidFile

use std::convert::Infallible;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;

use crate::bootstrap::{DaemonResources, PidFile};
use crate::cli::Cli;
use crate::error::Error;
use crate::session::Session;

/// What the supervisor needs to know about the child it spawns per
/// session. Built from a [`Cli`] in production, or constructed
/// directly by tests.
///
/// Pure runtime concern: paths the supervisor doesn't read at runtime
/// (pid file, socket) live in [`DaemonResources`] instead.
///
/// [`DaemonResources`]: crate::bootstrap::DaemonResources
#[derive(Clone, Debug)]
pub struct SupervisorConfig {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub rows: u16,
    pub cols: u16,
}

impl SupervisorConfig {
    #[must_use]
    pub fn from_cli(cli: &Cli) -> Self {
        let (command, args) = cli.child_command();
        Self {
            command,
            args,
            cwd: cli.cwd.clone(),
            rows: cli.rows,
            cols: cli.cols,
        }
    }
}

pub struct Supervisor {
    listener: UnixListener,
    config: SupervisorConfig,
    session: Option<Session>,
    /// Held purely for its `Drop` — removes the pid file on daemon
    /// exit. Underscore prefix tells the linter we never read this;
    /// it's a liveness guard, not a value.
    _pid_file: Option<PidFile>,
}

impl Supervisor {
    /// Construct from already-prepared [`DaemonResources`]. Does no
    /// I/O. Pair with [`bootstrap::bring_up`] in production or
    /// [`bootstrap::bring_up_with`] in tests.
    ///
    /// [`bootstrap::bring_up`]: crate::bootstrap::bring_up
    /// [`bootstrap::bring_up_with`]: crate::bootstrap::bring_up_with
    #[must_use]
    pub fn new(resources: DaemonResources) -> Self {
        Self {
            listener: resources.listener,
            config: resources.config,
            session: None,
            _pid_file: resources.pid_file,
        }
    }

    /// Accept-loop forever. Each accepted connection runs to completion
    /// before the next is accepted; the underlying session persists. The
    /// `Infallible` Ok type encodes the operational invariant: this
    /// function only ever returns on error.
    pub fn serve(&mut self) -> Result<Infallible, Error> {
        loop {
            self.serve_one()?;
        }
    }

    /// Accept one connection and attach it to the (lazily-spawned)
    /// session. Returns when the conn ends. The session keeps running.
    pub fn serve_one(&mut self) -> Result<(), Error> {
        let (stream, _addr) = self
            .listener
            .accept()
            .map_err(|source| Error::Accept { source })?;
        tracing::info!("client attached");

        let session = self.session_mut()?;
        let result = session.attach(stream);

        tracing::info!("client detached");
        result
    }

    /// Return a mutable reference to a live session, spawning one (or
    /// replacing a dead one) if necessary. `Option::take` releases the
    /// borrow on `self.session` so we can either re-`insert` the same
    /// value (live) or replace it with a freshly-spawned one (dead).
    /// Without `take`, NLL still extends the original mutable borrow
    /// across the spawn branch.
    fn session_mut(&mut self) -> Result<&mut Session, Error> {
        if let Some(mut existing) = self.session.take() {
            if !existing.child_exited() {
                return Ok(self.session.insert(existing));
            }
            tracing::info!("previous session ended; spawning fresh session");
        }
        let new_session = Session::spawn(
            &self.config.command,
            &self.config.args,
            self.config.cwd.as_deref(),
            self.config.rows,
            self.config.cols,
        )?;
        Ok(self.session.insert(new_session))
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::thread;
    use std::time::{Duration, Instant};

    use codemux_wire::{self as wire, ErrorCode, Message};

    use super::*;
    use crate::bootstrap;

    fn cat_config() -> SupervisorConfig {
        SupervisorConfig {
            command: "cat".to_string(),
            args: Vec::new(),
            cwd: None,
            rows: 24,
            cols: 80,
        }
    }

    fn supervisor_for(socket: &Path) -> Result<Supervisor, Box<dyn std::error::Error>> {
        let resources = bootstrap::bring_up_with(socket, None, cat_config())?;
        Ok(Supervisor::new(resources))
    }

    /// End-to-end smoke for the wire protocol: handshake completes,
    /// then a typed `PtyData` frame echoes back through the cat child
    /// as `PtyData` frames. We use the "appears twice" trick (PTY echo
    /// + cat reply) on the accumulated `PtyData` payloads.
    #[test]
    fn echo_through_cat_pty() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("test.sock");
        let mut supervisor = supervisor_for(&socket)?;

        let serve = thread::spawn(move || supervisor.serve_one());

        let mut stream = wait_for_unix_socket(&socket, Duration::from_secs(2))?;
        stream.set_read_timeout(Some(Duration::from_secs(1)))?;

        let _daemon_pid = client_handshake(&mut stream, 24, 80, "test-agent")?;
        send_pty_data(&mut stream, b"hello\n")?;

        let got = drain_pty_data_until(&mut stream, b"hello", 2, Duration::from_millis(1500));
        let count = count_occurrences(&got, b"hello");
        assert!(
            count >= 2,
            "expected 'hello' to appear at least twice in PtyData payloads \
             (PTY echo + cat reply); got {count} occurrences in {got:?}",
        );

        drop(stream);

        let join_result = serve.join();
        let Ok(serve_result) = join_result else {
            panic!("serve thread panicked");
        };
        serve_result?;
        Ok(())
    }

    /// **The reason this daemon exists.** Handshake, write, disconnect.
    /// Reattach (new handshake), write again. The second `PtyData`
    /// reaching `cat` and being echoed back proves the PTY child
    /// survived the first disconnect — session continuity at the
    /// supervisor level, exercising the wire protocol re-handshake
    /// path.
    #[test]
    fn session_survives_client_disconnect_and_reattach() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("survive.sock");
        let mut supervisor = supervisor_for(&socket)?;

        let serve = thread::spawn(move || -> Result<(), Error> {
            supervisor.serve_one()?;
            supervisor.serve_one()?;
            Ok(())
        });

        {
            let mut s1 = wait_for_unix_socket(&socket, Duration::from_secs(2))?;
            s1.set_read_timeout(Some(Duration::from_secs(1)))?;
            client_handshake(&mut s1, 24, 80, "test-agent")?;
            send_pty_data(&mut s1, b"first\n")?;
            let got = drain_pty_data_until(&mut s1, b"first", 2, Duration::from_millis(1500));
            let count = count_occurrences(&got, b"first");
            assert!(
                count >= 2,
                "first attach: expected 'first' twice (echo + cat reply); \
                 got {count} occurrences in {got:?}",
            );
        }

        {
            let mut s2 = wait_for_unix_socket(&socket, Duration::from_secs(2))?;
            s2.set_read_timeout(Some(Duration::from_secs(1)))?;
            client_handshake(&mut s2, 24, 80, "test-agent")?;
            send_pty_data(&mut s2, b"second\n")?;
            let got = drain_pty_data_until(&mut s2, b"second", 2, Duration::from_millis(1500));
            let count = count_occurrences(&got, b"second");
            assert!(
                count >= 2,
                "second attach: expected 'second' twice (proves session survived \
                 the first disconnect); got {count} occurrences in {got:?}",
            );
        }

        let join_result = serve.join();
        let Ok(serve_result) = join_result else {
            panic!("serve thread panicked");
        };
        serve_result?;
        Ok(())
    }

    /// A client that announces an unsupported protocol version gets an
    /// `Error{VersionMismatch}` frame from the daemon, and the
    /// supervisor's `serve_one` returns `Error::VersionMismatch` rather
    /// than treating it as a transport failure.
    #[test]
    fn handshake_version_mismatch_returns_error_frame() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("vermismatch.sock");
        let mut supervisor = supervisor_for(&socket)?;

        let serve = thread::spawn(move || supervisor.serve_one());

        let mut stream = wait_for_unix_socket(&socket, Duration::from_secs(2))?;
        stream.set_read_timeout(Some(Duration::from_secs(1)))?;

        let bogus = wire::PROTOCOL_VERSION.wrapping_add(99);
        let hello = Message::Hello {
            protocol_version: bogus,
            rows: 24,
            cols: 80,
            agent_id: String::new(),
        };
        stream.write_all(&hello.encode()?)?;

        let frame = read_one_frame(&mut stream, Duration::from_secs(2))?
            .ok_or("daemon closed without sending an Error frame")?;
        let Message::Error { code, .. } = frame else {
            panic!("expected Error frame, got {frame:?}");
        };
        assert_eq!(code, ErrorCode::VersionMismatch);

        let join_result = serve.join();
        let Ok(serve_result) = join_result else {
            panic!("serve thread panicked");
        };
        let Err(err) = serve_result else {
            panic!("serve_one should have errored on version mismatch");
        };
        assert!(
            matches!(err, Error::VersionMismatch { .. }),
            "expected Error::VersionMismatch, got {err:?}",
        );
        Ok(())
    }

    /// A client whose first frame is something other than `Hello`
    /// triggers `Error::HandshakeMissing` carrying the offending tag,
    /// and the supervisor surfaces it from `serve_one`.
    #[test]
    fn handshake_with_non_hello_first_frame_returns_handshake_missing()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("nonhello.sock");
        let mut supervisor = supervisor_for(&socket)?;

        let serve = thread::spawn(move || supervisor.serve_one());

        let mut stream = wait_for_unix_socket(&socket, Duration::from_secs(2))?;
        let bogus = Message::PtyData(b"i'm not Hello".to_vec()).encode()?;
        stream.write_all(&bogus)?;
        drop(stream);

        let join_result = serve.join();
        let Ok(serve_result) = join_result else {
            panic!("serve thread panicked");
        };
        let Err(err) = serve_result else {
            panic!("serve_one should have errored on missing Hello");
        };
        let Error::HandshakeMissing { got_tag } = err else {
            panic!("expected HandshakeMissing, got {err:?}");
        };
        assert_eq!(got_tag, 0x10, "tag should be PtyData (0x10)");
        Ok(())
    }

    /// A client that disconnects before sending a complete `Hello`
    /// triggers `Error::HandshakeIncomplete` rather than a generic
    /// transport error.
    #[test]
    fn handshake_with_eof_before_hello_returns_handshake_incomplete()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("eof.sock");
        let mut supervisor = supervisor_for(&socket)?;

        let serve = thread::spawn(move || supervisor.serve_one());

        let stream = wait_for_unix_socket(&socket, Duration::from_secs(2))?;
        drop(stream);

        let join_result = serve.join();
        let Ok(serve_result) = join_result else {
            panic!("serve thread panicked");
        };
        let Err(err) = serve_result else {
            panic!("serve_one should have errored on EOF before Hello");
        };
        assert!(
            matches!(err, Error::HandshakeIncomplete),
            "expected HandshakeIncomplete, got {err:?}",
        );
        Ok(())
    }

    /// Placeholder dispatch handlers (Resize/Signal/Ping/Pong) must not
    /// close the connection — the next `PtyData` frame still flows.
    #[test]
    fn inbound_placeholder_frames_do_not_close_connection() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("placeholders.sock");
        let mut supervisor = supervisor_for(&socket)?;

        let serve = thread::spawn(move || supervisor.serve_one());

        let mut stream = wait_for_unix_socket(&socket, Duration::from_secs(2))?;
        stream.set_read_timeout(Some(Duration::from_secs(1)))?;

        client_handshake(&mut stream, 24, 80, "test-agent")?;

        for frame in [
            Message::Resize {
                rows: 50,
                cols: 132,
            },
            Message::Signal(wire::Signal::Int),
            Message::Ping { nonce: 0xDEAD_BEEF },
            Message::Pong { nonce: 0xCAFE_BABE },
        ] {
            stream.write_all(&frame.encode()?)?;
        }
        send_pty_data(&mut stream, b"after-placeholders\n")?;

        let got = drain_pty_data_until(
            &mut stream,
            b"after-placeholders",
            2,
            Duration::from_millis(1500),
        );
        let count = count_occurrences(&got, b"after-placeholders");
        assert!(
            count >= 2,
            "connection must survive placeholder frames; got {count} occurrences in {got:?}",
        );

        drop(stream);

        let join_result = serve.join();
        let Ok(serve_result) = join_result else {
            panic!("serve thread panicked");
        };
        serve_result?;
        Ok(())
    }

    /// A client sending a server-only frame (`Hello`/`HelloAck`/
    /// `ChildExited`) post-handshake is a protocol violation — the
    /// daemon closes the connection cleanly without erroring
    /// `serve_one`.
    #[test]
    fn inbound_server_only_frame_closes_connection_cleanly()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("server-only.sock");
        let mut supervisor = supervisor_for(&socket)?;

        let serve = thread::spawn(move || supervisor.serve_one());

        let mut stream = wait_for_unix_socket(&socket, Duration::from_secs(2))?;
        stream.set_read_timeout(Some(Duration::from_secs(1)))?;

        client_handshake(&mut stream, 24, 80, "test-agent")?;

        let bogus = Message::Hello {
            protocol_version: wire::PROTOCOL_VERSION,
            rows: 24,
            cols: 80,
            agent_id: "agent".to_string(),
        }
        .encode()?;
        stream.write_all(&bogus)?;

        await_clean_close(
            &mut stream,
            Duration::from_secs(2),
            "daemon did not close conn after server-only inbound frame",
        );

        let join_result = serve.join();
        let Ok(serve_result) = join_result else {
            panic!("serve thread panicked");
        };
        serve_result?;
        Ok(())
    }

    /// A client sending an `Error` frame causes the daemon to close
    /// the connection cleanly.
    #[test]
    fn inbound_error_frame_from_client_closes_connection_cleanly()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("client-error.sock");
        let mut supervisor = supervisor_for(&socket)?;

        let serve = thread::spawn(move || supervisor.serve_one());

        let mut stream = wait_for_unix_socket(&socket, Duration::from_secs(2))?;
        stream.set_read_timeout(Some(Duration::from_secs(1)))?;

        client_handshake(&mut stream, 24, 80, "test-agent")?;

        let err_frame = Message::Error {
            code: ErrorCode::Internal,
            message: "client gave up".to_string(),
        }
        .encode()?;
        stream.write_all(&err_frame)?;

        await_clean_close(
            &mut stream,
            Duration::from_secs(2),
            "daemon did not close conn after client Error frame",
        );

        let join_result = serve.join();
        let Ok(serve_result) = join_result else {
            panic!("serve thread panicked");
        };
        serve_result?;
        Ok(())
    }

    fn wait_for_unix_socket(
        path: &Path,
        timeout: Duration,
    ) -> Result<UnixStream, Box<dyn std::error::Error>> {
        let deadline = Instant::now() + timeout;
        loop {
            match UnixStream::connect(path) {
                Ok(s) => return Ok(s),
                Err(_) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(e) => return Err(Box::new(e)),
            }
        }
    }

    /// Send a `Hello` frame and read back a `HelloAck`. Returns the
    /// daemon's reported pid for any tests that want to assert on it.
    fn client_handshake(
        stream: &mut UnixStream,
        rows: u16,
        cols: u16,
        agent_id: &str,
    ) -> Result<u32, Box<dyn std::error::Error>> {
        let hello = Message::Hello {
            protocol_version: wire::PROTOCOL_VERSION,
            rows,
            cols,
            agent_id: agent_id.to_string(),
        };
        stream.write_all(&hello.encode()?)?;
        let frame = read_one_frame(stream, Duration::from_secs(2))?
            .ok_or("daemon closed before HelloAck")?;
        let Message::HelloAck { daemon_pid, .. } = frame else {
            return Err(format!("expected HelloAck, got {frame:?}").into());
        };
        Ok(daemon_pid)
    }

    fn send_pty_data(
        stream: &mut UnixStream,
        bytes: &[u8],
    ) -> Result<(), Box<dyn std::error::Error>> {
        let frame = Message::PtyData(bytes.to_vec()).encode()?;
        stream.write_all(&frame)?;
        stream.flush()?;
        Ok(())
    }

    /// Read until exactly one complete frame is available; return
    /// `Ok(None)` only on clean EOF before any frame appears.
    fn read_one_frame(
        stream: &mut UnixStream,
        timeout: Duration,
    ) -> Result<Option<Message>, Box<dyn std::error::Error>> {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 256];
        let deadline = Instant::now() + timeout;
        loop {
            if let Some((msg, _consumed)) = wire::try_decode(&buf)? {
                return Ok(Some(msg));
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "timed out waiting for frame; buffered {} bytes: {buf:?}",
                    buf.len(),
                )
                .into());
            }
            match stream.read(&mut tmp) {
                Ok(0) => return Ok(None),
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => return Err(Box::new(e)),
            }
        }
    }

    /// Read frames from `stream`, accumulate each `PtyData` payload,
    /// and stop once `needle` appears `target` times in the
    /// accumulated bytes (or the timeout / EOF hits). Non-`PtyData`
    /// frames are silently drained.
    fn drain_pty_data_until(
        stream: &mut UnixStream,
        needle: &[u8],
        target: usize,
        timeout: Duration,
    ) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut accumulated = Vec::new();
        let mut tmp = [0u8; 1024];
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            loop {
                match wire::try_decode(&buf) {
                    Ok(Some((Message::PtyData(bytes), consumed))) => {
                        buf.drain(..consumed);
                        accumulated.extend_from_slice(&bytes);
                    }
                    Ok(Some((_other, consumed))) => {
                        buf.drain(..consumed);
                    }
                    Ok(None) => break,
                    Err(_) => return accumulated,
                }
            }
            if count_occurrences(&accumulated, needle) >= target {
                break;
            }
            match stream.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    break;
                }
                Err(_) => break,
            }
        }
        accumulated
    }

    fn count_occurrences(haystack: &[u8], needle: &[u8]) -> usize {
        if needle.is_empty() || haystack.len() < needle.len() {
            return 0;
        }
        haystack
            .windows(needle.len())
            .filter(|w| *w == needle)
            .count()
    }

    /// Block until the daemon closes the stream (`read` returns 0) or
    /// the deadline elapses. Discards any frames the daemon sends
    /// meanwhile — used by tests that only care that the close happens.
    fn await_clean_close(stream: &mut UnixStream, timeout: Duration, msg: &'static str) {
        let mut sink = [0u8; 256];
        let deadline = Instant::now() + timeout;
        loop {
            assert!(Instant::now() < deadline, "{msg}");
            match stream.read(&mut sink) {
                Ok(0) => return,
                Ok(_) => {}
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(_) => return,
            }
        }
    }
}
