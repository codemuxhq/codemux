//! AC-008 (cancel the spawn minibuffer): pin the open-then-Esc path
//! through a real PTY. Unit tests in `apps/tui/src/spawn.rs` cover the
//! per-state Esc transitions (no-selection-closes, search-mode-clears,
//! selection-clears, host-zone-returns-to-path) — this PTY test closes
//! the chord-to-modal-open-to-Esc-closes pipeline end-to-end.
//!
//! Also the first PTY test that exercises the spawn-modal codepath at
//! all. Lays groundwork for downstream modal-flow tests (AC-002 scratch
//! spawn, AC-004 autocomplete, AC-006 drilldown, etc.) by proving the
//! `prefix c` chord lands a modal we can observe.
//!
//! Gating mirrors the other PTY tests: `test-fakes` feature, `#[ignore]`,
//! `#[serial]`.

#![cfg(feature = "test-fakes")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

// Sibling test files consume helpers this file doesn't (`wait_for_exit`);
// same allow-on-import pattern as the rest of the suite.
#[allow(dead_code)]
mod common;

use std::time::Duration;

use serial_test::serial;

use common::{screen_eventually, send_keys, spawn_codemux};

/// Boot codemux against the fake agent, send `Ctrl+B c` to open the
/// spawn minibuffer, assert the modal chrome lands on screen, then
/// send `Esc` and assert the modal goes away.
///
/// **Observation strategy:** when the modal opens with default config
/// (Fuzzy mode + Path zone focused), the prompt line renders the host
/// placeholder `local` immediately after the bold `@` host-marker
/// span. The literal substring `@local` is unique to the open-modal
/// chrome — it does not appear in the agent pane (the fake's prompt
/// is `FAKE_AGENT_READY> `), the navigator (default agent name is
/// `agent-1`), or the status bar segments. Presence vs. absence of
/// `@local` is a clean structural diff.
///
/// **Why a single Esc suffices to close:** with no input typed and
/// no wildmenu selection armed, the modal is in the "nav mode, no
/// selection" branch (see `esc_in_nav_mode_with_no_selection_closes_modal`
/// in `apps/tui/src/spawn.rs`), and the first Esc closes immediately.
/// The "two-Esc" path (clears filter chars / selection first) is
/// pinned at the unit level; not re-pinning it here keeps the test
/// focused on the single canonical open-then-cancel gesture.
///
/// **Uncovered by this test:** the "previously-focused agent regains
/// focus on close" clause — codemux has exactly one agent here, so
/// the focus-restore behavior is trivially satisfied. Left for a
/// future multi-agent test once spawn-from-modal lands.
#[test]
#[ignore = "slow-tier PTY E2E; runs via `just check-e2e` / `just test-e2e`"]
#[serial]
fn esc_in_spawn_modal_closes_the_modal() {
    let mut handle = spawn_codemux();

    // Steady state: fake's prompt is on screen, no modal open yet.
    // Checking both directions guards against any future change that
    // would open the modal at boot and make the post-toggle assertion
    // vacuous.
    let before = screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            c.contains("FAKE_AGENT_READY") && !c.contains("@local")
        },
        Duration::from_secs(5),
    );
    assert!(
        !before.contents().contains("@local"),
        "expected no spawn modal before chord; got:\n{}",
        before.contents()
    );

    // Open the modal: prefix + `c`.
    send_keys(&mut handle, "\x02c");

    let opened = screen_eventually(
        &mut handle,
        |s| s.contents().contains("@local"),
        Duration::from_secs(5),
    );
    assert!(
        opened.contents().contains("@local"),
        "expected spawn modal after `prefix c`; got:\n{}",
        opened.contents()
    );

    // Cancel the modal: single Esc closes it because the modal opens
    // with no input typed and no wildmenu selection armed.
    send_keys(&mut handle, "\x1b");

    let closed = screen_eventually(
        &mut handle,
        |s| !s.contents().contains("@local"),
        Duration::from_secs(5),
    );
    assert!(
        !closed.contents().contains("@local"),
        "expected spawn modal to close after Esc; got:\n{}",
        closed.contents()
    );
}
