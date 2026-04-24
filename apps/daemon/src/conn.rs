//! Per-connection byte shuttle.
//!
//! Stage 0 keeps this dumb: socket reads land in the PTY writer, PTY output
//! lands in the socket. No protocol framing, no resize messages, no signals
//! — Stage 1 layers `codemux-wire` on top of this same I/O shape.
//!
//! Threading: one background thread reads from the socket and feeds the PTY;
//! the calling thread drains the PTY output channel and writes to the
//! socket. When either direction ends (client EOF, child died, or socket
//! write fails), an `AtomicBool` plus a socket `Shutdown::Both` wake the
//! other side so both can exit. The conn function returns once both
//! directions have wound down.
//!
//! The shuttle uses `std::thread::scope` so it can borrow the PTY writer
//! and channel from the caller (the `Session`) instead of consuming them.
//! That's load-bearing: the PTY must outlive the connection so the next
//! client can re-attach to the same child.

use std::io::{Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError};

use crate::error::Error;

/// Read chunk size for the socket → PTY direction.
const SOCKET_READ_BUF: usize = 8 * 1024;

/// Polling cadence for the PTY → socket direction. The poll exists so the
/// loop can notice when the peer thread set the stop flag without having
/// to wait for the next PTY chunk to arrive (which may be never if the
/// child is idle).
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Shuttle bytes between `stream` and the PTY pieces (`writer`, `rx`)
/// until either side winds down. Borrows `writer` and `rx` so the caller
/// (`Session`) retains ownership across re-attaches.
pub fn run(
    stream: UnixStream,
    writer: &mut (dyn Write + Send),
    rx: &Receiver<Vec<u8>>,
) -> Result<(), Error> {
    // Three handles to the same socket: read for the bg thread, write for
    // the main loop, signal for the post-loop wake-up. `Shutdown::Both` on
    // any handle affects the underlying socket.
    let read_stream = stream.try_clone()?;
    let write_stream = stream.try_clone()?;
    let signal_stream = stream;

    let stop = Arc::new(AtomicBool::new(false));

    thread::scope(|s| -> Result<(), Error> {
        let stop_for_bg = Arc::clone(&stop);
        let bg = s.spawn(move || {
            let result = socket_to_pty(read_stream, writer);
            // Release: the only thing the reader thread observes is this
            // flag. There is no associated data to publish; a release
            // store paired with the acquire load below is the textbook
            // single-flag pattern.
            stop_for_bg.store(true, Ordering::Release);
            result
        });

        pty_to_socket(write_stream, rx, &stop);

        // Main loop ended: wake the bg thread (which may still be blocked
        // on `read`). Best-effort — if the socket is already torn down,
        // shutdown returns NotConnected and we don't care.
        let _ = signal_stream.shutdown(Shutdown::Both);

        bg.join().map_err(|_| Error::Io {
            source: std::io::Error::other("socket-to-pty thread panicked"),
        })?
    })
}

fn socket_to_pty(mut stream: UnixStream, writer: &mut (dyn Write + Send)) -> Result<(), Error> {
    let mut buf = vec![0u8; SOCKET_READ_BUF];
    loop {
        match stream.read(&mut buf) {
            // EOF: client closed its write side. Normal end of conn.
            Ok(0) => return Ok(()),
            Ok(n) => {
                if let Err(e) = writer.write_all(&buf[..n]) {
                    // PTY closed underneath us — the child probably died.
                    // End the conn cleanly; the supervisor will detect the
                    // dead child on the next accept.
                    tracing::debug!("pty write failed: {e}");
                    return Ok(());
                }
                // Best-effort flush. Most PTY writers are unbuffered so this
                // is a no-op, but it's cheap insurance for buffered impls.
                let _ = writer.flush();
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => {
                // Includes the case where the main thread shut down the
                // socket to wake us. Treat as normal end of conn.
                tracing::debug!("socket read failed: {e}");
                return Ok(());
            }
        }
    }
}

fn pty_to_socket(mut stream: UnixStream, rx: &Receiver<Vec<u8>>, stop: &Arc<AtomicBool>) {
    while !stop.load(Ordering::Acquire) {
        match rx.recv_timeout(POLL_INTERVAL) {
            Ok(chunk) => {
                if let Err(e) = stream.write_all(&chunk) {
                    tracing::debug!("socket write failed: {e}");
                    return;
                }
                let _ = stream.flush();
            }
            Err(RecvTimeoutError::Timeout) => {}
            // Sender (PTY reader thread) hung up — child died.
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}
