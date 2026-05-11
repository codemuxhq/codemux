//! AC-043 (manual half, now hermetic): daemon survives SSH disconnect
//! via `setsid -f`.
//!
//! The sibling `proto_setsid.rs` already pins the testable half from
//! the daemon side: a `codemuxd` launched under `setsid` becomes its
//! own session leader (`sid == pid`). That assertion alone does not
//! prove the production gesture works end-to-end — it leaves open
//! whether the SSH transport tearing down actually preserves the
//! detached daemon. AC-043's `**Failure modes:**` block (and the
//! e2e plan's matrix preamble) flagged this as "needs a real sshd"
//! and stayed manual until now.
//!
//! This test bridges the gap. It spawns a real `sshd` subprocess via
//! the `common::ssh` harness, opens a long-lived SSH `ControlMaster`
//! against it, launches `codemuxd` over that master under
//! `setsid -f` (the exact gesture `codemuxd-bootstrap` uses), kills
//! the master with `ssh -O exit`, and verifies via `kill -0 <pid>`
//! that the daemon process is still alive on the host. Final
//! belt-and-suspenders: open a fresh ssh session and re-issue
//! `kill -0` so the assertion is not relying on the test process's
//! own connection state.
//!
//! `kill -0` is a sound liveness probe: signal `0` skips delivery
//! entirely and only runs the permission + pid-exists checks
//! (`kill(2)` man page). It is the canonical idiom for "is this
//! process alive without disturbing it" and is what the daemon
//! itself uses to detect stale pid files (see
//! `apps/daemon/src/bootstrap.rs::PidFile::acquire`).
//!
//! Gating mirrors the other slow-tier proto tests:
//! - `#![cfg(feature = "test-fakes")]` — the harness references
//!   `CARGO_BIN_EXE_fake_daemon_agent`, only materialized when the
//!   feature is on.
//! - `#[ignore]` — slow-tier, runs via `just check-e2e`.
//! - Linux-only: the harness reads `/proc/<pid>` indirectly via
//!   `kill -0` (which works on every Unix), but the production
//!   gesture this AC pins is Linux's `setsid` semantics. macOS / BSD
//!   would also pass on the survival assertion, but the bootstrap's
//!   primary target is Linux and we keep the gate matching
//!   `proto_setsid.rs` for consistency.

#![cfg(feature = "test-fakes")]
#![cfg(target_os = "linux")]
// Free-floating helpers in this integration test crate fall outside
// the `clippy.toml` `allow-unwrap-in-tests` / `allow-expect-in-tests`
// carve-out (which only covers `#[test]` / `#[cfg(test)]` scopes).
// Same file-scope allow as `proto_redeploy.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[allow(dead_code)]
mod common;

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

use common::ssh::{SshTestHost, spawn_sshd};

/// How long to wait for the pid-file to appear after the
/// `setsid -f codemuxd` ssh command returns. The production bootstrap
/// budgets 5 s for the same poll; we mirror it.
const PID_FILE_READY_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to wait for sshd to flush the `ControlMaster`'s
/// `-O exit` request. The master is in-process on a Unix domain socket
/// shared with our `ssh -O exit` client, so this is fork/exec latency
/// only.
const CONTROL_MASTER_TEARDOWN_TIMEOUT: Duration = Duration::from_secs(2);

#[test]
#[ignore = "slow-tier daemon E2E; runs via `just check-e2e` / `just test-e2e`"]
fn daemon_survives_ssh_controlmaster_kill() {
    let sshd = spawn_sshd(&[]);

    // Per-test scratch for the daemon's socket / pid / log paths.
    // Putting them under a tempdir keeps the test hermetic — the
    // daemon never writes to the developer's real `~/.cache/codemuxd/`,
    // and Drop unlinks every artifact.
    let scratch = TempDir::new().expect("tempdir for daemon artifacts");
    let socket_path = scratch.path().join("daemon.sock");
    let pid_path = scratch.path().join("daemon.pid");
    let log_path = scratch.path().join("daemon.log");

    // Per-test `ControlMaster` socket lives in its own tempdir so its
    // path stays short — OpenSSH refuses control sockets whose
    // `sockaddr_un` exceeds `sizeof(sun_path)` (108 chars on Linux),
    // and nesting under a long workspace path can blow past that.
    // A bare `tempfile::TempDir` lands under `/tmp/.tmpXXXXXX/`,
    // comfortably under the cap.
    let ctl_dir = TempDir::new().expect("tempdir for control-master socket");
    let ctl_socket = ctl_dir.path().join("ctl.sock");

    open_control_master(&sshd, &ctl_socket);
    launch_daemon_under_setsid(&sshd, &ctl_socket, &socket_path, &pid_path, &log_path);

    // `setsid -f` returns immediately; the actual codemuxd startup
    // (bind socket, write pid) happens asynchronously after the fork.
    wait_for_path(&pid_path, PID_FILE_READY_TIMEOUT);
    let daemon_pid = read_pid(&pid_path);
    assert!(
        process_is_alive(daemon_pid),
        "daemon was not alive right after setsid -f (pid={daemon_pid}). \
         log tail: {tail}",
        tail = tail_log(&log_path),
    );

    close_control_master(&sshd, &ctl_socket);

    // The load-bearing assertion. If the bootstrap's `setsid -f`
    // gesture works, the daemon's process group has been detached
    // from the SSH session's; killing the SSH client should not
    // propagate to the daemon. `kill -0` runs the kernel's
    // permission + pid-exists check without delivering a signal.
    assert!(
        process_is_alive(daemon_pid),
        "AC-043 regressed: daemon (pid={daemon_pid}) died when the SSH ControlMaster \
         was killed. log tail: {tail}",
        tail = tail_log(&log_path),
    );

    // Belt-and-suspenders: open a brand-new SSH connection and
    // re-run `kill -0` from inside the remote shell. This rules out
    // a scenario where the test-local `kill(2)` saw the daemon as
    // alive only because nothing has reaped its proc entry yet from
    // the kernel's perspective. A fresh ssh-and-shell round trip
    // forces a real syscall on the host.
    assert_pid_alive_via_fresh_ssh(&sshd, daemon_pid);

    // Teardown: the daemon is now an orphan reparented to PID 1
    // (which is the whole point of AC-043). Drop won't reach it.
    // Kill it ourselves so we don't leak a process per test run.
    reap_orphaned_daemon(daemon_pid);
}

/// Open an SSH `ControlMaster` against `sshd`. `-M` marks this
/// connection as the master; `-S <path>` is the socket siblings
/// (and `ssh -O exit`) attach to; `-fNT` forks into the background
/// after authentication, opens no terminal, and runs no remote
/// command — the connection is a pure mux carrier.
fn open_control_master(sshd: &SshTestHost, ctl_socket: &Path) {
    let status = sshd
        .ssh_command()
        .arg("-M")
        .arg("-S")
        .arg(ctl_socket)
        .arg("-f")
        .arg("-N")
        .arg("-T")
        // The master must own its own ControlMaster directive
        // regardless of what the system ssh_config says.
        .arg("-o")
        .arg("ControlMaster=yes")
        .arg("-o")
        .arg("ControlPersist=no")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn ssh ControlMaster");
    assert!(
        status.success(),
        "ssh ControlMaster exited non-zero: {status:?}"
    );
    wait_for_path(ctl_socket, Duration::from_secs(3));
}

/// Launch `codemuxd` through the open `ControlMaster` under `setsid -f`.
/// The trailing argv mirrors the production bootstrap shape almost
/// verbatim (see `spawn_remote_daemon` in
/// `crates/codemuxd-bootstrap/src/lib.rs`): same redirect of stdin to
/// /dev/null and merge of stdout+stderr into a single file, same
/// `setsid -f` invocation. The differences are `--foreground` (test
/// scratch avoids the real $HOME / tracing overhead) and the trailing
/// `-- <fake>` (we run a deterministic in-tree fake instead of
/// `claude`).
fn launch_daemon_under_setsid(
    sshd: &SshTestHost,
    ctl_socket: &Path,
    socket_path: &Path,
    pid_path: &Path,
    log_path: &Path,
) {
    let codemuxd_bin = env!("CARGO_BIN_EXE_codemuxd");
    let fake_bin = env!("CARGO_BIN_EXE_fake_daemon_agent");
    let remote_cmd = format!(
        "setsid -f {codemuxd_bin} \
         --foreground \
         --socket {socket} \
         --pid-file {pid} \
         --log-file {log} \
         --agent-id ac043 \
         -- {fake_bin} \
         </dev/null >{log}.stderr 2>&1",
        codemuxd_bin = shell_quote(codemuxd_bin),
        fake_bin = shell_quote(fake_bin),
        socket = shell_quote_path(socket_path),
        pid = shell_quote_path(pid_path),
        log = shell_quote_path(log_path),
    );
    let out = ssh_via_master(sshd, ctl_socket)
        .arg(&remote_cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("ssh through ControlMaster to setsid -f the daemon");
    assert!(
        out.status.success(),
        "remote setsid -f returned non-zero ({status:?}). stderr={stderr}",
        status = out.status,
        stderr = String::from_utf8_lossy(&out.stderr),
    );
}

/// Kill the `ControlMaster` with `ssh -O exit`. The master receives
/// the request over its own control socket, closes its TCP connection
/// to sshd, and exits. From sshd's perspective this is exactly the
/// same as a normal client disconnect — which is the SSH transport
/// failure mode AC-043 cares about.
fn close_control_master(sshd: &SshTestHost, ctl_socket: &Path) {
    let status = sshd
        .ssh_command()
        .arg("-S")
        .arg(ctl_socket)
        .arg("-O")
        .arg("exit")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("spawn ssh -O exit");
    assert!(
        status.success(),
        "ssh -O exit returned non-zero: {status:?}"
    );
    wait_for_path_gone(ctl_socket, CONTROL_MASTER_TEARDOWN_TIMEOUT);
}

/// Open a fresh ssh connection and run `kill -0 <pid>` on the remote
/// side. Asserts the command exits 0. Used as a sanity check that the
/// daemon survived the `ControlMaster` exit even from the perspective
/// of a brand-new SSH session.
fn assert_pid_alive_via_fresh_ssh(sshd: &SshTestHost, daemon_pid: u32) {
    let out = sshd
        .ssh_command()
        .arg(format!("kill -0 {daemon_pid}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn fresh-ssh kill -0");
    assert!(
        out.status.success(),
        "fresh-ssh `kill -0 {daemon_pid}` failed after ControlMaster exit \
         (status={status:?}, stderr={stderr}). The daemon's pid is gone — \
         either it died with the SSH session (AC-043 regression) or its pid \
         was never written correctly.",
        status = out.status,
        stderr = String::from_utf8_lossy(&out.stderr),
    );
}

/// Send `SIGTERM` to `daemon_pid`, wait briefly, then `SIGKILL` if
/// still alive. Mirrors the bootstrap's `sleep 1; SIGKILL` fallback.
fn reap_orphaned_daemon(daemon_pid: u32) {
    let _ = Command::new("/usr/bin/kill")
        .arg("-TERM")
        .arg(daemon_pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let deadline = Instant::now() + Duration::from_secs(1);
    while process_is_alive(daemon_pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if process_is_alive(daemon_pid) {
        let _ = Command::new("/usr/bin/kill")
            .arg("-KILL")
            .arg(daemon_pid.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Build an `ssh` command targeting the test sshd via an already-open
/// `ControlMaster` socket. The `-S <ctl_socket>` option tells the local
/// `ssh` to attach to the existing master instead of opening a fresh
/// connection.
fn ssh_via_master(sshd: &SshTestHost, ctl_socket: &Path) -> Command {
    let mut cmd = sshd.ssh_command();
    cmd.arg("-S").arg(ctl_socket);
    cmd
}

/// Block until `path` exists, panic on timeout. The pid file appears
/// just before the daemon's accept loop is ready, so its existence is
/// a sound liveness gate.
fn wait_for_path(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!(
        "expected path {} to exist within {timeout:?}",
        path.display(),
    );
}

/// Block until `path` no longer exists. Used after `ssh -O exit` to
/// gate on the `ControlMaster` actually tearing down — without this we
/// race the master's socket-unlink against the survival assertion.
fn wait_for_path_gone(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!(
        "expected path {} to disappear within {timeout:?}",
        path.display(),
    );
}

/// Read the pid the daemon wrote to `path`. The daemon writes a
/// trailing newline; `trim` handles it.
fn read_pid(path: &Path) -> u32 {
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read pid file {}: {e}", path.display()));
    raw.trim().parse::<u32>().unwrap_or_else(|e| {
        panic!(
            "pid file {} did not contain a u32 (got {raw:?}): {e}",
            path.display(),
        )
    })
}

/// Returns true if `pid` is a live (non-zombie) process accessible from
/// this test.
///
/// First gate: `kill -0` via the binary (the workspace's
/// `unsafe_code = "forbid"` lint blocks `libc::kill`). That alone is
/// insufficient — `kill -0` succeeds on zombies (`Z` state) because
/// the pid still occupies a slot in the kernel process table until
/// the parent (or `init` after reparenting) calls `waitpid`. NLM
/// flagged the race: AC-043's "daemon survives" assertion would
/// silently pass if the daemon crashed under `setsid -f` and `init`
/// hadn't yet reaped the zombie. The fix: also read
/// `/proc/<pid>/stat` field 3 (process state) and reject `Z` (Zombie)
/// and `X` (Dead). Same idiom `proto_setsid.rs` already uses for its
/// SID check.
fn process_is_alive(pid: u32) -> bool {
    let kill_ok = Command::new("/usr/bin/kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    kill_ok && !proc_state_is_dead(pid)
}

/// Read `/proc/<pid>/stat` field 3 and return true if the process is
/// in `Z` (Zombie) or `X` (Dead) state. The `comm` field (field 2)
/// can contain spaces and parentheses, so we strip up to the last
/// `)` before splitting on whitespace — the standard idiom for
/// parsing `/proc/<pid>/stat` (mirrors `proto_setsid.rs::session_id`).
///
/// Returns `false` (i.e. "treat as alive") if `/proc/<pid>/stat` is
/// unreadable, missing, or malformed. The `kill_ok` gate in the
/// caller already handled "pid does not exist"; this helper's only
/// job is the zombie/dead refinement.
fn proc_state_is_dead(pid: u32) -> bool {
    let Ok(raw) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some((_, tail)) = raw.rsplit_once(')') else {
        return false;
    };
    let mut fields = tail.split_whitespace();
    // First field after `comm` is `state` (one of R/S/D/Z/T/X/...).
    matches!(fields.next(), Some("Z" | "X"))
}

/// Concatenate the daemon's `.log` and `.log.stderr` tails for panic
/// messages. Empty files render as `(empty)` so the panic stays
/// readable; missing files render as `(missing)`. Truncates each
/// file's contents to 4 KiB so a runaway log can't overwhelm the
/// failure output.
fn tail_log(log_path: &Path) -> String {
    let stderr_path = format!("{}.stderr", log_path.display());
    let main = read_truncated(log_path, 4096);
    let err = read_truncated(Path::new(&stderr_path), 4096);
    format!(
        "\n--- {log_path} ---\n{main}\n--- {stderr_path} ---\n{err}\n",
        log_path = log_path.display(),
    )
}

fn read_truncated(path: &Path, cap: usize) -> String {
    match std::fs::read_to_string(path) {
        Ok(s) if s.is_empty() => "(empty)".into(),
        Ok(s) if s.len() <= cap => s,
        Ok(s) => format!("...{}", &s[s.len() - cap..]),
        Err(_) => "(missing)".into(),
    }
}

/// Single-quote a string for inclusion in a POSIX shell command. The
/// shell expands `\\\\\\\\'\\\\\\\\` inside a single-quoted run as a literal `'`, then
/// re-opens the quoting. We refuse paths that already contain a `'` —
/// the test harness writes every path so this is a programmer-error
/// check, not a sanitization seam.
fn shell_quote(s: &str) -> String {
    assert!(
        !s.contains('\''),
        "shell_quote: refusing to escape a string containing a single quote: {s:?}",
    );
    format!("'{s}'")
}

fn shell_quote_path(path: &Path) -> String {
    shell_quote(
        path.to_str()
            .unwrap_or_else(|| panic!("path {} not utf-8", path.display())),
    )
}
