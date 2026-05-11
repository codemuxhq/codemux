//! Wire-protocol harness for the slow-tier daemon E2E tests
//! (`tests/proto_*.rs`).
//!
//! Spawns the real `codemuxd` binary as a subprocess via
//! `std::process::Command` (no PTY), points it at a per-test Unix
//! socket under a `tempfile::TempDir`, and exposes two surfaces tests
//! reach for:
//!
//! - [`spawn_codemuxd`] / [`spawn_codemuxd_with_agent`] — boot the
//!   daemon in foreground mode against a deterministic child binary.
//! - [`WireClient`] — connect to the daemon's socket, perform the
//!   `Hello` / `HelloAck` handshake, send typed frames, and poll
//!   decoded responses via [`WireClient::frames_eventually`].
//!
//! ## Determinism rules
//!
//! Mirrors the project-wide rule from
//! `docs/plans/2026-05-10--e2e-testing.md`:
//!
//! - **Never `sleep()` waiting for a frame.** The only allowed wait is
//!   [`WireClient::frames_eventually`]'s polling loop, which uses the
//!   socket's read timeout to wake every [`POLL_INTERVAL`] and re-check
//!   the predicate against the decoded-frame log. On timeout it panics
//!   with the frames seen so far so the failure message is the wire
//!   trace the test was expecting.
//! - **Drop reaps the child.** Leaving a `codemuxd` process behind
//!   between tests would have it cling to its tempdir-socket and
//!   confuse the next test. [`CodemuxdHandle::drop`] kills + waits the
//!   child; nothing inside `Drop` panics.
//! - **No `#[serial]`.** Each test gets its own `TempDir`, so socket
//!   paths never collide; tests run in parallel by default. (Mirrors
//!   the design decision in the T4 plan: the TUI tier needs `#[serial]`
//!   because PTY allocation races, the daemon tier does not.)

// Test helpers panic on setup failure; `expect("...")` gives the clearest
// possible failure message before any assertion runs. The workspace
// `clippy.toml` enables `allow-unwrap-in-tests` / `allow-expect-in-tests`,
// but those flags only cover `#[test]` / `#[cfg(test)]` scopes — free-
// floating helpers in an integration test crate fall outside the
// carve-out, so the allow stays at file scope here. Same pattern as
// `apps/tui/tests/common/mod.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

// SSH transport harness used by `proto_ssh_disconnect.rs` (AC-043).
// Lives in its own submodule so the wire-only proto tests don't pull
// `sshd` / `ssh-keygen` requirements when they don't need them; each
// proto test selects what it imports via `use common::ssh::...`.
pub mod ssh;

use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use codemux_wire::{self as wire, Message};
use tempfile::TempDir;

/// Polling cadence for the wire client's read + predicate-recheck loop.
/// Matches the TUI harness's 5 ms — short enough that a 1 s deadline
/// gets ~200 wake-ups, long enough that an idle CPU stays idle.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// How long to wait for the daemon's socket file to appear after spawn.
/// Generous because a cold-build `target/` (first test run after
/// `cargo clean`) can take seconds to start the binary on slower hosts;
/// this is one of the harness's two slow paths and gets the bigger
/// budget. The second is [`SHUTDOWN_TIMEOUT`].
const SOCKET_READY_TIMEOUT: Duration = Duration::from_secs(5);

/// Upper bound on `child.wait()` during `Drop`. We kill first, so this
/// is really "how long for SIGKILL to propagate"; one second is enough
/// on any platform the daemon supports, and Drop must not hang.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

/// Owns one `codemuxd` subprocess plus the per-test scratch directory
/// holding the socket. Tests interact via [`Self::connect`] (or
/// [`Self::socket_path`] for tests that need the raw path, e.g. AC-044's
/// kill-then-rebind dance).
///
/// All fields are private — the only ways out are the accessor methods.
pub struct CodemuxdHandle {
    /// Tempdir holding the socket (and, on real daemon runs, the pid /
    /// log files). Held so its `Drop` runs at teardown after the child
    /// is reaped.
    _dir: TempDir,
    socket: PathBuf,
    /// The daemon process. `Option` so `Drop` can move it out and
    /// `kill` + `wait` to reap.
    child: Option<Child>,
    /// Daemon's PID at spawn time. Cached because once `child` has been
    /// moved out in `Drop`, the pid is unrecoverable — and AC-043's
    /// process-group check reads `/proc/<pid>` while the daemon is
    /// still live.
    pid: u32,
}

impl CodemuxdHandle {
    /// Absolute path to the daemon's listening socket. Useful for tests
    /// that need to dispatch external `Command`s against the same path
    /// (AC-044) or assert on its existence as a liveness probe.
    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket
    }

    /// PID of the spawned daemon. Snapshot taken at spawn; remains valid
    /// for `/proc/<pid>` reads as long as the daemon is alive (which it
    /// is until `Drop` runs).
    #[must_use]
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Open a fresh connection to the daemon's socket and perform the
    /// `Hello` / `HelloAck` handshake. Returns a [`WireClient`] ready to
    /// send typed frames.
    ///
    /// `rows` / `cols` are the geometry the daemon will resize the
    /// master PTY to (and the basis for its snapshot encoding). Most
    /// tests pass `(24, 80)`; AC-027's snapshot test exercises the
    /// "client geometry differs from previous attach" path.
    ///
    /// # Panics
    ///
    /// Panics if the socket can't be reached within
    /// [`SOCKET_READY_TIMEOUT`] or if the handshake fails. Both are
    /// programmer / environment errors at this layer.
    pub fn connect(&self, rows: u16, cols: u16, agent_id: &str) -> WireClient {
        let stream = wait_for_unix_socket(&self.socket, SOCKET_READY_TIMEOUT);
        WireClient::handshake(stream, rows, cols, agent_id)
    }
}

impl Drop for CodemuxdHandle {
    fn drop(&mut self) {
        // NLM-flagged: use `.ok()` to consume the Result, not `let _ =`
        // (which trips `clippy::let_underscore_must_use` under our
        // `clippy::pedantic`-warn lint surface).
        if let Some(mut child) = self.child.take() {
            child.kill().ok();
            // Bounded wait so a wedged daemon doesn't hang test
            // teardown forever. `try_wait` polls without blocking; we
            // loop with a tiny sleep until SHUTDOWN_TIMEOUT.
            let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(POLL_INTERVAL);
                    }
                    _ => {
                        // Either we hit the deadline or try_wait erred;
                        // either way, give up — the OS will clean up
                        // the zombie when the test process exits.
                        break;
                    }
                }
            }
        }
        // `_dir` drops here implicitly, unlinking the socket and any
        // log file the daemon might have written.
    }
}

/// Spawn `codemuxd` in foreground mode pointing at the in-tree
/// [`fake_daemon_agent`] stub. Equivalent to
/// [`spawn_codemuxd_with_agent`] with the bundled fake.
///
/// Returns once the daemon's socket file exists on disk. The caller is
/// free to `connect()` immediately afterwards.
///
/// # Panics
///
/// Panics if the daemon binary can't be located, the subprocess can't
/// be spawned, or the socket doesn't appear within
/// [`SOCKET_READY_TIMEOUT`].
pub fn spawn_codemuxd() -> CodemuxdHandle {
    spawn_codemuxd_with_agent(env!("CARGO_BIN_EXE_fake_daemon_agent"))
}

/// Spawn `codemuxd` in foreground mode against an arbitrary child
/// binary. The binary will be exec'd inside the daemon's PTY (via
/// `clap`'s trailing `--` argv), so its boot output flows through
/// `vt100` into the daemon's mirrored screen.
///
/// `agent_bin` should be an absolute path. Callers in the same package
/// reach for `env!("CARGO_BIN_EXE_<name>")` so cargo materializes the
/// path at compile time.
pub fn spawn_codemuxd_with_agent(agent_bin: &str) -> CodemuxdHandle {
    let dir = TempDir::new().expect("create tempdir for daemon socket");
    let socket = dir.path().join("codemuxd.sock");

    let codemuxd_bin = env!("CARGO_BIN_EXE_codemuxd");

    let socket_str = socket
        .to_str()
        .expect("socket path is utf-8 (tempfile uses ascii-only suffixes)");

    let mut cmd = Command::new(codemuxd_bin);
    cmd.arg("--socket")
        .arg(socket_str)
        .arg("--foreground")
        .arg("--")
        .arg(agent_bin)
        // Discard stdio: foreground mode emits tracing to stderr, which
        // would pollute test output. The daemon does not need stdin for
        // anything (its `--socket` is the only input surface).
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let child = cmd.spawn().expect("spawn codemuxd subprocess");
    let pid = child.id();

    wait_for_socket_file(&socket, SOCKET_READY_TIMEOUT);

    CodemuxdHandle {
        _dir: dir,
        socket,
        child: Some(child),
        pid,
    }
}

/// Block until `path` exists, or panic on timeout.
///
/// The daemon's bind sequence is `bind → chmod 0600 → log "supervisor
/// bound"`; the socket file appears on the filesystem before the
/// accept loop runs, so this loop is the minimum wait that guarantees
/// a subsequent `connect()` will at worst block briefly (rather than
/// fail with `ENOENT`).
fn wait_for_socket_file(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    panic!(
        "daemon socket did not appear at {} within {timeout:?}",
        path.display(),
    );
}

/// Connect to a Unix socket with the same bounded-polling pattern as
/// [`wait_for_socket_file`]. We poll because `wait_for_socket_file`
/// races `bind` (file exists) against `listen` (kernel ready); a single
/// `connect()` between the two would return `ECONNREFUSED`. The TUI's
/// harness uses the same idiom.
fn wait_for_unix_socket(path: &Path, timeout: Duration) -> UnixStream {
    let deadline = Instant::now() + timeout;
    loop {
        match UnixStream::connect(path) {
            Ok(s) => return s,
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(POLL_INTERVAL);
            }
            Err(e) => panic!(
                "could not connect to {} within {timeout:?}: {e}",
                path.display(),
            ),
        }
    }
}

/// A connected, post-handshake wire client. Owns the `UnixStream` plus
/// a streaming decode buffer; tests send typed [`Message`]s and poll
/// the daemon's responses via [`Self::frames_eventually`].
///
/// Dropping the client closes the socket, which the daemon reads as a
/// clean detach (the session survives — that is, after all, the point
/// of the daemon).
pub struct WireClient {
    stream: UnixStream,
    /// Bytes read from the socket that haven't yet decoded into a full
    /// frame. Lives across [`Self::pump`] calls because TCP can deliver
    /// a frame fractionally.
    buf: Vec<u8>,
    /// Daemon PID reported in the `HelloAck`. Exposed via
    /// [`Self::daemon_pid`] for tests that want to assert on it.
    daemon_pid: u32,
}

impl WireClient {
    /// Perform the `Hello` / `HelloAck` handshake on `stream`. Returns
    /// a client ready for typed I/O.
    ///
    /// Exposed (rather than wrapped inside `CodemuxdHandle::connect`)
    /// so tests that need to drive a connection against a daemon they
    /// spawned manually — AC-044's redeploy test attaches across two
    /// `spawn_codemuxd_at` calls onto the same tempdir-scoped socket —
    /// can reuse the same wire-protocol logic.
    ///
    /// NLM-flagged: sets a non-zero read timeout on the underlying
    /// socket so [`Self::pump`]'s blocking read wakes every
    /// [`POLL_INTERVAL`], letting [`Self::frames_eventually`]'s
    /// deadline check actually fire.
    ///
    /// # Panics
    ///
    /// Panics on transport error, handshake-frame timeout, or
    /// `HelloAck` shape mismatch — all programmer / environment errors
    /// at this layer.
    pub fn handshake(mut stream: UnixStream, rows: u16, cols: u16, agent_id: &str) -> Self {
        stream
            .set_read_timeout(Some(POLL_INTERVAL))
            .expect("set_read_timeout on UnixStream");

        let hello = Message::Hello {
            protocol_version: wire::PROTOCOL_VERSION,
            rows,
            cols,
            agent_id: agent_id.to_string(),
        };
        let hello_bytes = hello.encode().expect("encode Hello");
        stream.write_all(&hello_bytes).expect("write Hello");
        stream.flush().expect("flush Hello");

        let mut buf = Vec::with_capacity(256);
        let ack = read_one_frame(&mut stream, &mut buf, Duration::from_secs(2))
            .expect("read HelloAck within 2s");
        let Message::HelloAck { daemon_pid, .. } = ack else {
            panic!("expected HelloAck, got {ack:?}");
        };

        Self {
            stream,
            buf,
            daemon_pid,
        }
    }

    /// PID the daemon reported in its `HelloAck`.
    #[must_use]
    pub fn daemon_pid(&self) -> u32 {
        self.daemon_pid
    }

    /// Encode `msg` and write the framed bytes to the socket. Flush
    /// afterwards so the daemon doesn't sit on a partial frame waiting
    /// for the kernel to coalesce a sibling write that may never come.
    ///
    /// # Panics
    ///
    /// Panics on encode or transport error.
    pub fn send(&mut self, msg: &Message) {
        let bytes = msg.encode().expect("encode Message");
        self.stream.write_all(&bytes).expect("write to socket");
        self.stream.flush().expect("flush socket");
    }

    /// Pump bytes from the socket once and append any complete decoded
    /// frames onto `out`. Returns `Ok(true)` if the channel is still
    /// live (more reads may yield bytes), `Ok(false)` on clean EOF.
    ///
    /// `WouldBlock` / `TimedOut` are treated as "no bytes right now,
    /// but the channel is live" — that's the wake-up that lets
    /// [`Self::frames_eventually`]'s deadline tick.
    fn pump(&mut self, out: &mut Vec<Message>) -> std::io::Result<bool> {
        let mut tmp = [0u8; 4096];
        match self.stream.read(&mut tmp) {
            Ok(0) => Ok(false),
            Ok(n) => {
                self.buf.extend_from_slice(&tmp[..n]);
                self.drain_decoded(out);
                Ok(true)
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                // Drain anyway — earlier reads may have left a complete
                // frame in `buf` that we couldn't decode yet because the
                // last pump returned partway through.
                self.drain_decoded(out);
                Ok(true)
            }
            Err(e) => Err(e),
        }
    }

    /// Pop every complete frame currently buffered onto `out`. Bytes
    /// from a partial trailing frame stay in `self.buf` for the next
    /// pump. A decode error closes the door — we don't try to resync
    /// mid-stream, same convention as the daemon.
    fn drain_decoded(&mut self, out: &mut Vec<Message>) {
        loop {
            match wire::try_decode(&self.buf) {
                Ok(Some((msg, consumed))) => {
                    self.buf.drain(..consumed);
                    out.push(msg);
                }
                Ok(None) => return,
                Err(e) => panic!("wire decode error: {e}"),
            }
        }
    }

    /// Poll the wire stream until `predicate(&frames)` returns true OR
    /// the deadline expires.
    ///
    /// `frames` is the running list of every decoded frame seen on this
    /// client; predicates can match on the latest frame, count
    /// occurrences, or scan for a marker across the whole trace.
    ///
    /// On success: returns the full frame list.
    /// On timeout: panics with the frame trace so the failure message
    /// shows what the daemon actually sent.
    ///
    /// # Panics
    ///
    /// Panics on timeout (intentional). Panics on transport error.
    pub fn frames_eventually<P>(&mut self, predicate: P, timeout: Duration) -> Vec<Message>
    where
        P: Fn(&[Message]) -> bool,
    {
        let mut frames = Vec::new();
        let deadline = Instant::now() + timeout;
        loop {
            let live = self.pump(&mut frames).expect("pump frames from socket");
            if predicate(&frames) {
                return frames;
            }
            assert!(
                live && Instant::now() < deadline,
                "frames_eventually: predicate did not hold within {timeout:?}\n\
                 ----- frame trace ({} frames) -----\n\
                 {}\n\
                 ----- end -----",
                frames.len(),
                summarize_frames(&frames),
            );
            // No explicit sleep here — `pump` itself blocks up to
            // POLL_INTERVAL on the socket read timeout, which is the
            // single allowed backoff on this path.
        }
    }
}

/// Convenience: drain `client` until at least one `PtyData` frame
/// arrives, then return the concatenated payload of every `PtyData`
/// seen so far. Non-`PtyData` frames are silently dropped.
///
/// Used by AC-027's snapshot test where the assertion is "the first
/// `PtyData` after handshake contains the marker from the previous
/// session." The predicate matches on payload bytes, so collapsing the
/// frame stream into a single byte vec is the natural shape.
pub fn collect_pty_data_until<P>(
    client: &mut WireClient,
    predicate: P,
    timeout: Duration,
) -> Vec<u8>
where
    P: Fn(&[u8]) -> bool,
{
    let mut acc = Vec::new();
    let deadline = Instant::now() + timeout;
    let mut frames = Vec::new();
    loop {
        let live = client.pump(&mut frames).expect("pump frames");
        // Drain newly-arrived frames in order so the assembled `acc`
        // reflects the daemon's wire ordering. `Vec::drain(..)` consumes
        // front-to-back; `pop` would reverse the byte stream.
        for msg in frames.drain(..) {
            if let Message::PtyData(bytes) = msg {
                acc.extend_from_slice(&bytes);
            }
        }
        if predicate(&acc) {
            return acc;
        }
        if !live || Instant::now() >= deadline {
            return acc;
        }
    }
}

/// Single-shot helper: read frames from a raw `UnixStream` until one
/// decodes, or the timeout fires. Used during the handshake before the
/// `WireClient` is constructed.
fn read_one_frame(
    stream: &mut UnixStream,
    buf: &mut Vec<u8>,
    timeout: Duration,
) -> std::io::Result<Message> {
    stream.set_read_timeout(Some(POLL_INTERVAL))?;
    let deadline = Instant::now() + timeout;
    let mut tmp = [0u8; 1024];
    loop {
        match wire::try_decode(buf) {
            Ok(Some((msg, consumed))) => {
                buf.drain(..consumed);
                return Ok(msg);
            }
            Ok(None) => {}
            Err(e) => {
                return Err(std::io::Error::other(format!("wire decode error: {e}")));
            }
        }
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(
                ErrorKind::TimedOut,
                format!("no complete frame within {timeout:?}"),
            ));
        }
        match stream.read(&mut tmp) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    ErrorKind::UnexpectedEof,
                    "socket closed before frame",
                ));
            }
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {}
            Err(e) => return Err(e),
        }
    }
}

/// Render a frame list for the timeout panic message. Each frame is one
/// line with the tag name and a short payload summary; long `PtyData`
/// payloads are truncated to the first 64 bytes so the panic stays
/// readable.
fn summarize_frames(frames: &[Message]) -> String {
    let mut s = String::new();
    for (i, frame) in frames.iter().enumerate() {
        use std::fmt::Write as _;
        match frame {
            Message::PtyData(bytes) => {
                let preview_len = bytes.len().min(64);
                let preview = String::from_utf8_lossy(&bytes[..preview_len]);
                let _ = writeln!(
                    s,
                    "  [{i}] PtyData ({} bytes): {preview:?}{}",
                    bytes.len(),
                    if bytes.len() > preview_len {
                        " …"
                    } else {
                        ""
                    },
                );
            }
            other => {
                let _ = writeln!(s, "  [{i}] {other:?}");
            }
        }
    }
    if s.is_empty() {
        s.push_str("  (no frames received)\n");
    }
    s
}
