//! First slow-tier PTY test: boots the real `codemux` binary inside an
//! 80x24 PTY against the in-tree `fake_agent` stub and asserts the
//! fake's prompt makes it onto the rendered cell grid.
//!
//! Why this test: it pins the entire wiring path — `--agent-bin` /
//! `CODEMUX_AGENT_BIN` plumbing through clap, the `AgentSpawner` Port
//! dispatching to `BinaryAgentSpawner`, the spawned PTY's bytes
//! flowing through the runtime's render loop, and the `vt100` parser
//! seeing what the real terminal would. If this test passes, the T2
//! harness is alive.
//!
//! Gating:
//! - `#![cfg(feature = "test-fakes")]` because the harness reaches for
//!   `env!("CARGO_BIN_EXE_fake_agent")` — without the feature, that
//!   bin target doesn't exist and the build fails before tests run.
//! - `#[ignore]` so a default `cargo test` (and thus `just check`)
//!   skips it. The slow tier ships through `just check-e2e`, which
//!   passes `--ignored`.
//! - `#[serial]` because the harness allocates a real PTY and reads
//!   the master end on a background thread. Two of these in parallel
//!   would race on shared resources (pty allocation, terminal-size
//!   negotiation if it ever lands) without buying coverage.

#![cfg(feature = "test-fakes")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use serial_test::serial;

use common::{screen_eventually, spawn_codemux};

/// Smoke test: codemux launches its initial agent automatically (no
/// modal at boot — see `runtime::run`'s up-front `spawn_local_agent`
/// call), so the fake agent's prompt should appear on the screen
/// without any additional input from the harness.
///
/// No keystrokes are sent. Forwarding bytes into the fake's stdin
/// (which it discards) and into the runtime's key dispatcher would
/// mask a real regression where the agent doesn't render until
/// prodded. T3 will introduce a key-sending helper when there's an
/// actual lifecycle test to drive.
#[test]
#[ignore = "slow-tier PTY E2E; runs via `just check-e2e` / `just test-e2e`"]
#[serial]
fn fake_agent_prompt_renders() {
    let mut handle = spawn_codemux();

    let screen = screen_eventually(
        &mut handle,
        |s| s.contents().contains("FAKE_AGENT_READY"),
        // 5s is generous — the spawn path is sub-second on a warm
        // build. Bigger budget here catches a cold-cache `target/`
        // (first run after a `cargo clean`) without re-tuning.
        Duration::from_secs(5),
    );

    // Belt-and-suspenders: `screen_eventually` already asserted the
    // predicate held; this assert just makes the failure message
    // obvious if the predicate ever changes shape and stops checking
    // what the test name promises.
    assert!(
        screen.contents().contains("FAKE_AGENT_READY"),
        "expected fake agent prompt on screen; got:\n{}",
        screen.contents()
    );
}
