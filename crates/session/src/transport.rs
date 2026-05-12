//! `AgentTransport` — the seam between the runtime and the per-agent PTY.
//!
//! The runtime renders bytes and forwards keystrokes; whether those bytes
//! come from a child spawned in a *local* PTY or tunneled from a
//! *remote* `codemuxd` is the transport's concern, not the runtime's.
//! Stage 3 introduced this enum and ported the local variant; Stage 4
//! lights up the SSH variant in place.
//!
//! ## Why an enum and not a trait
//!
//! Every transport has the same six operations
//! (`try_read`/`write`/`resize`/`signal`/`try_wait`/`kill`) and there
//! will only ever be a small, closed set of variants (`Local`,
//! `SshDaemon`, and conceivably one or two others). A
//! `Box<dyn Transport>` would buy us nothing and cost us a vtable
//! indirection on every PTY byte. The enum keeps the dispatch local,
//! exhaustive, and inlinable.
//!
//! The enum is `#[non_exhaustive]` so external callers must add a
//! catch-all arm when matching.

use std::ffi::OsStr;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Child as ProcessChild;
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::Duration;

use codemux_wire::{self as wire, Message, Signal};
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded, unbounded};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::error::Error;

/// PTY read chunk size. Mirrors `apps/daemon/src/pty.rs::READ_BUFFER_SIZE`
/// — 8 KiB balances syscall overhead against burst output from a
/// terminal-mode child.
const READ_BUFFER_SIZE: usize = 8 * 1024;

/// Socket read chunk size for the SSH framed reader. Same 8 KiB choice
/// as the daemon's `inbound_loop` for symmetry.
const SOCKET_READ_BUF: usize = 8 * 1024;

/// How long [`SshDaemonPty::attach`] waits for the daemon's `HelloAck`
/// before declaring the handshake stalled. Longer than the daemon's
/// matching timeout because the SSH tunnel adds first-byte latency.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

/// Outbound frame queue depth for the SSH writer thread.
///
/// The writer thread owns the unix-socket write half and drains this
/// queue with blocking `write_all`s; the runtime's `write_frame` only
/// ever does a non-blocking `try_send` so a stalled SSH tunnel can
/// never freeze the event loop. 256 frames is generous for normal
/// interactive use (a user typing 10 keys/sec fills it in ~25 s) — if
/// it ever saturates, the link is genuinely stuck and the agent
/// transitions to Crashed via the existing exit-code reaper.
const WRITER_QUEUE_CAPACITY: usize = 256;

/// Where an agent's PTY actually lives. The runtime owns one of these
/// per agent and only ever talks to it through the inherent methods on
/// this enum.
#[non_exhaustive]
pub enum AgentTransport {
    /// Child process spawned in a local PTY on the same host as the
    /// runtime. The default and only Stage 3 variant.
    Local(LocalPty),
    /// Tunneled connection to a `codemuxd` running on a remote host.
    /// Stage 4 lights this up via the `codemuxd-bootstrap` adapter
    /// crate (which calls `bootstrap` then [`SshDaemonPty::attach`]).
    SshDaemon(SshDaemonPty),
}

impl AgentTransport {
    /// Drain whatever bytes the transport has buffered since the last
    /// call. Returns immediately; an empty `Vec` means "no new output
    /// right now", **not** "transport closed". Use [`Self::try_wait`] to
    /// detect liveness.
    ///
    /// Each chunk preserves the boundary the reader thread observed,
    /// which doesn't matter for the vt100 parser (it processes bytes
    /// incrementally) but does keep memory ownership contiguous with
    /// what the channel produced — saves a concat copy.
    pub fn try_read(&mut self) -> Vec<Vec<u8>> {
        match self {
            Self::Local(p) => p.try_read(),
            Self::SshDaemon(p) => p.try_read(),
        }
    }

    /// Forward `data` to the agent's stdin (local PTY) or the remote
    /// daemon (which writes it to the remote PTY).
    ///
    /// # Errors
    /// Returns [`Error::Pty`] for local transport, [`Error::Wire`] or
    /// [`Error::Pty`] for SSH transport.
    pub fn write(&mut self, data: &[u8]) -> Result<(), Error> {
        match self {
            Self::Local(p) => p.write(data),
            Self::SshDaemon(p) => p.write(data),
        }
    }

    /// Apply a new PTY geometry. Best-effort by convention: an `Err`
    /// here means the child sees a stale size until the next resize, a
    /// harmless cosmetic glitch (claude re-lays-out on the next paint).
    /// The runtime currently logs and continues — see
    /// `apps/tui/src/runtime.rs::resize_agents`.
    ///
    /// # Errors
    /// Returns [`Error::Pty`] for local transport, [`Error::Wire`] or
    /// [`Error::Pty`] for SSH transport.
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<(), Error> {
        match self {
            Self::Local(p) => p.resize(rows, cols),
            Self::SshDaemon(p) => p.resize(rows, cols),
        }
    }

    /// Forward `sig` to the agent's child. Both transports only
    /// implement [`Signal::Kill`] today; other signals reach the child
    /// as the byte `0x03` via [`Self::write`] for Ctrl-C, or surface
    /// [`Error::SignalNotSupported`] for the rest.
    ///
    /// # Errors
    /// Returns [`Error::SignalNotSupported`] for non-Kill signals on
    /// either transport, [`Error::Pty`] for local kill failure, or
    /// [`Error::Wire`]/[`Error::Pty`] for SSH transport failures.
    pub fn signal(&mut self, sig: Signal) -> Result<(), Error> {
        match self {
            Self::Local(p) => p.signal(sig),
            Self::SshDaemon(p) => p.signal(sig),
        }
    }

    /// Liveness check. `None` = still alive, `Some(code)` = exited
    /// with the given status. Best-effort: an underlying `try_wait`
    /// I/O error is reported as `None` so the runtime keeps the agent
    /// in the navigator until the next poll cycle (matches the existing
    /// retain-on-exit behaviour).
    pub fn try_wait(&mut self) -> Option<i32> {
        match self {
            Self::Local(p) => p.try_wait(),
            Self::SshDaemon(p) => p.try_wait(),
        }
    }

    /// Kill the agent's child. Equivalent to `signal(Signal::Kill)` on
    /// both transports.
    ///
    /// # Errors
    /// Same envelope as [`Self::signal`].
    pub fn kill(&mut self) -> Result<(), Error> {
        match self {
            Self::Local(p) => p.kill(),
            Self::SshDaemon(p) => p.kill(),
        }
    }

    /// Build a transport backed by a real local PTY running `cat`. Test-only
    /// seam so downstream crates (e.g. `codemux-cli`) can construct a
    /// `RuntimeAgent` without a `claude` binary on PATH and without
    /// matching on `#[non_exhaustive]` `AgentTransport` from outside this
    /// crate. The `cat` child sits on its TTY waiting for input and is
    /// reaped on `Drop` of the returned transport.
    ///
    /// # Errors
    /// Same envelope as [`LocalPty::spawn`].
    #[cfg(any(test, feature = "test-util"))]
    pub fn for_test(label: String, rows: u16, cols: u16) -> Result<Self, Error> {
        LocalPty::spawn(OsStr::new("cat"), &[], label, None, rows, cols).map(Self::Local)
    }

    /// Build a transport that runs `sh -c 'exit 0'` so the child
    /// terminates immediately with exit code 0. Lets downstream tests
    /// drive the clean-exit reap path that the long-running `cat`
    /// child in [`Self::for_test`] never reaches. Same `Drop`
    /// semantics; the already-exited child is reaped in `LocalPty::drop`.
    ///
    /// # Errors
    /// Same envelope as [`LocalPty::spawn`].
    #[cfg(any(test, feature = "test-util"))]
    pub fn for_test_clean_exit(label: String, rows: u16, cols: u16) -> Result<Self, Error> {
        LocalPty::spawn(
            OsStr::new("sh"),
            &["-c".to_string(), "exit 0".to_string()],
            label,
            None,
            rows,
            cols,
        )
        .map(Self::Local)
    }
}

/// A child process spawned inside a local PTY.
///
/// Owns the same shape the previous `RuntimeAgent` had inline
/// (`master + writer + child + rx`). The `master` is held to keep the
/// master fd open — closing it would make the child see EOF on its
/// tty and exit immediately. Same invariant as
/// `apps/daemon/src/session.rs::Session::master`; if you find yourself
/// tempted to drop it earlier, don't.
pub struct LocalPty {
    label: String,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    rx: Receiver<Vec<u8>>,
    master: Box<dyn MasterPty + Send>,
}

impl LocalPty {
    /// Spawn `command args...` inside a fresh PTY of size `rows x cols`,
    /// optionally with `cwd` as the working directory.
    ///
    /// Public so tests can spawn `cat` instead of the production
    /// `claude` binary; the runtime always reaches this through
    /// [`crate::BinaryAgentSpawner`].
    ///
    /// # Errors
    /// Returns [`Error::Pty`] when the kernel can't allocate a PTY or
    /// the PTY's reader/writer can't be cloned, and [`Error::Spawn`]
    /// when `command` itself can't be launched.
    pub fn spawn(
        command: &OsStr,
        args: &[String],
        label: String,
        cwd: Option<&Path>,
        rows: u16,
        cols: u16,
    ) -> Result<Self, Error> {
        tracing::debug!(label, ?command, ?cwd, rows, cols, "spawning local PTY");

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| Error::Pty {
                source: Box::new(std::io::Error::other(format!("open pty: {e}"))),
            })?;

        let mut cmd = CommandBuilder::new(command);
        cmd.args(args);
        if let Some(cwd) = cwd {
            cmd.cwd(cwd);
        }
        let child = pair.slave.spawn_command(cmd).map_err(|e| Error::Spawn {
            command: command.to_string_lossy().into_owned(),
            source: Box::new(std::io::Error::other(format!(
                "spawn `{}` (is it on PATH?): {e}",
                command.to_string_lossy(),
            ))),
        })?;
        // Closing the slave fd on the parent side is required so the
        // child sees EOF on its tty when it exits — without this, the
        // master read never returns EOF and the reader thread spins.
        drop(pair.slave);

        let writer = pair.master.take_writer().map_err(|e| Error::Pty {
            source: Box::new(std::io::Error::other(format!("take pty writer: {e}"))),
        })?;
        let reader = pair.master.try_clone_reader().map_err(|e| Error::Pty {
            source: Box::new(std::io::Error::other(format!("clone pty reader: {e}"))),
        })?;
        let master = pair.master;
        let rx = spawn_pty_reader_thread(reader);
        Ok(Self {
            label,
            writer,
            child,
            rx,
            master,
        })
    }

    fn try_read(&mut self) -> Vec<Vec<u8>> {
        let mut chunks = Vec::new();
        while let Ok(bytes) = self.rx.try_recv() {
            chunks.push(bytes);
        }
        chunks
    }

    fn write(&mut self, data: &[u8]) -> Result<(), Error> {
        self.writer.write_all(data).map_err(|e| Error::Pty {
            source: Box::new(e),
        })
    }

    fn resize(&mut self, rows: u16, cols: u16) -> Result<(), Error> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| Error::Pty {
                source: Box::new(std::io::Error::other(format!("resize: {e}"))),
            })
    }

    fn signal(&mut self, sig: Signal) -> Result<(), Error> {
        // `portable-pty`'s `Child` only exposes `kill()` (SIGKILL).
        // Other signals would need `unsafe libc::kill` or a `nix` dep —
        // both deliberately out of scope here. The runtime delivers
        // Ctrl-C as the byte `0x03` through `write` instead, which is
        // the right interactive-terminal semantics anyway. The SSH
        // variant tunnels signals to the daemon, where the same
        // constraint applies on the remote side.
        if sig == Signal::Kill {
            return self.kill();
        }
        Err(Error::SignalNotSupported { signal: sig })
    }

    fn try_wait(&mut self) -> Option<i32> {
        // `portable-pty` exit codes are u32 (it folds Unix signal-killed
        // children into 128 + signum); cast saturating to i32 so a
        // hypothetical >2^31 code doesn't wrap into a negative — that
        // would lie to callers who treat negative as "killed by signal".
        match self.child.try_wait() {
            Ok(Some(status)) => Some(i32::try_from(status.exit_code()).unwrap_or(i32::MAX)),
            // Both `Ok(None)` (still running) and `Err(_)` (transient
            // wait failure) get reported as "alive" — the runtime
            // re-polls on the next tick and will reap on a future call.
            _ => None,
        }
    }

    fn kill(&mut self) -> Result<(), Error> {
        self.child.kill().map_err(|e| Error::Pty {
            source: Box::new(e),
        })
    }
}

impl Drop for LocalPty {
    fn drop(&mut self) {
        // Best-effort cleanup matching `apps/daemon/src/session.rs`:
        // log at debug so a stuck-zombie investigation has a
        // breadcrumb, but never panic from drop. `child.kill` /
        // `child.wait` may fail because the child is already gone
        // (EOF, SIGCHLD already reaped) — that's normal, not an error
        // worth surfacing.
        if let Err(e) = self.child.kill() {
            tracing::debug!(label = %self.label, "drop: child.kill failed: {e}");
        }
        if let Err(e) = self.child.wait() {
            tracing::debug!(label = %self.label, "drop: child.wait failed: {e}");
        }
    }
}

/// Tunnels the agent's PTY through `codemuxd` running on a remote host.
///
/// Owns four pieces:
/// - `writer_tx`: bounded queue feeding the writer thread. The runtime
///   pushes encoded frames here via non-blocking `try_send`; the writer
///   thread owns the socket write-half and drains the queue with
///   blocking `write_all`s. This is the decoupling that keeps the
///   event loop responsive when the SSH tunnel slows down — see
///   [`WRITER_QUEUE_CAPACITY`] for the rationale.
/// - `rx`: PTY chunks the framed reader thread drained from the
///   socket. Other inbound frames (`Pong`, `Error`) are absorbed
///   silently; `ChildExited` sets `exit_code` and ends the reader.
/// - `exit_code`: a single-set cell shared with the reader and writer
///   threads. Empty while the remote child is alive; populated once
///   `ChildExited` arrives, the socket EOFs, the writer hits an I/O
///   error, or the writer queue saturates. [`OnceLock`] gives us
///   write-once semantics without the lock-and-poison plumbing a
///   `Mutex<Option<i32>>` would impose.
/// - `tunnel`: the `ssh -N -L` subprocess. Killed on `Drop` so we
///   don't leak ssh processes when an agent is removed. Optional so
///   tests can attach against a local socket without spinning up a
///   tunnel.
pub struct SshDaemonPty {
    label: String,
    writer_tx: Sender<Vec<u8>>,
    rx: Receiver<Vec<u8>>,
    exit_code: Arc<OnceLock<i32>>,
    /// Diagnostic only — the daemon's pid on the remote host. Logged
    /// from `attach`; not otherwise consumed.
    daemon_pid: u32,
    tunnel: Option<ProcessChild>,
}

impl SshDaemonPty {
    /// Take an established [`UnixStream`] (post-bootstrap), perform the
    /// `Hello`/`HelloAck` handshake, spawn the framed reader thread,
    /// and return the constructed transport.
    ///
    /// `tunnel` is the `ssh -N -L` subprocess from the
    /// `codemuxd-bootstrap` adapter in production; tests pass `None`
    /// to attach against an in-process socket without a tunnel.
    ///
    /// # Errors
    /// Returns [`Error::Handshake`] for any handshake failure
    /// (timeouts, oversized frames, version mismatch, framing
    /// errors). The tunnel subprocess (if Some) is killed before
    /// returning so a failed attach doesn't leak it.
    pub fn attach(
        stream: UnixStream,
        label: String,
        agent_id: &str,
        rows: u16,
        cols: u16,
        tunnel: Option<ProcessChild>,
    ) -> Result<Self, Error> {
        // `set_read_timeout(Some(_))` bounds the handshake; the framed
        // reader clears it back to `None` (blocking) once it owns its
        // clone of the stream. Failures are best-effort — macOS can
        // EINVAL if the peer closed mid-call; the next read will
        // detect EOF on its own.
        if let Err(e) = stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT)) {
            tracing::debug!(label, "set_read_timeout(Some) failed pre-handshake: {e}");
        }

        let result = perform_handshake(&stream, agent_id, rows, cols);
        let daemon_pid = match result {
            Ok(pid) => pid,
            Err(e) => {
                if let Some(mut t) = tunnel {
                    let _ = t.kill();
                    let _ = t.wait();
                }
                return Err(e);
            }
        };

        // Clear the timeout so the framed reader blocks indefinitely
        // on the next `read`. Best-effort; same caveat as above.
        if let Err(e) = stream.set_read_timeout(None) {
            tracing::debug!(label, "set_read_timeout(None) post-handshake failed: {e}");
        }

        let read_stream = stream.try_clone().map_err(|e| Error::Handshake {
            source: Box::new(e),
        })?;
        let exit_code = Arc::new(OnceLock::new());
        let rx = spawn_framed_reader_thread(read_stream, Arc::clone(&exit_code));
        // The writer thread takes ownership of the original `stream`;
        // the reader already has its own clone. When `SshDaemonPty`
        // drops, `writer_tx` drops first, the writer thread's `recv`
        // returns Err, the thread exits, the original FD closes, and
        // the reader hits EOF on its own clone (or has already exited
        // via `ChildExited`).
        let writer_tx = spawn_framed_writer_thread(
            stream,
            Arc::clone(&exit_code),
            label.clone(),
            WRITER_QUEUE_CAPACITY,
        );

        tracing::info!(
            label = %label,
            daemon_pid,
            "SSH transport attached to remote codemuxd",
        );
        Ok(Self {
            label,
            writer_tx,
            rx,
            exit_code,
            daemon_pid,
            tunnel,
        })
    }

    fn try_read(&mut self) -> Vec<Vec<u8>> {
        let mut chunks = Vec::new();
        while let Ok(bytes) = self.rx.try_recv() {
            chunks.push(bytes);
        }
        chunks
    }

    fn write(&mut self, data: &[u8]) -> Result<(), Error> {
        let frame = Message::PtyData(data.to_vec()).encode()?;
        self.enqueue_frame(frame)
    }

    fn resize(&mut self, rows: u16, cols: u16) -> Result<(), Error> {
        let frame = Message::Resize { rows, cols }.encode()?;
        self.enqueue_frame(frame)
    }

    fn signal(&mut self, sig: Signal) -> Result<(), Error> {
        // The remote daemon's `handle_inbound::Signal` arm only kills
        // on `Signal::Kill` (workspace forbids `unsafe libc::kill`).
        // Mirror that constraint locally so a non-Kill signal surfaces
        // here rather than being silently dropped over the wire.
        if sig != Signal::Kill {
            return Err(Error::SignalNotSupported { signal: sig });
        }
        let frame = Message::Signal(sig).encode()?;
        self.enqueue_frame(frame)
    }

    fn try_wait(&mut self) -> Option<i32> {
        // `OnceLock::get` is a non-blocking, lock-free read. Returns
        // `None` while the framed reader hasn't seen `ChildExited`
        // (or transport EOF) yet, `Some(code)` once it has.
        self.exit_code.get().copied()
    }

    fn kill(&mut self) -> Result<(), Error> {
        self.signal(Signal::Kill)
    }

    /// Hand an already-encoded frame to the writer thread.
    ///
    /// Non-blocking by design: the producer (the runtime's event loop)
    /// must never stall on a slow SSH tunnel. On a full or disconnected
    /// queue the agent is marked exited so the runtime reaps it via the
    /// existing Crashed path on the next frame, and the caller gets a
    /// `Pty` error to surface.
    fn enqueue_frame(&mut self, frame: Vec<u8>) -> Result<(), Error> {
        match self.writer_tx.try_send(frame) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => {
                tracing::warn!(
                    label = %self.label,
                    capacity = WRITER_QUEUE_CAPACITY,
                    "SSH writer queue saturated; tunnel stalled, marking agent exited",
                );
                let _ = self.exit_code.set(-1);
                Err(Error::Pty {
                    source: "SSH writer queue saturated".into(),
                })
            }
            Err(TrySendError::Disconnected(_)) => {
                // Writer thread exited (write error). It already set
                // exit_code; surface the failure to the caller.
                Err(Error::Pty {
                    source: "SSH writer thread closed".into(),
                })
            }
        }
    }
}

impl Drop for SshDaemonPty {
    fn drop(&mut self) {
        // Closing the socket EOFs the framed reader; the thread exits
        // naturally without further coordination.
        if let Some(mut tunnel) = self.tunnel.take() {
            if let Err(e) = tunnel.kill() {
                tracing::debug!(
                    label = %self.label,
                    "drop: tunnel.kill failed: {e}",
                );
            }
            if let Err(e) = tunnel.wait() {
                tracing::debug!(
                    label = %self.label,
                    "drop: tunnel.wait failed: {e}",
                );
            }
        }
        tracing::debug!(label = %self.label, daemon_pid = self.daemon_pid, "SSH transport dropped");
    }
}

/// Send `Hello`, read `HelloAck`, validate the version, return the
/// daemon's pid. All errors map to [`Error::Handshake`] so the TUI
/// surfaces a handshake-specific hint.
fn perform_handshake(
    mut stream: &UnixStream,
    agent_id: &str,
    rows: u16,
    cols: u16,
) -> Result<u32, Error> {
    let hello = Message::Hello {
        protocol_version: wire::PROTOCOL_VERSION,
        rows,
        cols,
        agent_id: agent_id.to_string(),
    };
    let hello_bytes = hello.encode().map_err(|source| Error::Handshake {
        source: Box::new(source),
    })?;
    stream
        .write_all(&hello_bytes)
        .map_err(|source| Error::Handshake {
            source: Box::new(source),
        })?;
    stream.flush().map_err(|source| Error::Handshake {
        source: Box::new(source),
    })?;

    let mut buf = Vec::with_capacity(64);
    let mut tmp = [0u8; 256];
    loop {
        match wire::try_decode(&buf).map_err(|source| Error::Handshake {
            source: Box::new(source),
        })? {
            Some((
                Message::HelloAck {
                    protocol_version,
                    daemon_pid,
                },
                _consumed,
            )) => {
                if protocol_version != wire::PROTOCOL_VERSION {
                    return Err(Error::Handshake {
                        source: format!(
                            "protocol version mismatch: client v{}, daemon v{}",
                            wire::PROTOCOL_VERSION,
                            protocol_version,
                        )
                        .into(),
                    });
                }
                return Ok(daemon_pid);
            }
            Some((Message::Error { code, message }, _)) => {
                return Err(Error::Handshake {
                    source: format!("daemon rejected handshake: {code:?}: {message}").into(),
                });
            }
            Some((other, _)) => {
                return Err(Error::Handshake {
                    source: format!("expected HelloAck, got tag 0x{:02X}", other.tag(),).into(),
                });
            }
            None => {}
        }
        let n = stream.read(&mut tmp).map_err(|source| Error::Handshake {
            source: Box::new(source),
        })?;
        if n == 0 {
            return Err(Error::Handshake {
                source: "EOF before HelloAck".into(),
            });
        }
        buf.extend_from_slice(&tmp[..n]);
    }
}

/// Background reader: drains the unix socket, decodes wire frames, and
/// dispatches each. `PtyData` chunks go to the channel; `ChildExited`
/// records the exit code and ends the reader; other inbound frames
/// (`Pong`, daemon-emitted `Error`) are absorbed at debug level.
///
/// On EOF or read error, sets `exit_code` to `-1` if not already set,
/// so [`SshDaemonPty::try_wait`] reports liveness loss even when the
/// daemon never sent `ChildExited` (e.g. SSH tunnel died mid-session).
fn spawn_framed_reader_thread(
    mut read_stream: UnixStream,
    exit_code: Arc<OnceLock<i32>>,
) -> Receiver<Vec<u8>> {
    let (tx, rx) = unbounded::<Vec<u8>>();
    thread::spawn(move || {
        let mut buf: Vec<u8> = Vec::new();
        let mut tmp = vec![0u8; SOCKET_READ_BUF];
        'outer: loop {
            // Drain every complete frame currently in `buf` before
            // reading more. Mirrors the daemon's `inbound_loop` shape.
            loop {
                match wire::try_decode(&buf) {
                    Ok(Some((msg, consumed))) => {
                        buf.drain(..consumed);
                        match msg {
                            Message::PtyData(bytes) => {
                                if tx.send(bytes).is_err() {
                                    break 'outer;
                                }
                            }
                            Message::ChildExited { exit_code: code } => {
                                // First set wins; subsequent
                                // `EOF -> -1` writes from the read
                                // loop below are silently ignored
                                // by `OnceLock::set`.
                                let _ = exit_code.set(code);
                                break 'outer;
                            }
                            Message::Pong { nonce } => {
                                tracing::debug!("inbound Pong nonce={nonce}");
                            }
                            Message::Error { code, message } => {
                                tracing::warn!("daemon sent Error frame: {code:?}: {message}",);
                                let _ = exit_code.set(-1);
                                break 'outer;
                            }
                            // Other variants (Hello, HelloAck, Resize,
                            // Signal, Ping) shouldn't reach this loop:
                            // Hello/HelloAck are handshake-only;
                            // Resize/Signal/Ping are client-to-daemon.
                            // Log and absorb rather than break — a
                            // future protocol revision might legitimately
                            // start sending more server-to-client frames.
                            other => {
                                tracing::debug!(
                                    "framed reader absorbed unexpected frame tag=0x{:02X}",
                                    other.tag(),
                                );
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        tracing::warn!("framed reader decode failed: {e}");
                        let _ = exit_code.set(-1);
                        break 'outer;
                    }
                }
            }
            match read_stream.read(&mut tmp) {
                Ok(0) => {
                    let _ = exit_code.set(-1);
                    break;
                }
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                Err(e) => {
                    tracing::debug!("framed reader read failed: {e}");
                    let _ = exit_code.set(-1);
                    break;
                }
            }
        }
    });
    rx
}

/// Background writer: drains a bounded queue of pre-encoded frames and
/// performs the blocking `write_all` on the unix socket. The runtime's
/// `enqueue_frame` only ever does a non-blocking `try_send` against
/// the returned `Sender`, so a slow or stalled SSH tunnel can never
/// freeze the event loop.
///
/// Exits on:
/// - sender side dropped (the `SshDaemonPty` was dropped) — clean
///   shutdown; the original socket FD is closed by the local `stream`
///   moving out of scope.
/// - any `write_all` failure — sets `exit_code(-1)` so the runtime
///   reaps the agent into Crashed on its next poll.
fn spawn_framed_writer_thread(
    mut stream: UnixStream,
    exit_code: Arc<OnceLock<i32>>,
    label: String,
    capacity: usize,
) -> Sender<Vec<u8>> {
    let (tx, rx) = bounded::<Vec<u8>>(capacity);
    thread::spawn(move || {
        for frame in rx {
            if let Err(e) = stream.write_all(&frame) {
                tracing::debug!(label = %label, "framed writer write_all failed: {e}");
                let _ = exit_code.set(-1);
                break;
            }
            // `UnixStream::flush` is a no-op (no userspace buffer); the
            // local PTY writer doesn't call it either, so we mirror
            // that and avoid an untestable error branch.
        }
        // `stream` drops here, closing the writer's FD. The reader's
        // clone may still be alive; it'll exit via EOF (after tunnel
        // teardown) or via `ChildExited`.
    });
    tx
}

/// Background reader: drains the PTY master and pushes chunks into a
/// crossbeam channel. Exits on EOF or read error (including the master
/// being dropped). Same shape as `apps/daemon/src/pty.rs` and the prior
/// `apps/tui/src/runtime.rs::spawn_reader_thread`.
fn spawn_pty_reader_thread(mut reader: Box<dyn Read + Send>) -> Receiver<Vec<u8>> {
    let (tx, rx) = unbounded::<Vec<u8>>();
    thread::spawn(move || {
        let mut buf = vec![0u8; READ_BUFFER_SIZE];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    rx
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::os::unix::net::UnixListener;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use super::*;

    /// Round-trip the full `LocalPty` surface that the runtime depends on:
    /// write some bytes, drain the echoed output, resize, kill. `cat`
    /// echoes stdin to stdout so we don't need the production `claude`
    /// binary on `PATH`.
    ///
    /// Models `apps/daemon/src/pty.rs::tests::spawn_with_cwd_sets_child_working_directory`.
    #[test]
    fn local_transport_round_trips_write_read_resize_kill() -> Result<(), Box<dyn std::error::Error>>
    {
        let pty = LocalPty::spawn(OsStr::new("cat"), &[], "test-cat".into(), None, 24, 80)?;
        let mut transport = AgentTransport::Local(pty);

        // `cat` is line-buffered when its stdin is a TTY, so the carriage
        // return triggers an echo of the line back through the master.
        transport.write(b"hello\r")?;

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut got = Vec::new();
        loop {
            for chunk in transport.try_read() {
                got.extend_from_slice(&chunk);
            }
            if String::from_utf8_lossy(&got).contains("hello") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "expected `cat` to echo `hello` within 2s, got {:?}",
                String::from_utf8_lossy(&got),
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        // Resize succeeds while the child is still alive; the geometry
        // change is invisible to a `cat` child but the syscall path is
        // exercised end-to-end.
        transport.resize(40, 120)?;

        // Verify try_wait reports alive before kill, dead after.
        assert!(
            transport.try_wait().is_none(),
            "freshly-spawned cat should still be alive",
        );

        transport.kill()?;

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if transport.try_wait().is_some() {
                return Ok(());
            }
            assert!(
                Instant::now() < deadline,
                "cat did not die within 2s of kill()",
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Local transport rejects non-Kill signals with a precise variant
    /// rather than silently dropping them. Future work (stage where
    /// `nix` arrives or unsafe is permitted) will replace this with
    /// real signal delivery.
    #[test]
    fn local_signal_only_supports_kill() -> Result<(), Box<dyn std::error::Error>> {
        let pty = LocalPty::spawn(OsStr::new("cat"), &[], "test-sig".into(), None, 24, 80)?;
        let mut transport = AgentTransport::Local(pty);

        for sig in [Signal::Hup, Signal::Int, Signal::Term] {
            let Err(err) = transport.signal(sig) else {
                unreachable!("non-Kill signal {sig:?} must error on local transport");
            };
            assert!(
                matches!(err, Error::SignalNotSupported { signal: s } if s == sig),
                "expected SignalNotSupported({sig:?}), got {err:?}",
            );
        }

        // Kill is the one signal portable-pty exposes — verify it
        // actually reaps the child.
        transport.signal(Signal::Kill)?;

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if transport.try_wait().is_some() {
                return Ok(());
            }
            assert!(
                Instant::now() < deadline,
                "cat did not die within 2s of signal(Kill)",
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// SSH transport's `signal` rejects non-Kill variants without
    /// touching the wire. Mirrors the local transport and the daemon's
    /// `handle_inbound::Signal` arm.
    #[test]
    fn ssh_transport_signal_only_supports_kill() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("daemon.sock");
        let _daemon = FakeDaemon::spawn(&sock_path, FakeDaemonScript::default());
        let stream = wait_for_connect(&sock_path);
        let mut pty =
            SshDaemonPty::attach(stream, "test-sig".into(), "agent-0", 24, 80, None).unwrap();

        for sig in [Signal::Hup, Signal::Int, Signal::Term] {
            let Err(err) = pty.signal(sig) else {
                unreachable!("non-Kill signal {sig:?} must error on SSH transport");
            };
            assert!(
                matches!(err, Error::SignalNotSupported { signal: s } if s == sig),
                "expected SignalNotSupported({sig:?}), got {err:?}",
            );
        }
    }

    /// Full handshake against an in-process daemon: client sends Hello,
    /// daemon sends `HelloAck`, attach succeeds and reports the daemon's
    /// pid via tracing.
    #[test]
    fn ssh_transport_handshake_completes_against_in_process_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("daemon.sock");
        let _daemon = FakeDaemon::spawn(&sock_path, FakeDaemonScript::default());
        let stream = wait_for_connect(&sock_path);
        let pty =
            SshDaemonPty::attach(stream, "test-handshake".into(), "agent-0", 24, 80, None).unwrap();
        // Daemon's HelloAck carries `daemon_pid = 0xDEAD_BEEF` per the
        // FakeDaemon default. Confirm we wired that through.
        assert_eq!(pty.daemon_pid, 0xDEAD_BEEF);
    }

    /// Round-trip write / read / resize against a fake daemon that
    /// echoes every `PtyData` payload back as `PtyData`. Confirms the full
    /// outbound encode + inbound decode pipeline.
    #[test]
    fn ssh_transport_round_trips_write_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("daemon.sock");
        let _daemon = FakeDaemon::spawn(&sock_path, FakeDaemonScript::echo());
        let stream = wait_for_connect(&sock_path);
        let mut pty =
            SshDaemonPty::attach(stream, "test-rt".into(), "agent-0", 24, 80, None).unwrap();

        pty.write(b"hello over ssh").unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut got = Vec::new();
        loop {
            for chunk in pty.try_read() {
                got.extend_from_slice(&chunk);
            }
            if got == b"hello over ssh" {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "echo daemon should round-trip within 2s, got {got:?}",
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Sending `Signal::Kill` reaches the fake daemon which responds
    /// with `ChildExited`; `try_wait` then reports the exit code.
    #[test]
    fn ssh_transport_kill_marks_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("daemon.sock");
        let _daemon = FakeDaemon::spawn(&sock_path, FakeDaemonScript::kill_yields_exit(137));
        let stream = wait_for_connect(&sock_path);
        let mut pty =
            SshDaemonPty::attach(stream, "test-kill".into(), "agent-0", 24, 80, None).unwrap();

        assert!(pty.try_wait().is_none(), "alive immediately after attach");
        pty.kill().unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(code) = pty.try_wait() {
                assert_eq!(code, 137);
                return;
            }
            assert!(
                Instant::now() < deadline,
                "expected ChildExited within 2s of kill()",
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// `Resize` frames reach the daemon. The fake daemon records the
    /// most recent resize and the test asserts on it after a brief
    /// settle window.
    #[test]
    fn ssh_transport_resize_reaches_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("daemon.sock");
        let recorded_resize: Arc<Mutex<Option<(u16, u16)>>> = Arc::new(Mutex::new(None));
        let script = FakeDaemonScript::record_resize(Arc::clone(&recorded_resize));
        let _daemon = FakeDaemon::spawn(&sock_path, script);
        let stream = wait_for_connect(&sock_path);
        let mut pty =
            SshDaemonPty::attach(stream, "test-resize".into(), "agent-0", 24, 80, None).unwrap();

        pty.resize(50, 200).unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let snap = recorded_resize
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some((rows, cols)) = *snap {
                assert_eq!((rows, cols), (50, 200));
                return;
            }
            drop(snap);
            assert!(
                Instant::now() < deadline,
                "fake daemon should observe Resize within 2s",
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Transport closure (peer EOF) marks the agent as exited even
    /// when the daemon never sent `ChildExited`. Models the
    /// "SSH tunnel died mid-session" path.
    #[test]
    fn ssh_transport_eof_marks_exit_code_minus_one() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("daemon.sock");
        let _daemon = FakeDaemon::spawn(&sock_path, FakeDaemonScript::handshake_then_close());
        let stream = wait_for_connect(&sock_path);
        let mut pty =
            SshDaemonPty::attach(stream, "test-eof".into(), "agent-0", 24, 80, None).unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(code) = pty.try_wait() {
                assert_eq!(code, -1);
                return;
            }
            assert!(Instant::now() < deadline, "expected exit-on-EOF within 2s",);
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Producer-side writes never block the runtime when the SSH
    /// tunnel is wedged. The fake daemon completes the handshake then
    /// stops reading; the kernel send buffer fills, the writer thread
    /// blocks on `write_all`, then the bounded producer queue
    /// saturates. Each `write` call must return promptly (Ok or Err),
    /// and the agent must transition to "exited" so the runtime can
    /// reap it via the existing Crashed path.
    ///
    /// Regression for the freeze the user could trigger over slow
    /// SSH: the synchronous `socket_writer.write_all` in the event
    /// loop would block until TCP backpressure cleared (or
    /// `ServerAliveCountMax` killed the tunnel ~45 s later).
    #[test]
    fn ssh_transport_writes_do_not_block_when_daemon_stuck() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("daemon.sock");
        let _daemon = FakeDaemon::spawn(&sock_path, FakeDaemonScript::handshake_then_silent());
        let stream = wait_for_connect(&sock_path);
        let mut pty =
            SshDaemonPty::attach(stream, "test-stuck".into(), "agent-0", 24, 80, None).unwrap();

        // Push large frames until either we hit the queue-saturated
        // error or we've exceeded a sane upper bound. Each call is
        // bounded (the producer is non-blocking) so the whole loop
        // must complete well within the deadline. With 8 KiB payloads
        // we fill macOS' default UnixStream send buffer + the 256-deep
        // producer queue in well under 1000 iterations.
        let payload = vec![b'x'; 8 * 1024];
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut saturated = false;
        for i in 0..2000 {
            assert!(
                Instant::now() < deadline,
                "producer must not block: still spinning at iteration {i}",
            );
            match pty.write(&payload) {
                Ok(()) => {}
                Err(Error::Pty { .. }) => {
                    saturated = true;
                    break;
                }
                Err(other) => panic!("unexpected error from non-blocking write: {other:?}"),
            }
        }
        assert!(
            saturated,
            "expected the bounded writer queue to surface a Pty error within 2000 iterations",
        );

        // Once saturation is reported, the agent must look exited so
        // `reap_dead_transports` transitions it to Crashed on the
        // next frame.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(code) = pty.try_wait() {
                assert_eq!(code, -1);
                return;
            }
            assert!(
                Instant::now() < deadline,
                "expected try_wait to report exit within 2s of writer saturation",
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// `enqueue_frame` surfaces a `Pty` error once the writer thread
    /// has exited, even if the queue itself isn't yet saturated. The
    /// fake daemon closes the connection right after the handshake;
    /// the writer thread observes EPIPE on its first `write_all`,
    /// drops the channel receiver, and any subsequent producer-side
    /// `write` lands on the `TrySendError::Disconnected` branch.
    #[test]
    fn ssh_transport_writes_after_writer_exit_return_pty_error() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("daemon.sock");
        let _daemon = FakeDaemon::spawn(&sock_path, FakeDaemonScript::handshake_then_close());
        let stream = wait_for_connect(&sock_path);
        let mut pty =
            SshDaemonPty::attach(stream, "test-disconn".into(), "agent-0", 24, 80, None).unwrap();

        // Push frames until the producer reports a `Pty` error. The
        // first call may succeed (frame queued before the writer fails)
        // but subsequent calls land on `Disconnected` once the writer
        // thread has dropped the receiver.
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            assert!(
                Instant::now() < deadline,
                "expected writer thread to exit and surface Pty error within 2s",
            );
            match pty.write(b"x") {
                Ok(()) => thread::sleep(Duration::from_millis(20)),
                Err(Error::Pty { .. }) => return,
                Err(other) => panic!("unexpected error from disconnected writer: {other:?}"),
            }
        }
    }

    // ----- Test helpers -----

    /// Hand-rolled in-process daemon for SSH transport tests. Plays
    /// just enough of the wire protocol for [`SshDaemonPty`]'s tests
    /// to exercise their paths — we don't pull in a `codemuxd`
    /// dev-dep because the workspace's allowed-edges policy
    /// (CLAUDE.md) keeps `crates/session` deliberately thin.
    struct FakeDaemon {
        // Held only for its `Drop`: joining cleans up the listener
        // thread when the test ends.
        _handle: thread::JoinHandle<()>,
    }

    impl FakeDaemon {
        fn spawn(sock_path: &Path, script: FakeDaemonScript) -> Self {
            let listener = UnixListener::bind(sock_path).unwrap();
            let handle = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                Self::run_handshake(&mut stream);
                script.run(stream);
            });
            Self { _handle: handle }
        }

        /// Read Hello, send `HelloAck` with `daemon_pid = 0xDEAD_BEEF`.
        fn run_handshake(stream: &mut UnixStream) {
            let mut buf = Vec::with_capacity(64);
            let mut tmp = [0u8; 256];
            let _hello = loop {
                if let Some((msg, consumed)) = wire::try_decode(&buf).unwrap() {
                    buf.drain(..consumed);
                    break msg;
                }
                let n = stream.read(&mut tmp).unwrap();
                assert!(n != 0, "FakeDaemon: client closed before Hello");
                buf.extend_from_slice(&tmp[..n]);
            };
            let ack = Message::HelloAck {
                protocol_version: wire::PROTOCOL_VERSION,
                daemon_pid: 0xDEAD_BEEF,
            }
            .encode()
            .unwrap();
            stream.write_all(&ack).unwrap();
            stream.flush().unwrap();
        }
    }

    /// Post-handshake behaviour for the [`FakeDaemon`]. Each variant is
    /// a closure-bag because the cases need different daemon-side state.
    enum FakeDaemonScript {
        /// Read frames and discard them.
        Default,
        /// Echo every `PtyData` payload back as `PtyData`.
        Echo,
        /// On `Signal::Kill`, reply `ChildExited` with the given code.
        KillYieldsExit(i32),
        /// Record the most recent Resize geometry.
        RecordResize(Arc<Mutex<Option<(u16, u16)>>>),
        /// Close the connection immediately (test EOF handling).
        HandshakeThenClose,
        /// Hold the connection open after the handshake but never read
        /// from it again. The kernel send buffer fills, then the
        /// writer thread blocks on `write_all`, then the runtime's
        /// bounded producer queue saturates — the path that proves
        /// non-blocking write semantics.
        HandshakeThenSilent,
    }

    impl FakeDaemonScript {
        fn default() -> Self {
            Self::Default
        }
        fn echo() -> Self {
            Self::Echo
        }
        fn kill_yields_exit(code: i32) -> Self {
            Self::KillYieldsExit(code)
        }
        fn record_resize(slot: Arc<Mutex<Option<(u16, u16)>>>) -> Self {
            Self::RecordResize(slot)
        }
        fn handshake_then_close() -> Self {
            Self::HandshakeThenClose
        }
        fn handshake_then_silent() -> Self {
            Self::HandshakeThenSilent
        }

        fn run(self, mut stream: UnixStream) {
            if matches!(self, Self::HandshakeThenClose) {
                drop(stream);
                return;
            }
            if matches!(self, Self::HandshakeThenSilent) {
                // Park the stream alive without reading. Keeping a
                // reference holds the FD open so the client side sees
                // backpressure rather than EOF.
                std::thread::park();
                drop(stream);
                return;
            }
            let mut buf = Vec::new();
            let mut tmp = vec![0u8; 4096];
            'outer: loop {
                while let Some((msg, consumed)) = match wire::try_decode(&buf) {
                    Ok(v) => v,
                    Err(_) => break 'outer,
                } {
                    buf.drain(..consumed);
                    match (&self, msg) {
                        (Self::Echo, Message::PtyData(bytes)) => {
                            let frame = Message::PtyData(bytes).encode().unwrap();
                            if stream.write_all(&frame).is_err() {
                                break 'outer;
                            }
                            let _ = stream.flush();
                        }
                        (Self::KillYieldsExit(code), Message::Signal(Signal::Kill)) => {
                            let frame = Message::ChildExited { exit_code: *code }.encode().unwrap();
                            if stream.write_all(&frame).is_err() {
                                break 'outer;
                            }
                            let _ = stream.flush();
                            // After ChildExited the conn is over.
                            break 'outer;
                        }
                        (Self::RecordResize(slot), Message::Resize { rows, cols }) => {
                            let mut guard = slot
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner);
                            *guard = Some((rows, cols));
                        }
                        // Other (script, frame) pairs: silently absorb.
                        _ => {}
                    }
                }
                match stream.read(&mut tmp) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&tmp[..n]),
                }
            }
        }
    }

    /// Connect to a unix socket with a brief retry loop; the daemon
    /// thread races with the test's connect, so the bind may race past
    /// us by a few ms.
    fn wait_for_connect(sock_path: &Path) -> UnixStream {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match UnixStream::connect(sock_path) {
                Ok(s) => return s,
                Err(_) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(e) => panic!("could not connect to {sock_path:?}: {e}"),
            }
        }
    }
}
