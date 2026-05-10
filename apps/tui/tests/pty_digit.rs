//! AC-010 (focus an agent by ordinal digit): pin the `prefix <digit>`
//! dispatch end-to-end with a real two-agent codemux PTY. The runtime
//! already carries unit-test cousins for the dispatch wiring
//! (`prefix_digit_focuses_by_one_indexed_position`,
//! `prefix_then_digit_stays_sticky`, and `prefix_zero_is_consumed_no_focus`
//! in `apps/tui/src/runtime.rs`), but none drive the chord through the
//! real keymap with live agents on screen and observe the rendered focus
//! indicator moving (or refusing to move) per ordinal.
//!
//! This test composes on top of AC-002's spawn-from-modal path
//! (`pty_spawn_action.rs::enter_in_empty_modal_spawns_second_agent_in_scratch_dir`)
//! and AC-009 / AC-011's two-agent setup (`pty_focus.rs`, `pty_bounce.rs`):
//! the setup steps below are the same end-to-end pin those tests ship,
//! and the digit-focus assertions here only make sense once that pipeline
//! is known to work.
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

/// Spawn a second agent via the scratch flow, then send `Ctrl+B` followed
/// by the digit stream `1 2 5 0` and assert focus moves to ordinal 1,
/// back to ordinal 2, then ignores `5` (out of range with only two
/// agents) and `0` (reserved).
///
/// **Why test four digits, not all nine:** the two-agent setup pins the
/// only distinctions the AC actually expresses -- in-range ordinals (1
/// and 2), out-of-range ordinals (5 with only two agents), and the
/// reserved zero. A nine-agent setup would walk 1-9 but would only
/// repeat the in-range case eight more times while adding eight extra
/// spawn-from-modal cycles to setup; the assertion surface that catches
/// regressions is the in-range / out-of-range / zero split, not the
/// raw count of digits exercised.
///
/// **Observation strategy:** `render_left_pane` (in `runtime.rs::4689`)
/// prefixes the focused agent's row with `"> "` and unfocused rows
/// with `"  "`. So `> [1]` means "agent 1 is focused" and `> [2]`
/// means "agent 2 is focused". Asserting on those substrings reads
/// the rendered grid directly, the same way AC-009's `pty_focus.rs`
/// and AC-011's `pty_bounce.rs` read the focus indicator.
///
/// **Why one `\x02` for four digits, not four:** the runtime's state
/// machine is sticky for nav dispatches (`is_nav_dispatch` in
/// `runtime.rs` matches `FocusAt`), so after the first `\x021` fires
/// `FocusAt(0)` the prefix stays armed and subsequent bare digits keep
/// dispatching. Sending `\x02` again between digits would interpret the
/// second `\x02` as the double-prefix passthrough gesture (the prefix
/// is still armed, so the runtime forwards `^B` literally to the
/// focused PTY) and pollute the agent stream rather than re-arming.
/// AC-009's `pty_focus.rs` pins the same sticky behavior for
/// `FocusNext`, AC-011's `pty_bounce.rs` pins it for `FocusLast`; this
/// test pins it for `FocusAt`.
///
/// **Why the 100ms sleep before the no-op assertions:** a no-op
/// dispatch produces no screen change, so `screen_eventually` polling
/// for a steady state cannot distinguish "the predicate was already
/// true and we never observed a flip" from "the dispatch did flip
/// focus and we caught the pre-flip state on the first poll". A small
/// bounded sleep after sending the digit AND before asserting gives
/// the runtime enough time to render any erroneous flip before we
/// check. This is the one acceptable use of `sleep` in the harness:
/// it is bounded, it probes for absence-of-change, and the alternative
/// (asserting on a transient state) would silently mask regressions
/// that turn a no-op into an off-by-one focus jump.
///
/// **Why the setup duplicates AC-002 / AC-009 / AC-011:** those tests
/// are the canonical pins for "spawning from the modal produces a
/// second tab" and "two agents are focusable via a prefix chord."
/// This test re-walks the same setup steps because the digit-focus
/// assertions only make sense from the two-agent steady state.
/// Extracting a shared helper would couple the four tests -- when
/// AC-002 evolves (e.g. spawn-from-modal returns focus to the new
/// agent vs. the old one), the helper would have to absorb that
/// change and AC-010's contract would silently move with it, masking
/// any regression in the digit dispatch itself.
#[test]
#[ignore = "slow-tier PTY E2E; runs via `just check-e2e` / `just test-e2e`"]
#[serial]
fn prefix_digit_focuses_by_ordinal_and_ignores_out_of_range() {
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
    // the same end-state AC-002 / AC-009 / AC-011 pin; we assert it
    // here as the setup-correctness gate before exercising the digit
    // dispatch.
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

    // Arm prefix once and dispatch FocusAt(0). Focus 2 -> 1.
    send_keys(&mut handle, "\x021");
    let after_1 = screen_eventually(
        &mut handle,
        |s| s.contents().contains("> [1]"),
        Duration::from_secs(5),
    );
    assert!(
        after_1.contents().contains("> [1]"),
        "expected agent 1 focused after `prefix 1`; got:\n{}",
        after_1.contents()
    );
    assert!(
        !after_1.contents().contains("> [2]"),
        "expected agent 2 unfocused after `prefix 1`; got:\n{}",
        after_1.contents()
    );

    // Sticky prefix is still armed (`FocusAt` is a nav dispatch); a
    // bare `2` flips focus 1 -> 2 without re-pressing the prefix. See
    // the function's doc comment for why sending `\x022` here would
    // be wrong (double-prefix passthrough pollutes the agent's PTY).
    send_keys(&mut handle, "2");
    let after_2 = screen_eventually(
        &mut handle,
        |s| s.contents().contains("> [2]"),
        Duration::from_secs(5),
    );
    assert!(
        after_2.contents().contains("> [2]"),
        "expected agent 2 focused after `2`; got:\n{}",
        after_2.contents()
    );
    assert!(
        !after_2.contents().contains("> [1]"),
        "expected agent 1 unfocused after `2`; got:\n{}",
        after_2.contents()
    );

    // Out-of-range digit: `5` resolves to `FocusAt(4)`, and the
    // dispatch handler guards `idx < nav.agents.len()` -- with only
    // two agents the dispatch is a no-op. Sleep briefly to give the
    // runtime a chance to (erroneously) render a flip if the guard
    // regressed, then assert the focus is still on agent 2.
    send_keys(&mut handle, "5");
    std::thread::sleep(Duration::from_millis(100));
    let after_5 = screen_eventually(
        &mut handle,
        |s| s.contents().contains("> [2]"),
        Duration::from_secs(5),
    );
    assert!(
        after_5.contents().contains("> [2]"),
        "expected agent 2 still focused after out-of-range `5`; got:\n{}",
        after_5.contents()
    );
    assert!(
        !after_5.contents().contains("> [1]"),
        "expected agent 1 unfocused after out-of-range `5`; got:\n{}",
        after_5.contents()
    );

    // Reserved zero: `compute_awaiting_dispatch` returns
    // `KeyDispatch::Consume` for `prefix 0` (no `FocusAt`), so the
    // dispatch never fires. Same sleep-then-assert pattern as the
    // out-of-range case.
    send_keys(&mut handle, "0");
    std::thread::sleep(Duration::from_millis(100));
    let after_0 = screen_eventually(
        &mut handle,
        |s| s.contents().contains("> [2]"),
        Duration::from_secs(5),
    );
    assert!(
        after_0.contents().contains("> [2]"),
        "expected agent 2 still focused after reserved `0`; got:\n{}",
        after_0.contents()
    );
    assert!(
        !after_0.contents().contains("> [1]"),
        "expected agent 1 unfocused after reserved `0`; got:\n{}",
        after_0.contents()
    );
}
