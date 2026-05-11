//! AC-032 (spawn modal opens at the TUI startup cwd, not the focused
//! agent's cwd): pin the runtime's call-site assertion through a real
//! PTY.
//!
//! **What this pins:** the modal's `SpawnMinibuffer::open(initial_cwd, …)`
//! call site in `apps/tui/src/runtime.rs` uses the runtime's CAPTURED
//! `initial_cwd` parameter, not the focused agent's cwd. Unit tests in
//! `apps/tui/src/spawn.rs` (`open_seeds_path_with_cwd_and_marks_auto_seeded`,
//! `open_precise_seeds_path_with_cwd`) already pin that
//! `SpawnMinibuffer::open(cwd, …)` seeds the path zone with whatever cwd
//! it receives — what they cannot pin is which cwd the runtime hands in.
//! AC-032's "Tests" block in `docs/003--acceptance-criteria.md` flags
//! that gap as uncovered. This test closes it by spawning a second
//! agent at a different cwd, focusing it, opening the modal again, and
//! asserting the path STILL reflects the startup cwd. If a future
//! "smart" refactor changes the seed to use the focused agent's cwd,
//! this test catches it.
//!
//! **Why Precise mode (via config):** the default Fuzzy mode opens with
//! an empty path zone — no auto-seed (see the `if default_mode ==
//! SearchMode::Precise` guard in `SpawnMinibuffer::open` around line
//! 496 of `apps/tui/src/spawn.rs`). A Fuzzy-mode test therefore cannot
//! distinguish "seeded from startup cwd" from "seeded from focused
//! agent's cwd": both render empty. Switching to Precise via `[spawn]
//! default_mode = "precise"` in the injected config makes the cwd
//! visible in the prompt, which is the observable surface the AC
//! actually constrains.
//!
//! **Why we stay in Popup chrome:** the test's positive signature
//! (`apps/tui` in the path zone) and negative signature (the scratch
//! tempdir absolute path) only need to appear in the modal. Flipping to
//! `LeftPane` would add agent labels to the screen, and the first
//! agent's `agent_body_text` resolves to the repo name `codemux` —
//! which also appears in the absolute path the test is asserting on.
//! That would create false positives. Staying in `Popup` keeps the
//! modal as the only place the path-zone text can appear.
//!
//! **Why we don't verify agent 2's actual cwd directly:** scratch-dir
//! and explicit-path spawn-cwd resolution are pinned at the unit level
//! (see `expand_scratch` tests in `apps/tui/src/config.rs` plus the
//! `pty_spawn_action.rs` PTY test that proves a scratch spawn lands a
//! second tab). This test focuses on the MODAL's seeding behavior on
//! the second open — the surface AC-032 specifically guards. Trusting
//! the existing coverage for the rest keeps the assertion focused.
//!
//! Gating mirrors the rest of the slow tier: `test-fakes` feature,
//! `#[ignore]`, `#[serial]`.

#![cfg(feature = "test-fakes")]

// Sibling test files consume helpers this file doesn't (`wait_for_exit`);
// same allow-on-import pattern as the rest of the suite.
#[allow(dead_code)]
mod common;

use std::time::Duration;

use serial_test::serial;
use tempfile::TempDir;

use common::{screen_eventually, send_keys, spawn_codemux_with_config};

/// Boot codemux with `[spawn] default_mode = "precise"` so the modal
/// auto-seeds the path zone with the runtime's `initial_cwd`. Open the
/// modal once and confirm the seed matches the startup cwd. Cancel,
/// spawn a second agent at the scratch tempdir (focus follows the
/// spawn), then open the modal AGAIN and assert the path zone STILL
/// reflects the startup cwd — never the focused agent's scratch cwd.
///
/// **Positive signature:** `apps/tui` — the harness passes
/// `std::env::current_dir()` as the codemux child's cwd, which for
/// `cargo test -p codemux-tui` is the crate root `<repo>/apps/tui`. The
/// trailing `apps/tui` segment is unique to that path on this repo's
/// filesystem and, with Popup chrome, can only appear in the modal's
/// path zone.
///
/// **Negative signature:** the scratch tempdir's absolute path string.
/// If the modal incorrectly seeded from the focused agent's cwd, the
/// prompt would show the tempdir path (e.g. `/tmp/.tmpXYZ/`). Asserting
/// that exact path string is absent guards against the regression
/// without depending on tempfile's naming scheme staying constant.
#[test]
#[ignore = "slow-tier PTY E2E; runs via `just check-e2e` / `just test-e2e`"]
#[serial]
fn modal_seeds_path_from_startup_cwd_not_focused_agent_cwd() {
    // Scratch tempdir held in the test so it outlives the codemux child.
    // Same pattern as `pty_spawn_action.rs`.
    let scratch = TempDir::new().expect("scratch tempdir");
    let scratch_path = scratch
        .path()
        .to_str()
        .expect("scratch tempdir path must be valid UTF-8")
        .to_string();
    // `{:?}` formatting on `&str` produces a TOML-compatible quoted
    // string with `"` and `\` properly escaped — same defensive
    // formatting as the sibling spawn-action test.
    let config = format!("[spawn]\ndefault_mode = \"precise\"\nscratch_dir = {scratch_path:?}\n");

    let mut handle = spawn_codemux_with_config(&config);

    // 1. Wait for steady state: fake's prompt is on screen, no modal yet.
    //    `@local` is the host-placeholder fingerprint (see `pty_spawn.rs`).
    screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            c.contains("FAKE_AGENT_READY") && !c.contains("@local")
        },
        Duration::from_secs(5),
    );

    // 2. Open the spawn modal for the first time. With Precise mode the
    //    path zone auto-seeds with the startup cwd. Wait for the modal
    //    chrome (`@local`) AND the path signature (`apps/tui`) so we
    //    don't race on a partially-rendered prompt line.
    send_keys(&mut handle, "\x02c");
    let opened_first = screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            c.contains("@local") && c.contains("apps/tui")
        },
        Duration::from_secs(5),
    );
    assert!(
        opened_first.contents().contains("apps/tui"),
        "expected first modal open to seed path with startup cwd (containing `apps/tui`); got:\n{}",
        opened_first.contents()
    );

    // 3. Clear the auto-seeded path with Ctrl-U (the modal binds it to
    //    "clear the focused field" — see `handle_ctrl_shortcut` in
    //    `apps/tui/src/spawn.rs`). Avoids depending on how many
    //    Backspace presses are needed to fully delete a multi-segment
    //    absolute path, which would vary with the test runner's cwd.
    send_keys(&mut handle, "\x15");
    screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            // Modal still open AND path no longer contains the seeded
            // startup-cwd signature (i.e. the field is now empty).
            c.contains("@local") && !c.contains("apps/tui")
        },
        Duration::from_secs(5),
    );

    // 4. Type the scratch tempdir as an absolute path WITH trailing
    //    slash, then Enter. The trailing `/` puts the modal in
    //    "Precise + path ends with `/`" nav mode, which `refresh`
    //    treats as no-selection-auto-armed (see the `nav_mode_no_select`
    //    branch in `apps/tui/src/spawn.rs` around line 1268). Without
    //    a selection, Enter skips `apply_path_completion` and falls
    //    through to `ModalOutcome::Spawn { host: "local", path }`,
    //    which the runtime resolves into a local PTY spawn rooted at
    //    the typed path. Spawn flow focuses the new agent
    //    automatically — agent 2 is now the focused agent with cwd =
    //    scratch tempdir, satisfying the AC's "second agent in
    //    `~/work/proj-B`" precondition.
    //
    //    Without the trailing `/`, `refresh` would auto-arm
    //    `selected = Some(0)` on whatever wildmenu entry matched first
    //    (other `.tmp*` dirs hang around in `/tmp/` from prior test
    //    runs and pollute the candidate list). Enter would then
    //    descend into THAT folder instead of spawning here, and the
    //    test would race on which `.tmp*` entry sorted first.
    send_keys(&mut handle, &scratch_path);
    send_keys(&mut handle, "/");
    // Wait for the modal to register the trailing slash. Path-ends-in-`/`
    // is the "nav mode, no auto-arm" signal the spawn flow needs, and
    // checking the prompt for the exact `<scratch>/` substring is the
    // cheapest available read of that state.
    let typed = format!("{scratch_path}/");
    screen_eventually(
        &mut handle,
        |s| s.contents().contains(&typed),
        Duration::from_secs(5),
    );
    send_keys(&mut handle, "\r");

    // 5. Wait for the modal to close AND the new agent to be ready.
    //    Modal close: `@local` gone. Agent ready: `FAKE_AGENT_READY` on
    //    screen (the focused pane in Popup chrome is now agent 2, also
    //    a `fake_agent` process emitting the same banner).
    screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            !c.contains("@local") && c.contains("FAKE_AGENT_READY")
        },
        Duration::from_secs(10),
    );

    // 6. Open the modal AGAIN with the second (scratch-cwd) agent
    //    focused. AC-032 says the path zone must STILL seed with the
    //    startup cwd, not the focused agent's scratch cwd.
    send_keys(&mut handle, "\x02c");
    let opened_second = screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            c.contains("@local") && c.contains("apps/tui")
        },
        Duration::from_secs(5),
    );

    // Positive assertion: path zone reflects the startup cwd.
    assert!(
        opened_second.contents().contains("apps/tui"),
        "AC-032: expected modal to re-seed with STARTUP cwd (`apps/tui`) on second open with a different agent focused; got:\n{}",
        opened_second.contents()
    );

    // Negative assertion: path zone does NOT reflect the focused
    // agent's scratch cwd. If the runtime had been refactored to seed
    // from `nav.focused_agent().cwd`, the scratch tempdir path would
    // appear here. Asserting on the exact tempdir path catches that
    // regression precisely.
    assert!(
        !opened_second.contents().contains(&scratch_path),
        "AC-032: expected modal NOT to seed with focused agent's scratch cwd ({scratch_path}); got:\n{}",
        opened_second.contents()
    );
}
