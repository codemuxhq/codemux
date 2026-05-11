//! AC-014 (force-close a live agent) + AC-036 (reaping the last agent
//! auto-exits codemux): pin the kill-chord-to-process-exit pipeline
//! end-to-end. Unit tests already cover that `kill_focused` shrinks
//! the agent Vec and that the run loop returns `Ok(())` when
//! `agents.is_empty()`, but no test asserts the chord-to-exit chain
//! through a real PTY. This one does.
//!
//! Process exit is the strongest possible end-to-end signal — there is
//! no observation surface beyond it. If the chord ever stops dispatching
//! to `KillAgent`, or the run loop stops auto-exiting on empty Vec,
//! this test fails on the `expect(..)` and the failure message names
//! the broken contract directly.
//!
//! Gating mirrors `pty_smoke.rs` and `pty_nav.rs`: `test-fakes`
//! feature, `#[ignore]` so the slow tier ships through `just check-e2e`
//! only, and `#[serial]` because the PTY harness is not safe to run in
//! parallel.

#![cfg(feature = "test-fakes")]

// Sibling test files consume helpers this one doesn't (the SGR mouse
// surface, the master-byte-log helper); same allow-on-import pattern
// as `pty_smoke.rs` / `pty_nav.rs`.
#[allow(dead_code)]
mod common;

use std::time::Duration;

use serial_test::serial;

use common::{screen_eventually, send_keys, spawn_codemux, wait_for_exit};

/// Boot codemux against the fake agent, wait for the prompt to land,
/// send `Ctrl+B x` (the default `KillAgent` chord), and assert the
/// codemux process exits cleanly within a small timeout.
///
/// **What this pins:**
/// - The default `KillAgent` chord (`Ctrl+B` then `x`) routes through
///   the prefix state machine to `nav.kill_focused()`. (AC-014.)
/// - When `kill_focused` shrinks the agent Vec to empty, the run
///   loop's post-reap `if nav.agents.is_empty() { return Ok(()) }`
///   branch fires, the `TerminalGuard` drops, and the process returns
///   0. (AC-036.)
///
/// **Prefix chord strategy:** Option B — hard-coded `"\x02"` (`Ctrl+B`),
/// same rationale as `pty_nav.rs`. `Bindings::default()` in
/// `apps/tui/src/keymap.rs` sets `prefix = Ctrl+B` and `kill_agent =
/// 'x'`; the harness uses defaults. If either default ever changes,
/// this test fails loudly and the fix is one byte.
///
/// **Why we wait for the prompt before issuing the chord:** without
/// the gate, `Ctrl+B x` could race a still-spawning agent. The runtime
/// would happily dispatch the chord during boot, but a `kill_focused`
/// before `agents` is wired up would either no-op (race we hide) or
/// kill an agent we never observed (false positive). Asserting on the
/// fake's prompt before the chord forces the steady state.
#[test]
#[ignore = "slow-tier PTY E2E; runs via `just check-e2e` / `just test-e2e`"]
#[serial]
fn kill_last_agent_auto_exits_codemux() {
    let mut handle = spawn_codemux();

    let _settled = screen_eventually(
        &mut handle,
        |s| s.contents().contains("FAKE_AGENT_READY"),
        Duration::from_secs(5),
    );

    send_keys(&mut handle, "\x02x");

    // 5s budget for the kill → reap → teardown → exit chain. The
    // fast path (kill_focused removes synchronously, next loop tick
    // hits the empty-Vec branch) is sub-second on a warm build; the
    // bigger budget covers a cold-cache `target/` (first run after
    // `cargo clean`) without re-tuning.
    let status = wait_for_exit(&mut handle, Duration::from_secs(5))
        .expect("codemux did not exit within 5s of `Ctrl+B x` on the only agent");

    assert!(
        status.success(),
        "expected clean exit (status 0); got {status:?}"
    );
}

/// Boot codemux against the fake agent, wait for the prompt to land,
/// send `Ctrl+B q` (the default `Quit` chord), and assert the codemux
/// process exits cleanly. Pins AC-016 — distinct from AC-036 above
/// because the exit path is different: `Quit` flows through
/// `KeyDispatch::Exit => return Ok(())` (`runtime.rs::3571`), not
/// through the post-reap `agents.is_empty()` branch. Both routes
/// land on the same `TerminalGuard` teardown, but the dispatch
/// surface they exercise is independent.
///
/// **Why a second exit test in this file:** the kill test pins
/// "the last agent went away, so codemux exits"; this one pins
/// "the user explicitly asked to quit, so codemux exits". Both
/// matter and both can regress independently — e.g. someone could
/// remove `KeyDispatch::Exit` from the dispatcher and `prefix q`
/// would silently no-op while AC-036 stays green.
#[test]
#[ignore = "slow-tier PTY E2E; runs via `just check-e2e` / `just test-e2e`"]
#[serial]
fn prefix_q_quits_codemux_cleanly() {
    let mut handle = spawn_codemux();

    let _settled = screen_eventually(
        &mut handle,
        |s| s.contents().contains("FAKE_AGENT_READY"),
        Duration::from_secs(5),
    );

    // Ctrl+B = default prefix; `q` = default `Quit` chord.
    send_keys(&mut handle, "\x02q");

    let status = wait_for_exit(&mut handle, Duration::from_secs(5))
        .expect("codemux did not exit within 5s of `Ctrl+B q`");

    assert!(
        status.success(),
        "expected clean exit (status 0); got {status:?}"
    );
}
