//! AC-038 (a panic restores the terminal before the report is printed):
//! pin the panic-hook + `TerminalGuard` interaction end to end. Unit
//! tests can prove `TerminalGuard::drop` runs the right escape
//! sequences in the right order, but nothing previously asserted that
//! those sequences actually land BEFORE the color-eyre panic report
//! in the byte stream the user sees.
//!
//! ## What this pins
//!
//! - The runtime installs a panic hook (in `runtime::run`) that calls
//!   `restore_terminal_flags(guard_flags)` BEFORE invoking the upstream
//!   color-eyre hook. Without that wiring, the panic report writes to
//!   stdout while we are still on the alt-screen with mouse capture on,
//!   bracketed paste on, etc. -- and the user sees a stack trace pasted
//!   in the middle of the agent pane instead of on their normal shell.
//! - The unwind path runs the `TerminalGuard`'s `Drop` second; together
//!   they double-stamp the teardown, which is harmless because the
//!   sequences are idempotent on the terminal side.
//!
//! ## How the test triggers the panic
//!
//! The hidden `--panic-after <ms>` CLI seam arms a deadline at the top
//! of the event loop; the next tick after that deadline calls
//! `panic!`. The harness boots codemux with `--panic-after=50`, waits
//! for the child to exit, then inspects the master-side byte stream.
//!
//! ## How we assert "before"
//!
//! `\x1b[?1049l` is the DEC alt-screen-off sequence the
//! `LeaveAlternateScreen` command emits as part of `restore_terminal_flags`.
//! `"The application panicked (crashed)."` is the literal first line
//! of color-eyre's default panic report (see `color_eyre/src/config.rs`
//! around line 800 -- the `"The application panicked (crashed)."`
//! header is styled with `panic_header` and rendered first).
//!
//! Both pieces show up in the raw master-side byte stream. The
//! ordering assertion: the alt-screen-off sequence's byte offset is
//! less than the panic-header's byte offset. If the panic hook is
//! broken (no wrap), the panic report goes out first and the order
//! flips.
//!
//! Gating mirrors the rest of the slow tier: `test-fakes` feature,
//! `#[ignore]`, `#[serial]`.

#![cfg(feature = "test-fakes")]

#[allow(dead_code)]
mod common;

use std::time::Duration;

use serial_test::serial;

use common::{master_bytes_eventually, spawn_codemux_with_args, wait_for_exit};

#[test]
#[ignore = "slow-tier PTY E2E; runs via `just check-e2e` / `just test-e2e`"]
#[serial]
fn panic_restores_terminal_before_color_eyre_report() {
    let agent_bin = env!("CARGO_BIN_EXE_fake_agent");
    // 200 ms gives the runtime time to enter raw mode, render the
    // first frame, and pump a couple of ticks before the deadline
    // fires. A too-tight value risks the panic firing before
    // EnterAlternateScreen lands and AC-038's "restore the terminal"
    // becomes vacuous.
    let mut handle = spawn_codemux_with_args(agent_bin, "", &["--panic-after=200"]);

    // Wait for both signals to appear in the master byte stream: the
    // alt-screen-off escape AND the panic header. Either could in
    // principle land first; the ordering assertion below decides the
    // verdict.
    let bytes = master_bytes_eventually(
        &mut handle,
        |b| {
            byte_index_of(b, b"\x1b[?1049l").is_some()
                && byte_index_of(b, b"The application panicked").is_some()
        },
        Duration::from_secs(10),
    );

    let alt_off =
        byte_index_of(&bytes, b"\x1b[?1049l").expect("alt-screen-off escape on master byte stream");
    let panic_header = byte_index_of(&bytes, b"The application panicked")
        .expect("color-eyre panic header on master byte stream");

    assert!(
        alt_off < panic_header,
        "AC-038: expected `\\x1b[?1049l` (alt-screen-off) at offset {alt_off} to precede \
         color-eyre panic header at offset {panic_header}.\n\
         Without the panic-hook wrap the report writes to stdout while the alt-screen \
         is still up; the user would see a garbled stack trace over their last frame.\n\
         Raw bytes (lossy utf-8): {}",
        String::from_utf8_lossy(&bytes),
    );

    // Belt-and-suspenders: the child process must have actually
    // exited (the panic must propagate, not silently swallow the
    // unwind), and its status must report non-zero. A panic on the
    // main thread yields a non-success exit status; if it ever
    // becomes zero (e.g. someone wires `catch_unwind` around the
    // event loop), this assert is the canary.
    let status = wait_for_exit(&mut handle, Duration::from_secs(5))
        .expect("codemux did not exit within 5s of the panic-after deadline");
    assert!(
        !status.success(),
        "expected non-zero exit after panic-after; got {status:?}"
    );
}

/// Find the byte offset of the first occurrence of `needle` in
/// `haystack`. Std doesn't ship a `Vec<u8>::find` so we inline a small
/// linear scan -- `bytes` is at most a few KB here (single test boot,
/// short panic), so the O(n*m) cost is negligible. Pulled into a
/// helper so the assertion site reads as "expected alt-off before
/// panic-header" rather than nested index arithmetic.
fn byte_index_of(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
