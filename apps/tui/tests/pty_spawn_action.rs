//! AC-002 (spawn a local agent in the scratch directory): pin the
//! "Enter on an empty modal" path through a real PTY.
//!
//! Unit tests in `apps/tui/src/spawn.rs` cover scratch-dir resolution
//! and the `Enter` outcome encoding (`empty_path_with_no_selection_emits_spawn_scratch`
//! and friends); this PTY test closes the chord-to-rendered-second-tab
//! pipeline end-to-end.
//!
//! Strategically, this is the first PTY test that ends with a
//! *second* live agent in the navigator. It unlocks downstream
//! multi-agent tests (AC-009 focus cycle, AC-011 bounce, AC-034
//! prior-focus recording) by proving the spawn-from-modal path runs
//! to completion.
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

/// Boot codemux with a config that points `[spawn] scratch_dir` at a
/// per-test tempdir (so the test does not pollute the developer's
/// `~/.codemux/scratch`). Flip to `LeftPane` chrome so the tab list
/// is visible, open the spawn modal with `Ctrl+B c`, press Enter on
/// the empty modal, and assert:
///
/// 1. The modal chrome (`@local`) goes away.
/// 2. The `LeftPane` navigator now shows the second agent (`agent-2`)
///    alongside the initial `agent-1`.
///
/// **Why `LeftPane` chrome:** the default `Popup` chrome only renders
/// the focused agent's pane, so "a second tab exists" is not directly
/// observable on screen. Flipping to `LeftPane` via `prefix v` makes
/// the navigator list visible and gives both agent labels a stable
/// place to appear. The `pty_nav.rs` test pins the chrome flip
/// itself; this test composes on top of it.
///
/// **Why we assert on the `[2]` ordinal instead of `agent-2`:** the
/// `LeftPane` renders each row as `[<one-indexed-position>] <body>`,
/// where `<body>` is `agent_body_text` — `<repo>: <title>` if both
/// are known, falling back to the static `agent-N` label only when
/// repo AND title are both unresolved. In this test the first agent's
/// cwd is the codemux repo (visible body `codemux`) and the second
/// agent's cwd is a tempdir (visible body is the tempdir basename).
/// The static `agent-N` label is therefore NOT on screen. The
/// ordinal prefix `[2]` is the structural fingerprint — it's the same
/// signal AC-010's digit-jump path uses, and it's stable across
/// however `agent_body_text` evolves.
///
/// **Why config-injected `scratch_dir`:** the default
/// `~/.codemux/scratch` would land on the developer's home dir,
/// which the harness should not touch. Pointing it at a tempdir
/// keeps the test self-contained and makes the scratch-resolution
/// code path visible to the test (a regression that broke
/// `expand_scratch` would surface here as codemux refusing to spawn).
#[test]
#[ignore = "slow-tier PTY E2E; runs via `just check-e2e` / `just test-e2e`"]
#[serial]
fn enter_in_empty_modal_spawns_second_agent_in_scratch_dir() {
    // Scratch tempdir held in the test so it outlives the codemux child.
    // Path is absolute, which `expand_scratch` accepts as-is.
    let scratch = TempDir::new().expect("scratch tempdir");
    let scratch_path = scratch.path().to_string_lossy().into_owned();
    let config = format!("[spawn]\nscratch_dir = \"{scratch_path}\"\n");

    let mut handle = spawn_codemux_with_config(&config);

    // 1. Wait for steady state: fake's prompt is on screen, no modal yet.
    screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            c.contains("FAKE_AGENT_READY") && !c.contains("@local")
        },
        Duration::from_secs(5),
    );

    // 2. Flip to `LeftPane` so the navigator list is observable. The
    //    ` agents ` block title is the `LeftPane` chrome fingerprint
    //    (see `pty_nav.rs`'s test).
    send_keys(&mut handle, "\x02v");
    screen_eventually(
        &mut handle,
        |s| s.contents().contains(" agents "),
        Duration::from_secs(5),
    );

    // 3. Open the spawn modal. `@local` is the host placeholder
    //    fingerprint (see `pty_spawn.rs`'s test).
    send_keys(&mut handle, "\x02c");
    screen_eventually(
        &mut handle,
        |s| s.contents().contains("@local"),
        Duration::from_secs(5),
    );

    // 4. Press Enter on the empty modal. With the modal in default
    //    fuzzy mode, an empty path zone with no wildmenu selection
    //    routes through the scratch-dir spawn path (unit-pinned by
    //    `empty_path_with_no_selection_emits_spawn_scratch`).
    send_keys(&mut handle, "\r");

    // 5. Modal closes AND the navigator lists both `[1]` and `[2]`.
    //    Asserting both in one predicate guards against a partial
    //    regression where the modal closes but no second agent appears
    //    (or vice versa). The ordinal prefixes are stable; the body
    //    labels (`codemux`, the tempdir basename) are not.
    let after = screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            !c.contains("@local") && c.contains("[1]") && c.contains("[2]")
        },
        Duration::from_secs(10),
    );
    assert!(
        !after.contents().contains("@local"),
        "expected modal to close after Enter; got:\n{}",
        after.contents()
    );
    assert!(
        after.contents().contains("[1]"),
        "expected `[1]` (first agent) still in navigator; got:\n{}",
        after.contents()
    );
    assert!(
        after.contents().contains("[2]"),
        "expected `[2]` (newly-spawned agent) in navigator; got:\n{}",
        after.contents()
    );
}
