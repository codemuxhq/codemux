//! AC-007 (saved project alias resolves through the minibuffer): pin
//! the typed-alias → wildmenu-row → Enter-spawns gesture end-to-end
//! through the real keymap, fuzzy worker, modal, and runtime.
//!
//! **What this pins:** a `[[spawn.projects]]` entry declared in
//! `config.toml` flows through to:
//!   1. `score_fuzzy` matching the typed query against the project's
//!      `name` (the alias) and emitting the resolved path as a hit.
//!   2. The wildmenu rendering a named-project row (`★` star + alias +
//!      collapsed path) for that hit. The assertion targets the alias +
//!      path basename (ASCII) rather than the `★` glyph to keep the
//!      test robust to any future vt100-parser swap; the glyph itself
//!      is pinned by `named_project_row_*` unit tests.
//!   3. `confirm()` resolving the highlighted hit into the project's
//!      path and emitting `ModalOutcome::Spawn` so the runtime can
//!      spawn an agent at the resolved cwd.
//!
//! Unit tests in `apps/tui/src/spawn.rs`
//! (`build_project_meta_*`, `host_bound_tilde_project_round_trips_as_literal_path`,
//! `local_only_tilde_project_emits_locally_expanded_candidate`,
//! `score_fuzzy_named_project_outranks_git_repo`,
//! `named_project_row_*`) pin each piece of the pipeline in isolation;
//! this test runs them through the real PTY → vt100 surface and asserts
//! the user-visible result.
//!
//! **Why a LOCAL named project (host: None):** host-bound entries
//! trigger `PrepareHostThenSpawn`, which kicks off the SSH bootstrap
//! state machine — that path needs a real `sshd` to succeed, which the
//! hermetic PTY harness can't provide. Local-only entries fall through
//! to plain `Spawn { host: "local", path: <resolved> }`, which the
//! runtime can satisfy in-process. The `@host` badge rendering is
//! already covered by `named_project_row_renders_host_badge_next_to_name`;
//! re-pinning it at the PTY level would require infrastructure (a fake
//! SSH endpoint) that hasn't been built. End-to-end host-bound
//! coverage is explicitly listed as uncovered in AC-007's `**Tests:**`
//! block.
//!
//! **Why we point `search_roots` at an empty tempdir:** the fuzzy
//! worker only emits results once `set_index` lands with a
//! `cached_dirs` snapshot (see `runtime.rs::tick_fuzzy_dispatch` and
//! `IndexState::cached_dirs`). With the default `search_roots = ["~"]`
//! the indexer walks the developer's $HOME — slow, non-deterministic,
//! and could time out the test. A literally-empty roots list
//! (`search_roots = []`) is treated as an error
//! (`IndexError::NoRoots`) which puts the index into `Failed` state,
//! and `cached_dirs()` returns `None` for `Failed` — the worker is
//! never dispatched. The fix is a real-but-empty tempdir as the
//! search root: the walker traverses it in microseconds, the state
//! settles to `Ready { dirs: vec![] }`, and the worker scores the
//! empty index against the named project list. The named project's
//! score path is `score_fuzzy`'s "named first" loop (around line 2114
//! of `apps/tui/src/spawn.rs`) and runs independently of `dirs`.
//!
//! **Why we use an absolute tempdir path (no `~/`):** the test must be
//! hermetic. `~/myproj` would resolve to the developer's home dir,
//! which would either leak into the test (false positives) or fail to
//! exist (false negatives on the spawn). An absolute tempdir path
//! passes through `expand_named_project_path` unchanged (the tilde-
//! expansion arms only fire on `~/...` prefixes) and gives the test a
//! known, isolated cwd for the spawned agent.
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

/// Boot codemux with a config that declares a single `[[spawn.projects]]`
/// entry pointing at a tempdir. Open the spawn modal (default Fuzzy
/// mode), type the alias name, assert the named-project row surfaces in
/// the wildmenu (alias + tempdir basename on the same logical line),
/// then press Enter and assert a second agent spawned (modal closes +
/// `[2]` appears in the `LeftPane` navigator).
///
/// **Signature for "alias resolved into a wildmenu row":** the alias
/// name `myproj` plus the project tempdir's basename appear together
/// on the same logical line — `named_project_row` is the only chrome
/// that renders the alias + path pair (the prompt line above just
/// shows `myproj` as the typed `fuzzy_query` echo, no path next to
/// it). Asserting on the tempdir basename (not the full path) keeps
/// the assertion stable in the face of `clip_middle` trimming long
/// paths; the basename is at the tail of the rendered row and is
/// always preserved (`clip_middle` clips from the head with a leading
/// `…` ellipsis).
///
/// **Signature for "Enter spawned at the resolved path":** modal closes
/// (`@local` gone) AND the `LeftPane` navigator shows `[2]` (a second
/// agent slot exists). The actual cwd of the spawned agent is not
/// observable directly — `fake_agent` doesn't print its cwd — but the
/// path-resolution math is pinned by the unit tests
/// (`local_only_tilde_project_emits_locally_expanded_candidate` and
/// `score_fuzzy` callers); this test pins the *end-to-end pipeline*
/// from typed alias to a spawned tab.
#[test]
#[ignore = "slow-tier PTY E2E; runs via `just check-e2e` / `just test-e2e`"]
#[serial]
fn saved_alias_in_config_resolves_to_real_path() {
    // Scratch tempdir held in the test so it outlives the codemux child.
    // Mirroring `pty_modal_cwd.rs` and `pty_spawn_action.rs`.
    let project_dir = TempDir::new().expect("project tempdir");
    let project_path = project_dir
        .path()
        .to_str()
        .expect("project tempdir path must be valid UTF-8")
        .to_string();
    // Tempdir basename used as the wildmenu-row signature. Used in the
    // post-typing assertion to distinguish "the named-project row
    // rendered" from "the alias is just echoed in the prompt line".
    // The basename always survives `clip_middle` (which trims from the
    // head with a leading `…`).
    let project_basename = project_dir
        .path()
        .file_name()
        .and_then(|s| s.to_str())
        .expect("project tempdir basename must be valid UTF-8")
        .to_string();
    // Separate tempdir for scratch_dir so the default `~/.codemux/scratch`
    // doesn't pollute the developer's home. The actual scratch dir is
    // unused by this test (we spawn through an alias, not the empty-
    // modal-Enter scratch path), but the config field still has to
    // resolve to *something*; tempdir avoids any home write.
    let scratch = TempDir::new().expect("scratch tempdir");
    let scratch_path = scratch
        .path()
        .to_str()
        .expect("scratch tempdir path must be valid UTF-8")
        .to_string();
    // Empty tempdir used as the indexer's `search_roots` so the walker
    // settles to `Ready { dirs: vec![] }` instead of failing with
    // `NoRoots` (which a literal `search_roots = []` produces). See
    // the file header for why an empty-but-real root matters.
    let index_root = TempDir::new().expect("index-root tempdir");
    let index_root_path = index_root
        .path()
        .to_str()
        .expect("index-root tempdir path must be valid UTF-8")
        .to_string();

    // Config:
    //   - `search_roots` points at the empty index-root tempdir so the
    //     walker completes instantly with zero dirs but settles to
    //     `Ready` (not `Failed`); the fuzzy worker is then dispatched
    //     and surfaces named projects independently.
    //   - `[[spawn.projects]]` declares `name = "myproj"` pointing at
    //     the project tempdir. No `host = ...` so this is a local-only
    //     entry — Enter routes to `Spawn`, not `PrepareHostThenSpawn`.
    //   - `default_mode` is left at the default (Fuzzy) — named
    //     projects only surface through the fuzzy worker's
    //     `score_fuzzy`, not Precise mode's `read_dir` completion.
    //
    // `{:?}` formatting on `&str` produces a TOML-compatible quoted
    // string with `"` and `\` properly escaped — same defensive
    // formatting as the sibling spawn tests.
    let config = format!(
        "[spawn]\n\
         search_roots = [{index_root_path:?}]\n\
         scratch_dir = {scratch_path:?}\n\
         \n\
         [[spawn.projects]]\n\
         name = \"myproj\"\n\
         path = {project_path:?}\n",
    );

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

    // 2. Flip to `LeftPane` chrome so the navigator's agent list is
    //    visible. The ` agents ` block title is the chrome fingerprint
    //    (same signal as `pty_spawn_action.rs`). Doing this BEFORE
    //    opening the modal means the post-spawn `[2]` assertion can
    //    fire as soon as the spawn completes — no extra chrome flip
    //    after the modal closes.
    send_keys(&mut handle, "\x02v");
    screen_eventually(
        &mut handle,
        |s| s.contents().contains(" agents "),
        Duration::from_secs(5),
    );

    // 3. Open the spawn modal. `@local` is the host-placeholder
    //    fingerprint visible in the modal's prompt line.
    send_keys(&mut handle, "\x02c");
    screen_eventually(
        &mut handle,
        |s| s.contents().contains("@local"),
        Duration::from_secs(5),
    );

    // 4. Type the alias name. In Fuzzy + Path mode, the typed chars
    //    land in `fuzzy_query` (not `path`), the runtime dispatches
    //    the query to the fuzzy worker, and the worker's
    //    `score_fuzzy` matches against the named project's `name`
    //    field and emits the project path as a hit. The result lands
    //    via `set_fuzzy_results`, which auto-arms `selected = Some(0)`
    //    for the freshly-arrived first hit.
    send_keys(&mut handle, "myproj");

    // 5. Wait for the wildmenu to render the named-project row.
    //    Signature: the alias name `myproj` AND the project tempdir's
    //    basename appear together. The alias shows up twice — once in
    //    the prompt line (echo of the typed `fuzzy_query`) and once
    //    in the rendered row — and the basename is unique to the
    //    row's dim trailing path. Asserting on both narrows the match
    //    to "the named-project row landed, not just the typed echo".
    //    Stays ASCII-only on purpose: the `★` glyph used in the row
    //    is reliable through the harness's `vt100` parser, but
    //    asserting on ASCII keeps the test robust to any future
    //    parser swap and dodges the wide-glyph / wcwidth pitfalls
    //    documented for TUIs at large.
    let wildmenu = screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            c.contains("myproj") && c.contains(&project_basename)
        },
        Duration::from_secs(10),
    );
    let wildmenu_text = wildmenu.contents();
    assert!(
        wildmenu_text.contains("myproj"),
        "expected `myproj` alias name in wildmenu row; got:\n{wildmenu_text}",
    );
    assert!(
        wildmenu_text.contains(&project_basename),
        "expected project tempdir basename `{project_basename}` in wildmenu row; got:\n{wildmenu_text}",
    );

    // 6. Enter to spawn. With `selected = Some(0)` auto-armed by
    //    `set_fuzzy_results`, `confirm()` resolves the highlighted hit
    //    to the project path and emits `ModalOutcome::Spawn { host:
    //    "local", path: <project_path> }` (the named project has no
    //    `host =` field, so the `PrepareHostThenSpawn` branch in
    //    `confirm` doesn't fire — see lines 1119-1133 of
    //    `apps/tui/src/spawn.rs`). The runtime handles `Spawn` by
    //    spawning a local PTY at the resolved cwd.
    send_keys(&mut handle, "\r");

    // 7. Modal closes AND a second agent appears. The two-condition
    //    predicate guards against a partial regression — modal closed
    //    but no spawn, or spawn happened but modal lingered.
    let after = screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            !c.contains("@local") && c.contains("[1]") && c.contains("[2]")
        },
        Duration::from_secs(10),
    );
    let after_text = after.contents();
    assert!(
        !after_text.contains("@local"),
        "expected modal to close after Enter on aliased row; got:\n{after_text}",
    );
    assert!(
        after_text.contains("[1]"),
        "expected `[1]` (first agent) still in navigator; got:\n{after_text}",
    );
    assert!(
        after_text.contains("[2]"),
        "expected `[2]` (alias-spawned agent) in navigator; got:\n{after_text}",
    );
}
