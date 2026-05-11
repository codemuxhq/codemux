//! AC-006 (drill into a folder, then spawn at the chosen depth): pin
//! the Tab-descend + Enter-to-spawn gesture end-to-end through the
//! real keymap, modal, and runtime.
//!
//! **What this pins:** in Precise + Path mode, pressing `Down` to
//! highlight a folder candidate and then `Tab` makes the path zone
//! adopt the candidate's full path (with trailing `/`), and a
//! subsequent `Enter` at that depth spawns a new agent. Unit tests in
//! `apps/tui/src/spawn.rs`
//! (`tab_descends_into_folder_in_one_step`,
//! `enter_with_selection_in_precise_descends`,
//! `enter_without_selection_in_precise_spawns`) pin the modal-state
//! transitions in isolation; this test runs the same gestures through
//! the real PTY → vt100 surface and asserts the user-visible result.
//!
//! **Why Precise mode (via config):** Tab in Fuzzy mode has different
//! semantics (it applies the highlighted candidate as the literal path
//! without descending; descent only exists in Precise nav mode — see
//! the `tab_is_no_op_in_fuzzy_path_zone` unit test). Forcing Precise
//! via `[spawn] default_mode = "precise"` puts the modal on the code
//! path AC-006 actually constrains.
//!
//! **Why we create the subdirectory upfront and type the tempdir path
//! manually:** the modal's auto-seeded path in Precise mode is the
//! runtime's startup cwd (the cargo crate root, `apps/tui/`). That
//! directory isn't under our control as a clean parent — its real
//! children (`src/`, `tests/`, etc.) would pollute the wildmenu. We
//! create a dedicated tempdir with a single known subdirectory inside,
//! clear the auto-seeded path with Ctrl+U (same gesture
//! `pty_modal_cwd.rs` uses), and type the tempdir path so the wildmenu
//! lists exactly one candidate (`drilldown_child/`). Then `Down → Tab`
//! drills into it, and `Enter` spawns.
//!
//! **What we DO NOT assert:** the actual cwd of the spawned agent.
//! `fake_agent` doesn't expose its cwd to the screen. This test pins
//! the modal-state pipeline (path zone update + wildmenu refresh +
//! Enter-spawns ending); unit tests in `spawn.rs` and `config.rs` pin
//! the cwd-resolution math.
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

/// Boot codemux with `[spawn] default_mode = "precise"`, create a
/// tempdir containing one known subdirectory, then drive the modal
/// through: clear → type tempdir path with trailing `/` → Down (arm
/// selection) → Tab (descend) → Enter (spawn at depth). Assert the
/// path zone reflects the descended folder before Enter, and that a
/// second agent appears in the `LeftPane` navigator after.
///
/// **Signature for "drilled":** after Tab, the wildmenu transitions
/// from listing `drilldown_child/` as a candidate row to showing the
/// Precise-mode no-matches sentinel `(no matches — Enter spawns at
/// literal path)` (see the `filtered.is_empty()` branch of
/// `wildmenu_view` in `apps/tui/src/spawn.rs`). The empty wildmenu is
/// the structural fingerprint of "we're now inside the empty
/// `drilldown_child` folder," distinct from "we're at the tempdir and
/// `drilldown_child` is a candidate."
///
/// **Signature for "spawn happened":** flip to `LeftPane` (prefix v) and
/// assert the navigator block shows ` [2]` somewhere — the second
/// agent's index. The first agent was spawned at boot; the second is
/// the one we just drilled-and-spawned.
#[test]
#[ignore = "slow-tier PTY E2E; runs via `just check-e2e` / `just test-e2e`"]
#[serial]
fn tab_descends_into_folder_and_enter_spawns_at_depth() {
    // Scratch tempdir held in the test so it outlives the codemux
    // child. Mirroring `pty_modal_cwd.rs` and `pty_spawn_action.rs`,
    // but anchored under `/tmp` so the rendered prompt fits in the
    // 80-col PTY harness on macOS — the platform default `$TMPDIR`
    // (`/var/folders/.../T/.tmpXXXXXX/`, ~60 chars) leaves no room
    // for the trailing `drilldown_child` to be visible in the prompt
    // zone after the descent (the assertion below requires it). On
    // Linux `TempDir::new()` already lands under `/tmp`; on macOS
    // `/tmp` is a symlink to `/private/tmp`, but `tempfile` returns
    // the un-canonicalized `/tmp/.tmpXXXXXX` path here, which keeps
    // the prompt under the 80-column ceiling.
    let scratch = TempDir::new_in("/tmp").expect("scratch tempdir under /tmp");
    let scratch_path = scratch
        .path()
        .to_str()
        .expect("scratch tempdir path must be valid UTF-8")
        .to_string();
    // Create exactly one subdirectory inside the tempdir so the
    // wildmenu has exactly one candidate to autocomplete to. A bare
    // empty tempdir would render the no-matches sentinel and Tab
    // would be a no-op — there's nothing to drill into.
    std::fs::create_dir(scratch.path().join("drilldown_child"))
        .expect("mkdir drilldown_child inside scratch tempdir");

    // `{:?}` formatting on `&str` produces a TOML-compatible quoted
    // string with `"` and `\` properly escaped — same defensive
    // formatting as the sibling spawn tests.
    let config = format!("[spawn]\ndefault_mode = \"precise\"\nscratch_dir = {scratch_path:?}\n");

    let mut handle = spawn_codemux_with_config(&config);

    // 1. Wait for steady state: fake's prompt is on screen, no modal yet.
    //    `@local` is the host-placeholder fingerprint visible in the
    //    modal's prompt line (also used by `pty_modal_cwd.rs`).
    screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            c.contains("FAKE_AGENT_READY") && !c.contains("@local")
        },
        Duration::from_secs(5),
    );

    // 2. Open the spawn modal. With Precise mode the path zone
    //    auto-seeds to the startup cwd (`apps/tui` here).
    send_keys(&mut handle, "\x02c");
    screen_eventually(
        &mut handle,
        |s| s.contents().contains("@local"),
        Duration::from_secs(5),
    );

    // 3. Clear the auto-seeded path with Ctrl-U. Same gesture as
    //    `pty_modal_cwd.rs`; avoids depending on how many Backspaces
    //    are needed to delete a multi-segment absolute path.
    send_keys(&mut handle, "\x15");

    // 4. Type the tempdir path with a trailing `/`. The trailing
    //    slash puts the modal in Precise nav mode (no auto-arm of
    //    `selected`), which is the exact state AC-006 targets. The
    //    wildmenu now lists `drilldown_child/` as the sole candidate.
    let typed_path = format!("{scratch_path}/");
    send_keys(&mut handle, &typed_path);
    screen_eventually(
        &mut handle,
        |s| s.contents().contains("drilldown_child"),
        Duration::from_secs(5),
    );

    // 5. Press Down to arm `selected = Some(0)` on `drilldown_child/`.
    //    In Precise nav mode (path ends with `/`) the modal does NOT
    //    auto-arm a selection — the user has to explicitly highlight
    //    a candidate before Tab can descend. See `move_selection_forward`
    //    and the `tab_descends_into_folder_in_one_step` unit test
    //    (which sets `selected = Some(0)` manually).
    send_keys(&mut handle, "\x1b[B"); // ESC [ B = ANSI Down arrow

    // 6. Tab to descend. After this:
    //    - The path zone becomes `<tempdir>/drilldown_child/`.
    //    - `selected` clears.
    //    - The wildmenu lists the (empty) folder's children — none —
    //      so the Precise-mode no-matches sentinel appears.
    send_keys(&mut handle, "\t");

    // Signature for "we drilled": the no-matches sentinel is visible.
    // The verbatim string lives in `wildmenu_view`'s
    // `PathMode::Local` arm. If that copy ever changes, the assertion
    // fires on the empty wildmenu predicate; the failure message
    // includes the rendered screen.
    let after_drill = screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            c.contains("(no matches") && c.contains("drilldown_child")
        },
        Duration::from_secs(5),
    );
    assert!(
        after_drill.contents().contains("drilldown_child"),
        "expected path zone to show `drilldown_child` after Tab descent; got:\n{}",
        after_drill.contents()
    );
    assert!(
        after_drill.contents().contains("(no matches"),
        "expected Precise no-matches sentinel after descent into empty `drilldown_child/`; got:\n{}",
        after_drill.contents()
    );

    // 7. Enter to spawn at the descended depth. With no selection
    //    armed (Tab cleared it) and `path_origin = UserTyped` (we
    //    typed the prefix, then Tab marked it user-typed), Enter
    //    falls through to `ModalOutcome::Spawn { host: "local",
    //    path: "<tempdir>/drilldown_child/" }`. Same code path the
    //    `enter_without_selection_in_precise_spawns` unit test pins.
    send_keys(&mut handle, "\r");

    // 8. Wait for the modal to close and the new agent's prompt to
    //    render. Modal close: `@local` gone. New agent ready:
    //    `FAKE_AGENT_READY` on screen (the new focused pane is
    //    another `fake_agent`).
    screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            !c.contains("@local") && c.contains("FAKE_AGENT_READY")
        },
        Duration::from_secs(10),
    );

    // 9. Flip to LeftPane chrome (prefix v) so the navigator's agent
    //    list is visible. The first agent is `[1]`, the second is
    //    `[2]`. Asserting on ` [2]` proves a second agent exists —
    //    the drilldown spawn took effect.
    send_keys(&mut handle, "\x02v");
    let after_spawn = screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            c.contains(" agents ") && c.contains("[2]")
        },
        Duration::from_secs(5),
    );
    assert!(
        after_spawn.contents().contains("[2]"),
        "expected second agent in navigator after drilldown spawn; got:\n{}",
        after_spawn.contents()
    );
}
