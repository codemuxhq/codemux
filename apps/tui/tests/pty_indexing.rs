//! AC-045 (indexing runs in the background; input stays interactive):
//! pin the "indexer never blocks the keystroke handler" guarantee
//! through a real PTY.
//!
//! **What this pins:** the fuzzy directory index is built on a worker
//! thread (see `apps/tui/src/index_worker.rs::start_index`) and the
//! modal's per-keystroke `handle` path does NOT join on it. If the
//! indexer were ever refactored to run inline on the main loop (or to
//! block the modal until `IndexState::Ready`), typing into the path
//! zone while the walker is still enumerating would freeze — the typed
//! substring would not reach the rendered screen within the deadline,
//! and this test would fail.
//!
//! Two surfaces are observed simultaneously:
//!
//! 1. The wildmenu sentinel `⠋ indexing…` (see
//!    `SpawnMinibuffer::fuzzy_state_view` in `apps/tui/src/spawn.rs`
//!    around line 1741) — present iff the indexer is in
//!    `IndexState::Building`. Renderer-level unit tests already cover
//!    the per-state sentinel selection; what they cannot pin is that
//!    the runtime actually sees `Building` long enough for the user to
//!    observe it.
//! 2. A user-typed substring inside the prompt — present iff the
//!    keystroke handler ran while the walker was still building.
//!
//! Observing both in the same frame is the AC-045 invariant: the index
//! is running AND the path zone is accepting input.
//!
//! **Why a synthesized many-subdir tempdir:** the default search root
//! (`~`) and the codemux repo itself walk fast enough that the walker
//! can transition `Building → Ready` between two consecutive frames,
//! and `screen_eventually` would never catch the `⠋ indexing…`
//! sentinel. Creating a tempdir with `MANY_SUBDIRS` empty subdirectories
//! and pointing `[spawn] search_roots` at it forces the walker to spend
//! enough wall-clock time enumerating to make the `Building` state
//! observable. The `ignore::WalkBuilder` walks each entry plus
//! per-entry git-ignore stat overhead, so even empty subdirs cost real
//! syscalls.
//!
//! **Why `MANY_SUBDIRS = 4000`:** trade-off between setup cost and
//! observation window. On a warm dev box SSD, ~4000 subdirs costs
//! roughly 100–300 ms of walker time — comfortably wider than the
//! `screen_eventually` 5 ms polling cadence, so at least one frame
//! lands while `Building` holds. Setup itself (4000 `mkdir` calls) is
//! ~50–150 ms on the same hardware, so the total test cost stays under
//! a second. If the walker ever speeds up enough that this flakes,
//! raise the count rather than lowering the deadline — the test is a
//! race against the index, not against the runtime.
//!
//! **Trap to document:** if the indexer finishes before the harness
//! observes the `Building` state (e.g. on a tmpfs root, a much faster
//! filesystem, or a future walker optimization), the predicate never
//! sees `⠋ indexing…` and `screen_eventually` panics with the rendered
//! screen. Bump `MANY_SUBDIRS` and re-run the 5x flake check before
//! merging.
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

/// Synthesized subdirectory count for the slow-walker tempdir. See the
/// file-level doc comment for the tuning rationale. Raising this widens
/// the observation window at a roughly-linear setup cost; lowering it
/// risks flake on fast filesystems.
const MANY_SUBDIRS: usize = 4000;

/// AC-045: while the indexer is `Building`, the wildmenu shows the
/// indexing sentinel AND the path zone keeps accepting keystrokes.
///
/// **Positive signature (indexer running):** the literal substring
/// `indexing` (lower-case) — produced by the `⠋ indexing…` sentinel in
/// `fuzzy_state_view`. Matching on `indexing` rather than `⠋` avoids
/// any future spinner-frame rotation (the worker today only ever
/// renders frame 0, but the rendering helper holds the full cycle —
/// see `FRAMES` near line 2743 of `apps/tui/src/spawn.rs`).
///
/// **Positive signature (path zone responsive):** the literal substring
/// `xyz123` — six ASCII chars that cannot appear anywhere else on the
/// rendered screen (not in the fake agent's prompt `FAKE_AGENT_READY>`,
/// not in the navigator's default agent label `agent-1`, not in any
/// path under the synthesized tempdir). When the runtime renders the
/// prompt for Fuzzy + Path mode, the `fuzzy_query` is interpolated into
/// the prompt span (see `apps/tui/src/spawn.rs` around line 1874), so
/// the typed substring appears in the screen contents iff the handler
/// ran for each Char.
///
/// **Why a single `screen_eventually` for both:** matching both
/// substrings in the same predicate guarantees a frame where the
/// indexer was still `Building` AND the typed query had already
/// rendered. Two separate `screen_eventually` calls would let the
/// indexer finish between them, breaking the simultaneity invariant
/// the AC requires.
///
/// **Why the harness does NOT close the modal at the end:** the
/// `Drop` impl on `CodemuxHandle` kills the child and reaps the
/// reader. No additional teardown is needed; explicitly Esc-ing the
/// modal would only buy a coverage of close-while-indexing, which is
/// a separate concern not pinned by AC-045.
#[test]
#[ignore = "slow-tier PTY E2E; runs via `just check-e2e` / `just test-e2e`"]
#[serial]
fn modal_indexing_sentinel_renders_while_path_zone_accepts_input() {
    // Slow-walker tempdir: many empty subdirs so the `WalkBuilder`
    // spends enough wall-clock time enumerating that the runtime
    // renders at least one frame in `IndexState::Building`. See the
    // file-level doc comment for the tuning rationale.
    let big = TempDir::new().expect("big tempdir for slow walker");
    for i in 0..MANY_SUBDIRS {
        std::fs::create_dir(big.path().join(format!("dir_{i:05}"))).expect("mkdir subdir");
    }
    let big_path = big
        .path()
        .to_str()
        .expect("big tempdir path must be valid UTF-8")
        .to_string();

    // Scratch tempdir so the default-Enter path (which lazily creates
    // `~/.codemux/scratch`) doesn't touch the developer's real home.
    // Same pattern as `pty_spawn_action.rs` / `pty_modal_cwd.rs`.
    let scratch = TempDir::new().expect("scratch tempdir");
    let scratch_path = scratch
        .path()
        .to_str()
        .expect("scratch tempdir path must be valid UTF-8")
        .to_string();

    // `{:?}` on `&str` produces TOML-compatible quoted strings with
    // backslashes / quotes properly escaped. `default_mode` is omitted
    // so the modal opens in Fuzzy (the default), which is the engine
    // the AC's "wildmenu shows a spinner sentinel" clause applies to.
    let config =
        format!("[spawn]\nscratch_dir = {scratch_path:?}\nsearch_roots = [{big_path:?}]\n");

    let mut handle = spawn_codemux_with_config(&config);

    // Steady state: fake's prompt is on screen, no modal yet. Same
    // baseline predicate as `pty_spawn.rs`.
    screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            c.contains("FAKE_AGENT_READY") && !c.contains("@local")
        },
        Duration::from_secs(5),
    );

    // Open the spawn modal. With default Fuzzy mode the path zone
    // opens empty (no auto-seed in Fuzzy) and the wildmenu renders the
    // indexing sentinel for the duration of the walker's `Building`
    // state. The runtime triggers a fresh SWR refresh on modal open
    // (see `runtime.rs::3582`), which kicks the walker against our
    // synthesized many-subdir tempdir.
    send_keys(&mut handle, "\x02c");

    // Type a distinctive substring into the path zone WHILE the walker
    // is still enumerating. `xyz123` is six ASCII characters; none of
    // them are `/`, `~`, `@`, or any other modal action chord, so each
    // Char arm of `SpawnMinibuffer::handle` simply pushes to
    // `fuzzy_query` and calls `mark_fuzzy_stale` (a non-blocking
    // synchronous op — no worker round-trip). The prompt re-renders
    // with the typed substring on the very next frame.
    //
    // Sent in one batch so the kernel's pty buffer has the whole
    // string queued before the runtime drains a single read; sending
    // char-by-char would let the runtime interleave a `tick` between
    // each, giving the walker extra time to finish before we observe
    // the simultaneity.
    send_keys(&mut handle, "xyz123");

    // Single predicate: BOTH the indexing sentinel AND the typed query
    // must be visible in the same frame. If the walker finished
    // between the keystroke and the screen drain, `indexing` would be
    // gone (the sentinel transitions to either the project hits or to
    // `(no matches)` once `Ready`), and the predicate would fail —
    // surfacing exactly the regression the AC guards against.
    let opened = screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            c.contains("indexing") && c.contains("xyz123")
        },
        Duration::from_secs(5),
    );

    // Belt-and-suspenders assertions: `screen_eventually` already
    // guaranteed both substrings rendered together, but separate
    // asserts produce clearer failure messages if the predicate is
    // ever reshaped and silently drops one half.
    let contents = opened.contents();
    assert!(
        contents.contains("indexing"),
        "AC-045: expected `indexing` sentinel while walker still Building; got:\n{contents}",
    );
    assert!(
        contents.contains("xyz123"),
        "AC-045: expected typed query `xyz123` in path zone while indexing; got:\n{contents}",
    );
}
