//! AC-035 (reaping the focused agent moves focus to the new tail, not
//! to `previous_focused`): pin the `remove_at` focus-clamp branch
//! end-to-end with a real three-agent codemux PTY. The runtime already
//! carries unit-test cousins for the clamp (`kill_focused_clamps_focus_when_killing_last_tab`
//! and the `remove_at` bookkeeping tests in `apps/tui/src/runtime.rs`),
//! but none drive the kill chord through the real keymap with three
//! live agents on screen, a carefully-set `previous_focused` slot, and
//! observe that the rendered focus indicator clamps to the NEW tail
//! rather than bouncing to the recorded prior focus.
//!
//! This test composes on top of AC-002's spawn-from-modal path
//! (`pty_spawn_action.rs::enter_in_empty_modal_spawns_second_agent_in_scratch_dir`),
//! AC-010's digit-jump dispatch (`pty_digit.rs`), and AC-014's kill
//! chord (`pty_lifecycle.rs::kill_last_agent_auto_exits_codemux`): the
//! setup steps below combine all three, and the clamp assertion here
//! only makes sense once those pipelines are known to work.
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

/// Spawn three agents in the scratch dir, drive focus to a state where
/// `previous_focused` points at `[1]` and `focused` is the tail `[3]`,
/// send `Ctrl+B x` to kill the focused agent, and assert focus clamps
/// to the new tail `[2]` rather than bouncing back to `[1]`.
///
/// **Why three agents (and not two):** the AC's distinguishing behavior
/// — "focus moves to the new tail, NOT to `previous_focused`" — is
/// only observable when those two slots are different. With two
/// agents, killing the focused tail (`[2]`) leaves a single agent
/// (`[1]`), which is simultaneously the new tail AND the only possible
/// `previous_focused` slot. The clamp and the (hypothetical) bounce
/// would land on the same agent, and the test could not tell them
/// apart. Three agents is the minimum that distinguishes the two
/// branches.
///
/// **Why we shuffle focus before killing:** straight after spawning
/// agent 3, the navigator records `previous_focused = [2]` (the agent
/// that was focused before the spawn promoted `[3]`). Killing `[3]` in
/// that state would clamp focus to `[2]`, but `previous_focused` is
/// also `[2]`, so the test could not tell the clamp branch apart from
/// a buggy bounce. The two `prefix <digit>` moves (`1` then `3`)
/// rewire the bookkeeping to `focused=[3], previous_focused=[1]`,
/// which makes the clamp (`[2]`) and the hypothetical bounce (`[1]`)
/// distinguishable.
///
/// **Why `\x021` then bare `3` then bare `x`:** the prefix state
/// machine is sticky for nav dispatches (`is_nav_dispatch` in
/// `runtime.rs` returns true for `FocusAt`, false for `KillAgent`),
/// so after `\x021` fires `FocusAt(0)` the state stays armed, the
/// subsequent bare `3` fires `FocusAt(2)` and stays armed, and the
/// final bare `x` fires `KillAgent` and falls back to `Idle`.
/// Sending `\x02x` instead would interpret the second `\x02` as the
/// double-prefix passthrough gesture (the prefix is still armed from
/// the digit moves) and forward `^B` to the focused PTY, which would
/// pollute the agent stream and break the kill dispatch. AC-009's
/// `pty_focus.rs`, AC-010's `pty_digit.rs`, and AC-011's `pty_bounce.rs`
/// all pin the same sticky-mode behavior for different dispatches;
/// this test rides on it for the digit moves and exits it cleanly via
/// the non-nav `KillAgent`.
///
/// **Observation strategy:** `render_left_pane` (in `runtime.rs::4689`)
/// prefixes the focused agent's row with `"> "` and unfocused rows
/// with `"  "`. So `> [2]` means "agent 2 is focused". The kill
/// removes the `[3]` row from the navigator entirely, so the
/// post-kill predicate also asserts the screen no longer contains
/// `[3]` anywhere — proving the reap landed AND the focus moved to
/// the new tail. The two negative assertions (`!contains("> [1]")`,
/// `!contains("[3]")`) are the load-bearing ones: a regression that
/// bounced focus to `previous_focused` would land on `[1]` (visible
/// as `> [1]`), and a regression that failed to actually reap the
/// killed agent would still show `[3]` in the navigator.
///
/// **What this pins:**
/// - `remove_at`'s focus-clamp branch (`runtime.rs::4690-4694`,
///   specifically `if idx == self.focused { self.focused =
///   self.focused.min(self.agents.len() - 1); }`): when the removed
///   slot was the focused tail, focus snaps to the new tail
///   (`agents.len() - 1`).
/// - The non-bounce semantics of the kill path: `kill_focused` does
///   NOT call `change_focus`, so it does NOT consult
///   `previous_focused`. The bounce slot is a `FocusLast` concern
///   (AC-011), not a kill concern (AC-035).
/// - The `previous_focused` bookkeeping survives the reap:
///   `previous_focused = Some(0)` is below the removed `idx = 2`, so
///   neither the `prev > idx` decrement branch nor the `prev == idx`
///   clear branch fires; the pointer stays valid. A regression that
///   incorrectly cleared `previous_focused` on every reap would not
///   be caught here (it would require a follow-up `prefix Tab` to
///   observe), but a regression that bounced focus through it on the
///   kill itself would be caught directly.
#[test]
#[ignore = "slow-tier PTY E2E; runs via `just check-e2e` / `just test-e2e`"]
#[serial]
fn reap_focused_agent_clamps_to_new_tail_not_previous_focused() {
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

    // Spawn agent 2 in scratch. After this, focus is on `[2]` and
    // `previous_focused = Some(0)` (the spawn flow records the prior
    // focus when it promotes the new agent — AC-034).
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
            !c.contains("@local") && c.contains("> [2]")
        },
        Duration::from_secs(10),
    );

    // Spawn agent 3 in scratch. After this, focus is on `[3]` and
    // `previous_focused = Some(1)` (the slot `[2]` occupied before
    // being demoted by the spawn). Crucially, `previous_focused` now
    // points at the slot that WOULD become the new tail after killing
    // `[3]` — so we have to rewire it below before the kill to make
    // the AC-035 distinction observable.
    send_keys(&mut handle, "\x02c");
    screen_eventually(
        &mut handle,
        |s| s.contents().contains("@local"),
        Duration::from_secs(5),
    );
    send_keys(&mut handle, "\r");
    let after_spawn3 = screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            !c.contains("@local") && c.contains("> [3]")
        },
        Duration::from_secs(10),
    );
    assert!(
        after_spawn3.contents().contains("[1]"),
        "expected `[1]` in navigator after spawning the third agent; got:\n{}",
        after_spawn3.contents()
    );
    assert!(
        after_spawn3.contents().contains("[2]"),
        "expected `[2]` in navigator after spawning the third agent; got:\n{}",
        after_spawn3.contents()
    );
    assert!(
        after_spawn3.contents().contains("> [3]"),
        "expected agent 3 focused after spawn-from-modal; got:\n{}",
        after_spawn3.contents()
    );

    // Rewire focus: `prefix 1` moves focus `[3] -> [1]`, recording
    // `previous_focused = Some(2)` (the slot `[3]` was at). Sticky
    // prefix stays armed because `FocusAt` is a nav dispatch.
    send_keys(&mut handle, "\x021");
    screen_eventually(
        &mut handle,
        |s| s.contents().contains("> [1]"),
        Duration::from_secs(5),
    );

    // Sticky prefix is still armed; bare `3` moves focus `[1] -> [3]`,
    // recording `previous_focused = Some(0)`. After this:
    //   focused = 2 (one-indexed: `[3]`)
    //   previous_focused = Some(0) (one-indexed: `[1]`)
    // — distinct from the new tail (`[2]`, index 1) that the kill
    // clamp will produce, which is the whole point of the rewire.
    send_keys(&mut handle, "3");
    screen_eventually(
        &mut handle,
        |s| s.contents().contains("> [3]"),
        Duration::from_secs(5),
    );

    // Kill the focused agent. Sticky prefix is still armed from the
    // digit moves above; bare `x` fires `KillAgent` (NOT a nav
    // dispatch, so the state machine falls back to Idle) which calls
    // `kill_focused` -> `remove_at(2)`. The clamp branch
    // (`if idx == self.focused { self.focused =
    // self.focused.min(self.agents.len() - 1); }`) sets `focused = 1`
    // — the new tail, which displays as `> [2]`. Sending `\x02x`
    // here would be wrong: the prefix is already armed, so the second
    // `\x02` would hit the double-prefix passthrough and forward `^B`
    // to the focused PTY.
    send_keys(&mut handle, "x");
    let after_kill = screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            c.contains("> [2]") && !c.contains("[3]")
        },
        Duration::from_secs(5),
    );

    // The clamp branch landed on the new tail.
    assert!(
        after_kill.contents().contains("> [2]"),
        "expected focus clamped to new tail `[2]` after killing `[3]`; got:\n{}",
        after_kill.contents()
    );
    // It did NOT bounce to `previous_focused = [1]`. This is the
    // load-bearing negative assertion: a regression that incorrectly
    // routed the kill through `change_focus(previous_focused)` would
    // land on `[1]` and fail right here.
    assert!(
        !after_kill.contents().contains("> [1]"),
        "expected focus NOT bounced to `previous_focused = [1]` after killing `[3]`; got:\n{}",
        after_kill.contents()
    );
    // The killed agent's row is gone from the navigator — proves the
    // reap actually shrunk the agent Vec. Without this, a regression
    // that left a stale row but moved the focus indicator would still
    // pass the two assertions above.
    assert!(
        !after_kill.contents().contains("[3]"),
        "expected `[3]` row removed from navigator after kill; got:\n{}",
        after_kill.contents()
    );
}
