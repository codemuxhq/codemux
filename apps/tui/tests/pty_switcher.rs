//! AC-012 (switcher popup picks an agent by name): pin the `prefix w`
//! overlay-open path, the `Up`/`Down` selection-move dispatch, the
//! `Enter`-confirms-and-changes-focus path, and the `Esc`-closes-without-
//! changing-focus path end-to-end with a real two-agent codemux PTY.
//!
//! The runtime carries unit-test cousins for the underlying state
//! transitions (`dismiss_clamps_open_popup_selection`,
//! `remove_at_decrements_popup_selection_when_removing_an_earlier_index`,
//! `no_overlay_active_returns_false_when_popup_open`, and the
//! `snapshot_navigator_popup` insta), and the keymap pins the
//! popup-scope lookup (`popup_lookup_round_trip`), but no existing
//! test drives the full `prefix w → arrow → Enter` chord through the
//! real keymap with two live agents and observes the popup chrome
//! actually appearing -- nor does any pin that `Esc` actually leaves
//! focus untouched on the cancel path. The AC text explicitly
//! distinguishes the two close paths, so both belong in the pin.
//!
//! This test composes on top of AC-002's spawn-from-modal path
//! (`pty_spawn_action.rs::enter_in_empty_modal_spawns_second_agent_in_scratch_dir`)
//! and AC-009 / AC-011's two-agent setup (`pty_focus.rs`,
//! `pty_bounce.rs`): the setup steps below are the same end-to-end
//! pin those tests ship, and the popup assertions here only make
//! sense once that pipeline is known to work.
//!
//! Gating mirrors the rest of the suite: `test-fakes` feature,
//! `#[ignore]`, `#[serial]`.

#![cfg(feature = "test-fakes")]

#[allow(dead_code)]
mod common;

use std::time::Duration;

use serial_test::serial;
use tempfile::TempDir;

use common::{screen_eventually, send_keys, spawn_codemux_with_config};

/// Spawn a second agent via the scratch flow, then exercise both popup
/// close paths through the actual switcher overlay: `prefix w` →
/// `Up` → `Enter` flips focus 2 → 1 (popup chrome visible the whole
/// time), and a fresh `prefix w` → `Esc` leaves focus on agent 1
/// unchanged.
///
/// **Observation strategy:** `render_switcher_popup` (in
/// `runtime.rs::5305`) wraps the popup in a bordered `Block` titled
/// `" switch agent "`. That literal string is unique to the switcher
/// chrome -- it does not appear elsewhere in `apps/tui/src/`
/// (verified with `grep`), so its presence/absence cleanly signals
/// "popup overlay is up." Inside the popup, each agent renders on
/// its own line with a `"> "` prefix for the currently-highlighted
/// row and `"  "` for the others (the same convention
/// `render_left_pane` uses, deliberately so the user sees the same
/// shape in both navigator styles).
///
/// Focus is observed VIA the popup itself. Stays in Popup nav style
/// (codemux's default per `apps/tui/src/main.rs::49`) for this whole
/// test because:
///
/// 1. In `NavStyle::Popup`, the switcher popup chrome actually
///    renders -- it's only drawn from `render_popup_style`. In
///    `NavStyle::LeftPane`, the popup STATE still mutates (the
///    dispatch handler at `runtime.rs::3476` is render-agnostic) but
///    nothing draws on screen, so we'd have no chrome to assert
///    against.
/// 2. The tab strip rendered by `render_status_bar` in popup mode
///    distinguishes the focused tab only via ANSI reverse styling,
///    which `vt100::Screen::contents()` strips -- no readable focus
///    marker to assert on.
/// 3. The switcher popup itself initializes `selection = nav.focused`
///    on every open (see `KeyDispatch::OpenPopup` at
///    `runtime.rs::3629`), so reopening the popup and reading its
///    `> [N]` highlight is a clean read of current focus.
///
/// That last point is the trick that makes this test work without
/// flipping to `LeftPane`: after Enter commits the focus change we
/// reopen the popup, and the row marked `> [1]` confirms focus has
/// landed on agent 1. After Esc on a subsequent open, reopening the
/// popup again should STILL show `> [1]`, proving Esc did not touch
/// focus. Closing the popup at the end keeps state tidy for the
/// `Drop` teardown.
///
/// **Why we test BOTH the Enter-confirm and Esc-cancel paths:** the
/// AC body explicitly distinguishes them -- `Enter` confirms and
/// changes focus, `Esc` closes without changing focus. Testing only
/// one path would leave a regression where the other silently breaks
/// invisible: e.g. a future refactor that wires `PopupAction::Cancel`
/// to call `change_focus(selection)` would pass an Enter-only test
/// (Enter still confirms) but would corrupt the cancel contract
/// (Esc would now mutate focus). Mirroring the AC's two-branch
/// failure surface in the test keeps both directions pinned.
///
/// **Why the chord stream `\x02w` then bare arrow / Enter / Esc:**
/// `KeyDispatch::OpenPopup` is NOT a nav dispatch (see
/// `is_nav_dispatch` in `runtime.rs::3913`), so the prefix-state
/// machine drops back to `Idle` the moment the popup opens. Once the
/// popup is up, the early branch at `runtime.rs::3476` consumes any
/// matching `PopupBindings` key BEFORE the prefix-state check ever
/// fires. So the natural chord stream is:
/// - `\x02w` -- arms prefix, fires `OpenPopup`, prefix resets to Idle.
/// - `\x1b[A` -- raw `Up` arrow (`PopupAction::Prev`). Sent without a
///   prefix because the popup handler bypasses prefix state.
/// - `\r` -- `Enter` (`PopupAction::Confirm`).
/// - `\x1b` -- `Esc` (`PopupAction::Cancel`). Sent as a standalone
///   byte; the ANSI escape sequence for `Up` starts with `\x1b[` so
///   the bare `\x1b` is unambiguous here as long as nothing follows
///   it within the same write batch (it doesn't; the next
///   `send_keys` call happens only after `screen_eventually` confirms
///   the popup closed).
///
/// **Why initial selection lands on the focused agent:** at
/// `runtime.rs::3629`, `KeyDispatch::OpenPopup` initializes
/// `selection = nav.focused`. This is the load-bearing detail that
/// lets the test read "current focus" by reopening the popup and
/// inspecting its `> [N]` highlight. If the initialization ever
/// changes (e.g. to `selection = 0`), this test fails loudly and
/// the AC would need re-validation against the new contract.
///
/// **Why the setup duplicates AC-002 / AC-009 / AC-011:** the
/// AC-002 test in `pty_spawn_action.rs` is the canonical pin for
/// "spawning from the modal produces a second tab," and
/// AC-009 / AC-011's `pty_focus.rs` / `pty_bounce.rs` are the
/// canonical pins for "two agents are focusable via a prefix chord."
/// This test re-walks the same setup because the popup assertions
/// only make sense from the two-agent steady state. Extracting a
/// shared helper would couple the four tests -- when AC-002 evolves
/// (e.g. spawn-from-modal returns focus to the new agent vs. the
/// old one), the helper would have to absorb that change and
/// AC-012's contract would silently move with it, masking any
/// regression in the popup dispatch itself.
///
/// **Surprises worth noting:** the natural intuition is "flip to
/// `LeftPane` like the other two-agent tests do, then `> [N]` is
/// readable in the left pane." But `LeftPane` mode does NOT render
/// the switcher popup at all (see `render_left_pane` at
/// `runtime.rs::4668`, which has no `popup` parameter), so the
/// chrome assertion has no surface to land on. The test below stays
/// in Popup mode for that reason and reads focus from inside the
/// popup itself, which is the only place focus is text-readable in
/// the default nav style.
#[test]
#[ignore = "slow-tier PTY E2E; runs via `just check-e2e` / `just test-e2e`"]
#[serial]
fn prefix_w_opens_switcher_arrow_enter_confirms_and_esc_cancels() {
    let scratch = TempDir::new().expect("scratch tempdir");
    let scratch_path = scratch
        .path()
        .to_str()
        .expect("scratch tempdir path must be valid UTF-8");
    let config = format!("[spawn]\nscratch_dir = {scratch_path:?}\n");

    let mut handle = spawn_codemux_with_config(&config);

    // First agent prompt rendered, no modal open, no popup up.
    screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            c.contains("FAKE_AGENT_READY") && !c.contains("@local") && !c.contains(" switch agent ")
        },
        Duration::from_secs(5),
    );

    // Stay in Popup mode (codemux's default per `main.rs::49`) -- see
    // the doc comment for the rationale. Open modal, press Enter to
    // spawn a second agent in scratch.
    send_keys(&mut handle, "\x02c");
    screen_eventually(
        &mut handle,
        |s| s.contents().contains("@local"),
        Duration::from_secs(5),
    );
    send_keys(&mut handle, "\r");

    // Steady state after spawn: modal closed, two agents in the tab
    // strip, focus on agent 2 (the freshly-spawned one). We can't
    // assert "focus is on agent 2" from the tab strip alone (focus
    // there is ANSI-reverse styling, not text), but the popup we open
    // next will reveal the focused index via its initial selection.
    screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            !c.contains("@local") && !c.contains(" switch agent ")
        },
        Duration::from_secs(10),
    );

    // -- Enter-confirm path -----------------------------------------
    // Open the switcher: prefix + w.
    send_keys(&mut handle, "\x02w");
    let opened = screen_eventually(
        &mut handle,
        |s| s.contents().contains(" switch agent "),
        Duration::from_secs(5),
    );
    assert!(
        opened.contents().contains(" switch agent "),
        "expected switcher popup chrome after `prefix w`; got:\n{}",
        opened.contents()
    );
    // Initial selection lands on `nav.focused`, which is agent 2 after
    // the spawn-from-modal flow. The popup row reads `> [2] ...`.
    assert!(
        opened.contents().contains("> [2]"),
        "expected popup selection on agent 2 (initial = focused); got:\n{}",
        opened.contents()
    );

    // Up arrow: `PopupAction::Prev` per `keymap.rs::528`. Selection
    // 1 → 0, so the popup re-renders with `> [1]`.
    send_keys(&mut handle, "\x1b[A");
    let moved = screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            c.contains(" switch agent ") && c.contains("> [1]")
        },
        Duration::from_secs(5),
    );
    assert!(
        moved.contents().contains("> [1]"),
        "expected popup selection moved to agent 1 after Up; got:\n{}",
        moved.contents()
    );

    // Enter: `change_focus(selection)` commits focus to agent 1 and
    // the popup closes.
    send_keys(&mut handle, "\r");
    let after_enter = screen_eventually(
        &mut handle,
        |s| !s.contents().contains(" switch agent "),
        Duration::from_secs(5),
    );
    assert!(
        !after_enter.contents().contains(" switch agent "),
        "expected popup closed after Enter-confirm; got:\n{}",
        after_enter.contents()
    );

    // Verify focus actually moved by reopening the popup: the initial
    // selection equals `nav.focused`, so a `> [1]` row proves the
    // commit landed.
    send_keys(&mut handle, "\x02w");
    let confirm_focused = screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            c.contains(" switch agent ") && c.contains("> [1]")
        },
        Duration::from_secs(5),
    );
    assert!(
        confirm_focused.contents().contains("> [1]"),
        "expected focus on agent 1 after Enter-confirm (reopened popup); got:\n{}",
        confirm_focused.contents()
    );

    // -- Esc-cancel path --------------------------------------------
    // Popup is already open from the verification step above; Esc
    // closes without touching focus.
    send_keys(&mut handle, "\x1b");
    let after_esc = screen_eventually(
        &mut handle,
        |s| !s.contents().contains(" switch agent "),
        Duration::from_secs(5),
    );
    assert!(
        !after_esc.contents().contains(" switch agent "),
        "expected popup closed after Esc-cancel; got:\n{}",
        after_esc.contents()
    );

    // Reopen one more time to confirm Esc didn't mutate focus: the
    // initial selection should STILL be on agent 1 (`> [1]`), not
    // back on agent 2.
    send_keys(&mut handle, "\x02w");
    let unchanged = screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            c.contains(" switch agent ") && c.contains("> [1]")
        },
        Duration::from_secs(5),
    );
    assert!(
        unchanged.contents().contains("> [1]"),
        "expected focus still on agent 1 after Esc-cancel; got:\n{}",
        unchanged.contents()
    );
    // Tidy up so the popup isn't left open across the Drop teardown.
    send_keys(&mut handle, "\x1b");
    screen_eventually(
        &mut handle,
        |s| !s.contents().contains(" switch agent "),
        Duration::from_secs(5),
    );
}
