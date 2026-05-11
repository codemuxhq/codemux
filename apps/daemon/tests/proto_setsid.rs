//! AC-043: Daemon survives SSH disconnect via `setsid -f`.
//!
//! Split coverage, as the AC body and the matrix preamble both flag:
//!
//! - The bootstrap side (`crates/codemuxd-bootstrap`) already pins the
//!   load-bearing stdio redirect that lets `setsid -f` actually detach
//!   from the SSH session's pipes. Without it, ssh hangs after
//!   disconnect.
//! - The real failure mode ("kill the SSH `ControlMaster`, observe
//!   daemon survives") needs a real sshd and stays manual.
//! - **This file** pins the testable middle: when `codemuxd` is
//!   launched under `setsid` (the way the bootstrap does), the running
//!   daemon ends up as a session leader. It does NOT inherit the
//!   spawning process's session id and it does NOT re-parent itself
//!   onto its caller's pgroup. If a future refactor of the daemon
//!   binary accidentally re-acquired a controlling terminal (e.g. an
//!   `init_tracing` rewrite that opens `/dev/tty`), this test fails;
//!   without it, that regression would only surface on a real SSH
//!   disconnect.
//!
//! Read `getsid(2)` via `/proc/<pid>/stat` — field 6 of that file is
//! the kernel-managed session id (`sid`). The daemon-side test
//! infrastructure forbids `unsafe` (workspace lint), so calling
//! `libc::getsid` is off the table; `/proc` is the supported
//! diagnostic surface on every Linux the daemon supports.

#![cfg(feature = "test-fakes")]
#![cfg(target_os = "linux")]

#[allow(dead_code)]
mod common;

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// Read the session id of `pid` from `/proc/<pid>/stat`. Field 6 (1-
/// indexed) of `stat` is `session`, the kernel-managed session id —
/// equal to `getsid(pid)`.
///
/// The `comm` field (field 2) can contain spaces and parentheses, so
/// naive whitespace splitting would mis-index everything after it. We
/// strip everything up to the last `)` first, then split on whitespace
/// — the standard idiom for parsing `/proc/<pid>/stat`.
///
/// Returns `None` if the pid is gone, the file is unreadable, or the
/// stat line is malformed.
fn session_id(pid: u32) -> Option<i64> {
    let raw = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let tail = raw.rsplit_once(')')?.1.trim_start();
    let fields: Vec<&str> = tail.split_whitespace().collect();
    // After `comm`, fields are: state ppid pgrp session ... — so
    // index 3 (0-indexed) is `session`.
    fields.get(3)?.parse::<i64>().ok()
}

#[test]
#[ignore = "slow-tier daemon E2E; runs via `just check-e2e` / `just test-e2e`"]
// `daemon_pid` and `daemon_sid` are distinct concepts the assert
// explicitly relates (sid == pid is the load-bearing equality); naming
// them apart would obscure that relation.
#[allow(clippy::similar_names)]
fn daemon_spawned_under_setsid_becomes_its_own_session_leader() {
    // Skip cleanly if `setsid` is missing from the host (containerised
    // CI on a stripped-down base image, for instance). The AC's actual
    // production path runs on a remote host where `setsid` is part of
    // util-linux; CI absence is an environment limitation, not a
    // failure of the daemon contract.
    if Command::new("which")
        .arg("setsid")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_or(true, |s| !s.success())
    {
        eprintln!("skipping: setsid not on PATH");
        return;
    }

    let dir = TempDir::new().expect("create tempdir for daemon socket");
    let socket = dir.path().join("setsid.sock");
    let socket_str = socket.to_str().expect("socket path is utf-8");

    // `setsid` (no `-f`) execs the command as a new session leader and
    // waits. With `-f`, it `fork()`s first and the parent exits
    // immediately, which is what the bootstrap uses on the remote. For
    // a local test we want a handle on the spawned daemon's pid; using
    // bare `setsid` with `Stdio::null()` for the three standard fds
    // mirrors what the remote shell ends up with after the bootstrap's
    // `</dev/null >...stderr 2>&1` redirects.
    let codemuxd_bin = env!("CARGO_BIN_EXE_codemuxd");
    let fake_bin = env!("CARGO_BIN_EXE_fake_daemon_agent");
    let mut cmd = Command::new("setsid");
    cmd.arg(codemuxd_bin)
        .arg("--socket")
        .arg(socket_str)
        .arg("--foreground")
        .arg("--")
        .arg(fake_bin)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().expect("spawn setsid codemuxd");
    let daemon_pid = child.id();

    // Bounded poll for the socket so we know the daemon actually came
    // up and didn't immediately exit (which would still produce a child
    // pid but no SID we can sensibly query).
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        socket.exists(),
        "daemon under setsid did not bind its socket within 5s",
    );

    // Read SIDs BEFORE killing the child. After `wait`, `/proc/<pid>`
    // disappears and `session_id` returns None.
    let daemon_sid =
        session_id(daemon_pid).unwrap_or_else(|| panic!("could not read /proc/{daemon_pid}/stat"));
    let test_sid = session_id(std::process::id())
        .expect("could not read /proc/self/stat for the test process");

    // Tear down cleanly before any assertion so a failed assert doesn't
    // leak a daemon.
    child.kill().ok();
    child.wait().ok();

    // The daemon is its own session leader: its SID equals its PID.
    // This is the defining property of a `setsid`-spawned process and
    // the only durable signal a future refactor would have to break to
    // re-introduce the AC-043 regression class.
    assert_eq!(
        daemon_sid,
        i64::from(daemon_pid),
        "daemon spawned under setsid should be a session leader \
         (sid == pid); got sid={daemon_sid}, pid={daemon_pid}",
    );

    // And the test process's SID is different — `setsid` actually
    // detached. If a future change to the daemon's startup grabbed a
    // controlling terminal (e.g. opening `/dev/tty`), the kernel would
    // restore the test's SID; this assert catches that.
    assert_ne!(
        daemon_sid, test_sid,
        "daemon's SID must differ from the test process's SID \
         (test_sid={test_sid}, daemon_sid={daemon_sid}). The daemon \
         did not detach from the spawning session.",
    );
}
