//! AC-005 (quick-switch to precise mode by typing `~` or `/`): pin the
//! Fuzzy-to-Precise auto-switch path through a real PTY. Unit tests in
//! `apps/tui/src/spawn.rs` cover the per-key branch logic
//! (`tilde_in_fuzzy_with_empty_query_enters_navigation_at_home`,
//! `slash_in_fuzzy_with_empty_query_enters_navigation_at_root`,
//! plus the compose-key variants); this PTY test closes the
//! chord-to-modal-open-to-shortcut-typed-to-prompt-flipped pipeline
//! end-to-end.
//!
//! Gating mirrors the other PTY tests: `test-fakes` feature,
//! `#[ignore]`, `#[serial]`.

#![cfg(feature = "test-fakes")]

// Sibling test files consume helpers this file doesn't (`wait_for_exit`);
// same allow-on-import pattern as the rest of the suite.
#[allow(dead_code)]
mod common;

use std::time::Duration;

use serial_test::serial;

use common::{home_path, screen_eventually, send_keys, spawn_codemux};

/// Open the spawn modal in default Fuzzy mode, then exercise BOTH
/// quick-switch shortcuts (`~` and `/`) in sequence, asserting after
/// each that the modal transitioned to Precise mode with the
/// appropriate path seed.
///
/// **Observation strategy:** the prompt label flips from `find:  `
/// (Fuzzy mode + Path zone focused) to `spawn: ` (every other state).
/// Source: `apps/tui/src/spawn.rs` around lines 1797-1805, where the
/// label is chosen based on `search_mode == Fuzzy && focused == Path`.
/// Those two 7-byte ASCII strings are unique to the modal prompt — they
/// do not appear in the agent pane (the fake's prompt is
/// `FAKE_AGENT_READY> `), the navigator, or the status bar. Presence vs.
/// absence of `find:  ` vs. `spawn: ` is a clean structural diff.
///
/// **What this pins:** the chord-to-mode-transition path through
/// `SpawnMinibuffer::handle` — specifically the
/// `tilde_or_slash_in_fuzzy_with_empty_query` branch that re-seeds the
/// path and flips `search_mode` from Fuzzy to Precise. The unit tests
/// pin the in-memory state changes (`m.path == "$HOME/"`,
/// `m.search_mode == Precise`); this test pins that those state changes
/// actually reach the rendered screen through the runtime's render
/// loop.
///
/// **Why we test BOTH `~` and `/`:** they exercise distinct seed paths
/// (home vs. root) via the same Fuzzy-to-Precise transition. A
/// regression that broke one branch (e.g. a typo in the `$HOME`
/// expansion, or a stale path-zone refresh after the slash seed) might
/// leave the other working — covering both at the PTY level catches
/// the asymmetric break that unit tests alone might mask if someone
/// only refactors one path.
///
/// **Why one test, not two separate tests:** the parts share setup
/// (boot codemux, wait for the fake's prompt, open the modal) and
/// differ only in one keystroke. Bundling them avoids two PTY spawn
/// cycles (~0.4s overhead each on top of the ~1s steady-state boot)
/// and keeps the slow tier responsive. The doc comment makes the two
/// parts visible; each part also asserts independently before moving
/// on, so a failure points at the specific shortcut that broke.
///
/// **HOME assumption:** the harness redirects the spawned codemux's
/// `HOME` to a per-test tempdir (see `apps/tui/tests/common/mod.rs`),
/// so `~` expansion lands at that tempdir. We read the redirected
/// path via `home_path(&handle)` and assert the rendered path
/// contains it as a prefix. This is cross-platform (no hardcoded
/// `/home/` vs `/Users/`) and robust against any future change in
/// how the harness picks the HOME location. The negative assertion
/// in Part 2 reuses the same prefix to guard against accidental
/// home-seeding from the `/` shortcut.
#[test]
#[ignore = "slow-tier PTY E2E; runs via `just check-e2e` / `just test-e2e`"]
#[serial]
fn tilde_or_slash_in_fuzzy_modal_switches_to_precise_mode() {
    let mut handle = spawn_codemux();
    // The harness redirects `HOME` to a per-test tempdir (see the doc
    // comment on `spawn_codemux`), so the spawned codemux's `~`
    // expansion lands there — not at the developer's real home. Read
    // the redirected path off the handle and assert against it. This
    // keeps the test cross-platform (no hardcoded `/home/` vs
    // `/Users/`) and makes the assertion robust against any future
    // change in how the harness picks the per-test HOME location.
    let home = home_path(&handle)
        .to_str()
        .expect("HOME tempdir path must be valid UTF-8")
        .to_string();
    let home_prefix = format!("{home}/");

    // Steady state: fake's prompt is on screen, no modal open yet.
    // Checking both directions guards against any future change that
    // would open the modal at boot and make the post-toggle assertion
    // vacuous.
    let before = screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            c.contains("FAKE_AGENT_READY") && !c.contains("@local")
        },
        Duration::from_secs(5),
    );
    assert!(
        !before.contents().contains("@local"),
        "expected no spawn modal before chord; got:\n{}",
        before.contents()
    );

    // -------- Part 1: `~` switches to Precise + seeds $HOME --------

    // Open the modal: prefix + `c`. Default Fuzzy mode + empty path =
    // the exact preconditions for the tilde-at-empty-query branch.
    send_keys(&mut handle, "\x02c");

    let opened_fuzzy = screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            c.contains("@local") && c.contains("find:")
        },
        Duration::from_secs(5),
    );
    assert!(
        opened_fuzzy.contents().contains("find:"),
        "expected `find:` label (Fuzzy + Path zone) on modal open; got:\n{}",
        opened_fuzzy.contents()
    );

    // Type `~`. Should flip the modal into Precise mode and seed
    // `$HOME/` into the path zone.
    send_keys(&mut handle, "~");

    let after_tilde = screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            // Both must hold: label flipped to `spawn:` (mode is
            // Precise) AND the path has been seeded to the expanded
            // `$HOME/` prefix.
            c.contains("spawn:") && c.contains(&home_prefix)
        },
        Duration::from_secs(5),
    );
    let after_tilde_text = after_tilde.contents();
    assert!(
        after_tilde_text.contains("spawn:"),
        "expected `spawn:` label after `~` (Fuzzy -> Precise); got:\n{after_tilde_text}",
    );
    assert!(
        !after_tilde_text.contains("find:"),
        "expected `find:` label gone after `~`-driven switch; got:\n{after_tilde_text}",
    );
    assert!(
        after_tilde_text.contains(&home_prefix),
        "expected path seeded with $HOME prefix `{home_prefix}` after `~`; got:\n{after_tilde_text}",
    );

    // Close the modal so we can re-enter Fuzzy mode for Part 2. The
    // user_search_mode preference is preserved (Fuzzy), so the next
    // open returns to the same starting state as Part 1.
    send_keys(&mut handle, "\x1b");

    let closed = screen_eventually(
        &mut handle,
        |s| !s.contents().contains("@local"),
        Duration::from_secs(5),
    );
    assert!(
        !closed.contents().contains("@local"),
        "expected modal closed before Part 2; got:\n{}",
        closed.contents()
    );

    // -------- Part 2: `/` switches to Precise + seeds `/` --------

    // Reopen the modal. We assert the `find:` label is back to confirm
    // user_search_mode was preserved — closing the tilde-driven modal
    // should NOT have flipped the persisted preference to Precise.
    send_keys(&mut handle, "\x02c");

    let opened_again = screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            c.contains("@local") && c.contains("find:")
        },
        Duration::from_secs(5),
    );
    assert!(
        opened_again.contents().contains("find:"),
        "expected `find:` label on reopen (user_search_mode preserved); got:\n{}",
        opened_again.contents()
    );

    // Type `/`. Should flip the modal into Precise mode and seed `/`.
    send_keys(&mut handle, "/");

    // The seeded `/` lives at the end of the prompt line as
    // ` : /`. Using ` : /` (with surrounding spaces) avoids matching
    // any stray `/` elsewhere on screen (the agent pane could happen
    // to have a path-shaped string in the fake's output; the modal's
    // ` : ` separator before the path zone is unique).
    let after_slash = screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            c.contains("spawn:") && c.contains(" : /")
        },
        Duration::from_secs(5),
    );
    let after_slash_text = after_slash.contents();
    assert!(
        after_slash_text.contains("spawn:"),
        "expected `spawn:` label after `/` (Fuzzy -> Precise); got:\n{after_slash_text}",
    );
    assert!(
        !after_slash_text.contains("find:"),
        "expected `find:` label gone after `/`-driven switch; got:\n{after_slash_text}",
    );
    assert!(
        after_slash_text.contains(" : /"),
        "expected path seeded with `/` (root) after `/`; got:\n{after_slash_text}",
    );
    // Negative assertion: the `/` shortcut seeds root, not $HOME, so
    // the home-prefix substring must NOT appear in the path zone.
    // (`$HOME` could still show up elsewhere on screen — e.g. if the
    // status bar ever exposed cwd — so this is a guarded check, not a
    // hard `!contains`. Today the modal is the only chrome with paths
    // on screen, so the simpler form is fine.)
    assert!(
        !after_slash_text.contains(&home_prefix),
        "expected no $HOME prefix `{home_prefix}` after `/` (root seed); got:\n{after_slash_text}",
    );

    // Tidy: close the modal so Drop sees a clean state.
    send_keys(&mut handle, "\x1b");
    let _ = screen_eventually(
        &mut handle,
        |s| !s.contents().contains("@local"),
        Duration::from_secs(5),
    );
}
