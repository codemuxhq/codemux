//! AC-027: Daemon serves a screen-state snapshot on every attach.
//!
//! Wire-level coverage of the snapshot-on-reattach contract. The
//! supervisor unit tests in `apps/daemon/src/supervisor.rs` already
//! pin the in-process behaviour (manual writes to the `Mutex<Parser>`,
//! in-process `serve_one`); this test pins the same contract through
//! the actual binary: spawn `codemuxd` as a subprocess, attach via a
//! Unix-stream wire client, write through one connection, drop it,
//! reconnect with a brand-new connection, assert the first `PtyData`
//! frame after the second `HelloAck` contains the prior session's
//! marker.
//!
//! The fake agent (`cat`-like in spirit but written in Rust for portability)
//! is overridden via `spawn_codemuxd_with_agent` so the test can drive
//! a deterministic echo path: bytes the client writes via `PtyData`
//! reach the agent's stdin; the agent does NOT echo back (it just
//! discards), so the screen state we're verifying comes purely from
//! the PTY's line-discipline echo of the typed bytes.
//!
//! Gating mirrors `proto_smoke.rs`: `#![cfg(feature = "test-fakes")]`
//! plus `#[ignore]`.

#![cfg(feature = "test-fakes")]

#[allow(dead_code)]
mod common;

use std::time::Duration;

use common::{collect_pty_data_until, spawn_codemuxd};

/// The marker bytes we write through the first attach. Chosen so they
/// stand out in the encoded `state_formatted` output: an alphanumeric
/// run is preserved verbatim by `vt100` cell encoding, while ESC
/// sequences would be reordered by the parser.
const MARKER: &[u8] = b"snapshot-marker-027";

#[test]
#[ignore = "slow-tier daemon E2E; runs via `just check-e2e` / `just test-e2e`"]
fn snapshot_on_reattach_includes_prior_session_screen_state() {
    let daemon = spawn_codemuxd();

    // First attach: write the marker, wait until it shows up in the
    // PTY's line-discipline echo, then drop the client. The daemon's
    // vt100 mirror has the marker on its grid at this point.
    {
        let mut client = daemon.connect(24, 80, "snap-agent");
        // Discard the initial snapshot frame (clear + cursor home + the
        // fake agent's boot prompt). We don't care about its contents
        // here; the test's load-bearing assertion is on the SECOND
        // attach's snapshot.
        let _initial = collect_pty_data_until(
            &mut client,
            |bytes| {
                bytes
                    .windows(b"FAKE_AGENT_READY".len())
                    .any(|w| w == b"FAKE_AGENT_READY")
            },
            Duration::from_secs(2),
        );

        client.send(&codemux_wire::Message::PtyData({
            let mut v = MARKER.to_vec();
            v.push(b'\n');
            v
        }));

        // Wait for the echo to come back so we know the marker has been
        // ingested by the daemon's parser. Without this, the second
        // attach could race ahead of the first attach's PtyData write
        // and find an empty mirror.
        let echoed = collect_pty_data_until(
            &mut client,
            |bytes| bytes.windows(MARKER.len()).any(|w| w == MARKER),
            Duration::from_secs(2),
        );
        assert!(
            echoed.windows(MARKER.len()).any(|w| w == MARKER),
            "first attach: line-discipline echo should include the marker; \
             got {} bytes: {:?}",
            echoed.len(),
            String::from_utf8_lossy(&echoed),
        );
        // `client` drops here — clean detach.
    }

    // Second attach: connect with a fresh client, do NOT write anything.
    // The very first PtyData frame the daemon emits must already
    // contain the marker; that's the snapshot. Without snapshot
    // replay, this read would either hang (no further bytes from an
    // idle child) or arrive with only the fake's boot prompt.
    {
        let mut client = daemon.connect(24, 80, "snap-agent");
        let snapshot = collect_pty_data_until(
            &mut client,
            |bytes| bytes.windows(MARKER.len()).any(|w| w == MARKER),
            Duration::from_secs(3),
        );
        assert!(
            snapshot.windows(MARKER.len()).any(|w| w == MARKER),
            "reattach snapshot should include the marker the previous \
             attach left on the screen, without any client-side writes; \
             got {} bytes: {:?}",
            snapshot.len(),
            String::from_utf8_lossy(&snapshot),
        );
    }
}
