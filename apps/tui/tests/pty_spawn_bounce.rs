//! AC-034 (spawning a new agent records the prior focus as the bounce
//! slot): pin the spawn-from-modal flow's side effect on the
//! `previous_focused` slot with a real two-agent codemux PTY. After
//! the spawn lands and focus moves to the freshly-promoted agent, a
//! single `prefix Tab` must bounce back to the agent that was focused
//! before the spawn — proving the spawn flow correctly recorded the
//! prior focus when it promoted the new agent.
//!
//! This test sits between AC-002 (spawn-from-modal produces a second
//! agent) and AC-011 (prefix Tab bounces between two pre-existing
//! agents). It is the integration glue between the two: without
//! AC-034, the spawn flow could silently fail to set
//! `previous_focused`, and AC-011's test — which uses
//! spawn-from-modal as its setup — would silently break.
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
/// once and assert focus bounces back to agent 1 — proving the spawn
/// flow recorded agent 1 as `previous_focused` when it promoted agent 2.
///
/// **What this pins vs AC-002 / AC-011:** AC-002 proves a second agent
/// appears on the navigator after `prefix c` + Enter; AC-011 proves
/// `prefix Tab` oscillates focus between two agents (assuming the
/// previous-focus slot is already populated). AC-034 sits in between:
/// it proves the SPAWN flow itself correctly records the prior focus
/// as the bounce slot. That side effect is invisible to AC-002 (which
/// only checks the second agent appears) and assumed-correct by
/// AC-011 (which inherits its two-agent setup from the spawn flow).
/// Without AC-034, a regression where the spawn flow forgot to set
/// `previous_focused` would silently break AC-011's setup pre-condition,
/// and AC-011's test would degrade into an opaque "focus didn't move"
/// failure rather than pointing at the actual bug (the spawn flow's
/// missing side effect).
///
/// **Observation strategy:** `render_left_pane` (in `runtime.rs::4689`)
/// prefixes the focused agent's row with `"> "` and unfocused rows
/// with `"  "`. So `> [1]` means "agent 1 is focused" and `> [2]`
/// means "agent 2 is focused". Asserting on those substrings reads
/// the rendered grid directly, the same way AC-009's `pty_focus.rs`
/// and AC-011's `pty_bounce.rs` read the focus indicator.
///
/// **Why this is a SINGLE bounce, not oscillating:** the AC-034 body
/// only asserts "the spawn recorded prior focus correctly" — and that
/// is fully proven by the first `prefix Tab` going back to agent 1.
/// Adding a second `Tab` would only re-prove AC-011's contract (that
/// repeated `prefix Tab` oscillates), which is already pinned by
/// `pty_bounce.rs`. Keeping this test to one bounce keeps the failure
/// signal clean: a failure here is unambiguously "the spawn flow did
/// not record the prior focus," not "the bounce dispatch is broken."
///
/// **Why the setup duplicates AC-002 / AC-011:** intentional — the
/// duplication IS the integration point. AC-034 is the proof that the
/// spawn flow's side effect (setting `previous_focused`) is correct,
/// and the only way to observe that side effect is to walk the
/// AC-002 setup and then assert the AC-011-style bounce works on the
/// very first `prefix Tab`. Extracting a shared helper would couple
/// the three tests — when AC-002 evolves (e.g. spawn-from-modal
/// returns focus to the old agent instead of the new one), the
/// helper would have to absorb that change and AC-034's contract
/// would silently move with it, masking the exact regression this
/// test is here to catch.
#[test]
#[ignore = "slow-tier PTY E2E; runs via `just check-e2e` / `just test-e2e`"]
#[serial]
fn spawn_from_modal_records_prior_focus_so_tab_bounces_back() {
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
    // While the modal is open, agent 1 is the focused agent under it;
    // the spawn flow must record agent 1 as `previous_focused` when
    // it promotes agent 2 — that side effect is what this test pins.
    send_keys(&mut handle, "\x02c");
    screen_eventually(
        &mut handle,
        |s| s.contents().contains("@local"),
        Duration::from_secs(5),
    );
    send_keys(&mut handle, "\r");

    // Steady state after spawn: both agents in the navigator, modal
    // closed, focus on the freshly-spawned agent (agent 2). This is
    // the same end-state AC-002 and AC-011 pin; we assert it here as
    // the setup-correctness gate before exercising the bounce dispatch.
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

    // The one assertion AC-034 is here for: a single `prefix Tab`
    // bounces back to agent 1. This is only possible if the spawn
    // flow recorded agent 1 as `previous_focused` when it promoted
    // agent 2. If that side effect is missing, `FocusLast` would
    // either be a no-op (no previous slot) or target the wrong agent,
    // and the predicate would time out with `> [2]` still showing.
    send_keys(&mut handle, "\x02\t");
    let after_tab = screen_eventually(
        &mut handle,
        |s| s.contents().contains("> [1]"),
        Duration::from_secs(5),
    );
    assert!(
        after_tab.contents().contains("> [1]"),
        "expected agent 1 focused after `prefix Tab` bounce; got:\n{}",
        after_tab.contents()
    );
    assert!(
        !after_tab.contents().contains("> [2]"),
        "expected agent 2 unfocused after `prefix Tab` bounce; got:\n{}",
        after_tab.contents()
    );
}
