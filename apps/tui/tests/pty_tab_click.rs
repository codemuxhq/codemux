//! AC-019 (click a tab to focus it): pin the press-on-tab -> release-on-
//! same-tab dispatch end-to-end through SGR mouse encoding. Unit tests
//! already cover the dispatcher in isolation
//! (`apps/tui/src/runtime.rs::tab_mouse_dispatch_up_same_tab_is_a_click`)
//! and the hitbox recorder
//! (`render_status_bar_records_one_hitbox_per_agent_in_order`); this test
//! drives a real codemux PTY, synthesizes a click against the tab strip,
//! and asserts the focus moved.
//!
//! ## Why we spawn a second agent inline
//!
//! The smallest test fixture for a tab click is two agents. The harness
//! only boots one (the auto-spawned agent-1), so the test opens the
//! spawn modal and pushes a second through the scratch-dir path -- the
//! same flow `pty_spawn_action.rs::enter_in_empty_modal_spawns_second_agent_in_scratch_dir`
//! pins as a self-contained chord-to-second-tab E2E.
//!
//! ## Why we click tab 1 (not tab 2)
//!
//! The spawn-from-modal flow leaves focus on the newly-spawned agent
//! (agent-2; pinned by `pty_spawn_bounce.rs` and AC-034). So agent-2 is
//! focused at the moment we click. Clicking tab 1 is the meaningful
//! gesture -- it has to move focus. Clicking tab 2 would be a no-op
//! (it's already focused) and the test would silently pass even if the
//! dispatch was broken.
//!
//! ## Why we observe via the reverse-video tab highlight
//!
//! `tab_index_style` paints the focused tab's `[N]` ordinal in REVERSED
//! style. vt100's `Cell::inverse()` reports that. The test reads the
//! cell at the tab's ordinal column and asserts the inverse bit moved
//! from tab 2's slot to tab 1's slot -- that's the renderer's
//! "this-tab-is-focused" signal, expressed through the screen rather
//! than the internal `nav.focused` index.
//!
//! Gating mirrors the rest of the slow tier: `test-fakes` feature,
//! `#[ignore]`, `#[serial]`.

#![cfg(feature = "test-fakes")]

#[allow(dead_code)]
mod common;

use std::time::Duration;

use serial_test::serial;
use tempfile::TempDir;

use common::{
    MouseButton, screen_eventually, send_keys, send_mouse_click, spawn_codemux_with_config,
};

#[test]
#[ignore = "slow-tier PTY E2E; runs via `just check-e2e` / `just test-e2e`"]
#[serial]
fn click_unfocused_tab_moves_focus() {
    // Per-test scratch dir so the spawn flow doesn't land in the
    // developer's `~/.codemux/scratch`. Same shape as
    // `pty_spawn_action.rs`.
    let scratch = TempDir::new().expect("scratch tempdir");
    let scratch_path = scratch
        .path()
        .to_str()
        .expect("scratch tempdir path must be valid UTF-8");
    let config = format!("[spawn]\nscratch_dir = {scratch_path:?}\n");

    let mut handle = spawn_codemux_with_config(&config);

    // Steady state: agent-1's prompt is on screen, no modal.
    screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            c.contains("FAKE_AGENT_READY") && !c.contains("@local")
        },
        Duration::from_secs(5),
    );

    // Spawn agent-2 via the empty-modal + Enter flow (AC-002).
    send_keys(&mut handle, "\x02c");
    screen_eventually(
        &mut handle,
        |s| s.contents().contains("@local"),
        Duration::from_secs(5),
    );
    send_keys(&mut handle, "\r");

    // Wait for both tab ordinals to render in the status strip on the
    // bottom row. `tab_index_style` paints the focused ordinal in
    // REVERSED style; immediately after a spawn the focus has moved
    // to agent-2, so tab 2's `[2]` is the inverse one. Asserting
    // inverse-on-tab-2-before is the canary that the spawn completed
    // before we synthesize the click.
    let bottom_row = 23_u16; // 0-based; the 80x24 PTY's last row
    let before = screen_eventually(
        &mut handle,
        |s| {
            // Both tabs visible AND tab 2 ordinal is inverse.
            let row_text = row_contents(s, bottom_row);
            row_text.contains(" 1 ")
                && row_text.contains(" 2 ")
                && tab_ordinal_is_inverse(s, bottom_row, '2')
        },
        Duration::from_secs(10),
    );
    assert!(
        tab_ordinal_is_inverse(&before, bottom_row, '2'),
        "expected tab 2 to be focused (inverse) before click; got row:\n{}",
        row_contents(&before, bottom_row)
    );
    assert!(
        !tab_ordinal_is_inverse(&before, bottom_row, '1'),
        "expected tab 1 to be unfocused (not inverse) before click; got row:\n{}",
        row_contents(&before, bottom_row)
    );

    // Tab 1's ordinal `1` sits in cell ` 1 ` at column 2 (1-based). The
    // strip starts at column 1 with a leading space, so the digit is
    // at column 2. `send_mouse_click` speaks 1-based coords; the runtime
    // hit-tests at the 0-based `column = 1, row = 23` cell which is
    // inside the recorded hitbox for agent-1's tab.
    send_mouse_click(
        &mut handle,
        MouseButton::Left { ctrl: false },
        2,
        bottom_row.saturating_add(1),
    );

    // After the click, focus must move to tab 1: its ordinal becomes
    // REVERSED and tab 2's loses the inverse. Asserting both
    // directions guards against a regression where the click toggles
    // the wrong tab (`tab_mouse_dispatch_up_same_tab_is_a_click`
    // already pins the dispatcher; this is the runtime-level
    // observation).
    let after = screen_eventually(
        &mut handle,
        |s| {
            tab_ordinal_is_inverse(s, bottom_row, '1')
                && !tab_ordinal_is_inverse(s, bottom_row, '2')
        },
        Duration::from_secs(5),
    );
    assert!(
        tab_ordinal_is_inverse(&after, bottom_row, '1'),
        "expected tab 1 to be focused (inverse) after click; got row:\n{}",
        row_contents(&after, bottom_row)
    );
    assert!(
        !tab_ordinal_is_inverse(&after, bottom_row, '2'),
        "expected tab 2 to be unfocused (not inverse) after click; got row:\n{}",
        row_contents(&after, bottom_row)
    );
}

/// Read the visible text of a single row out of the vt100 screen.
/// `Screen::contents()` returns the whole grid joined by newlines;
/// splitting on `\n` and indexing avoids assuming a fixed byte offset
/// per row (wide glyphs would shift the offset; tests in this file
/// only land ASCII on screen, but the split is the safer pattern).
fn row_contents(screen: &vt100::Screen, row: u16) -> String {
    screen
        .contents()
        .lines()
        .nth(row as usize)
        .unwrap_or("")
        .to_string()
}

/// True when the tab-ordinal cell with the given digit on `row` has
/// the `inverse` modifier set -- the renderer's "this tab is focused"
/// signal (see `tab_index_style`).
///
/// The tab strip lays out each ordinal as ` N ` (space, digit, space)
/// and the agent ids themselves can contain matching digits
/// (`agent-a64a96219336...` carries every digit), so a naive
/// first-hit-of-digit walk would land inside the label instead of on
/// the ordinal. The fingerprint we rely on: the ordinal cell is
/// flanked by spaces AND those spaces share the same inverse state as
/// the digit (the renderer applies the same `tab_index_style` to all
/// three cells -- `format!(" {} ", i + 1)`). Walking right-to-left
/// would also work; we go left-to-right and require the prefix space
/// to share the digit's inverse state.
fn tab_ordinal_is_inverse(screen: &vt100::Screen, row: u16, digit: char) -> bool {
    let cols = screen.size().1;
    let digit_str = digit.to_string();
    for col in 1..cols {
        let Some(prev) = screen.cell(row, col.saturating_sub(1)) else {
            continue;
        };
        let Some(cur) = screen.cell(row, col) else {
            continue;
        };
        if prev.contents() == " " && cur.contents() == digit_str && prev.inverse() == cur.inverse()
        {
            return cur.inverse();
        }
    }
    false
}
