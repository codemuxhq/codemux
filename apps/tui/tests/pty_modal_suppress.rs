//! AC-033 (spawn modal swallows all keystrokes while open): pin the
//! runtime's "if a modal is open, `dispatch_key` never runs" branch
//! through a real PTY. Unit tests in `apps/tui/src/spawn.rs` cover the
//! modal's keymap-level filters (`ctrl_modified_keys_are_dropped`,
//! `lock_for_bootstrap_drops_typing_keys`, and the per-zone Esc
//! transitions), but no existing test asserts that pressing chords
//! through the real PTY actually bypasses the runtime's prefix-arm
//! state machine and direct-dispatch path while the modal is up.
//!
//! Gating mirrors the other PTY tests: `test-fakes` feature,
//! `#[ignore]` so the slow tier ships through `just check-e2e` only,
//! and `#[serial]` because the PTY harness is not parallel-safe.

#![cfg(feature = "test-fakes")]

// Sibling test files consume helpers this file doesn't (`wait_for_exit`);
// same allow-on-import pattern as the rest of the suite.
#[allow(dead_code)]
mod common;

use std::thread;
use std::time::Duration;

use serial_test::serial;

use common::{screen_eventually, send_keys, spawn_codemux};

/// Open the spawn modal, fire two chords that would normally produce
/// observable runtime effects (help overlay and chrome flip), and
/// assert neither effect happens. Then close the modal and re-fire the
/// chrome-flip chord to prove the suppression was modal-scoped, not
/// permanent.
///
/// **What this pins:** the runtime's "modal is open ⇒ short-circuit
/// `dispatch_key`" branch in `apps/tui/src/runtime.rs` around line
/// 3151 — the `if let Some(ui) = spawn_ui.as_mut()` block that ends
/// with `continue;` near line 3473. Without that branch, the prefix
/// chord would arm the runtime's prefix-key state machine and `?`
/// would fire `KeyDispatch::OpenHelp`, popping the help overlay over
/// the modal. The branch is the single load-bearing line that makes
/// the modal a true keyboard-modal element.
///
/// **Observation strategy:** absence of the two unique chrome strings
/// the chords would normally produce — ` codemux help ` (help
/// overlay's bordered block title, see `render_help`) and ` agents `
/// (`LeftPane` navigator title, see `render_left_pane`) — combined with
/// continued presence of `@local` (the modal's host-marker chrome).
/// `@local` staying visible is what proves the keys reached the modal
/// at all; the missing chrome strings are what proves they didn't also
/// reach the runtime.
///
/// **Why two keys (`?` and `\x02v`):** they exercise different paths
/// in `dispatch_key`. `?` is a single `Char` that, when the runtime's
/// prefix is armed, would map to `OpenHelp`. `\x02v` is a Ctrl-modified
/// byte followed by a `Char` — it would arm the runtime's prefix
/// state machine and then dispatch `PrefixAction::ToggleNav`. Both
/// are dropped by the modal but through different rules: `?` is just
/// routed to the path field as a literal character (the modal may
/// refresh its wildmenu on the new query but never surfaces a help
/// overlay), while `\x02` is filtered by the modal's
/// `ctrl_modified_keys_are_dropped` guard. Hitting both paths in one
/// test catches regressions in either direction.
///
/// **Why the sanity check (sending `\x02v` AFTER closing the modal):**
/// a regression that disabled chrome flipping entirely — not just
/// during modal display — would silently pass the two negative
/// assertions above. Re-firing the chord after Esc closes the modal,
/// and asserting that the `LeftPane` title NOW appears, proves the
/// chord itself is wired correctly and only the modal was suppressing
/// it. Without this step the test could pass against a broken build.
///
/// **Why the 100ms sleeps:** same rationale as AC-010 — these are
/// probes for absence-of-change. `screen_eventually` panics on
/// timeout, so it can't directly assert "nothing happened"; we sleep
/// briefly to give any erroneous chrome a fair window to appear, then
/// snapshot and assert the screen still doesn't contain it. The sleep
/// is bounded and only used for negative assertions; positive ones
/// still use `screen_eventually`.
///
/// **Side effect we intentionally don't assert on:** typing `?` into
/// the modal's path field can trigger a wildmenu refresh and surface
/// matches whose names start with `?`. We don't pin that — the key
/// assertion is the *absence* of the help overlay, not what the
/// modal's path field renders.
#[test]
#[ignore = "slow-tier PTY E2E; runs via `just check-e2e` / `just test-e2e`"]
#[serial]
fn modal_absorbs_help_chord_and_prefix_chord_while_open() {
    let mut handle = spawn_codemux();

    // Steady state: fake's prompt rendered, no modal yet. Checking
    // both directions guards against a future change that would open
    // the modal at boot and make subsequent assertions vacuous.
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

    // Open the modal: prefix + `c`. Wait for the `@local` host-marker
    // chrome to confirm the modal is up.
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

    // --- Probe 1: help chord ---
    //
    // Send `?`. Normally (with the runtime's prefix armed) this would
    // open the help overlay. Inside the modal it should just route to
    // the path field as a literal character — the modal's keymap has
    // no binding for `?` that would surface help chrome.
    send_keys(&mut handle, "?");

    // Give any erroneous help overlay a fair window to appear, then
    // snapshot and assert it didn't. Same rationale as AC-010: this
    // is a negative assertion, so we have to wait rather than poll.
    thread::sleep(Duration::from_millis(100));
    let after_help = screen_eventually(
        &mut handle,
        |s| s.contents().contains("@local"),
        Duration::from_secs(5),
    );
    assert!(
        after_help.contents().contains("@local"),
        "expected modal to stay open after `?`; got:\n{}",
        after_help.contents()
    );
    assert!(
        !after_help.contents().contains(" codemux help "),
        "expected no help overlay after `?` while modal is open; got:\n{}",
        after_help.contents()
    );

    // --- Probe 2: prefix + v ---
    //
    // Send `\x02v`. Normally this would arm the runtime's prefix and
    // dispatch `PrefixAction::ToggleNav`, flipping the chrome to
    // LeftPane. Inside the modal, the `\x02` byte is filtered by the
    // modal's `ctrl_modified_keys_are_dropped` guard before the
    // runtime ever sees it, so neither byte should reach the prefix
    // state machine.
    send_keys(&mut handle, "\x02v");

    thread::sleep(Duration::from_millis(100));
    let after_prefix = screen_eventually(
        &mut handle,
        |s| s.contents().contains("@local"),
        Duration::from_secs(5),
    );
    assert!(
        after_prefix.contents().contains("@local"),
        "expected modal to stay open after `\\x02v`; got:\n{}",
        after_prefix.contents()
    );
    assert!(
        !after_prefix.contents().contains(" agents "),
        "expected no LeftPane navigator title after `\\x02v` while modal is open; got:\n{}",
        after_prefix.contents()
    );

    // --- Sanity check: chord works once modal is closed ---
    //
    // Close the modal with Esc, then re-fire `\x02v`. The LeftPane
    // title should now appear, proving the suppression above was
    // scoped to the modal — not a permanent disablement of the chord.
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

    send_keys(&mut handle, "\x02v");
    let flipped = screen_eventually(
        &mut handle,
        |s| s.contents().contains(" agents "),
        Duration::from_secs(5),
    );
    assert!(
        flipped.contents().contains(" agents "),
        "expected LeftPane navigator title after `\\x02v` once modal is closed; got:\n{}",
        flipped.contents()
    );
}
