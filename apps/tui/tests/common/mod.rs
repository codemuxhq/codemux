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
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system};
use tempfile::TempDir;

/// Wheel direction for [`send_mouse_wheel`]. Mirrors the two SGR mouse
/// "buttons" 64 (up) and 65 (down) that crossterm decodes as
/// `MouseEventKind::ScrollUp` / `ScrollDown` (see crossterm's
/// `parse_cb`).
#[derive(Clone, Copy, Debug)]
pub enum WheelKind {
    Up,
    Down,
}

/// Mouse button for [`send_mouse_click`] / [`send_mouse_drag`].
/// `Left { ctrl }` exposes the Ctrl modifier (SGR bit `0b0001_0000`)
/// because Ctrl+Left is the gesture that opens a URL through the
/// runtime's hover-and-open path (AC-041); the harness exposes it as a
/// field on `Left` so the call sites read like the gesture they pin
/// (`MouseButton::Left { ctrl: true }`) rather than a follow-on
/// modifier argument every caller has to remember to set to `false`.
#[derive(Clone, Copy, Debug)]
pub enum MouseButton {
    Left { ctrl: bool },
    Middle,
    Right,
}

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
    /// Per-test `HOME` redirect. Shields the spawned codemux from the
    /// developer's real `~/.claude/settings.json` — the `agent_meta`
    /// worker reads it for the focused agent's model/effort and renders
    /// the value into the status-bar segment, which shrinks the tab
    /// strip's left area. On a developer box (with a populated
    /// settings.json), the segment can appear mid-test and truncate
    /// tab labels, breaking AC-020's ordinal-to-label assertion in
    /// `pty_tab_drag.rs`. Held here so the dir lives as long as the
    /// child does; dropped after the child is reaped. The
    /// `~`-expansion test in `pty_quick_switch.rs` reads this path
    /// via [`home_path`] so it asserts on the same value codemux sees.
    home: TempDir,
    /// Master end of the PTY. Held purely so its `Drop` runs at
    /// teardown (closing the master FD, releasing the kernel-side pty
    /// pair); the reader thread already has its own clone via
    /// `try_clone_reader`, and the writer is taken out separately.
    /// Underscore-prefixed so dead-code analysis recognizes the
    /// drop-only intent — same pattern as `_xdg_home`.
    _master: Box<dyn MasterPty + Send>,
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
    /// Raw bytes seen on the master side, kept independently of the
    /// vt100 parser so tests can assert on escape sequences the parser
    /// has already consumed (e.g. the OSC 52 clipboard write pinned by
    /// AC-021, or the alt-screen-exit / panic-report ordering pinned
    /// by AC-038). Appended to by the reader thread; drained for
    /// inspection on the test thread via [`master_bytes_eventually`].
    master_log: Arc<Mutex<Vec<u8>>>,
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
/// - `HOME` is redirected to a fresh tempdir so the developer's
///   `~/.claude/settings.json` cannot leak in: the `agent_meta` worker
///   reads `model`/`effortLevel` from it and renders them into the
///   status-bar segment, which shrinks the tab-strip area and
///   truncates labels mid-test (AC-020's `pty_tab_drag.rs` was the
///   first test to surface this). The codemux runtime's log file
///   moves into the tempdir alongside, which is harmless — each test
///   gets a fresh log dir that the `Drop` cleans up. The `~`-expansion
///   test in `pty_quick_switch.rs` reads `$HOME` at runtime so it
///   picks up the redirected value automatically.
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
    spawn_codemux_with_config("")
}

/// Same as [`spawn_codemux`] but with a `config.toml` written into the
/// per-test XDG tempdir before the codemux subprocess boots.
///
/// Use this whenever a test needs to override defaults that the modal
/// or runtime would otherwise resolve against the developer's real
/// machine — e.g. `[spawn] scratch_dir` (default `~/.codemux/scratch`,
/// which a PTY spawn test would otherwise create on the dev box).
///
/// `extra` is the body of the config file. Pass `""` for "no config"
/// behavior (equivalent to [`spawn_codemux`]). The harness does not
/// add any defaults of its own; whatever the caller passes is what
/// codemux sees.
///
/// The XDG tempdir is owned by the returned handle and dropped when
/// it drops, so the config file disappears alongside the child.
pub fn spawn_codemux_with_config(extra: &str) -> CodemuxHandle {
    spawn_codemux_with_agent_bin(env!("CARGO_BIN_EXE_fake_agent"), extra)
}

/// Boot codemux against an arbitrary agent binary path, with an
/// optional `config.toml` body written into the per-test XDG tempdir.
///
/// The default flow is [`spawn_codemux`] / [`spawn_codemux_with_config`],
/// both of which point `CODEMUX_AGENT_BIN` at the happy-path
/// `fake_agent`. This helper is the carve-out for tests that need a
/// different stub binary — today that means
/// `pty_crash::agent_nonzero_exit_renders_crashed_banner` pointing at
/// `fake_agent_crashing` to drive the Ready -> Crashed transition end
/// to end (AC-037).
///
/// `agent_bin` is the absolute path to the stub binary. Callers are
/// expected to use `env!("CARGO_BIN_EXE_<name>")` so cargo materializes
/// the path at compile time; passing an arbitrary string here would
/// drop the build-time guarantee that the binary actually exists.
///
/// `extra` follows the same contract as [`spawn_codemux_with_config`]:
/// empty means no config file, non-empty is written verbatim into
/// `$XDG_CONFIG_HOME/codemux/config.toml` before the subprocess boots.
///
/// # Panics
///
/// Same as [`spawn_codemux`].
pub fn spawn_codemux_with_agent_bin(agent_bin: &str, extra: &str) -> CodemuxHandle {
    spawn_codemux_with_args(agent_bin, extra, &[])
}

/// Boot codemux with arbitrary extra CLI args (in addition to the
/// agent-bin env var and the per-test XDG config). Used by AC-038 and
/// AC-041 PTY tests to drive the hidden `--panic-after` and
/// `--record-opens-to` seams; production code never sees this path.
///
/// `extra_args` is the literal argv tail appended after `codemux`. The
/// harness does not validate the arguments -- clap inside codemux
/// will reject anything it doesn't recognise and exit non-zero, which
/// the test observes as a child-process exit before its
/// `screen_eventually` fires.
pub fn spawn_codemux_with_args(
    agent_bin: &str,
    extra_config: &str,
    extra_args: &[&str],
) -> CodemuxHandle {
    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: ROWS,
            cols: COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let codemux_bin = env!("CARGO_BIN_EXE_codemux");

    let mut cmd = CommandBuilder::new(codemux_bin);
    for arg in extra_args {
        cmd.arg(arg);
    }
    cmd.env("CODEMUX_AGENT_BIN", agent_bin);
    cmd.env("TERM", "xterm-256color");
    // Empty XDG config dir: prevents the developer's
    // `~/.config/codemux/config.toml` from rebinding the prefix or any
    // other binding the test exercises. `config::config_path` reads
    // `XDG_CONFIG_HOME` first and only falls back to `$HOME/.config`
    // when XDG is unset/empty, so this single env var fully shields
    // the spawned codemux from user-side config.
    let xdg_home = TempDir::new().expect("tempdir for XDG_CONFIG_HOME");
    if !extra_config.is_empty() {
        let codemux_subdir = xdg_home.path().join("codemux");
        std::fs::create_dir_all(&codemux_subdir).expect("mkdir XDG/codemux");
        std::fs::write(codemux_subdir.join("config.toml"), extra_config)
            .expect("write config.toml into XDG tempdir");
    }
    cmd.env("XDG_CONFIG_HOME", xdg_home.path());
    // Per-test HOME shielding: see the doc comment on `spawn_codemux`.
    // The dir is empty — `current_model_and_effort` will fail to find
    // `~/.claude/settings.json` and the model status segment stays
    // hidden, leaving the tab strip's left area at full width.
    let home = TempDir::new().expect("tempdir for HOME");
    cmd.env("HOME", home.path());
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
    let master_log: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let reader_log = Arc::clone(&master_log);
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
                        // Mirror the chunk into the raw byte log
                        // before forwarding to the parser channel.
                        // Lock contention is bounded by the number of
                        // tests running serially (one), so the lock
                        // is uncontended in practice. A poisoned lock
                        // is benign here — we drop the bytes on the
                        // floor and let the test fail loud via
                        // `master_bytes_eventually`'s panic.
                        if let Ok(mut log) = reader_log.lock() {
                            log.extend_from_slice(&chunk[..n]);
                        }
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
        home,
        _master: pair.master,
        writer: Some(writer),
        child: Some(child),
        rx,
        // `scrollback_len = 0` — the harness asserts on the visible
        // 80x24 grid only. Tests that need scrollback can grow this
        // later; today none do.
        parser: vt100::Parser::new(ROWS, COLS, 0),
        master_log,
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

/// Path of the per-test `HOME` redirect set on the spawned codemux's
/// environment. Tests that need to assert on `~`-expansion behavior
/// must read this rather than the test process's own `HOME` — the two
/// no longer match (see the doc comment on [`spawn_codemux`] for why
/// the harness redirects). First consumer:
/// `pty_quick_switch::tilde_or_slash_in_fuzzy_modal_switches_to_precise_mode`,
/// which asserts the rendered path contains the expanded `$HOME/`
/// after typing `~`.
pub fn home_path(handle: &CodemuxHandle) -> &std::path::Path {
    handle.home.path()
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

/// Poll the raw master-side byte stream until `predicate(&[u8])`
/// returns true OR the deadline expires.
///
/// Parallel to [`screen_eventually`] but exposes the byte buffer
/// rather than the parsed `vt100::Screen`. Use when the test needs to
/// assert on a sequence the parser already consumed -- e.g. the OSC 52
/// clipboard write that AC-021 pins (the bytes go through vt100 as a
/// "no-op" escape; nothing lands on the cell grid), or the alt-screen
/// exit / panic-report ordering AC-038 pins (the byte order is the
/// whole point).
///
/// Returns a cloned snapshot of the byte buffer on success so the
/// caller can hold it without keeping the lock or borrowing `handle`.
///
/// # Panics
///
/// Panics on timeout (intentional, like `screen_eventually`).
pub fn master_bytes_eventually<P>(
    handle: &mut CodemuxHandle,
    predicate: P,
    timeout: Duration,
) -> Vec<u8>
where
    P: Fn(&[u8]) -> bool,
{
    let deadline = Instant::now() + timeout;
    loop {
        let live = drain_into_parser(handle);
        let snapshot = handle
            .master_log
            .lock()
            .expect("master byte log poisoned")
            .clone();
        if predicate(&snapshot) {
            return snapshot;
        }
        assert!(
            live && Instant::now() < deadline,
            "master_bytes_eventually: predicate did not hold within {:?}\n\
             ----- raw master bytes ({} bytes) -----\n{}\n----- end -----",
            timeout,
            snapshot.len(),
            String::from_utf8_lossy(&snapshot),
        );
        thread::sleep(Duration::from_millis(5));
    }
}

/// Drain the master byte log into a cloned snapshot without polling
/// for a predicate. Use after [`wait_for_exit`] has reaped the child:
/// at that point the byte stream is closed and finite, so a one-shot
/// read is enough. The polling shape of [`master_bytes_eventually`]
/// is the wrong tool for a post-mortem assertion -- repurposing it
/// with a `|_| true` predicate works but obfuscates "I am reading a
/// finalized log, not waiting for live bytes."
///
/// First consumers: `pty_config_invalid::invalid_config_exits_before_raw_mode`
/// and `pty_arg_invalid::invalid_*_arg_exits_*` (AC-030 / AC-031), which
/// assert the absence of `\x1b[?1049h` after the child has bailed out
/// before raw-mode entry.
pub fn master_bytes_snapshot(handle: &mut CodemuxHandle) -> Vec<u8> {
    // One final drain so any bytes the reader thread had buffered but
    // not yet pushed through the parser channel make it into the log.
    // `drain_into_parser` runs the parser too, but that's harmless --
    // the parser owns its own state, and the byte log is the
    // independent source of truth this helper returns.
    let _ = drain_into_parser(handle);
    handle
        .master_log
        .lock()
        .expect("master byte log poisoned")
        .clone()
}

/// Send an SGR mouse wheel event at the given 1-based cell coordinate.
///
/// SGR mouse format (DEC 1006, `?1006h`): `ESC [ < Cb ; Cx ; Cy M`.
/// Wheel events always end with `M` (wheel has no separate release;
/// crossterm decodes Cb=4 as `ScrollUp` and Cb=5 as `ScrollDown`,
/// regardless of the trailing `M`/`m`).
///
/// `x` / `y` are 1-based screen cells -- the same coordinate space the
/// SGR encoding speaks. The runtime's wheel handler ignores the
/// position (wheel-anywhere scrolls the focused agent; see
/// `runtime.rs::3677-3686`), so the harness allows callers to pass any
/// in-bounds cell. Picking the center of the agent pane is the safest
/// default for tests that don't care about the position.
pub fn send_mouse_wheel(handle: &mut CodemuxHandle, kind: WheelKind, x: u16, y: u16) {
    let cb = match kind {
        WheelKind::Up => 64,
        WheelKind::Down => 65,
    };
    write_sgr_mouse(handle, cb, x, y, 'M');
}

/// Send a press + release pair at the given 1-based cell coordinate.
///
/// Wired through [`MouseButton`] so the Ctrl modifier (for Ctrl+Click
/// on a URL, AC-041) is set on the `Left` variant. The encoding is two
/// SGR frames: a press (`M`) followed by a release (`m`); crossterm
/// derives the released button from the trailing case (lowercase `m`
/// means release; the Cb byte still names the button).
///
/// For drag-to-select, see [`send_mouse_drag`] -- a press + drag-motion
/// stream + release is a different sequence and SGR requires explicit
/// motion frames in between.
pub fn send_mouse_click(handle: &mut CodemuxHandle, button: MouseButton, x: u16, y: u16) {
    let cb_press = button_to_cb(button);
    write_sgr_mouse(handle, cb_press, x, y, 'M');
    // Release shares the same Cb (carries the modifier bits) but is
    // distinguished by the trailing `m` -- crossterm's `parse_csi_sgr_mouse`
    // flips `Down(b)` into `Up(b)` when it sees the lowercase tail.
    // We keep the same Cb (modifiers preserved) so the runtime sees the
    // same modifier state on press and release; differing modifiers
    // across the gesture would be a misuse of the SGR surface.
    write_sgr_mouse(handle, cb_press, x, y, 'm');
}

/// Send a press at `from`, a motion frame at `to`, then a release at
/// `to`. Used by AC-020 (drag-tab-to-reorder) and AC-021
/// (drag-to-select inside the agent pane).
///
/// SGR motion-with-button frames set the "dragging" bit
/// (`0b0010_0000` = 32) in Cb; the trailing terminator is `M` (motion
/// is a tracking event, not a release). One intermediate motion frame
/// at the destination is enough to make the runtime see a `Drag`
/// event; the unit tests in `runtime.rs` already pin that the
/// commit-on-release path doesn't depend on the count of in-flight
/// motion frames.
///
/// `from` / `to` are 1-based cells, matching [`send_mouse_click`] and
/// [`send_mouse_wheel`].
pub fn send_mouse_drag(
    handle: &mut CodemuxHandle,
    button: MouseButton,
    from: (u16, u16),
    to: (u16, u16),
) {
    let cb_press = button_to_cb(button);
    let cb_drag = cb_press | 0b0010_0000;
    write_sgr_mouse(handle, cb_press, from.0, from.1, 'M');
    write_sgr_mouse(handle, cb_drag, to.0, to.1, 'M');
    write_sgr_mouse(handle, cb_press, to.0, to.1, 'm');
}

/// Send a single Ctrl-held motion frame at the given 1-based cell.
/// Drives the Ctrl+hover URL underline / cursor-shape path (AC-041)
/// without arming a drag selection -- Cb = 3 (the "button 3 + motion"
/// encoding crossterm decodes as `MouseEventKind::Moved`) with the
/// Ctrl bit (16) and the motion bit (32) set: 3 | 32 | 16 = 51.
pub fn send_mouse_ctrl_hover(handle: &mut CodemuxHandle, x: u16, y: u16) {
    // Cb = 3 (button-3 release / motion-no-button) | dragging (32) | ctrl (16)
    write_sgr_mouse(handle, 3 | 0b0010_0000 | 0b0001_0000, x, y, 'M');
}

/// Translate a [`MouseButton`] into the SGR Cb byte for a press event.
/// Drag and release events reuse this Cb and twiddle the dragging
/// bit / terminator case respectively.
fn button_to_cb(button: MouseButton) -> u8 {
    match button {
        MouseButton::Left { ctrl: false } => 0,
        MouseButton::Left { ctrl: true } => 0b0001_0000,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

/// Write one SGR mouse frame to the master. The trailing byte is `M`
/// for press / wheel / motion frames and `m` for release frames; the
/// caller picks which.
fn write_sgr_mouse(handle: &mut CodemuxHandle, cb: u8, x: u16, y: u16, terminator: char) {
    let writer = handle
        .writer
        .as_mut()
        .expect("write_sgr_mouse called after writer was taken (Drop)");
    // `ESC [ < Cb ; Cx ; Cy <M|m>`, written straight to the master so the
    // drag helper's three frames don't each round-trip through a String.
    write!(writer, "\x1b[<{cb};{x};{y}{terminator}").expect("write SGR mouse");
    writer.flush().expect("flush SGR mouse");
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

        // `_master` drops here implicitly when `self` falls out of
        // scope, closing the master FD and releasing the kernel-side
        // pty pair.
    }
}
