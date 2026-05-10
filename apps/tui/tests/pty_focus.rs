//! AC-009 (cycle focus between agents): pin the `prefix n` dispatch
//! end-to-end with a real two-agent codemux PTY. The runtime has unit
//! tests for the dispatch wiring itself (`prefix_l_via_alias_focuses_next`
//! and friends), but none drive the chord through the real keymap with
//! two live agents on screen and observe the rendered focus indicator
//! flipping.
//!
//! This test composes on top of AC-002's spawn-from-modal path
//! (`pty_spawn_action.rs::enter_in_empty_modal_spawns_second_agent_in_scratch_dir`):
//! the setup steps below are the same end-to-end pin AC-002 ships,
//! and the focus assertions here only make sense once that pipeline
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

/// Spawn a second agent via the scratch flow, then send `Ctrl+B n n`
/// and assert focus cycles: agent 2 -> agent 1 -> agent 2.
///
/// **Observation strategy:** `render_left_pane` (in `runtime.rs::4689`)
/// prefixes the focused agent's row with `"> "` and unfocused rows
/// with `"  "`. So `> [1]` means "agent 1 is focused" and `> [2]`
/// means "agent 2 is focused". Asserting on those substrings reads
/// the rendered grid directly.
///
/// **Why one `\x02` for two moves, not two:** the prefix state
/// machine is sticky for nav dispatches (see `is_nav_dispatch` in
/// `runtime.rs`): after `prefix n` fires `FocusNext`, the prefix
/// state stays armed so the user can repeat the move without
/// re-pressing the prefix. Sending `\x02n\x02n` would trigger the
/// double-prefix passthrough on the second `\x02` (it's a literal
/// "forward Ctrl+B to the focused agent" gesture), polluting the
/// agent's PTY with `^B` and breaking the cycle. The correct chord
/// stream is `\x02nn`: arm once, cycle twice. This is also what a
/// human user would do with the actual keymap.
///
/// **Why we assert both moves and the wrap-around:** AC-009 specifies
/// "wrapping around to the first agent." A single `prefix n` proves
/// the dispatch fires; the second `n` proves the modular arithmetic
/// in the cycle is correct AND pins the sticky-prefix repeat
/// behavior. Without the wrap assertion, a regression that hardcoded
/// focus to "agent 1" (forgetting the `% nav.agents.len()`) would
/// still pass the single-move test. Without the sticky-mode chord
/// stream, a regression that broke `is_nav_dispatch(FocusNext)`
/// (which would drop us out of armed state after one move) would
/// pass too, because the original `\x02n\x02n` would re-arm by
/// brute force.
///
/// **Why the setup duplicates AC-002:** the AC-002 test in
/// `pty_spawn_action.rs` is the canonical pin for "spawning from the
/// modal produces a second tab." This test re-walks the same steps
/// because the focus assertions only make sense from that two-agent
/// state. Extracting a shared helper would couple the two tests --
/// when AC-002 evolves (e.g. spawn-from-modal returns focus to the
/// new agent vs. the old one), the helper would have to absorb that
/// change and AC-009's contract would silently move with it.
#[test]
#[ignore = "slow-tier PTY E2E; runs via `just check-e2e` / `just test-e2e`"]
#[serial]
fn prefix_n_cycles_focus_between_two_agents() {
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
    // the same end-state AC-002 pins; we assert it here as the
    // setup-correctness gate before exercising the focus dispatch.
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

    // First cycle: arm prefix and move once. Focus 2 -> 1.
    send_keys(&mut handle, "\x02n");
    let after_n1 = screen_eventually(
        &mut handle,
        |s| s.contents().contains("> [1]"),
        Duration::from_secs(5),
    );
    assert!(
        after_n1.contents().contains("> [1]"),
        "expected agent 1 focused after first `prefix n`; got:\n{}",
        after_n1.contents()
    );
    assert!(
        !after_n1.contents().contains("> [2]"),
        "expected agent 2 unfocused after first `prefix n`; got:\n{}",
        after_n1.contents()
    );

    // Second cycle: sticky prefix is still armed; a bare `n` wraps
    // focus 1 -> 2 without re-pressing Ctrl+B. See the function's
    // doc comment for why `\x02n` again would be wrong.
    send_keys(&mut handle, "n");
    let after_n2 = screen_eventually(
        &mut handle,
        |s| s.contents().contains("> [2]"),
        Duration::from_secs(5),
    );
    assert!(
        after_n2.contents().contains("> [2]"),
        "expected agent 2 focused after wrap-around `n`; got:\n{}",
        after_n2.contents()
    );
    assert!(
        !after_n2.contents().contains("> [1]"),
        "expected agent 1 unfocused after wrap-around `n`; got:\n{}",
        after_n2.contents()
    );
}
