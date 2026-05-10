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

// Test helpers panic on setup failure; `expect("...")` gives the
// clearest possible failure message before any assertion runs. The
// workspace `clippy.toml` enables `allow-unwrap-in-tests` /
// `allow-expect-in-tests`, but those flags only cover `#[test]` /
// `#[cfg(test)]` scopes — free-floating helpers in an integration
// test crate fall outside the carve-out, so the allow stays at
// file scope here.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read, Write};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system};
use tempfile::TempDir;

const ROWS: u16 = 24;
const COLS: u16 = 80;

/// Owns one in-PTY `codemux` subprocess plus the background reader
/// thread that pumps master-side bytes into the harness channel.
///
/// All fields are private — tests interact through [`spawn_codemux`]
/// and [`screen_eventually`].
pub struct CodemuxHandle {
    /// Empty `XDG_CONFIG_HOME` shielding the spawned codemux from the
    /// developer's real `~/.config/codemux/config.toml` (which would
    /// otherwise change the default prefix-key chord, the navigator
    /// style, and other binding-sensitive surfaces). Held here so the
    /// dir lives as long as the child does; dropped after the child
    /// is reaped in `Drop`.
    _xdg_home: TempDir,
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
/// - `XDG_CONFIG_HOME` is redirected to a fresh tempdir so the
///   developer's `~/.config/codemux/config.toml` cannot leak into the
///   test (it would otherwise rebind the prefix key, navigator chrome,
///   and any other binding-sensitive surface — see AC-013's
///   `pty_nav.rs`, the first config-sensitive PTY test).
/// - `HOME` is preserved (codemux's log path resolves under it); the
///   harness deliberately does NOT redirect `HOME` because the
///   runtime's log file is append-only and isolated per process — no
///   cross-test interference. The `XDG_CONFIG_HOME` redirect already
///   covers the config-loading concern that motivated worrying about
///   `HOME` in the first place.
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
    // Empty XDG config dir: prevents the developer's
    // `~/.config/codemux/config.toml` from rebinding the prefix or any
    // other binding the test exercises. `config::config_path` reads
    // `XDG_CONFIG_HOME` first and only falls back to `$HOME/.config`
    // when XDG is unset/empty, so this single env var fully shields
    // the spawned codemux from user-side config.
    let xdg_home = TempDir::new().expect("tempdir for XDG_CONFIG_HOME");
    cmd.env("XDG_CONFIG_HOME", xdg_home.path());
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
        _xdg_home: xdg_home,
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

/// Write `keys` to the PTY master so codemux receives them as if the
/// user typed. Newlines are NOT implicit; pass `"\r"` or `"\n"`
/// explicitly. ASCII control chars (e.g. `"\x02"` for Ctrl-B) work.
///
/// First consumer: `pty_nav::chrome_flips_from_popup_to_leftpane_on_prefix_v`
/// (AC-013, the prefix-chord toggle dispatch).
///
/// # Panics
///
/// Panics if the master writer has been taken (only happens during
/// `Drop`) or if the underlying write/flush fails. Both are programmer
/// errors at this layer — see the rationale on [`spawn_codemux`].
pub fn send_keys(handle: &mut CodemuxHandle, keys: &str) {
    let writer = handle
        .writer
        .as_mut()
        .expect("send_keys called after writer was taken (Drop)");
    writer.write_all(keys.as_bytes()).expect("write_all to PTY");
    writer.flush().expect("flush PTY writer");
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

/// Wait up to `timeout` for the spawned `codemux` process to exit on
/// its own. Returns the exit status if the child reaps within the
/// deadline, `None` on timeout.
///
/// Polls `try_wait` non-destructively so the child stays in the handle
/// for `Drop` to reap; running `Drop` after a successful
/// `wait_for_exit` is harmless because `kill` / `wait` on an
/// already-reaped child are silent at the `portable_pty` layer.
///
/// While polling, drains the master-side byte stream into the parser.
/// The reader thread keeps queuing during teardown (the
/// `TerminalGuard` Drop emits `?1049l` and friends) and an unattended
/// channel would balloon — small concern for one short test, but a
/// real one as the slow tier grows.
///
/// First consumer: `pty_lifecycle::kill_last_agent_auto_exits_codemux`
/// (AC-014 / AC-036, the kill-chord → empty-vec → return-Ok path).
///
/// # Panics
///
/// Panics if the handle's `child` slot has already been taken (only
/// happens during `Drop`).
pub fn wait_for_exit(handle: &mut CodemuxHandle, timeout: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        // Keep the reader-thread channel from filling up. Predicate
        // matching is not the goal here, just steady drainage.
        let _ = drain_into_parser(handle);

        let child = handle
            .child
            .as_mut()
            .expect("wait_for_exit called after child was taken (Drop)");
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }

        if Instant::now() >= deadline {
            return None;
        }
        // Same backoff cadence as `screen_eventually`: bounded polling,
        // not a "wait for the UI" delay.
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
