//! AC-011 (bounce to the previously-focused agent): pin the `prefix Tab`
//! dispatch end-to-end with a real two-agent codemux PTY. The runtime
//! already carries unit-test cousins for this wiring
//! (`prefix_tab_dispatches_focus_last` and
//! `change_focus_lets_alt_tab_bounce_via_two_calls` in
//! `apps/tui/src/runtime.rs`), but neither drives the chord through the
//! real keymap with two live agents on screen and observes the rendered
//! focus indicator flipping back and forth.
//!
//! This test composes on top of AC-002's spawn-from-modal path
//! (`pty_spawn_action.rs::enter_in_empty_modal_spawns_second_agent_in_scratch_dir`)
//! and AC-009's two-agent setup (`pty_focus.rs`): the setup steps below
//! are the same end-to-end pin those tests ship, and the bounce
//! assertions here only make sense once that pipeline is known to work.
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

/// Spawn a second agent via the scratch flow, then send `Ctrl+B Tab`
/// twice and assert focus oscillates: agent 2 -> agent 1 -> agent 2.
///
/// **Observation strategy:** `render_left_pane` (in `runtime.rs::4689`)
/// prefixes the focused agent's row with `"> "` and unfocused rows
/// with `"  "`. So `> [1]` means "agent 1 is focused" and `> [2]`
/// means "agent 2 is focused". Asserting on those substrings reads
/// the rendered grid directly, the same way AC-009's `pty_focus.rs`
/// reads the focus indicator.
///
/// **Why one `\x02` for two moves, not two:** the AC-011 text reads
/// "Repeated `prefix Tab` oscillates", which sounds like the user
/// should press the full prefix-plus-Tab chord twice. But the runtime's
/// state machine is sticky for nav dispatches (`is_nav_dispatch` in
/// `runtime.rs` returns true for `FocusLast`; the unit test
/// `prefix_then_tab_stays_sticky` pins this), and the unit test
/// `prefix_then_repeated_nav_keys_keeps_dispatching` shows the sticky
/// state allows N repeated nav keys after a single prefix. So sending
/// `\x02\t\x02\t` would interpret the second `\x02` not as a fresh
/// prefix but as the **double-prefix passthrough** gesture: the prefix
/// state is still armed from the first chord, and per
/// `compute_awaiting_dispatch` the second prefix-byte gets forwarded
/// literally to the focused PTY (visible as `^B` echoed by the fake
/// agent), and the subsequent `\t` becomes a stray Tab to the agent
/// rather than a `FocusLast` dispatch. The chord stream `\x02\t\t`
/// arms the prefix once, fires `FocusLast` twice, and produces the
/// oscillation the AC actually describes -- it is also what a human
/// user would type given how sticky-prefix works in practice.
/// AC-009's `pty_focus.rs::prefix_n_cycles_focus_between_two_agents`
/// pins the same sticky behavior for `FocusNext`; this test pins it
/// for `FocusLast` and additionally pins that the previous-focus slot
/// is recorded by the focus change so the second `Tab` flips back
/// (rather than, say, repeating the same `2 -> 1` move idempotently).
///
/// **Why the setup duplicates AC-002 / AC-009:** the AC-002 test in
/// `pty_spawn_action.rs` is the canonical pin for "spawning from the
/// modal produces a second tab," and AC-009's `pty_focus.rs` is the
/// canonical pin for "two agents are focusable via a prefix chord."
/// This test re-walks the same setup steps because the bounce
/// assertions only make sense from the two-agent steady state.
/// Extracting a shared helper would couple the three tests -- when
/// AC-002 evolves (e.g. spawn-from-modal returns focus to the new
/// agent vs. the old one), the helper would have to absorb that
/// change and AC-011's contract would silently move with it, masking
/// any regression in the bounce dispatch itself.
#[test]
#[ignore = "slow-tier PTY E2E; runs via `just check-e2e` / `just test-e2e`"]
#[serial]
fn prefix_tab_bounces_focus_between_two_agents() {
    let scratch = TempDir::new().expect("scratch tempdir");
    let scratch_path = scratch
        .path()
        .to_str()
        .expect("scratch tempdir path must be valid UTF-8");
    let config = format!("[spawn]\nscratch_dir = {scratch_path:?}\n");

    let mut handle = spawn_codemux_with_config(&config);

    // First agent prompt rendered, no modal open.
    screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            c.contains("FAKE_AGENT_READY") && !c.contains("@local")
        },
        Duration::from_secs(5),
    );

    // Flip to LeftPane so the focus indicator (`> ` prefix) is visible.
    send_keys(&mut handle, "\x02v");
    screen_eventually(
        &mut handle,
        |s| s.contents().contains(" agents "),
        Duration::from_secs(5),
    );

    // Open modal, press Enter to spawn a second agent in scratch.
    send_keys(&mut handle, "\x02c");
    screen_eventually(
        &mut handle,
        |s| s.contents().contains("@local"),
        Duration::from_secs(5),
    );
    send_keys(&mut handle, "\r");

    // Steady state after spawn: both agents in the navigator, modal
    // closed, focus on the freshly-spawned agent (agent 2). This is
    // the same end-state AC-002 and AC-009 pin; we assert it here as
    // the setup-correctness gate before exercising the bounce dispatch.
    // Critically, the spawn-from-modal flow also records agent 1 as
    // the previous focus, so the very first `prefix Tab` should
    // bounce back to it.
    let after_spawn = screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            !c.contains("@local") && c.contains("> [2]")
        },
        Duration::from_secs(10),
    );
    assert!(
        after_spawn.contents().contains("> [2]"),
        "expected agent 2 focused after spawn-from-modal; got:\n{}",
        after_spawn.contents()
    );
    assert!(
        !after_spawn.contents().contains("> [1]"),
        "expected agent 1 unfocused after spawn-from-modal; got:\n{}",
        after_spawn.contents()
    );

    // First bounce: prefix + Tab. Focus 2 -> 1 (the previously-focused
    // agent, recorded when the spawn-from-modal flow promoted agent 2).
    send_keys(&mut handle, "\x02\t");
    let after_tab1 = screen_eventually(
        &mut handle,
        |s| s.contents().contains("> [1]"),
        Duration::from_secs(5),
    );
    assert!(
        after_tab1.contents().contains("> [1]"),
        "expected agent 1 focused after first `prefix Tab`; got:\n{}",
        after_tab1.contents()
    );
    assert!(
        !after_tab1.contents().contains("> [2]"),
        "expected agent 2 unfocused after first `prefix Tab`; got:\n{}",
        after_tab1.contents()
    );

    // Second bounce: sticky prefix is still armed (`FocusLast` is a
    // nav dispatch); a bare `\t` flips focus 1 -> 2 without
    // re-pressing the prefix. See the function's doc comment for
    // why sending `\x02\t` here would be wrong (double-prefix
    // passthrough pollutes the agent's PTY with `^B`).
    send_keys(&mut handle, "\t");
    let after_tab2 = screen_eventually(
        &mut handle,
        |s| s.contents().contains("> [2]"),
        Duration::from_secs(5),
    );
    assert!(
        after_tab2.contents().contains("> [2]"),
        "expected agent 2 focused after second `prefix Tab`; got:\n{}",
        after_tab2.contents()
    );
    assert!(
        !after_tab2.contents().contains("> [1]"),
        "expected agent 1 unfocused after second `prefix Tab`; got:\n{}",
        after_tab2.contents()
    );
}
