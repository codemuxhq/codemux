//! AC-017 (enter scroll mode and navigate history) and AC-018 (typing
//! snaps to live; navigation preserves scroll): pin the mouse-wheel
//! entry path, the keyboard snap-back, and the per-agent offset
//! preservation across focus switches.
//!
//! Unit tests already cover the per-method invariants
//! (`nudge_scrollback_*`, `jump_to_top_*`, `snap_to_live_*`,
//! `scrollback_state_is_per_parser`). What no existing test could
//! pin: entering scroll mode through the real `Event::Mouse` branch,
//! and verifying that `prefix N` navigation chords do NOT trigger a
//! snap-to-live (only forwarded bytes do).
//!
//! Sibling file `pty_paste_snaps.rs` covers AC-039 (paste snaps to
//! live), sharing the same `fake_agent_with_history` fixture.
//!
//! Gating mirrors the rest of the slow tier: `test-fakes` feature,
//! `#[ignore]`, `#[serial]`.

#![cfg(feature = "test-fakes")]

#[allow(dead_code)]
mod common;

use std::time::Duration;

use serial_test::serial;

use common::{
    WheelKind, screen_eventually, send_keys, send_mouse_wheel, spawn_codemux_with_agent_bin,
    test_fake_bin,
};

/// AC-017: wheel-up enters scroll mode (badge appears, visible rows
/// shift up); subsequent wheel-up ticks deepen the scroll; the
/// keyboard `G` chord snaps back to live.
///
/// The history fake pre-loads 200 `HISTORY N` lines before its
/// prompt. With the visible 24-row grid we see `HISTORY 177` through
/// `HISTORY 199` plus the prompt at the bottom. Wheel-up shifts the
/// window into the buffer; one wheel tick moves the view by
/// `WHEEL_STEP = 3` rows.
#[test]
#[ignore = "slow-tier PTY E2E; runs via `just check-e2e` / `just test-e2e`"]
#[serial]
fn wheel_up_enters_scroll_mode_and_g_capital_snaps_to_live() {
    let agent_bin = test_fake_bin("fake_agent_with_history");
    let mut handle = spawn_codemux_with_agent_bin(&agent_bin, "");

    // Steady state: the prompt is on screen (it's the last byte
    // written, so seeing it confirms the 200-line history has fully
    // flushed into the parser).
    let settled = screen_eventually(
        &mut handle,
        |s| s.contents().contains("FAKE_AGENT_READY"),
        Duration::from_secs(5),
    );
    // Sanity: no scroll badge yet -- the indicator only paints when
    // `scrollback_offset > 0`.
    assert!(
        !settled.contents().contains("scroll"),
        "expected no scroll badge before wheel; got:\n{}",
        settled.contents()
    );

    // Wheel up over the pane. The exact column / row doesn't matter
    // -- the runtime's wheel handler is position-blind (see
    // `runtime.rs::3677`). Pick the middle of the pane.
    send_mouse_wheel(&mut handle, WheelKind::Up, 40, 12);

    // Scroll badge appears at the bottom-right of the pane. The
    // text is ` ↑ scroll N · esc ` (see `render_scroll_indicator`).
    // We assert the static suffix ` · esc ` rather than the exact
    // offset count -- the number can drift if WHEEL_STEP changes,
    // but the suffix is constant.
    let scrolled_once = screen_eventually(
        &mut handle,
        |s| s.contents().contains("· esc"),
        Duration::from_secs(2),
    );

    // Capture the wheel-1 offset out of the badge text so we can
    // compare against the wheel-2 offset below.
    let offset_after_one =
        parse_offset(&scrolled_once.contents()).expect("badge must include an integer offset");
    assert!(
        offset_after_one > 0,
        "expected offset > 0 after wheel; got {offset_after_one}",
    );

    // Wheel up again -- offset must grow. Pins that wheel-while-
    // scrolled-back deepens the scroll, not just toggles it.
    send_mouse_wheel(&mut handle, WheelKind::Up, 40, 12);
    let scrolled_twice = screen_eventually(
        &mut handle,
        |s| parse_offset(&s.contents()).is_some_and(|n| n > offset_after_one),
        Duration::from_secs(2),
    );
    let offset_after_two =
        parse_offset(&scrolled_twice.contents()).expect("offset still present after second wheel");
    assert!(
        offset_after_two > offset_after_one,
        "expected wheel-2 to deepen scroll past wheel-1 ({offset_after_one}); got {offset_after_two}",
    );

    // Now we're in scroll mode. The keyboard `G` chord (default
    // `ScrollAction::Bottom`) should snap back to live. The gate at
    // `runtime.rs:3519-3535` allows `bindings.on_scroll.lookup` to
    // fire ONLY when `scrollback_offset > 0`, which is now true.
    send_keys(&mut handle, "G");

    let snapped = screen_eventually(
        &mut handle,
        |s| !s.contents().contains("· esc"),
        Duration::from_secs(2),
    );
    assert!(
        !snapped.contents().contains("· esc"),
        "expected scroll badge gone after `G`; got:\n{}",
        snapped.contents()
    );
}

/// AC-018: per-agent offset is preserved across focus switches, but
/// typing a printable character snaps to live BEFORE the byte is
/// forwarded.
///
/// Two-agent fixture: both spawn against `fake_agent_with_history`
/// (both have a populated scrollback). The test scrolls agent 1
/// back, cycles focus to agent 2 and back via `prefix n n` (two
/// `FocusNext` dispatches with the sticky prefix), then asserts the
/// badge is still there on agent 1 -- proving navigation chords do
/// NOT call `snap_to_live`. Then it types a printable character and
/// asserts the badge disappears.
///
/// **Why two `fake_agent_with_history` instances:** the spawn flow
/// uses the same `CODEMUX_AGENT_BIN` env var for both agents; we
/// can't point them at different binaries from the harness. Both
/// agents print 200 history lines on boot, which is fine -- the
/// test only inspects agent 1's scroll state, and agent 2's history
/// makes no difference.
///
/// **Why `\x02nn` (one prefix, two `n`s):** the prefix state machine
/// is sticky for nav dispatches (see `pty_focus.rs` for the
/// rationale). A bare `\x02n\x02n` would forward `^B` into agent 1
/// on the second prefix, which the fake discards but which would
/// also trigger `snap_to_live` -- defeating the test's purpose.
#[test]
#[ignore = "slow-tier PTY E2E; runs via `just check-e2e` / `just test-e2e`"]
#[serial]
fn scrollback_offset_survives_navigation_and_resets_on_typing() {
    let agent_bin = test_fake_bin("fake_agent_with_history");
    let mut handle = spawn_codemux_with_agent_bin(&agent_bin, "");

    // Steady state on agent 1.
    screen_eventually(
        &mut handle,
        |s| s.contents().contains("FAKE_AGENT_READY"),
        Duration::from_secs(5),
    );

    // Spawn agent 2 via the modal. `\x02c` opens, `\r` confirms the
    // empty (default scratch) path -- same flow as
    // `pty_spawn_action.rs` / `pty_focus.rs`.
    send_keys(&mut handle, "\x02c");
    screen_eventually(
        &mut handle,
        |s| s.contents().contains("@local"),
        Duration::from_secs(5),
    );
    send_keys(&mut handle, "\r");
    // Modal closes and agent 2 takes focus.
    screen_eventually(
        &mut handle,
        |s| !s.contents().contains("@local") && s.contents().contains(" 2 "),
        Duration::from_secs(10),
    );

    // Switch focus back to agent 1 (sticky prefix, single chord).
    send_keys(&mut handle, "\x02n");
    // Wait a render tick so we know we're on agent 1.
    screen_eventually(
        &mut handle,
        |s| s.contents().contains("FAKE_AGENT_READY"),
        Duration::from_secs(2),
    );

    // Scroll agent 1 back via wheel.
    for _ in 0..3 {
        send_mouse_wheel(&mut handle, WheelKind::Up, 40, 12);
    }
    let scrolled = screen_eventually(
        &mut handle,
        |s| s.contents().contains("· esc"),
        Duration::from_secs(2),
    );
    let offset_before =
        parse_offset(&scrolled.contents()).expect("badge offset on agent 1 after wheel");

    // Cycle focus to agent 2 and back via `prefix n n` (sticky
    // prefix; second `n` keeps the chord armed). Each is a
    // navigation dispatch -- the runtime must NOT snap-to-live.
    send_keys(&mut handle, "\x02nn");

    // Back on agent 1: the badge should still be there at the same
    // offset (modulo any scrollback auto-bump from new lines, which
    // the fake doesn't emit after boot, so the value should be
    // exactly the same).
    let restored = screen_eventually(
        &mut handle,
        |s| s.contents().contains("· esc"),
        Duration::from_secs(2),
    );
    let offset_after_nav =
        parse_offset(&restored.contents()).expect("badge offset still present after nav");
    assert_eq!(
        offset_after_nav, offset_before,
        "AC-018: navigation chords must preserve scrollback offset; \
         was {offset_before} before nav, {offset_after_nav} after",
    );

    // Type a printable character. The runtime's `Forward` arm calls
    // `snap_to_live()` before writing the bytes -- the badge must
    // disappear.
    send_keys(&mut handle, "a");
    let snapped = screen_eventually(
        &mut handle,
        |s| !s.contents().contains("· esc"),
        Duration::from_secs(2),
    );
    assert!(
        !snapped.contents().contains("· esc"),
        "AC-018: expected typing to snap to live; got:\n{}",
        snapped.contents()
    );
}

/// Parse the integer N out of the scroll indicator text
/// (` ↑ scroll N · esc `). Returns `None` if the badge isn't visible
/// or if the format ever drifts -- the test sites already assert the
/// badge is on screen separately, so a `None` from this helper means
/// "format regressed."
fn parse_offset(contents: &str) -> Option<usize> {
    let after = contents.split("scroll").nth(1)?;
    let stripped = after.trim_start();
    let digits: String = stripped.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}
