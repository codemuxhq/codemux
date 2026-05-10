//! PTY harness for the slow-tier TUI E2E tests (`tests/pty_*.rs`).
//!
//! The harness opens a fresh 80x24 PTY pair, spawns the real `codemux`
//! binary into it with `CODEMUX_AGENT_BIN` pointing at the in-tree
//! `fake_agent` stub, and exposes two helpers:
//!
//! - [`spawn_codemux`] — boot the subprocess inside the PTY.
//! - [`screen_eventually`] — feed master-side bytes into a
//!   `vt100::Parser` and poll the resulting `Screen` until a predicate
//!   holds (or a deadline fires).
//!
//! ## Determinism rules
//!
//! These mirror the project-wide rule from the testing plan
//! (`docs/plans/2026-05-10--e2e-testing.md`):
//!
//! - **Never `sleep()` waiting for the screen to settle.** The only
//!   acceptable wait is `screen_eventually`'s polling loop, which
//!   re-checks the predicate after each tiny `yield`-style pause
//!   (sub-millisecond on an idle box, sub-10ms always). On timeout it
//!   panics with the actual rendered screen content so the failure
//!   message is the screen the test was expecting to see.
//! - **Drop kills the child.** Leaving a `codemux` process behind
//!   between tests would have it inherit the next test's PTY and
//!   poison the assertions. `Drop` for [`CodemuxHandle`] kills the
//!   child and waits for it; nothing inside `Drop` is allowed to
//!   panic.
//! - **The fake binary is reached via `CARGO_BIN_EXE_fake_agent`** —
//!   Cargo materializes that env var at compile time once the
//!   `test-fakes` feature is enabled. The whole harness is gated
//!   behind that feature at the test file level (`#![cfg(...)]`); the
//!   `env!` here would fail the build with the feature off.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read, Write};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

const ROWS: u16 = 24;
const COLS: u16 = 80;

/// Owns one in-PTY `codemux` subprocess plus the background reader
/// thread that pumps master-side bytes into the harness channel.
///
/// All fields are private — tests interact through [`spawn_codemux`]
/// and [`screen_eventually`].
pub struct CodemuxHandle {
    /// Master end of the PTY. Test writes (keystrokes) go here; the
    /// reader thread holds a separate clone via `try_clone_reader`.
    master: Box<dyn MasterPty + Send>,
    /// Writer for the master end. Held inside an `Option` so `Drop`
    /// can take it and explicitly drop it before killing the child —
    /// dropping the writer signals EOF on the slave's stdin and lets
    /// the fake agent exit cleanly on the happy path.
    writer: Option<Box<dyn Write + Send>>,
    /// The `codemux` child. `Option` so `Drop` can move it out and
    /// `kill` + `wait` to reap.
    child: Option<Box<dyn Child + Send + Sync>>,
    /// Receiver for byte chunks produced by the background reader
    /// thread. Each `Vec<u8>` is one read of the master end (bounded
    /// by the reader's 4 KiB stack buffer). When the reader exits
    /// (EOF or master gone), its `Sender` drops and `try_recv` will
    /// return `Disconnected` — the natural shutdown signal.
    rx: Receiver<Vec<u8>>,
    /// `vt100` parser fed from `rx` on every poll. Owned here (not in
    /// the reader thread) because `Screen` is borrowed from the parser
    /// and the predicate runs on the test thread.
    parser: vt100::Parser,
    /// Reader thread join handle. Kept so we can join it during `Drop`
    /// after the master is closed; without joining, the thread races
    /// the test runner's process exit and occasionally prints a
    /// `read error` line that pollutes test output.
    reader_handle: Option<thread::JoinHandle<()>>,
}

/// Boot the real `codemux` binary inside an 80x24 PTY against the
/// in-tree `fake_agent` stub.
///
/// The spawned process inherits a sanitized environment:
/// - `CODEMUX_AGENT_BIN` points at the fake.
/// - `TERM` is forced to `xterm-256color` so the runtime's
///   capability detection lands on a known surface (the developer's
///   `kitty` / `wezterm` would otherwise leak in and change the
///   escape sequences emitted, breaking `vt100` assertions).
/// - `HOME` is preserved (codemux reads `~/.cache/codemux/...`); the
///   harness deliberately does NOT redirect `HOME` to a tempdir
///   because the runtime's log file is append-only and isolated per
///   process — no cross-test interference.
///
/// # Panics
///
/// Panics if the PTY can't be opened, the binary can't be spawned, or
/// the master's reader/writer can't be cloned. All three are
/// programmer errors at this layer (the harness is expected to run on
/// a developer box or CI Linux runner with a functioning ptmx); a
/// panic gives a clearer test failure than a `Result` the caller would
/// just unwrap anyway.
pub fn spawn_codemux() -> CodemuxHandle {
    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: ROWS,
            cols: COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    // Build the command. `CARGO_BIN_EXE_codemux` and
    // `CARGO_BIN_EXE_fake_agent` are populated by Cargo at compile
    // time when the test target depends on each `[[bin]]` — the
    // `test-fakes` feature is what makes the second one materialize.
    let codemux_bin = env!("CARGO_BIN_EXE_codemux");
    let fake_bin = env!("CARGO_BIN_EXE_fake_agent");

    let mut cmd = CommandBuilder::new(codemux_bin);
    cmd.env("CODEMUX_AGENT_BIN", fake_bin);
    // Stable terminal capability surface for assertions. See doc above.
    cmd.env("TERM", "xterm-256color");
    // Keep the runtime out of the user's real config: production
    // codepaths read `~/.config/codemux/config.toml`, but the test
    // doesn't care about config — pointing `HOME` at a fresh tempdir
    // would also work, and is the right move once we have config-
    // sensitive PTY tests. For now, the unset `RUST_LOG` keeps logs
    // quiet enough.
    cmd.env_remove("RUST_LOG");
    // Cargo sets `cwd` to the package root for tests; set it
    // explicitly to make the harness independent of that detail.
    if let Ok(cwd) = std::env::current_dir() {
        cmd.cwd(cwd);
    }

    let child = pair.slave.spawn_command(cmd).expect("spawn codemux");

    // Drop the slave end now that the child has it. Holding it would
    // keep the master's reader from ever seeing EOF when the child
    // exits, which makes the reader thread leak.
    drop(pair.slave);

    let writer = pair.master.take_writer().expect("take_writer");
    let mut reader = pair.master.try_clone_reader().expect("clone_reader");

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let reader_handle = thread::Builder::new()
        .name("pty-harness-reader".into())
        .spawn(move || {
            // 4 KiB is the historical Linux pipe buffer size and a
            // reasonable upper bound on what a single PTY read returns.
            let mut chunk = [0u8; 4096];
            loop {
                match reader.read(&mut chunk) {
                    // Send each non-empty read as its own owned
                    // `Vec<u8>`; both `Ok(0)` (EOF) and `Err(_)`
                    // (master gone) end the thread cleanly. A failed
                    // `send` means the receiver is gone — the test
                    // is tearing down, so we exit too.
                    Ok(n) if n > 0 => {
                        if tx.send(Vec::from(&chunk[..n])).is_err() {
                            break;
                        }
                    }
                    _ => break,
                }
            }
        })
        .expect("spawn reader thread");

    CodemuxHandle {
        master: pair.master,
        writer: Some(writer),
        child: Some(child),
        rx,
        // `scrollback_len = 0` — the harness asserts on the visible
        // 80x24 grid only. Tests that need scrollback can grow this
        // later; today none do.
        parser: vt100::Parser::new(ROWS, COLS, 0),
        reader_handle: Some(reader_handle),
    }
}

/// Drain whatever the reader thread has queued into the parser.
/// Returns `true` if the channel is still live (more bytes may arrive),
/// `false` if the reader has dropped its `Sender` (EOF / process gone).
fn drain_into_parser(handle: &mut CodemuxHandle) -> bool {
    loop {
        match handle.rx.try_recv() {
            Ok(chunk) => handle.parser.process(&chunk),
            Err(TryRecvError::Empty) => return true,
            Err(TryRecvError::Disconnected) => return false,
        }
    }
}

/// Poll the parser until `predicate(&Screen)` returns true OR the
/// deadline expires.
///
/// On success: returns the matching screen (cloned out of the parser
/// so the caller can hold it without borrowing `handle`).
///
/// On timeout: panics. The panic message includes the rendered screen
/// contents so the test failure is "here is what the screen looked
/// like when we gave up" instead of an opaque deadline.
///
/// # Panics
///
/// Panics on timeout (intentional — see above).
pub fn screen_eventually<P>(
    handle: &mut CodemuxHandle,
    predicate: P,
    timeout: Duration,
) -> vt100::Screen
where
    P: Fn(&vt100::Screen) -> bool,
{
    let deadline = Instant::now() + timeout;
    loop {
        let live = drain_into_parser(handle);
        if predicate(handle.parser.screen()) {
            return handle.parser.screen().clone();
        }
        // Reader is gone and we've drained everything it ever sent —
        // no further bytes will arrive. One last predicate check
        // happened above; fall through to the timeout/panic branch
        // rather than busy-looping until the deadline.
        if !live || Instant::now() >= deadline {
            let screen = handle.parser.screen().clone();
            let contents = screen.contents();
            panic!(
                "screen_eventually: predicate did not hold within {timeout:?}\n\
                 ----- rendered screen -----\n{contents}\n\
                 ----- end -----"
            );
        }
        // Cheap pause. `yield_now` alone burns a CPU on a Linux box;
        // a 5ms sleep keeps the loop responsive (≤ 0.5% of a 1-second
        // timeout) without hammering. This is the single sleep
        // exception the determinism rule allows: it is bounded, it is
        // a polling backoff, it is not a "wait for the UI to settle"
        // delay.
        thread::sleep(Duration::from_millis(5));
    }
}

impl Drop for CodemuxHandle {
    fn drop(&mut self) {
        // Drop the writer first so the slave's stdin gets EOF — the
        // fake agent will exit on its own, and codemux will then
        // notice the child exited.
        drop(self.writer.take());

        if let Some(mut child) = self.child.take() {
            // Best-effort kill. The child may have already exited
            // (fake agent saw EOF, codemux noticed and bailed); a
            // failed kill on a dead child is fine — `wait` will reap
            // either way. Never panic from `Drop`.
            let _ = child.kill();
            let _ = child.wait();
        }

        // Reader thread depends on the master's reader hitting EOF.
        // Killing the child + dropping the slave (which we did at
        // spawn time) closes the slave end; the master's read returns
        // 0 and the thread exits.
        if let Some(handle) = self.reader_handle.take() {
            // Use a short-bounded join: if anything wedges, we'd
            // rather print a warning than hang the next test. The
            // primitive `join` blocks indefinitely; we don't have a
            // timed-join in `std`, so we trust the EOF path. If this
            // ever flakes we add a self-pipe wakeup.
            let _ = handle.join();
        }

        // `master` drops here implicitly — closing the master FD,
        // releasing the kernel-side pty pair.
        // (Field is in `self`; no `take` needed.)
        let _ = &self.master;
    }
}
