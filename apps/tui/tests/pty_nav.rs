//! AC-013 (toggle the navigator chrome): pin the `prefix v` dispatch
//! end-to-end. Renderer-level tests already cover what `LeftPane` and
//! `Popup` look like in isolation (`render_left_pane_*` and
//! `snapshot_navigator_popup` in `apps/tui/src/runtime.rs`), but no
//! existing test asserts that pressing the chord actually flips the
//! chrome through the real `dispatch_key` path. This test does.
//!
//! Gating mirrors `pty_smoke.rs`: `test-fakes` feature, `#[ignore]` so
//! the slow tier ships through `just check-e2e` only, and `#[serial]`
//! because the PTY harness is not safe to run in parallel.

#![cfg(feature = "test-fakes")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

// Each `tests/*.rs` integration target compiles `mod common` as its own
// crate; helpers consumed only by sibling test files (e.g. `wait_for_exit`,
// used by `pty_lifecycle.rs`) trip `dead_code` here. Same allow-on-import
// pattern as `pty_smoke.rs`.
#[allow(dead_code)]
mod common;

use std::time::Duration;

use serial_test::serial;

use common::{screen_eventually, send_keys, spawn_codemux};

/// Press the prefix chord then `v` and assert the chrome flips from
/// `Popup` to `LeftPane`.
///
/// **Observation strategy:** the `LeftPane` renderer wraps the
/// navigator in a bordered `Block` titled ` agents ` (see
/// `render_left_pane` in `apps/tui/src/runtime.rs`). That literal
/// string is unique to the `LeftPane` chrome — it does not appear in
/// `Popup`, where the agent pane fills the area edge-to-edge with the
/// fake's prompt. So the screen contents going from "no ` agents `
/// title" to "has ` agents ` title" is a clean, structural diff that
/// could only be produced by the chrome actually flipping.
///
/// **Prefix chord strategy:** Option B — hard-coded `"\x02"` (`Ctrl+B`).
/// `Bindings::default()` in `apps/tui/src/keymap.rs` sets
/// `prefix = Ctrl+B` and the harness does not pass a custom config.
/// Adding a `--prefix` CLI flag just for this test would be
/// over-engineering (compare `--agent-bin`, which exists because the
/// production architecture demanded a Port). If the default ever
/// changes, this test will fail loudly and the fix is one byte.
#[test]
#[ignore = "slow-tier PTY E2E; runs via `just check-e2e` / `just test-e2e`"]
#[serial]
fn chrome_flips_from_popup_to_leftpane_on_prefix_v() {
    let mut handle = spawn_codemux();

    // Wait for the initial Popup chrome to settle: the fake's prompt
    // is on screen AND the LeftPane navigator title is NOT yet there.
    // Checking both directions guards against a future change that
    // launches in LeftPane by default and would otherwise make the
    // post-toggle assertion vacuous.
    let before = screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            c.contains("FAKE_AGENT_READY") && !c.contains(" agents ")
        },
        Duration::from_secs(5),
    );
    assert!(
        !before.contents().contains(" agents "),
        "expected no LeftPane navigator title before toggle; got:\n{}",
        before.contents()
    );

    // Send the prefix chord (Ctrl+B) then `v`. Default keymap binds
    // these to the prefix key and `PrefixAction::ToggleNav`.
    send_keys(&mut handle, "\x02v");

    // Wait for the LeftPane navigator title to appear. If the toggle
    // dispatch is broken (chord unbound, prefix not armed, action not
    // wired to a chrome flip) this predicate never holds and
    // `screen_eventually` panics with the rendered screen.
    let after = screen_eventually(
        &mut handle,
        |s| s.contents().contains(" agents "),
        Duration::from_secs(5),
    );
    assert!(
        after.contents().contains(" agents "),
        "expected LeftPane navigator title after `prefix v`; got:\n{}",
        after.contents()
    );
}
