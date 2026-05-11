//! T4 smoke test: boots the real `codemuxd` binary against the
//! in-tree `fake_daemon_agent` stub and asserts the handshake completes
//! plus the daemon serves the fake's boot prompt as `PtyData`.
//!
//! Why this test: it pins the entire harness wiring path — subprocess
//! spawn via `CARGO_BIN_EXE_codemuxd`, foreground tracing, socket bind,
//! Unix-stream connect from the test, `Hello`/`HelloAck` round-trip,
//! `PtyData` decode of the fake's `FAKE_AGENT_READY> ` prompt. If this
//! test passes, the T4 harness is alive.
//!
//! Gating mirrors the TUI T3 smoke test:
//! - `#![cfg(feature = "test-fakes")]` — the harness references
//!   `env!("CARGO_BIN_EXE_fake_daemon_agent")` which only exists when
//!   the feature compiles the fake binary.
//! - `#[ignore]` — slow-tier, runs via `just test-e2e` / `just
//!   check-e2e`.
//! - **No** `#[serial]`: each test owns its own `TempDir`-scoped
//!   socket, so parallel runs do not race. (NLM-flagged the
//!   `tempdir + #[serial]` combination as redundant.)

#![cfg(feature = "test-fakes")]

#[allow(dead_code)]
mod common;

use std::time::Duration;

use codemux_wire::Message;

use common::{collect_pty_data_until, spawn_codemuxd};

#[test]
#[ignore = "slow-tier daemon E2E; runs via `just check-e2e` / `just test-e2e`"]
fn handshake_completes_and_fake_agent_prompt_arrives_as_pty_data() {
    let daemon = spawn_codemuxd();
    let mut client = daemon.connect(24, 80, "smoke-agent");

    assert_ne!(
        client.daemon_pid(),
        0,
        "HelloAck must report a non-zero daemon pid",
    );

    let payload = collect_pty_data_until(
        &mut client,
        |bytes| {
            bytes
                .windows(b"FAKE_AGENT_READY".len())
                .any(|w| w == b"FAKE_AGENT_READY")
        },
        Duration::from_secs(5),
    );

    assert!(
        payload
            .windows(b"FAKE_AGENT_READY".len())
            .any(|w| w == b"FAKE_AGENT_READY"),
        "expected fake-agent prompt on the wire; got {} bytes: {:?}",
        payload.len(),
        String::from_utf8_lossy(&payload),
    );

    // Belt-and-suspenders: with `Message` brought into scope, this also
    // serves as a compile-time check that the harness re-exports the
    // wire types correctly.
    let _ = Message::Ping { nonce: 0 };
}
