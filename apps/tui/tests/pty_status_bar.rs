//! AC-042 (status bar is hidden in `LeftPane` chrome): pin that the
//! bottom status bar renders in default `Popup` chrome, disappears
//! when chrome flips to `LeftPane` via `prefix v`, and comes back
//! when chrome flips back. The runtime-level rationale lives at
//! `apps/tui/src/runtime.rs::render_frame` — the `NavStyle::Popup`
//! branch calls `render_popup_style`, which calls `render_status_bar`;
//! the `NavStyle::LeftPane` branch calls `render_left_pane`, which
//! does NOT call `render_status_bar`. In `LeftPane` chrome the
//! navigator already occupies the left column with rich agent info,
//! so the status bar would duplicate or clutter that — hence the
//! suppression.
//!
//! Gating mirrors `pty_nav.rs`: `test-fakes` feature, `#[ignore]` so
//! the slow tier ships through `just check-e2e` only, and `#[serial]`
//! because the PTY harness is not safe to run in parallel.

#![cfg(feature = "test-fakes")]

// Each `tests/*.rs` integration target compiles `mod common` as its own
// crate; helpers consumed only by sibling test files (e.g. `wait_for_exit`,
// used by `pty_lifecycle.rs`) trip `dead_code` here. Same allow-on-import
// pattern as `pty_smoke.rs`.
#[allow(dead_code)]
mod common;

use std::time::Duration;

use serial_test::serial;

use common::{screen_eventually, send_keys, spawn_codemux};

/// Flip `Popup` -> `LeftPane` -> `Popup` and assert the status bar
/// presence tracks the chrome.
///
/// **Status bar signature:** the substring `" for help"` (with a
/// leading space). It comes from
/// `apps/tui/src/status_bar/segments.rs::PrefixHintSegment` whose
/// idle render is `format!("{prefix} {help} for help", ...)`. The
/// hint is the rightmost segment in the default stack and the
/// drop-from-the-left algorithm in `render_segments` makes it the
/// last segment to drop under width pressure — at 80 columns it is
/// effectively guaranteed to render. It's also the only stable
/// substring: `model` / `tokens` are sourced from the
/// `agent_meta_worker` which hasn't reported anything in this test
/// environment (no real `~/.claude/settings.json` lookup against the
/// fake agent's PID, no statusLine snapshot), and `branch` /
/// `worktree` hide themselves on the trunk / in the main checkout —
/// which is exactly where this test runs from. So the prefix hint
/// is the one signal that survives a hostile-to-segments environment.
/// And critically: a search of the codebase confirms `" for help"`
/// is produced only by `PrefixHintSegment` — no header, banner, or
/// chrome surface emits it elsewhere, so its presence on the rendered
/// grid is one-to-one with "the status bar drew this frame."
///
/// **Why the round-trip (`Popup` -> `LeftPane` -> `Popup`):** the
/// second flip back to `Popup` is the load-bearing assertion. Without
/// it, a bug that disabled the status bar globally (say, a refactor
/// that short-circuits `render_status_bar` to a no-op) would still
/// pass the "absent in `LeftPane`" check and the test would green up
/// while the feature was broken. The third assertion proves the
/// suppression is chrome-bound, not permanent — only the `LeftPane`
/// branch elides the bar, and flipping back must restore it.
///
/// **Why one PTY fixture:** chrome flips are cheap and the test
/// reuses the same harness state. Spinning a fresh `codemux` for each
/// of the three phases would triple the test's runtime without
/// catching anything a single fixture misses — the only state that
/// could leak between flips lives inside the runtime we're testing,
/// which is the point.
#[test]
#[ignore = "slow-tier PTY E2E; runs via `just check-e2e` / `just test-e2e`"]
#[serial]
fn status_bar_renders_in_popup_chrome_and_hides_in_leftpane() {
    let mut handle = spawn_codemux();

    // Phase 1 — default Popup chrome: wait for the fake's prompt AND
    // the status-bar hint. Predicating on both pins "the runtime
    // booted into Popup with the status bar drawn," not just "the
    // agent's bytes arrived." Without the hint check here, a future
    // change that launches in LeftPane by default would still satisfy
    // the agent-ready predicate and make the rest of the test
    // meaningless.
    let popup_initial = screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            c.contains("FAKE_AGENT_READY") && c.contains(" for help")
        },
        Duration::from_secs(5),
    );
    assert!(
        popup_initial.contents().contains(" for help"),
        "expected status-bar hint in default Popup chrome; got:\n{}",
        popup_initial.contents()
    );
    assert!(
        !popup_initial.contents().contains(" agents "),
        "expected NO LeftPane navigator title in default Popup chrome; got:\n{}",
        popup_initial.contents()
    );

    // Phase 2 — flip to LeftPane via the default prefix chord
    // (`Ctrl+B` then `v`). Same hard-coded `"\x02v"` rationale as
    // `pty_nav.rs`: harness ships an empty `XDG_CONFIG_HOME` so the
    // defaults from `Bindings::default()` are guaranteed in force.
    send_keys(&mut handle, "\x02v");

    // Wait for the LeftPane navigator title to appear AND the status-
    // bar hint to disappear. Predicating on both directions in one
    // predicate (instead of two sequential `screen_eventually` calls)
    // means we never race on "navigator drew first, status bar will
    // be cleared on the next frame" — the test waits until both have
    // settled.
    let after_flip = screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            c.contains(" agents ") && !c.contains(" for help")
        },
        Duration::from_secs(5),
    );
    assert!(
        after_flip.contents().contains(" agents "),
        "expected LeftPane navigator title after `prefix v`; got:\n{}",
        after_flip.contents()
    );
    assert!(
        !after_flip.contents().contains(" for help"),
        "expected NO status-bar hint in LeftPane chrome; got:\n{}",
        after_flip.contents()
    );

    // Phase 3 — flip back to Popup. The round-trip is what proves
    // the suppression is chrome-bound rather than a one-way kill of
    // the status bar. See the doc comment above for why this matters.
    send_keys(&mut handle, "\x02v");

    let restored = screen_eventually(
        &mut handle,
        |s| {
            let c = s.contents();
            c.contains(" for help") && !c.contains(" agents ")
        },
        Duration::from_secs(5),
    );
    assert!(
        restored.contents().contains(" for help"),
        "expected status-bar hint to reappear after flipping back to Popup; got:\n{}",
        restored.contents()
    );
    assert!(
        !restored.contents().contains(" agents "),
        "expected NO LeftPane navigator title after flipping back to Popup; got:\n{}",
        restored.contents()
    );
}
