//! AC-039 (pasting while scrolled-back snaps to live before the
//! bracketed-paste write): pin the paste-snap discipline end to end.
//!
//! Unit tests already cover the snap operation
//! (`snap_to_live_resets_offset_to_zero`) and the bracketed-paste
//! payload (`wrap_paste_emits_brackets_around_plain_text`). What no
//! existing test could pin: the runtime's paste arm calls
//! `snap_to_live()` BEFORE forwarding the bracketed payload to the
//! agent's PTY. Without that ordering, the user pastes into a window
//! they cannot see.
//!
//! Sibling file `pty_scroll.rs` covers AC-017 (wheel-up entry) and
//! AC-018 (per-agent offset preservation across nav).
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

/// AC-039: pasting while the focused agent is scrolled back snaps
/// the view to live BEFORE the bracketed-paste payload is written.
///
/// Observable signature: the scroll badge disappears immediately
/// after the paste byte sequence is delivered, regardless of what
/// the agent does with the payload bytes (the fake discards stdin,
/// so the payload itself is invisible -- the snap is the only
/// observable runtime effect).
///
/// The byte sequence `\x1b[200~payload\x1b[201~` is what crossterm's
/// `parse_csi_bracketed_paste` (see crossterm/src/event/sys/unix/parse.rs
/// around line 815) decodes as a single `Event::Paste("payload")`.
/// Our harness writes those raw bytes; the spawned codemux's
/// crossterm reader sees a paste event, and the runtime's
/// `Event::Paste(text)` arm routes it through the snap-then-forward
/// pipeline.
#[test]
#[ignore = "slow-tier PTY E2E; runs via `just check-e2e` / `just test-e2e`"]
#[serial]
fn paste_while_scrolled_back_snaps_to_live() {
    let agent_bin = test_fake_bin("fake_agent_with_history");
    let mut handle = spawn_codemux_with_agent_bin(&agent_bin, "");

    screen_eventually(
        &mut handle,
        |s| s.contents().contains("FAKE_AGENT_READY"),
        Duration::from_secs(5),
    );

    // Scroll back a few wheel ticks so we're deep into history. Any
    // single wheel would be enough; multiple ticks guard against a
    // future regression where the first tick is the special "enter
    // scroll mode" case treated differently.
    for _ in 0..3 {
        send_mouse_wheel(&mut handle, WheelKind::Up, 40, 12);
    }
    screen_eventually(
        &mut handle,
        |s| s.contents().contains("· esc"),
        Duration::from_secs(2),
    );

    // Synthesize a bracketed-paste sequence. Crossterm decodes this
    // as `Event::Paste("pasted")` and the runtime's paste arm calls
    // `snap_to_live()` before forwarding the wrapped payload.
    send_keys(&mut handle, "\x1b[200~pasted\x1b[201~");

    let snapped = screen_eventually(
        &mut handle,
        |s| !s.contents().contains("· esc"),
        Duration::from_secs(2),
    );
    assert!(
        !snapped.contents().contains("· esc"),
        "AC-039: expected paste to snap to live; got:\n{}",
        snapped.contents()
    );
}
