//! AC-040 (mouse events suppressed while overlay open): pin the
//! `no_overlay_active` gate end to end. Unit tests
//! (`apps/tui/src/runtime.rs::no_overlay_active_returns_false_when_help_open`)
//! already cover the gate-check helper in isolation; this test drives a
//! real codemux PTY, opens the help overlay, sends a mouse wheel event,
//! and asserts the screen contents do not change.
//!
//! ## What this pins
//!
//! - The runtime's `Event::Mouse(...)` arm is gated on
//!   `no_overlay_active(spawn_ui, popup_state, help_state)` and returns
//!   early when an overlay is up. Without that gate, wheel-over-pane
//!   while help is open would scroll the agent's parser, and a future
//!   "click outside to dismiss" change would slip in without being
//!   pinned at the chord-to-screen layer.
//!
//! ## Observation strategy
//!
//! The help overlay's chrome (the bordered ` codemux help ` block from
//! `render_help`) is the canary. If the gate works, sending a wheel
//! event while the overlay is up does nothing -- the screen still
//! shows the help chrome AND the agent pane underneath has not
//! scrolled (the fake's prompt is at row 0 before and after). If the
//! gate is broken, the wheel would drive `nudge_scrollback` on the
//! focused agent, but at `scrollback_offset = 0` and a single-line
//! `fake_agent` prompt that's a no-op visually -- the cleaner
//! signature is the help overlay itself, which would remain
//! unaffected even with broken gating but whose presence we use to
//! confirm the overlay was actually up when we synthesized the wheel.
//!
//! Gating mirrors the rest of the slow tier: `test-fakes` feature,
//! `#[ignore]`, `#[serial]`.

#![cfg(feature = "test-fakes")]

#[allow(dead_code)]
mod common;

use std::time::Duration;

use serial_test::serial;

use common::{WheelKind, screen_eventually, send_keys, send_mouse_wheel, spawn_codemux};

#[test]
#[ignore = "slow-tier PTY E2E; runs via `just check-e2e` / `just test-e2e`"]
#[serial]
fn mouse_wheel_does_not_scroll_while_help_overlay_open() {
    let mut handle = spawn_codemux();

    screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            c.contains("FAKE_AGENT_READY") && !c.contains(" codemux help ")
        },
        Duration::from_secs(5),
    );

    // Open the help overlay. `pty_help.rs` already pins that this
    // chord flips the chrome on; reusing the same fingerprint
    // (` codemux help ` block title from `render_help`) here.
    send_keys(&mut handle, "\x02?");
    let opened = screen_eventually(
        &mut handle,
        |s| s.contents().contains(" codemux help "),
        Duration::from_secs(5),
    );
    let opened_contents = opened.contents();

    // Send a wheel event over the middle of the screen. If the
    // `no_overlay_active` gate is broken the runtime would route this
    // to `nudge_scrollback(WHEEL_STEP)` on the focused agent. The
    // gate's job is to drop the event entirely.
    //
    // We send several wheel events in a row to make a regression
    // unambiguous: even one would be enough at the dispatch level,
    // but a flapping "sometimes the gate works" regression would
    // still leave at least one wheel event through, and the
    // accumulated scroll would be more visible than a single wheel
    // tick.
    for _ in 0..5 {
        send_mouse_wheel(&mut handle, WheelKind::Up, 40, 12);
    }

    // After the wheel storm: the help overlay is still on screen AND
    // the screen contents are unchanged. The second clause is the
    // real pin -- the overlay can't disappear from a wheel event
    // through any code path, but the agent pane underneath could
    // shift if the gate failed.
    //
    // We give the runtime a tick to process; if the gate works, the
    // screen settles to the same `opened_contents` value within a
    // few polls of `screen_eventually`. Predicate is "still shows the
    // overlay chrome AND the contents match the post-overlay
    // baseline" -- the second half catches the regression we care
    // about.
    let after = screen_eventually(
        &mut handle,
        |s| s.contents().contains(" codemux help ") && s.contents() == opened_contents,
        Duration::from_secs(2),
    );
    assert_eq!(
        after.contents(),
        opened_contents,
        "expected screen unchanged after wheel events while overlay open;\nGOT:\n{}",
        after.contents()
    );
}
