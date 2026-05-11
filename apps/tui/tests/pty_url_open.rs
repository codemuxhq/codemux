//! AC-041 (Ctrl+click on a URL hands it to the OS opener; Ctrl+hover
//! shows underline + hand cursor): pin the end-to-end Ctrl+click flow
//! through the runtime's `Event::Mouse` branch.
//!
//! Unit tests already cover the URL hit-test (`compute_hover_*`), the
//! opener trait (`url_opener_trait_supports_recording_mock_implementations`),
//! and the underline render (`paint_hover_url_if_active_underlines_url_range_and_tints_cyan`).
//! What no existing test could pin: the runtime actually wires those
//! pieces together. This test boots codemux with the hidden
//! `--record-opens-to <path>` seam swapping the production
//! `OsUrlOpener` for a `RecordingUrlOpener`, points it at the
//! `fake_agent_with_url` stub (prints `https://example.com/codemux-test`
//! on its boot prompt), Ctrl+clicks on a cell inside the URL, and
//! asserts the recording file gained exactly that URL.
//!
//! ## Why we don't also assert on the toast or browser launch
//!
//! The toast surface (`url_open_toast_*`) is unit-pinned, and the
//! recording opener always reports `Opened` so no toast fires on the
//! happy path. The actual browser launch is the OS-side step we are
//! deliberately stubbing out -- if you want to test that, you need a
//! real OS opener which is the AC-041 deferral the test seam closes.
//!
//! ## Why we also assert on the hover underline
//!
//! AC-041 has two clauses: (1) Ctrl+click opens, (2) Ctrl+hover shows
//! underline. The hover-underline path goes through
//! `paint_hover_url_if_active` which is unit-pinned, but again no
//! existing test drove a real Ctrl-modifier motion event through the
//! runtime and asserted on the rendered cells. We send a single Ctrl-
//! hover motion frame, then read the cell at the URL column and
//! assert `underline()` is true.
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
    MouseButton, screen_eventually, send_mouse_click, send_mouse_ctrl_hover,
    spawn_codemux_with_args,
};

const URL: &str = "https://example.com/codemux-test";

#[test]
#[ignore = "slow-tier PTY E2E; runs via `just check-e2e` / `just test-e2e`"]
#[serial]
fn ctrl_click_on_url_records_open_through_seam() {
    let agent_bin = env!("CARGO_BIN_EXE_fake_agent_with_url");
    let record_dir = TempDir::new().expect("record-opens tempdir");
    let record_path = record_dir.path().join("opens.log");
    let record_arg = format!("--record-opens-to={}", record_path.display());

    let mut handle = spawn_codemux_with_args(agent_bin, "", &[&record_arg]);

    // Wait for the URL to render in the agent pane. The fake prints
    // `FAKE_AGENT_READY> https://example.com/codemux-test ` on its
    // first row, so the substring lands cleanly on screen.
    let settled = screen_eventually(
        &mut handle,
        |s| s.contents().contains(URL),
        Duration::from_secs(5),
    );

    // Find the cell that holds the first character of the URL on the
    // top row. `vt100::Screen::cell` and a left-to-right walk are
    // robust to wide-glyph shifts (the URL is ASCII so each char is
    // one cell anyway, but the walk is correct in general).
    let (row, col) = find_url_origin(&settled, URL).expect("URL on screen");

    // Click a cell that's safely inside the URL -- past the `https://`
    // scheme prefix, well before the trailing space. The 10th
    // character of the URL string lands inside `example.com`, deep
    // enough that any off-by-one in the hit-test would still hit URL
    // text.
    let click_col = col.saturating_add(10);

    // Ctrl+hover first -- assert the cell renders with the underline
    // attribute. Pins the Ctrl+hover branch of AC-041.
    // SGR mouse encoding is 1-based.
    send_mouse_ctrl_hover(
        &mut handle,
        click_col.saturating_add(1),
        row.saturating_add(1),
    );
    let hovered = screen_eventually(
        &mut handle,
        |s| s.cell(row, click_col).is_some_and(vt100::Cell::underline),
        Duration::from_secs(5),
    );
    assert!(
        hovered
            .cell(row, click_col)
            .is_some_and(vt100::Cell::underline),
        "expected URL cell to render with underline under Ctrl+hover; cell at ({row},{click_col}): {:?}",
        hovered.cell(row, click_col).map(vt100::Cell::contents)
    );

    // Now Ctrl+click. The runtime routes this through the
    // `RecordingUrlOpener::open(url)` impl, which appends the URL
    // line to `record_path`.
    send_mouse_click(
        &mut handle,
        MouseButton::Left { ctrl: true },
        click_col.saturating_add(1),
        row.saturating_add(1),
    );

    // Poll until the file contains the URL. The opener writes
    // immediately from the runtime's event-loop thread (no detached
    // worker for the recording variant), so this should be fast --
    // a 5s budget covers any cold-cache I/O on first run.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(s) = std::fs::read_to_string(&record_path)
            && s.lines().any(|line| line == URL)
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "expected `--record-opens-to` to record URL line `{URL}` within 5s; \
             got file contents: {:?}",
            std::fs::read_to_string(&record_path),
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let recorded = std::fs::read_to_string(&record_path).expect("read record file");
    assert!(
        recorded.lines().any(|line| line == URL),
        "expected recording file to contain URL line `{URL}`; got:\n{recorded}",
    );
}

/// Locate the `(row, col)` of the first character of `needle` on the
/// vt100 screen. Walks the contents string and converts a byte offset
/// into a row/column pair, accounting only for ASCII (the URLs we
/// pin are ASCII; a wide-glyph URL would need extra width math).
fn find_url_origin(screen: &vt100::Screen, needle: &str) -> Option<(u16, u16)> {
    let contents = screen.contents();
    let idx = contents.find(needle)?;
    let prefix = &contents[..idx];
    let row = u16::try_from(prefix.matches('\n').count()).ok()?;
    let col_bytes = prefix.rfind('\n').map_or(idx, |nl| idx - (nl + 1));
    let col = u16::try_from(col_bytes).ok()?;
    Some((row, col))
}
