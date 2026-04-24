//! Per-connection wire-protocol handler.
//!
//! Stage 1 layers `codemux-wire` over the Stage 0 byte shuttle:
//!
//! 1. **Handshake.** Read a `Hello` frame from the client (with timeout),
//!    validate version, send `HelloAck`. A version mismatch sends an
//!    `Error{VersionMismatch}` frame and closes.
//! 2. **Inbound loop** (background thread). Decode frames as they arrive.
//!    `PtyData` payloads go to the PTY writer; `Resize`, `Signal`, `Ping`,
//!    `Pong` are decoded but not yet acted on (Stage 2 plumbs resize and
//!    signal through to the `Session`; Ping/Pong wires up).
//! 3. **Outbound loop** (calling thread). Wraps each PTY chunk from the
//!    channel as a `PtyData` frame and writes to the socket. On channel
//!    disconnect (child died), sends a `ChildExited` frame with a
//!    placeholder exit code (real code wires up in Stage 2 when the
//!    `Session` plumbs `try_wait` results across).
//!
//! `std::thread::scope` is load-bearing in two ways: the PTY writer and
//! rx channel belong to the `Session` (which outlives any single
//! connection), and the `UnixStream` itself is shared between threads as
//! `&UnixStream` borrows rather than via `try_clone()` (saves two fds and
//! a syscall per connection).

use std::io::{ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use codemux_wire::{self as wire, ErrorCode, Message};
use crossbeam_channel::{Receiver, RecvTimeoutError};

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
/// framed I/O. Borrows `writer` and `rx` from the caller (the `Session`)
/// so both survive across re-attaches. `stream` is taken by value because
/// `run` semantically owns the connection for its lifetime: when the
/// function returns, the socket is closed.
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

    let stop = Arc::new(AtomicBool::new(false));

    thread::scope(|s| -> Result<(), Error> {
        let stop_for_bg = Arc::clone(&stop);
        let stream_ref: &UnixStream = &stream;
        let bg = s.spawn(move || {
            let result = inbound_loop(stream_ref, handshake_buf, writer);
            // Release: we publish the flag; the outbound loop's Acquire
            // load pairs with this. No other shared data is being
            // synchronized, so a fence-only ordering is correct.
            stop_for_bg.store(true, Ordering::Release);
            result
        });

        outbound_loop(stream_ref, rx, &stop);

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
) -> Result<(), Error> {
    let mut tmp = vec![0u8; SOCKET_READ_BUF];
    loop {
        loop {
            match wire::try_decode(&buf) {
                Ok(Some((msg, consumed))) => {
                    buf.drain(..consumed);
                    if !handle_inbound(msg, writer) {
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
fn handle_inbound(msg: Message, writer: &mut (dyn Write + Send)) -> bool {
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
            tracing::debug!("inbound Resize {rows}x{cols} (Stage 2 will apply)");
            true
        }
        Message::Signal(sig) => {
            tracing::debug!("inbound Signal {sig:?} (Stage 2 will forward)");
            true
        }
        Message::Ping { nonce } => {
            tracing::debug!("inbound Ping nonce={nonce} (Stage 2 will Pong)");
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
/// code is a placeholder until Stage 2 plumbs the real value across.
///
/// The frame buffer is reused across iterations; only the per-chunk
/// `Vec<u8>` from the channel is allocation-fresh.
fn outbound_loop(mut stream: &UnixStream, rx: &Receiver<Vec<u8>>, stop: &Arc<AtomicBool>) {
    let mut frame_buf = Vec::with_capacity(SOCKET_READ_BUF + 16);
    while !stop.load(Ordering::Acquire) {
        match rx.recv_timeout(POLL_INTERVAL) {
            Ok(chunk) => {
                frame_buf.clear();
                if let Err(e) = Message::PtyData(chunk).encode_to(&mut frame_buf) {
                    tracing::warn!("PtyData encode failed: {e}");
                    return;
                }
                if let Err(e) = stream.write_all(&frame_buf) {
                    tracing::debug!("socket write failed: {e}");
                    return;
                }
                if let Err(e) = stream.flush() {
                    tracing::debug!("socket flush failed: {e}");
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                // Child died. Send ChildExited as the last frame so the
                // client renders a clean exit instead of a transport
                // error. Placeholder code; Stage 2 will source the real
                // value from `Session::child.try_wait()`.
                frame_buf.clear();
                if (Message::ChildExited { exit_code: 0 })
                    .encode_to(&mut frame_buf)
                    .is_ok()
                {
                    if let Err(e) = stream.write_all(&frame_buf) {
                        tracing::debug!("ChildExited write failed: {e}");
                    }
                    if let Err(e) = stream.flush() {
                        tracing::debug!("ChildExited flush failed: {e}");
                    }
                }
                return;
            }
        }
    }
}
