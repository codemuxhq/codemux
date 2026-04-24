//! Per-connection wire-protocol handler.
//!
//! Stage 1 layered `codemux-wire` over the Stage 0 byte shuttle. Stage 4
//! makes the inbound dispatch real: `Resize` actually resizes the master
//! PTY, `Signal::Kill` actually reaps the child, `Ping` actually replies
//! with `Pong`, and the outbound `ChildExited` carries the real exit
//! code from `child.try_wait()` instead of a placeholder zero.
//!
//! Both threads now write to the socket: outbound emits `PtyData` /
//! `ChildExited` per the existing pattern, inbound emits `Pong`. To
//! keep frames atomic we serialize all socket writes through a small
//! `Mutex<&UnixStream>` shared via `thread::scope`. Critical sections
//! are one frame each — no perf concern, and impossible to interleave
//! frame bytes in the wire output.
//!
//! `child` and `master` are reborrowed from the [`Session`] across the
//! same scope. Inbound owns `master` exclusively (resize is the only
//! caller); `child` is shared via `Mutex` because both inbound
//! (`Signal::Kill`) and outbound (`try_wait` on disconnect) reach it.
//!
//! `std::thread::scope` is load-bearing in two ways: the PTY writer and
//! rx channel belong to the [`Session`] (which outlives any single
//! connection), and the `UnixStream` itself is shared between threads
//! as `&UnixStream` borrows rather than via `try_clone()` (saves two
//! fds and a syscall per connection).
//!
//! [`Session`]: crate::session::Session

use std::io::{ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use codemux_wire::{self as wire, ErrorCode, Message};
use crossbeam_channel::{Receiver, RecvTimeoutError};
use portable_pty::{Child, MasterPty, PtySize};

use crate::error::Error;

/// Read chunk size for the socket → PTY direction. The wire decoder is
/// streaming-friendly (reassembles partial frames), so this is just a
/// throughput knob; 8 KiB matches the daemon's PTY reader.
const SOCKET_READ_BUF: usize = 8 * 1024;

/// Polling cadence for the PTY → socket direction. The poll exists so
/// the loop can notice when the inbound thread set the stop flag without
/// having to wait for the next PTY chunk to arrive (which may be never
/// if the child is idle).
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// How long the daemon waits for the client's `Hello` frame before
/// declaring the handshake stalled. Generous because cold-cache SSH
/// tunnels can introduce real latency on the first byte.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Information extracted from the client's `Hello` frame. Stage 1
/// captures it for logging and forward use; Stage 2's `Session::resize`
/// plumbing will consume `rows`/`cols`, and Stage 4 will route on
/// `agent_id`.
#[derive(Debug, Clone)]
pub struct HelloInfo {
    pub protocol_version: u8,
    pub rows: u16,
    pub cols: u16,
    pub agent_id: String,
}

/// Drive a single client connection through handshake and bidirectional
/// framed I/O. Borrows `writer`, `rx`, `master`, and `child` from the
/// caller (the `Session`) so all four survive across re-attaches. `stream`
/// is taken by value because `run` semantically owns the connection for
/// its lifetime: when the function returns, the socket is closed.
///
/// # Errors
/// Returns the handshake error if version negotiation or framing fails;
/// returns a transport error if the inbound thread panics. Clean EOF in
/// either direction is `Ok(())`.
#[allow(clippy::needless_pass_by_value)]
pub fn run(
    stream: UnixStream,
    writer: &mut (dyn Write + Send),
    rx: &Receiver<Vec<u8>>,
    master: &mut (dyn MasterPty + Send),
    child: &mut (dyn Child + Send + Sync),
) -> Result<(), Error> {
    let mut handshake_buf = Vec::with_capacity(256);
    let hello = match perform_handshake(&stream, &mut handshake_buf) {
        Ok(info) => info,
        Err(e) => {
            if let Err(shutdown_err) = stream.shutdown(Shutdown::Both)
                && shutdown_err.kind() != ErrorKind::NotConnected
            {
                tracing::debug!("handshake-failure shutdown failed: {shutdown_err}");
            }
            return Err(e);
        }
    };
    tracing::info!(
        protocol_version = hello.protocol_version,
        rows = hello.rows,
        cols = hello.cols,
        agent_id = %hello.agent_id,
        "handshake complete",
    );

    // The client's `Hello` advertised an initial geometry. Apply it now so
    // the child sees the right `winsize` from the first byte instead of
    // the bootstrap default the daemon was started with.
    if let Err(e) = master.resize(PtySize {
        rows: hello.rows,
        cols: hello.cols,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        tracing::warn!("initial resize from Hello geometry failed: {e}");
    }

    let stop = Arc::new(AtomicBool::new(false));
    // `Mutex<&UnixStream>` serializes socket writes so inbound's `Pong`
    // frames can't interleave with outbound's `PtyData`. The mutex is
    // scoped to `thread::scope`; it never escapes.
    let socket_writer: Mutex<&UnixStream> = Mutex::new(&stream);
    // `Mutex<&mut dyn Child + Send + Sync>` lets inbound (`Signal::Kill`)
    // and outbound (end-of-life `try_wait`) share the child reborrow.
    // Critical sections are nanoseconds; contention is impossible in
    // practice (inbound only locks on Signal frames; outbound only on
    // disconnect).
    let child_lock: Mutex<&mut (dyn Child + Send + Sync)> = Mutex::new(child);

    thread::scope(|s| -> Result<(), Error> {
        let stop_for_bg = Arc::clone(&stop);
        let stream_ref: &UnixStream = &stream;
        let socket_writer_ref = &socket_writer;
        let child_lock_ref = &child_lock;
        let bg = s.spawn(move || {
            let result = inbound_loop(
                stream_ref,
                handshake_buf,
                writer,
                master,
                child_lock_ref,
                socket_writer_ref,
            );
            // Release: we publish the flag; the outbound loop's Acquire
            // load pairs with this. No other shared data is being
            // synchronized, so a fence-only ordering is correct.
            stop_for_bg.store(true, Ordering::Release);
            result
        });

        outbound_loop(&socket_writer, rx, &stop, &child_lock);

        // Outbound loop ended: wake the inbound thread (which may still
        // be blocked on `read`). NotConnected is expected if the peer
        // already tore down the socket.
        if let Err(e) = stream.shutdown(Shutdown::Both)
            && e.kind() != ErrorKind::NotConnected
        {
            tracing::debug!("post-loop shutdown failed: {e}");
        }

        bg.join().map_err(|_| Error::Io {
            source: std::io::Error::other("inbound thread panicked"),
        })?
    })
}

/// Read the client's `Hello` frame, validate version, send `HelloAck`.
/// On version mismatch, sends an `Error{VersionMismatch}` frame before
/// returning the error so the client gets a structured rejection
/// instead of a bare disconnect.
///
/// Any bytes the client sent past the `Hello` frame stay in
/// `handshake_buf` and become the inbound loop's starting buffer — TCP
/// can deliver a single read covering "Hello + first `PtyData`" together.
fn perform_handshake(stream: &UnixStream, handshake_buf: &mut Vec<u8>) -> Result<HelloInfo, Error> {
    // Best-effort: setsockopt can return EINVAL on macOS if the peer has
    // already closed the socket between accept() and now. The next read
    // will detect EOF and return HandshakeIncomplete on its own — the
    // bound is just defensive against a slow but live peer.
    if let Err(e) = stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT)) {
        tracing::debug!("handshake set_read_timeout failed: {e}");
    }
    let hello_msg = read_one_frame(stream, handshake_buf)?;

    let (protocol_version, rows, cols, agent_id) = match hello_msg {
        Message::Hello {
            protocol_version,
            rows,
            cols,
            agent_id,
        } => (protocol_version, rows, cols, agent_id),
        other => {
            return Err(Error::HandshakeMissing {
                got_tag: other.tag(),
            });
        }
    };

    let mut writer = stream;
    if protocol_version != wire::PROTOCOL_VERSION {
        let err_frame = Message::Error {
            code: ErrorCode::VersionMismatch,
            message: format!(
                "daemon speaks v{}, client sent v{}",
                wire::PROTOCOL_VERSION,
                protocol_version,
            ),
        };
        if let Ok(bytes) = err_frame.encode() {
            if let Err(e) = writer.write_all(&bytes) {
                tracing::debug!("VersionMismatch frame write failed: {e}");
            }
            if let Err(e) = writer.flush() {
                tracing::debug!("VersionMismatch frame flush failed: {e}");
            }
        }
        return Err(Error::VersionMismatch {
            client: protocol_version,
            daemon: wire::PROTOCOL_VERSION,
        });
    }

    let ack = Message::HelloAck {
        protocol_version: wire::PROTOCOL_VERSION,
        daemon_pid: std::process::id(),
    };
    let ack_bytes = ack.encode()?;
    writer.write_all(&ack_bytes)?;
    writer.flush()?;

    // Clear the timeout only on the success path, and only best-effort:
    // setsockopt can fail if the socket transitioned to a closed state.
    // The inbound loop will block indefinitely on reads, which is what
    // we want post-handshake.
    if let Err(e) = stream.set_read_timeout(None) {
        tracing::debug!("post-handshake set_read_timeout(None) failed: {e}");
    }

    Ok(HelloInfo {
        protocol_version,
        rows,
        cols,
        agent_id,
    })
}

/// Read until exactly one complete frame is in the buffer; return it.
/// Bytes following the frame stay in `buf` for the next caller. EOF
/// before a complete frame returns [`Error::HandshakeIncomplete`].
fn read_one_frame(mut stream: &UnixStream, buf: &mut Vec<u8>) -> Result<Message, Error> {
    let mut tmp = [0u8; 1024];
    loop {
        if let Some((msg, consumed)) = wire::try_decode(buf)? {
            buf.drain(..consumed);
            return Ok(msg);
        }
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Err(Error::HandshakeIncomplete);
        }
        buf.extend_from_slice(&tmp[..n]);
    }
}

/// Decode frames from the socket and dispatch each to the PTY writer.
/// Returns `Ok(())` on clean EOF, `Ok(())` on bad frame (we close the
/// conn rather than try to resync), or `Err` only for unexpected
/// transport failures.
fn inbound_loop(
    mut stream: &UnixStream,
    mut buf: Vec<u8>,
    writer: &mut (dyn Write + Send),
    master: &mut (dyn MasterPty + Send),
    child_lock: &Mutex<&mut (dyn Child + Send + Sync)>,
    socket_writer: &Mutex<&UnixStream>,
) -> Result<(), Error> {
    let mut tmp = vec![0u8; SOCKET_READ_BUF];
    loop {
        loop {
            match wire::try_decode(&buf) {
                Ok(Some((msg, consumed))) => {
                    buf.drain(..consumed);
                    if !handle_inbound(msg, writer, master, child_lock, socket_writer) {
                        return Ok(());
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!("inbound frame decode failed: {e}");
                    return Ok(());
                }
            }
        }

        match stream.read(&mut tmp) {
            Ok(0) => return Ok(()),
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => {
                tracing::debug!("socket read failed: {e}");
                return Ok(());
            }
        }
    }
}

/// Apply one inbound message. Returns `false` to signal the inbound
/// loop should end (peer requested close, error frame received, child
/// already exited, etc.).
fn handle_inbound(
    msg: Message,
    writer: &mut (dyn Write + Send),
    master: &mut (dyn MasterPty + Send),
    child_lock: &Mutex<&mut (dyn Child + Send + Sync)>,
    socket_writer: &Mutex<&UnixStream>,
) -> bool {
    match msg {
        Message::PtyData(bytes) => {
            if let Err(e) = writer.write_all(&bytes) {
                tracing::debug!("pty write failed: {e}");
                return false;
            }
            if let Err(e) = writer.flush() {
                tracing::debug!("pty flush failed: {e}");
            }
            true
        }
        Message::Resize { rows, cols } => {
            // Best-effort: a failed resize means the child sees a stale
            // winsize until the next resize. Same convention as the
            // local transport; matches the runtime's `resize_agents`.
            if let Err(e) = master.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            }) {
                tracing::warn!("inbound Resize {rows}x{cols} failed: {e}");
            }
            true
        }
        Message::Signal(sig) => {
            // `portable_pty::Child` only exposes `kill()` (SIGKILL).
            // Other signals would need `unsafe libc::kill` or a `nix`
            // dep — both forbidden by the workspace's `unsafe_code =
            // "forbid"`. Matches `LocalPty::signal` exactly. Ctrl-C
            // reaches the child as the byte 0x03 via PtyData (the
            // right interactive-terminal semantics anyway), not via a
            // Signal frame.
            if matches!(sig, codemux_wire::Signal::Kill) {
                let mut guard = child_lock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Err(e) = guard.kill() {
                    tracing::debug!("inbound Signal::Kill failed: {e}");
                }
            } else {
                tracing::debug!(
                    "inbound Signal {sig:?} ignored — only Kill is supported on this transport",
                );
            }
            true
        }
        Message::Ping { nonce } => {
            let pong = match (Message::Pong { nonce }).encode() {
                Ok(bytes) => bytes,
                Err(e) => {
                    tracing::warn!("Pong encode failed: {e}");
                    return true;
                }
            };
            let mut guard = socket_writer
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Err(e) = guard.write_all(&pong) {
                tracing::debug!("Pong write failed: {e}");
                return false;
            }
            if let Err(e) = guard.flush() {
                tracing::debug!("Pong flush failed: {e}");
            }
            true
        }
        Message::Pong { nonce } => {
            tracing::debug!("inbound Pong nonce={nonce}");
            true
        }
        Message::Hello { .. } | Message::HelloAck { .. } | Message::ChildExited { .. } => {
            tracing::warn!(
                "post-handshake inbound of server-only frame tag=0x{:02X}; closing conn",
                msg.tag(),
            );
            false
        }
        Message::Error { code, message } => {
            tracing::info!("client sent error frame: code={code:?} message={message}");
            false
        }
        // `Message` is non_exhaustive; a future variant we don't know
        // about is closer to a protocol violation than a no-op.
        _ => {
            tracing::warn!(
                "inbound unknown message variant tag=0x{:02X}; closing conn",
                msg.tag(),
            );
            false
        }
    }
}

/// Wrap each PTY chunk in a `PtyData` frame and write it to the socket.
/// Sends `ChildExited` when the PTY rx channel disconnects (the
/// `Session`'s reader thread has hung up — child is dead). The exit
/// code comes from a real `child.try_wait()`; if the child hasn't been
/// reaped yet (race between rx-disconnect and SIGCHLD delivery) we
/// report `-1` so the client distinguishes "exited cleanly" from
/// "exited but we don't know with what code".
///
/// The frame buffer is reused across iterations; only the per-chunk
/// `Vec<u8>` from the channel is allocation-fresh.
fn outbound_loop(
    socket_writer: &Mutex<&UnixStream>,
    rx: &Receiver<Vec<u8>>,
    stop: &Arc<AtomicBool>,
    child_lock: &Mutex<&mut (dyn Child + Send + Sync)>,
) {
    let mut frame_buf = Vec::with_capacity(SOCKET_READ_BUF + 16);
    while !stop.load(Ordering::Acquire) {
        match rx.recv_timeout(POLL_INTERVAL) {
            Ok(chunk) => {
                frame_buf.clear();
                if let Err(e) = Message::PtyData(chunk).encode_to(&mut frame_buf) {
                    tracing::warn!("PtyData encode failed: {e}");
                    return;
                }
                let mut guard = socket_writer
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Err(e) = guard.write_all(&frame_buf) {
                    tracing::debug!("socket write failed: {e}");
                    return;
                }
                if let Err(e) = guard.flush() {
                    tracing::debug!("socket flush failed: {e}");
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                // Child died. Source the real exit code from try_wait.
                // A `try_wait` error or `Ok(None)` (child not yet
                // reaped — possible if rx disconnect arrived before
                // SIGCHLD) becomes -1: the client knows the child is
                // gone but not why.
                let exit_code = {
                    let mut guard = child_lock
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    match guard.try_wait() {
                        Ok(Some(status)) => {
                            // ExitStatus exit_code is u32; cast saturating
                            // to i32 so a hypothetical >2^31 code doesn't
                            // wrap into a negative (which would lie to
                            // callers who treat negative as "killed by
                            // signal").
                            i32::try_from(status.exit_code()).unwrap_or(i32::MAX)
                        }
                        _ => -1,
                    }
                };
                frame_buf.clear();
                if (Message::ChildExited { exit_code })
                    .encode_to(&mut frame_buf)
                    .is_ok()
                {
                    let mut guard = socket_writer
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if let Err(e) = guard.write_all(&frame_buf) {
                        tracing::debug!("ChildExited write failed: {e}");
                    }
                    if let Err(e) = guard.flush() {
                        tracing::debug!("ChildExited flush failed: {e}");
                    }
                }
                return;
            }
        }
    }
}
