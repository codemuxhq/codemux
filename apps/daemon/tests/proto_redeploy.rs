//! AC-044: Stale daemon is killed and re-deployed on local-binary
//! upgrade.
//!
//! The bootstrap-layer unit tests (`crates/codemuxd-bootstrap`) pin the
//! orchestration: the SIGTERM-then-SIGKILL prelude only runs on
//! `force_respawn`, the version probe drives the redeploy decision,
//! and the embedded source tarball machinery is well-formed. Those
//! tests own "did the bootstrap emit the right shell snippet."
//!
//! **This test** pins the protocol-layer view of the same event: an
//! existing daemon dies, a fresh daemon binds the same socket, and the
//! next wire-protocol attach succeeds against the new daemon. From the
//! client's vantage point this is "old session disappears, new
//! `HelloAck` reports a different `daemon_pid`."
//!
//! Two things this test deliberately does NOT pin (each owned by the
//! bootstrap layer or AC-027):
//! - Version detection — the bootstrap's `prepare_remote_skips_install_when_version_matches`
//!   and friends pin the "did the bootstrap decide to redeploy" half.
//! - In-flight Claude session loss — `Session`'s Drop kills the child;
//!   the supervisor unit tests cover this independent of redeploy.

#![cfg(feature = "test-fakes")]
// Test fixtures: `expect()` is the project's chosen failure shape for
// setup-time errors. Same rationale as `apps/daemon/tests/common/mod.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used)]
// The redeploy flow deliberately tears down `first` mid-test via
// `kill -TERM`, then re-issues `try_wait` in a polling loop. Clippy
// can't prove the `Child` is reaped on every path because `first.wait()`
// is gated behind the `try_wait()` poll; we DO reap (the poll asserts
// `reaped`), but the lint flags every spawn site that isn't a literal
// `child.wait().unwrap()` immediately after spawn.
#![allow(clippy::zombie_processes)]

#[allow(dead_code)]
mod common;

use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

use common::{collect_pty_data_until, test_fake_bin};

/// Spawn `codemuxd` in foreground mode at `socket`, with `Stdio::null()`
/// for the three standard fds. Returns the running `Child`.
///
/// Mirrors `common::spawn_codemuxd` but takes an explicit socket path so
/// the second spawn can re-use the first spawn's tempdir. The harness's
/// own spawn helper takes ownership of a fresh tempdir per call, which
/// would force this test to track two separate socket paths and defeat
/// the "fresh daemon binds the SAME socket the stale one held" half of
/// the AC.
fn spawn_codemuxd_at(socket: &Path) -> std::process::Child {
    let codemuxd_bin = env!("CARGO_BIN_EXE_codemuxd");
    let fake_bin = test_fake_bin("fake_daemon_agent");
    let socket_str = socket.to_str().expect("socket path is utf-8");

    let mut cmd = Command::new(codemuxd_bin);
    cmd.arg("--socket")
        .arg(socket_str)
        .arg("--foreground")
        .arg("--")
        .arg(&fake_bin)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd.spawn().expect("spawn codemuxd subprocess")
}

/// Wait up to `timeout` for a connect against `socket` to succeed.
/// Returns the established `UnixStream` so the caller can immediately
/// drive the handshake on it.
///
/// We probe with `connect` (not `path.exists()`) because the redeploy
/// test races two spawns onto the same socket path: between the first
/// daemon's Drop unlinking the socket and the second daemon's `bind`
/// recreating it, file-existence is unreliable. A successful `connect`
/// is the only signal that proves the kernel-side listener is ready
/// AND belongs to the current daemon — see the AC-044 body for the
/// stale-listener regression class this defends against.
fn wait_for_connect(socket: &Path, timeout: Duration) -> UnixStream {
    let deadline = Instant::now() + timeout;
    loop {
        match UnixStream::connect(socket) {
            Ok(s) => return s,
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => panic!(
                "could not connect to {} within {timeout:?}: {e}",
                socket.display(),
            ),
        }
    }
}

#[test]
#[ignore = "slow-tier daemon E2E; runs via `just check-e2e` / `just test-e2e`"]
fn killed_daemon_releases_socket_and_fresh_daemon_serves_new_session() {
    let dir = TempDir::new().expect("create tempdir for redeploy test");
    let socket = dir.path().join("redeploy.sock");

    // First daemon: come up, take a client through the handshake, see
    // the fake's boot prompt as `PtyData`. The `HelloAck`'s
    // `daemon_pid` is our handle on which daemon we're talking to.
    let mut first = spawn_codemuxd_at(&socket);
    let stream = wait_for_connect(&socket, Duration::from_secs(5));
    let mut client = common::WireClient::handshake(stream, 24, 80, "redeploy-agent");
    let first_daemon_pid = client.daemon_pid();
    assert_ne!(
        first_daemon_pid, 0,
        "first daemon must report a non-zero pid in HelloAck",
    );

    // Make sure the first daemon's screen has something on it before
    // we kill — exercises the same code path that the in-flight Claude
    // session would. We don't assert on the snapshot here; AC-027 owns
    // that. We just need the daemon's parser to be non-empty so the
    // teardown actually traverses the live-session path.
    let _ = collect_pty_data_until(
        &mut client,
        |bytes| {
            bytes
                .windows(b"FAKE_AGENT_READY".len())
                .any(|w| w == b"FAKE_AGENT_READY")
        },
        Duration::from_secs(2),
    );
    drop(client);

    // SIGTERM mirrors the bootstrap's kill prelude: give the daemon's
    // Drop cleanup (kill child PTY, remove pid file, unlink socket)
    // a chance to run. `Child::kill` on Linux sends SIGKILL, which the
    // bootstrap's prelude falls back to after SIGTERM; for the wire-
    // level assertion, either signal is sufficient — what we care
    // about is that the socket is gone and the next bind succeeds.
    //
    // We use `Command::new("kill")` rather than `Child::kill` because
    // SIGTERM is what the production redeploy path sends first, and we
    // want the daemon's Drop cleanup to actually run so the socket gets
    // unlinked (which is what proves "stale socket reaped"). `Child::kill`
    // would skip that path.
    let term_status = Command::new("kill")
        .arg("-TERM")
        .arg(first.id().to_string())
        .status()
        .expect("invoke `kill -TERM`");
    assert!(
        term_status.success(),
        "`kill -TERM` against the first daemon must succeed; got {term_status:?}",
    );

    // Wait for the first daemon to reap. `wait` would block; bounded
    // poll via `try_wait` keeps the teardown deterministic.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut reaped = false;
    while Instant::now() < deadline {
        if matches!(first.try_wait(), Ok(Some(_))) {
            reaped = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        reaped,
        "first daemon should exit within 5s of SIGTERM (Drop cleanup time)",
    );

    // The socket may be gone (daemon's Drop unlinked it) or still
    // present (kernel hadn't released the inode yet, or Drop's
    // `remove_file` raced the kill). Either way, the daemon-side
    // `bring_up` reaps a stale path before binding. We don't probe
    // for an intermediate "connect must fail" state because that's a
    // race against the OS releasing the listener — `wait_for_connect`
    // against the second daemon is what proves the new listener is
    // distinct.

    // Second daemon: spawn against the SAME socket path. This pins the
    // daemon-side `reap_stale_socket` + `bind` sequence: a leftover
    // socket file (if one survived the first daemon's Drop race) does
    // not block the new bind. If `reap_stale_socket` regressed, this
    // spawn would die with `EADDRINUSE`.
    let mut second = spawn_codemuxd_at(&socket);
    let stream2 = wait_for_connect(&socket, Duration::from_secs(5));
    let mut client2 = common::WireClient::handshake(stream2, 24, 80, "redeploy-agent");
    let second_daemon_pid = client2.daemon_pid();

    // The second `HelloAck` reports a different daemon pid — proof
    // we're talking to a fresh process, not an accidental reattach to
    // a survivor of the SIGTERM.
    assert_ne!(
        first_daemon_pid, second_daemon_pid,
        "second HelloAck must come from a different daemon process; \
         got first={first_daemon_pid}, second={second_daemon_pid}",
    );

    // And the second daemon's child is a fresh fake agent: its boot
    // prompt arrives over the wire just like a cold-start would.
    // Without a fresh spawn (e.g. a regression where the supervisor
    // re-attached us to a zombie's old PTY), there would be no boot
    // prompt to find.
    let payload = collect_pty_data_until(
        &mut client2,
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
        "second daemon should re-spawn the fake agent; got {} bytes: {:?}",
        payload.len(),
        String::from_utf8_lossy(&payload),
    );

    // Tidy up.
    drop(client2);
    second.kill().ok();
    second.wait().ok();
}
