//! AC-025 (help screen reflects the live keymap): pin the `prefix ?`
//! overlay open path through the real keymap, and the "any key closes"
//! return path. The renderer is already snapshot-tested
//! (`snapshot_help_screen`) and a unit test pins the open dispatch
//! (`prefix_question_mark_opens_help`), but no existing test asserts
//! that pressing the chord through a real PTY actually flips the
//! overlay on, then off again.
//!
//! Gating mirrors `pty_smoke.rs`, `pty_nav.rs`, and `pty_lifecycle.rs`:
//! `test-fakes` feature, `#[ignore]` so the slow tier ships through
//! `just check-e2e` only, `#[serial]` because the PTY harness is not
//! parallel-safe.

#![cfg(feature = "test-fakes")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

// Sibling test files (e.g. `pty_lifecycle.rs`) consume helpers this
// file doesn't (`wait_for_exit`); same allow-on-import pattern as
// `pty_smoke.rs` / `pty_nav.rs`.
#[allow(dead_code)]
mod common;

use std::time::Duration;

use serial_test::serial;

use common::{screen_eventually, send_keys, spawn_codemux};

/// Boot codemux against the fake agent, send `Ctrl+B ?` to open the
/// help overlay, assert the help chrome lands on screen, then send a
/// keystroke and assert the overlay goes away.
///
/// **Observation strategy:** the help screen wraps content in a
/// bordered `Block` titled ` codemux help ` (see `render_help` in
/// `apps/tui/src/runtime.rs`). That literal string is unique to the
/// open-help chrome — it does not appear in the agent pane, the
/// navigator chrome, or the spawn modal. The presence/absence of
/// that title is a clean structural diff.
///
/// **Close-key strategy:** the help-state branch in `dispatch_key`
/// (`runtime.rs::3127`) consumes *any* keypress to close the overlay
/// — friendly behavior for users who opened help by mistake. We
/// close with `Esc`, which is the natural "go back" gesture and also
/// pins the path through `KeyCode::Esc` rather than a printable
/// character.
///
/// **Prefix chord strategy:** Option B — hard-coded `"\x02"`
/// (`Ctrl+B`), same rationale as the other PTY tests. `Bindings::default()`
/// in `apps/tui/src/keymap.rs` sets `prefix = Ctrl+B` and `help = '?'`;
/// the harness uses defaults. If either default ever changes, this
/// test fails loudly and the fix is one byte.
#[test]
#[ignore = "slow-tier PTY E2E; runs via `just check-e2e` / `just test-e2e`"]
#[serial]
fn help_overlay_opens_and_closes_on_chord() {
    let mut handle = spawn_codemux();

    // Wait for the steady state: the fake's prompt has rendered AND
    // the help overlay is not yet up. Checking both directions guards
    // against a future change that would launch with help open and
    // make the post-toggle assertion vacuous.
    let before = screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            c.contains("FAKE_AGENT_READY") && !c.contains(" codemux help ")
        },
        Duration::from_secs(5),
    );
    assert!(
        !before.contents().contains(" codemux help "),
        "expected no help overlay before chord; got:\n{}",
        before.contents()
    );

    // Open help: prefix + `?`.
    send_keys(&mut handle, "\x02?");

    let opened = screen_eventually(
        &mut handle,
        |s| s.contents().contains(" codemux help "),
        Duration::from_secs(5),
    );
    assert!(
        opened.contents().contains(" codemux help "),
        "expected help overlay after `prefix ?`; got:\n{}",
        opened.contents()
    );

    // Close help: Esc. Any key works per `runtime.rs::3127`; Esc is
    // the "go back" gesture and also exercises the non-printable-key
    // path through `dispatch_key`.
    send_keys(&mut handle, "\x1b");

    let closed = screen_eventually(
        &mut handle,
        |s| !s.contents().contains(" codemux help "),
        Duration::from_secs(5),
    );
    assert!(
        !closed.contents().contains(" codemux help "),
        "expected help overlay to close after Esc; got:\n{}",
        closed.contents()
    );
}
