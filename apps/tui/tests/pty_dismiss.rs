//! AC-015 (dismiss a crashed/failed agent; no-op on live): pin both
//! branches of the dismiss chord end-to-end through a real PTY.
//!
//! Unit tests already cover each branch in isolation:
//!
//! - `prefix_d_dispatches_dismiss_agent` — `prefix d` resolves to
//!   `PrefixAction::DismissAgent` against `Bindings::default()`.
//! - `dismiss_no_op_on_focused_ready_agent` — `dismiss_focused` returns
//!   `false` and leaves the Vec untouched when the focused agent is
//!   `Ready`.
//! - `dismiss_removes_focused_crashed_agent_and_clamps_focus` /
//!   `dismiss_removes_focused_failed_agent` — `dismiss_focused`
//!   removes the entry and clamps focus when the focused agent is in a
//!   terminal state.
//!
//! What no existing test covers is the full pipeline: real PTY,
//! real `dispatch_key`, real `KeyDispatch::DismissAgent` arm, real
//! reap loop, real renderer. A regression that broke the wiring
//! between the chord and `nav.dismiss_focused()` — say, swapping in
//! `kill_focused` by mistake (which would happily punch through a
//! `Ready` agent) — would pass every unit test in the list above and
//! still ship the bug. This test would catch it: the no-op assertion
//! would fail because `[1]` would be gone.
//!
//! ## Why one PTY fixture for both branches
//!
//! The dismiss path and the no-op path share setup: both need a
//! Crashed agent AND a Ready agent live on the same codemux. Splitting
//! into two `#[test]` fns would double the spawn + modal-spawn + crash
//! cost (~1s per fixture on a warm box, more on a cold cache) for no
//! additional coverage — the two branches are independent code paths
//! that happen to live in the same function (`dismiss_focused`), so
//! exercising them sequentially against the same `NavState` is as
//! good as exercising them against two fresh ones.
//!
//! ## Why we use `fake_agent_crashing` for BOTH agents
//!
//! The harness sets `CODEMUX_AGENT_BIN` once per codemux process; both
//! the initial agent and modal-spawned agents are forked from the
//! same path. So we can't run agent 1 against `fake_agent` (happy
//! path, Ready) and agent 2 against `fake_agent_crashing` (Crashed) —
//! they have to share a binary.
//!
//! `fake_agent_crashing` is the right choice for both: it only exits
//! when its stdin delivers the literal `QUIT` line. Until then it
//! sits in `Ready` exactly like `fake_agent`. Sending `QUIT\r` only
//! to the focused agent's PTY (via the keymap Forward path) crashes
//! that one and leaves the other untouched. The byte path is the
//! same one `pty_crash.rs` documents.
//!
//! ## Why the no-op assertion uses a brief sleep
//!
//! Same rationale as AC-010's digit-jump no-op path: we are probing
//! the absence of a state change. `screen_eventually` waits for a
//! predicate to *become* true, which is the wrong primitive when the
//! observable is that something *did not* happen. The 200ms pause
//! gives codemux a generous window to (incorrectly) react to the
//! chord; if `[1]` is still on screen after that window, no reaction
//! occurred. This is the single sleep exception the determinism rule
//! allows for absence-of-event tests.
//!
//! ## What this DOESN'T verify directly
//!
//! - Dismiss removes a `Failed` agent (bootstrap-error path). The
//!   unit test `dismiss_removes_focused_failed_agent` pins that
//!   branch; reaching `Failed` end-to-end requires faking an SSH
//!   bootstrap failure, which is out of scope here. `Crashed` and
//!   `Failed` share the same `is_dismissable` guard, so exercising
//!   one through the PTY proves the chord-to-handler wiring for both.
//! - Focus clamping after dismiss-from-tail. The unit test
//!   `dismiss_removes_focused_crashed_agent_and_clamps_focus` pins
//!   the index bookkeeping; this test only asserts that the entry is
//!   gone from the navigator list.
//!
//! Gating mirrors the rest of the slow tier: `test-fakes` feature,
//! `#[ignore]` so the suite ships through `just check-e2e` only, and
//! `#[serial]` because the PTY harness is not safe to run in parallel.

#![cfg(feature = "test-fakes")]

#[allow(dead_code)]
mod common;

use std::thread;
use std::time::Duration;

use serial_test::serial;
use tempfile::TempDir;

use common::{screen_eventually, send_keys, spawn_codemux_with_agent_bin, test_fake_bin};

/// Boot codemux with `fake_agent_crashing` as the agent binary, spawn
/// a second agent through the modal so the navigator has two live
/// `Ready` agents, then exercise both AC-015 branches in sequence:
///
/// 1. **Dismiss path:** crash the focused (second) agent by sending
///    `QUIT\r` through the keymap Forward path. Wait for the `exit 42`
///    crash banner. Send `prefix d`. Assert `[2]` is gone from the
///    navigator and the banner is gone.
/// 2. **No-op path:** with `[1]` (still `Ready`) focused after the
///    dismiss, send `prefix d` again. Wait 200ms for codemux to
///    (incorrectly) react. Assert `[1]` is still on screen — meaning
///    `dismiss_focused` returned `false` and codemux is still alive.
///
/// **Observation strategy:** ordinal prefixes (`[1]`, `[2]`) in the
/// `LeftPane` chrome are the structural fingerprint, same as
/// `pty_spawn_action.rs`. The `exit 42` banner substring (unique to
/// `render_crash_banner`'s format string for a non-zero exit) is the
/// fingerprint for the Crashed state — see `pty_crash.rs` for the
/// full rationale on why `42` and not `1`.
///
/// **Why `prefix v` before `prefix c`:** the default `Popup` chrome
/// only renders the focused agent's pane, so a freshly-spawned second
/// agent is invisible until we flip to `LeftPane`. Same pattern as
/// `pty_spawn_action.rs`.
///
/// **Why we don't `wait_for_exit` after the no-op:** the no-op
/// assertion is implicitly a liveness assertion. If `prefix d`
/// dismissed the last live agent by mistake, AC-036's "last agent
/// gone -> codemux exits" path would fire and the codemux process
/// would terminate. `screen_eventually` would then either panic on
/// `Disconnected` channel or return a screen without `[1]`. Both
/// fail the assertion.
#[test]
#[ignore = "slow-tier PTY E2E; runs via `just check-e2e` / `just test-e2e`"]
#[serial]
fn prefix_d_dismisses_crashed_agent_and_is_noop_on_ready() {
    // Scratch tempdir held in the test so it outlives the codemux
    // child. Same setup pattern as `pty_spawn_action.rs`.
    let scratch = TempDir::new().expect("scratch tempdir");
    let scratch_path = scratch
        .path()
        .to_str()
        .expect("scratch tempdir path must be valid UTF-8");
    let config = format!("[spawn]\nscratch_dir = {scratch_path:?}\n");

    let agent_bin = test_fake_bin("fake_agent_crashing");
    let mut handle = spawn_codemux_with_agent_bin(&agent_bin, &config);

    // 1. Steady state: fake's prompt is on screen, no modal yet.
    screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            c.contains("FAKE_AGENT_READY") && !c.contains("@local")
        },
        Duration::from_secs(5),
    );

    // 2. Flip to LeftPane so the navigator list is observable.
    //    ` agents ` is the LeftPane chrome fingerprint (see
    //    `pty_nav.rs`).
    send_keys(&mut handle, "\x02v");
    screen_eventually(
        &mut handle,
        |s| s.contents().contains(" agents "),
        Duration::from_secs(5),
    );

    // 3. Spawn agent 2 through the modal. Both agents now run
    //    `fake_agent_crashing` but both are `Ready` — neither has
    //    received `QUIT`.
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
            !c.contains("@local") && c.contains("[1]") && c.contains("[2]")
        },
        Duration::from_secs(10),
    );

    // 4. Focus is now on agent 2 (newly-spawned agents take focus per
    //    AC-034). Crash it by sending `QUIT\r` through the keymap
    //    Forward path — the bytes reach agent 2's PTY, canonical-mode
    //    line discipline buffers them, the fake's `read_line` sees
    //    `QUIT\n`, matches `line.trim() == "QUIT"`, exits 42. The reap
    //    loop transitions agent 2 to `Crashed { exit_code: 42 }` and
    //    the renderer paints the red banner. See `pty_crash.rs` for
    //    the full byte-path doc.
    send_keys(&mut handle, "QUIT\r");
    let crashed = screen_eventually(
        &mut handle,
        |s| s.contents().contains("exit 42"),
        Duration::from_secs(10),
    );
    assert!(
        crashed.contents().contains("exit 42"),
        "expected crash banner with `exit 42`; got:\n{}",
        crashed.contents()
    );
    // Both tabs still in the navigator: [1] live, [2] crashed.
    assert!(
        crashed.contents().contains("[1]"),
        "expected `[1]` still in navigator after [2] crashed; got:\n{}",
        crashed.contents()
    );
    assert!(
        crashed.contents().contains("[2]"),
        "expected `[2]` still in navigator (crashed != removed); got:\n{}",
        crashed.contents()
    );

    // 5. DISMISS PATH: with the crashed agent 2 focused, send
    //    `prefix d`. `dismiss_focused` sees `Crashed`, removes the
    //    entry, focus clamps to `[1]` (the new and only tail).
    send_keys(&mut handle, "\x02d");
    let after_dismiss = screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            c.contains("[1]") && !c.contains("[2]") && !c.contains("exit 42")
        },
        Duration::from_secs(5),
    );
    assert!(
        !after_dismiss.contents().contains("[2]"),
        "expected `[2]` dismissed from navigator; got:\n{}",
        after_dismiss.contents()
    );
    assert!(
        !after_dismiss.contents().contains("exit 42"),
        "expected crash banner gone after dismiss; got:\n{}",
        after_dismiss.contents()
    );
    assert!(
        after_dismiss.contents().contains("[1]"),
        "expected `[1]` still alive after dismissing `[2]`; got:\n{}",
        after_dismiss.contents()
    );

    // 6. NO-OP PATH: focus is now on agent 1 (Ready). Send
    //    `prefix d` again. `dismiss_focused` sees `Ready`, returns
    //    `false`, leaves the Vec untouched.
    //
    //    We probe an absence-of-event: nothing about the screen is
    //    going to change in response to a no-op, so
    //    `screen_eventually` is the wrong primitive (it waits for a
    //    predicate to *become* true). A 200ms pause is the bounded
    //    window in which codemux would (incorrectly) act on the
    //    chord if the no-op guard regressed; if `[1]` is still on
    //    screen after that, the guard held.
    //
    //    Liveness corollary: if the guard had regressed and dismiss
    //    removed the last live agent, AC-036's "last agent gone ->
    //    codemux exits" path would fire, the PTY would close, the
    //    reader thread would disconnect, and the next
    //    `screen_eventually` would panic on `Disconnected` (or
    //    return without `[1]`). The single assertion `[1]` is on
    //    screen covers both the no-op and the liveness.
    send_keys(&mut handle, "\x02d");
    thread::sleep(Duration::from_millis(200));
    let after_noop = screen_eventually(
        &mut handle,
        |s| s.contents().contains("[1]"),
        Duration::from_secs(5),
    );
    assert!(
        after_noop.contents().contains("[1]"),
        "expected `[1]` still in navigator after no-op `prefix d` on Ready; got:\n{}",
        after_noop.contents()
    );
}
