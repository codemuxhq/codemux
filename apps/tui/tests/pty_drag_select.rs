//! AC-021 (drag-to-select and copy via OSC 52): pin the drag-to-select
//! pipeline end to end. Unit tests already cover the selection
//! lifecycle (`pane_mouse_dispatch_*`), the text extraction
//! (`vt100_contents_between_*`), and the OSC 52 framing
//! (`write_clipboard_to_emits_osc_52_with_base64_payload`); this test
//! drives a real codemux PTY, synthesizes the press-drag-release SGR
//! mouse sequence, and asserts the OSC 52 clipboard write lands on
//! the master byte stream with a base64-encoded payload that matches
//! the selected text.
//!
//! ## What this pins
//!
//! - The runtime's `Event::Mouse` branch translates left-press +
//!   motion-drag + left-release into `PaneMouseDispatch::Arm /
//!   Extend / Commit`, AND the loop's match arm runs
//!   `commit_selection` which writes `\x1b]52;c;<base64>\x07` to
//!   stdout (codemux's stdout = master-side of the test PTY).
//! - The selection content matches what `vt100::Screen::contents_between`
//!   reports for the dragged cell range -- the text the user can see
//!   between the anchor and head cells.
//!
//! ## Observation strategy
//!
//! Master-side bytes contain the OSC 52 escape verbatim (the parser
//! ignores it but it's still in the byte stream). We use
//! `master_bytes_eventually` to wait for the `\x1b]52;c;` prefix,
//! then decode the base64 payload up to the BEL terminator and
//! assert the result is the expected substring.
//!
//! Gating mirrors the rest of the slow tier: `test-fakes` feature,
//! `#[ignore]`, `#[serial]`.

#![cfg(feature = "test-fakes")]

#[allow(dead_code)]
mod common;

use std::time::Duration;

use base64::Engine;
use serial_test::serial;

use common::{
    MouseButton, master_bytes_eventually, screen_eventually, send_mouse_drag, spawn_codemux,
};

#[test]
#[ignore = "slow-tier PTY E2E; runs via `just check-e2e` / `just test-e2e`"]
#[serial]
fn drag_inside_pane_emits_osc_52_with_selected_text() {
    let mut handle = spawn_codemux();

    // Steady state: fake's prompt is on screen. `FAKE_AGENT_READY>` is
    // the cells we will drag across.
    let settled = screen_eventually(
        &mut handle,
        |s| s.contents().contains("FAKE_AGENT_READY"),
        Duration::from_secs(5),
    );

    // The fake's prompt sits on row 0. Find the cell column where
    // `FAKE` starts so we know the absolute cell coordinates for the
    // SGR mouse encoding. The harness's PTY is 80x24 in Popup chrome,
    // so the agent pane occupies the top 23 rows; row 0 column 0 is
    // the prompt's `F`.
    let prompt_row = 0_u16;
    let prompt_col = 0_u16;
    // Drag from `F` to the `_` of `FAKE_AGENT` (10 cells). The
    // selection is `FAKE_AGENT`; vt100::Screen::contents_between
    // returns the joined cell text. End coordinate is the LAST cell
    // to include (inclusive); the runtime adds 1 internally on the
    // end column (`end.col.saturating_add(1)`), so dragging from
    // column 0 to column 9 selects `FAKE_AGENT`.
    let select_end_col = 9_u16;
    let expected = "FAKE_AGENT";

    // Sanity check: the cells actually carry the expected glyphs.
    // Catches an upstream font/wide-glyph regression that would
    // shift the cell-range math.
    for (i, ch) in expected.chars().enumerate() {
        let col = prompt_col + u16::try_from(i).expect("column fits u16");
        let cell = settled
            .cell(prompt_row, col)
            .unwrap_or_else(|| panic!("expected a cell at ({prompt_row},{col})"));
        assert_eq!(
            cell.contents(),
            ch.to_string(),
            "cell ({prompt_row},{col}) is not the expected `{ch}`",
        );
    }

    // SGR mouse encoding is 1-based. `send_mouse_drag` does the
    // press-on-from, motion-at-to, release-at-to sequence.
    send_mouse_drag(
        &mut handle,
        MouseButton::Left { ctrl: false },
        (prompt_col.saturating_add(1), prompt_row.saturating_add(1)),
        (
            select_end_col.saturating_add(1),
            prompt_row.saturating_add(1),
        ),
    );

    // Wait for the OSC 52 byte stream to appear on the master side.
    // `\x1b]52;c;` is the OSC 52 prefix the runtime emits in
    // `write_clipboard_to` (see `write_clipboard_to_emits_osc_52_with_base64_payload`
    // for the framing pin). `\x07` (BEL) terminates the OSC.
    let bytes = master_bytes_eventually(
        &mut handle,
        |b| {
            let prefix = b"\x1b]52;c;";
            b.windows(prefix.len())
                .position(|w| w == prefix)
                .and_then(|start| {
                    b[start + prefix.len()..]
                        .iter()
                        .position(|&c| c == 0x07)
                        .map(|terminator| start + prefix.len() + terminator)
                })
                .is_some()
        },
        Duration::from_secs(5),
    );

    // Extract the base64 payload between `\x1b]52;c;` and the BEL.
    let prefix = b"\x1b]52;c;";
    let start = bytes
        .windows(prefix.len())
        .position(|w| w == prefix)
        .expect("OSC 52 prefix on master byte stream");
    let payload_start = start + prefix.len();
    let bel_offset = bytes[payload_start..]
        .iter()
        .position(|&c| c == 0x07)
        .expect("BEL terminator after OSC 52 payload");
    let payload = &bytes[payload_start..payload_start + bel_offset];

    let decoded = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .expect("OSC 52 payload must be valid base64");
    let text = String::from_utf8(decoded).expect("OSC 52 payload must be valid UTF-8");

    assert_eq!(
        text,
        expected,
        "expected OSC 52 payload to match the dragged cell range `{expected}`; \
         got `{text}`; raw bytes (lossy utf-8): {}",
        String::from_utf8_lossy(&bytes),
    );
}
