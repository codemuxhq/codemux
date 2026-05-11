//! AC-004 (path-zone wildmenu autocompletes against the focused host):
//! pin the typed-prefix → wildmenu-shows-candidates → Tab-applies
//! gesture end-to-end through the real keymap, modal, and runtime.
//!
//! **What this pins:** in Precise + Path + Search mode (i.e. the typed
//! input does NOT end with `/`), typing a prefix populates the wildmenu
//! with the `read_dir`-backed candidates whose basenames start with that
//! prefix, the first candidate auto-arms, and pressing `Tab` rewrites
//! the path field to the picked candidate verbatim. Unit tests in
//! `apps/tui/src/spawn.rs`
//! (`tab_in_path_zone_applies_highlighted_candidate`,
//! `tab_in_path_zone_with_no_selection_is_noop`,
//! `down_cycles_with_wrap`, `scan_dir_filters_out_files`,
//! `remote_completions_*`, `host_completions_*`) pin the in-memory
//! transitions in isolation; this test runs the same gestures through
//! the real PTY → vt100 surface and asserts the user-visible result.
//!
//! **Why Precise mode (via config):** Tab in Fuzzy mode is a no-op (see
//! `tab_is_no_op_in_fuzzy_path_zone`); the autocomplete-on-Tab semantic
//! AC-004 constrains only exists in Precise mode. Forcing Precise via
//! `[spawn] default_mode = "precise"` puts the modal on the code path
//! the AC actually constrains. Same pattern as `pty_drilldown.rs` and
//! `pty_modal_cwd.rs`.
//!
//! **Why we create a tempdir with deterministic names:** the modal's
//! auto-seeded path in Precise mode is the runtime's startup cwd (the
//! crate root, `apps/tui/`). Its real children (`src/`, `tests/`, etc.)
//! would make the wildmenu non-deterministic — any future file
//! addition in `apps/tui/` could flip the candidate ordering. A
//! dedicated tempdir with three known subdirectories (`aaa/`, `aab/`,
//! `bbb/`) gives a stable substrate. Typing the prefix `a` then
//! deterministically yields exactly two candidates (`aaa/`, `aab/`)
//! sorted lexicographically.
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
/// tempdir containing three known subdirectories (`aaa/`, `aab/`,
/// `bbb/`), then drive the modal through: clear auto-seed → type
/// `<tempdir>/a` → assert wildmenu shows BOTH `aaa` and `aab`
/// candidates → Tab to apply the highlighted (auto-armed first
/// candidate = `aaa/`) → assert the path zone now shows the applied
/// candidate AND the wildmenu transitioned to the no-matches sentinel
/// (proving refresh ran against the newly-applied empty folder).
///
/// **Signature for "Tab applied the first candidate":** after Tab,
/// the path zone contains `aaa` (the picked basename), the wildmenu
/// shows the Precise-mode no-matches sentinel `(no matches`, and the
/// unpicked sibling `aab` is gone. The sentinel is the structural
/// fingerprint of "the path now points inside an empty folder" —
/// distinct from "the path is at the tempdir and `aaa/` is a
/// candidate". The negative `!contains("aab")` discriminates "Tab
/// applied the right candidate" from "Tab no-op'd" (both `aaa` and
/// `aab` would still appear as wildmenu rows on no-op).
///
/// **What we deliberately DON'T assert here:** that `bbb` is filtered
/// out by the prefix `a`. The basename-prefix filter is pinned by the
/// `scan_dir_filters_out_files` unit test; bundling it into the E2E
/// test would complect two orthogonal concerns (Tab-applies vs
/// filter-excludes) into one failure surface.
#[test]
#[ignore = "slow-tier PTY E2E; runs via `just check-e2e` / `just test-e2e`"]
#[serial]
fn tab_in_path_zone_renders_wildmenu_and_applies_selection() {
    // Scratch tempdir held in the test so it outlives the codemux
    // child. Mirroring `pty_modal_cwd.rs` and `pty_drilldown.rs`.
    let scratch = TempDir::new().expect("scratch tempdir");
    let scratch_path = scratch
        .path()
        .to_str()
        .expect("scratch tempdir path must be valid UTF-8")
        .to_string();
    // Three subdirectories with deliberately overlapping prefixes:
    // `aaa/` and `aab/` share the `a` prefix; `bbb/` is the negative
    // control. Lexicographic sort puts `aaa` first, so the auto-armed
    // selection lands there and Tab applies it deterministically.
    for child in ["aaa", "aab", "bbb"] {
        std::fs::create_dir(scratch.path().join(child))
            .unwrap_or_else(|e| panic!("mkdir {child} inside scratch tempdir: {e}"));
    }

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
    //    auto-seeds to the startup cwd. We don't assert on that seed —
    //    `pty_modal_cwd.rs` already pins it — we just need the modal
    //    chrome to land before clearing.
    send_keys(&mut handle, "\x02c");
    screen_eventually(
        &mut handle,
        |s| s.contents().contains("@local"),
        Duration::from_secs(5),
    );

    // 3. Clear the auto-seeded path with Ctrl-U. Same gesture as the
    //    sibling drilldown / modal-cwd tests; avoids depending on how
    //    many Backspaces are needed to delete a multi-segment absolute
    //    path.
    send_keys(&mut handle, "\x15");

    // 4. Type the tempdir path WITH a trailing `/` followed by the
    //    prefix `a`. Without the trailing `/`, `split_path_for_completion`
    //    would treat the tempdir basename itself as the prefix and look
    //    in `/tmp/` for entries starting with `.tmpXYZ`. The trailing
    //    `/` anchors the parent, then `a` is the basename prefix
    //    `scan_dir` filters against — yielding `<tempdir>/aaa/` and
    //    `<tempdir>/aab/` as candidates.
    let typed_path = format!("{scratch_path}/a");
    send_keys(&mut handle, &typed_path);

    // 5. Wait for the wildmenu to render BOTH candidates. This is a
    //    SEQUENCING precondition: without confirming the wildmenu
    //    rendered, a subsequent Tab could no-op (empty wildmenu →
    //    `tab_in_path_zone_with_no_selection_is_noop` branch) and the
    //    post-Tab assertion would mis-attribute the failure.
    //
    //    We deliberately do NOT assert here that `bbb` is filtered
    //    out: the basename-prefix filter is pinned by the unit test
    //    `scan_dir_filters_out_files` (and the prefix logic in
    //    `path_completions`). Bundling that assertion into this test
    //    would complect "Tab-applies-highlighted" with "filter-
    //    excludes-non-matches" — two orthogonal behaviors that should
    //    fail independently. Architecture-guide guidance:
    //    `complect` (interleaving) is the anti-pattern, not cardinality.
    screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            c.contains("aaa") && c.contains("aab")
        },
        Duration::from_secs(5),
    );

    // 6. Tab to apply the auto-armed first candidate (`aaa/`). In
    //    Precise + Path + Search mode (input doesn't end with `/`),
    //    `refresh` auto-arms `selected = Some(0)`, so the first
    //    candidate is already highlighted when Tab fires. Same code
    //    path the `tab_in_path_zone_applies_highlighted_candidate`
    //    unit test pins (`apply_path_completion`).
    send_keys(&mut handle, "\t");

    // 7. After Tab:
    //    - The path zone shows `<tempdir>/aaa/` (with `just_descended`
    //      trimming the trailing `/` for one frame so the picked leaf
    //      reads as `aaa`).
    //    - `selected` clears.
    //    - The wildmenu lists the empty `aaa/` folder's children —
    //      none — so the Precise-mode no-matches sentinel appears.
    //    The `(no matches` substring is the verbatim head of the
    //    sentinel emitted by `wildmenu_view`'s `PathMode::Local` empty
    //    branch.
    let applied = screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            c.contains("aaa") && c.contains("(no matches")
        },
        Duration::from_secs(5),
    );
    let applied_text = applied.contents();
    assert!(
        applied_text.contains("aaa"),
        "expected path zone to show `aaa` after Tab applied the candidate; got:\n{applied_text}",
    );
    assert!(
        applied_text.contains("(no matches"),
        "expected Precise no-matches sentinel after Tab descended into empty `aaa/`; got:\n{applied_text}",
    );
    // Negative: the unpicked sibling `aab` must NOT still be visible.
    // This is the discriminator between "Tab applied `aaa/`" (correct,
    // the first auto-armed candidate) and "Tab no-op'd" (path is
    // still `<tempdir>/a`, both `aaa` and `aab` still in the
    // wildmenu). Without this, an off-by-one regression in
    // `move_selection_forward` (e.g. auto-arming index 1 instead of
    // 0) would still pass the positive assertion because `aaa` would
    // appear in BOTH the path zone (applied) and the wildmenu (stale
    // before the refresh).
    assert!(
        !applied_text.contains("aab"),
        "expected `aab` NOT visible after Tab applied `aaa/`; got:\n{applied_text}",
    );
}
