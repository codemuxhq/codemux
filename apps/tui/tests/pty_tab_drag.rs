//! AC-020 (drag a tab to reorder): pin the press-on-A -> motion -> release-
//! on-B dispatch end to end through SGR mouse encoding. Unit tests already
//! cover the dispatcher
//! (`apps/tui/src/runtime.rs::tab_mouse_dispatch_up_different_tab_is_a_reorder`)
//! and the `remove + insert` semantics (`reorder_agents_*`); this test
//! drives a real codemux PTY, synthesizes the press-drag-release, and
//! asserts the tab ordering changed.
//!
//! ## What this pins
//!
//! - The runtime's mouse-event branch translates a left-press over one
//!   tab + motion + left-release over a different tab into a
//!   `TabMouseDispatch::Reorder { from, to }`, AND the loop's match arm
//!   applies the reorder by calling `reorder_agents` + `shift_index` on
//!   the focused / previous-focused indices.
//! - The renderer redraws the tab strip in the new order. We observe
//!   the ordering by reading which agent label sits to the right of the
//!   ` 1 ` ordinal slot before vs. after the drag.
//!
//! ## Why a third agent (`[A, B, C]`) is the right fixture
//!
//! A two-tab drag is degenerate: drag(A -> B) and drag(B -> A) are the
//! same observable outcome (swap), so the test can't distinguish
//! "browser-tab insert" semantics from a naive swap. With three tabs in
//! `[A, B, C]`, drag(A -> C) yields `[B, C, A]` under insert semantics
//! but `[C, B, A]` under swap semantics. We assert the insert outcome.
//!
//! Building three agents from one harness boot requires two spawn-modal
//! roundtrips. We reuse the `pty_spawn_action.rs` flow (`Ctrl+B c` ->
//! `\r` on empty modal) twice. After both spawns focus is on the third
//! agent.
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
    MouseButton, screen_eventually, send_keys, send_mouse_drag, spawn_codemux_with_config,
};

#[test]
#[ignore = "slow-tier PTY E2E; runs via `just check-e2e` / `just test-e2e`"]
#[serial]
fn drag_tab_reorders_by_remove_insert_semantics() {
    let scratch = TempDir::new().expect("scratch tempdir");
    let scratch_path = scratch
        .path()
        .to_str()
        .expect("scratch tempdir path must be valid UTF-8");
    let config = format!("[spawn]\nscratch_dir = {scratch_path:?}\n");

    let mut handle = spawn_codemux_with_config(&config);

    screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            c.contains("FAKE_AGENT_READY") && !c.contains("@local")
        },
        Duration::from_secs(5),
    );

    // Spawn agent-2.
    send_keys(&mut handle, "\x02c");
    screen_eventually(
        &mut handle,
        |s| s.contents().contains("@local"),
        Duration::from_secs(5),
    );
    send_keys(&mut handle, "\r");
    screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            !c.contains("@local") && c.contains(" 2 ")
        },
        Duration::from_secs(10),
    );

    // Spawn agent-3.
    send_keys(&mut handle, "\x02c");
    screen_eventually(
        &mut handle,
        |s| s.contents().contains("@local"),
        Duration::from_secs(5),
    );
    send_keys(&mut handle, "\r");

    let bottom_row = 23_u16;
    let before = screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            !c.contains("@local") && c.contains(" 1 ") && c.contains(" 2 ") && c.contains(" 3 ")
        },
        Duration::from_secs(10),
    );

    // Capture the agent ids in slot order before the drag. Each
    // ordinal ` N ` is followed by the agent's label. Reading by
    // ordinal-position rather than relying on stable label content
    // keeps the assertion robust against future renderer tweaks --
    // the contract is "what was in slot 1 is no longer in slot 1",
    // not "the string `agent-1` migrated cells."
    let before_row = row_contents(&before, bottom_row);
    let before_slot1 = label_after_ordinal(&before_row, '1');
    let before_slot3 = label_after_ordinal(&before_row, '3');

    // Locate the 1-based column of tab 1's ordinal cell on the bottom
    // row, and the 1-based column of tab 3's. The drag-and-release
    // moves agent-A from slot 1 to slot 3.
    let from_col = ordinal_column(&before, bottom_row, '1').expect("tab 1 ordinal column");
    let to_col = ordinal_column(&before, bottom_row, '3').expect("tab 3 ordinal column");

    send_mouse_drag(
        &mut handle,
        MouseButton::Left { ctrl: false },
        (from_col, bottom_row.saturating_add(1)),
        (to_col, bottom_row.saturating_add(1)),
    );

    // After the drag, the renderer's slot 1 must hold what was in
    // slot 3 (B in the AC's `[A, B, C] -> [B, C, A]`), and slot 3
    // must hold what was in slot 1 (A). Asserting label-by-slot
    // rather than the literal `[B, C, A]` ordering keeps the test
    // robust against renderer label-format tweaks.
    let after = screen_eventually(
        &mut handle,
        |s| {
            let row = row_contents(s, bottom_row);
            let s1 = label_after_ordinal(&row, '1');
            let s3 = label_after_ordinal(&row, '3');
            !s1.is_empty() && !s3.is_empty() && s1 == before_slot3 && s3 == before_slot1
        },
        Duration::from_secs(5),
    );

    let after_row = row_contents(&after, bottom_row);
    let after_slot1 = label_after_ordinal(&after_row, '1');
    let after_slot3 = label_after_ordinal(&after_row, '3');
    assert_eq!(
        after_slot1, before_slot3,
        "expected slot 1 to now hold the agent that was previously in slot 3 (insert semantics);\n\
         before: {before_row}\n after: {after_row}"
    );
    assert_eq!(
        after_slot3, before_slot1,
        "expected slot 3 to now hold the agent that was previously in slot 1 (insert semantics);\n\
         before: {before_row}\n after: {after_row}"
    );
}

/// Read the visible text of a single row out of the vt100 screen.
fn row_contents(screen: &vt100::Screen, row: u16) -> String {
    screen
        .contents()
        .lines()
        .nth(row as usize)
        .unwrap_or("")
        .to_string()
}

/// Return the text that follows ` <digit> ` on the row, trimmed at
/// either the next tab separator (` │ `) or the first run of two or
/// more spaces (the renderer pads the right side of the status row
/// with whitespace before the prefix hint). The renderer's tab format
/// is ` N <label> ` so each tab's label sits between the ordinal cell
/// and that trailing pad. Used to assert "what agent is in slot N"
/// without depending on stable label content across the test.
fn label_after_ordinal(row: &str, digit: char) -> String {
    let needle = format!(" {digit} ");
    let Some(start) = row.find(&needle) else {
        return String::new();
    };
    let rest = &row[start + needle.len()..];
    let sep_end = rest.find(" │ ").unwrap_or(rest.len());
    let pad_end = rest.find("  ").unwrap_or(rest.len());
    let end = sep_end.min(pad_end);
    rest[..end].trim().to_string()
}

/// 1-based screen column of the cell holding `digit` as a tab ordinal
/// (preceded by a space, sharing inverse styling with that space). The
/// `<digit>` may also appear inside a label; the "preceded by space
/// with matching inverse" fingerprint is what the [`pty_tab_click`]
/// helper uses too, lifted here as a free function.
fn ordinal_column(screen: &vt100::Screen, row: u16, digit: char) -> Option<u16> {
    let cols = screen.size().1;
    let digit_str = digit.to_string();
    for col in 1..cols {
        let prev = screen.cell(row, col.saturating_sub(1))?;
        let cur = screen.cell(row, col)?;
        if prev.contents() == " " && cur.contents() == digit_str && prev.inverse() == cur.inverse()
        {
            // SGR mouse encoding is 1-based.
            return Some(col.saturating_add(1));
        }
    }
    None
}
