//! AC-037 (non-zero PTY exit transitions Ready -> Crashed, not silent
//! removal): pin the reap-on-dead-transport branch end to end. Unit
//! tests already cover the in-place `mark_crashed` transition
//! (`apps/tui/src/runtime.rs::reap_transitions_ready_with_dead_transport_to_crashed`)
//! and the renderer's red banner for a non-zero exit
//! (`apps/tui/src/runtime.rs::render_agent_pane_paints_red_banner_for_nonzero_exit_code`),
//! but neither drives the full pipeline through a real PTY and a real
//! child process that exits non-zero on its own.
//!
//! ## What this pins
//!
//! - The reap loop's non-zero branch in
//!   [`NavState::reap_dead_transports`]: when an agent's PTY child
//!   exits with a non-zero code, the agent is NOT removed from the
//!   `agents` Vec. It is transitioned to `AgentState::Crashed` in
//!   place, preserving the parser so scrollback access still works
//!   (per AC-017's `nudge_scrollback_moves_offset_on_crashed_agent`
//!   et al.).
//! - The renderer's red banner: `render_crash_banner` paints
//!   `" ✗ session ended (exit 42) — d to dismiss "` on the top row of
//!   the crashed pane. This test asserts the literal `"exit 42"`
//!   substring on the rendered screen, which only `render_crash_banner`
//!   produces -- a regression that silent-removed the agent on
//!   non-zero exit (re-introducing the bug AC-037 was written to
//!   pin) would never paint that string.
//!
//! ## Why we don't use `\x02x` (`KillAgent`) to trigger the crash
//!
//! AC-014's kill chord routes through `nav.kill_focused()`, which
//! synchronously removes the agent from the Vec via `remove_at`. That
//! path is the AC-014 / AC-036 surface -- it does not go through the
//! `reap_dead_transports` branch the AC-037 transition lives on. To
//! actually exercise the reap-on-dead-transport path, the child has
//! to exit on its own with a non-zero status; we cannot fake that
//! from the outside without changing what's being tested.
//!
//! ## How the test triggers the exit
//!
//! `fake_agent_crashing` exits 42 when its stdin delivers the literal
//! line `QUIT`. The test sends `QUIT\r` into the harness's PTY master.
//! Byte flow:
//!
//! 1. `send_keys(..., "QUIT\r")` writes `Q`, `U`, `I`, `T`, `\r` to
//!    the master side of the harness PTY.
//! 2. The slave end (codemux's stdin) sees those bytes as keystrokes;
//!    crossterm reads them and produces `KeyEvent`s.
//! 3. None of `Q`, `U`, `I`, `T`, `\r` is bound to a chord in
//!    `Bindings::default()` while the prefix state machine is `Idle`,
//!    so `dispatch_key` returns `KeyDispatch::Forward(bytes)` for
//!    each.
//! 4. The runtime's Forward arm
//!    (`runtime.rs::KeyDispatch::Forward => transport.write(&bytes)`)
//!    writes those bytes into the focused agent's PTY -- which is
//!    `fake_agent_crashing`'s stdin.
//! 5. The kernel's pty line discipline (canonical mode, default) sees
//!    `\r`, translates it to `\n`, and delivers the buffered line
//!    `QUIT\n` to `read_line`. The fake's `line.trim() == "QUIT"`
//!    fires and the process exits with code 42.
//! 6. codemux's next tick: `reap_dead_transports` sees the dead
//!    transport, observes exit code 42 (non-zero), calls
//!    `mark_crashed(42)`. The pane's state is now `Crashed { parser,
//!    exit_code: 42 }`. The next render tick paints the red banner.
//!
//! ## Why exit code 42 (not 1)
//!
//! Distinctive in the rendered banner. 1 is the catch-all
//! shell-error code; matching on `"exit 1"` in the screen contents
//! would be more permissive than we want, since a future regression
//! that surfaced `exit 1` from some unrelated error path would still
//! pass. `42` is unmistakable -- it can only come from
//! `fake_agent_crashing`'s deliberate `process::exit(42)`.
//!
//! Gating mirrors the rest of the slow tier: `test-fakes` feature,
//! `#[ignore]` so the suite ships through `just check-e2e` only, and
//! `#[serial]` because the PTY harness is not safe to run in parallel.

#![cfg(feature = "test-fakes")]

#[allow(dead_code)]
mod common;

use std::time::Duration;

use serial_test::serial;

use common::{screen_eventually, send_keys, spawn_codemux_with_agent_bin, test_fake_bin};

/// Boot codemux against `fake_agent_crashing`, wait for the prompt to
/// render, send `QUIT\r` through the keymap forward path to make the
/// agent exit with code 42, and assert the rendered screen carries
/// the red crash banner with the literal exit code.
///
/// **Observation strategy:** `render_crash_banner` (in
/// `apps/tui/src/runtime.rs`) is the only call site in the codebase
/// that emits the literal substring `"exit 42"` -- it formats
/// `" ✗ session ended (exit {n}) — {dismiss_label} to dismiss "` with
/// `n = 42` here. A test that asserts `"exit 42"` is on screen is
/// therefore asserting that:
///
/// 1. The reap loop observed the non-zero exit (otherwise
///    `mark_crashed` is never called and the state stays `Ready` --
///    or worse, AC-036's clean-exit path silently removes the agent).
/// 2. The renderer routed through the `AgentState::Crashed` arm
///    (otherwise the banner is never painted).
/// 3. The banner's format string survived (otherwise the substring
///    shifts and the assertion fingerprints the regression).
///
/// **Why we wait for the prompt before sending `QUIT`:** without the
/// gate, the bytes could race a still-spawning agent. The runtime
/// would dispatch the keystrokes through `Forward`, but if the
/// transport's `write` happened before the fake had a stdin reader
/// up, the bytes would be lost in the kernel pipe buffer and the
/// fake would never see `QUIT`. Asserting on the fake's prompt
/// before the chord forces the steady state.
///
/// **Timeout sizing:** 5s for the prompt (cold-cache spawn budget),
/// 5s for the banner (the dead-transport reap is sub-second on a
/// warm box -- the agent's `process::exit` returns immediately, the
/// next runtime tick reads `try_wait` and transitions, the following
/// render tick paints the banner). Generous over realistic budgets
/// to absorb a `cargo clean` first-run without retuning.
#[test]
#[ignore = "slow-tier PTY E2E; runs via `just check-e2e` / `just test-e2e`"]
#[serial]
fn agent_nonzero_exit_renders_crashed_banner() {
    let agent_bin = test_fake_bin("fake_agent_crashing");
    let mut handle = spawn_codemux_with_agent_bin(&agent_bin, "");

    // Steady state: fake's prompt is on screen, no modal is open.
    screen_eventually(
        &mut handle,
        |s| s.contents().contains("FAKE_AGENT_READY"),
        Duration::from_secs(5),
    );

    // Send `QUIT\r` -- the bytes flow through `KeyDispatch::Forward`
    // into the focused agent's PTY (see the file-level doc comment
    // for the full byte path). The fake's `read_line` sees `QUIT\n`
    // after pty canonicalization, matches `line.trim() == "QUIT"`,
    // and exits 42.
    send_keys(&mut handle, "QUIT\r");

    // The reap loop transitions the agent to `Crashed { exit_code: 42 }`
    // and the renderer paints the red banner. The literal `"exit 42"`
    // substring is unique to `render_crash_banner`'s format string
    // for a non-zero exit code.
    let crashed = screen_eventually(
        &mut handle,
        |s| s.contents().contains("exit 42"),
        Duration::from_secs(5),
    );

    assert!(
        crashed.contents().contains("exit 42"),
        "expected crash banner with `exit 42` on screen; got:\n{}",
        crashed.contents()
    );
    // Belt-and-suspenders: the banner also names the dismiss chord.
    // Default `dismiss_agent = d` from `Bindings::default()`. If the
    // default ever flipped, this assert is the canary; the test
    // above still passes on the exit-code substring alone, so this
    // is purely diagnostic.
    assert!(
        crashed.contents().contains("dismiss"),
        "expected crash banner to mention dismissal; got:\n{}",
        crashed.contents()
    );
}
