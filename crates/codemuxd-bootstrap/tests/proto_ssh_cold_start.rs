//! AC-003 (SSH transport leg): cold-start bootstrap, end-to-end.
//!
//! `apps/daemon/tests/proto_smoke.rs` already pins the daemon-side
//! post-handshake contract (handshake completes, fake-agent prompt
//! arrives as `PtyData`) and the bootstrap's orchestration is
//! unit-tested in this crate's `src/lib.rs`. This file bridges the
//! two: drive the real `prepare_remote` + `attach_agent` flow through
//! a real `sshd` via the shared `codemux_test_ssh_harness` crate, with
//! `cargo` shimmed to a pre-built binary copy and `claude` symlinked
//! to the in-tree fake agent so the cold path stays hermetic and
//! fast.
//!
//! Two scenarios:
//! - **cold start**: fresh remote `$HOME`; all 7 stages fire in order;
//!   handshake completes; `agent.version` lands on disk.
//! - **warm start**: second invocation against the same
//!   `~/.cache/codemuxd/`; only `VersionProbe` fires from
//!   `prepare_remote` (`TarballStage` / `Scp` / `RemoteBuild` are
//!   skipped); `attach_agent` still walks `DaemonSpawn` →
//!   `SocketTunnel` → `SocketConnect`; handshake completes.
//!
//! Three indirections keep the test hermetic without touching
//! production code:
//! - **`ShimmingRunner`**: a thin `CommandRunner` wrapper around
//!   `RealRunner` that translates `"ssh"` / `"scp"` to absolute paths
//!   under `SshTestHost::bin_dir()`. No `std::env::set_var` mutation —
//!   safe to run alongside other proto tests in parallel.
//! - **`cargo` shim**: a small shell script dropped into `bin_dir`.
//!   Recognizes the bootstrap's exact build invocation
//!   (`build --release --bin codemuxd`, see `remote_build` in
//!   `src/lib.rs`) and copies the pre-built `codemuxd` binary into
//!   where cargo would have placed it. Any other invocation is a
//!   quiet exit-0.
//! - **`claude` symlink**: `bin_dir/claude` → `fake_daemon_agent`.
//!   The bootstrap-spawned daemon defaults to `("claude", [])` when
//!   no trailing argv is given (`apps/daemon/src/cli.rs::child_command`)
//!   and uses `CommandBuilder::new("claude")`
//!   (`apps/daemon/src/pty.rs::spawn`), which performs PATH lookup
//!   against the daemon's process environment. The SSH session's PATH
//!   carries through `setsid -f` into the daemon, so the symlink
//!   resolves cleanly.
//!
//! ## Binary discovery
//!
//! `env!("CARGO_BIN_EXE_<name>")` only resolves to a binary in the
//! same package, and the binaries we need (`codemuxd`,
//! `fake_daemon_agent`) live in `apps/daemon`. So we resolve them at
//! test-time by walking from `CARGO_MANIFEST_DIR` to the workspace
//! target dir and constructing `<target>/<profile>/<bin>`. The
//! binaries are produced as a side effect of
//! `cargo test --workspace --features codemuxd/test-fakes,...`,
//! which is what `just check-e2e` runs. Standalone
//! `cargo test -p codemuxd-bootstrap -- --ignored` will fail loud
//! with a "binary not found" message pointing at the exact path; the
//! fix is the workspace-features invocation above.
//!
//! Linux-only for the same reason as
//! `apps/daemon/tests/proto_ssh_disconnect.rs`: macOS `pam_env`
//! semantics differ enough that `HOME` env-injection is not
//! guaranteed to take effect, and `/proc/<pid>/stat` is Linux-only.

#![cfg(target_os = "linux")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::cell::RefCell;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant};

use codemux_test_ssh_harness::{SshTestHost, spawn_sshd};
use codemuxd_bootstrap::{
    AttachConfig, CommandOutput, CommandRunner, RealRunner, Stage, attach_agent, bootstrap_version,
    prepare_remote,
};
use tempfile::TempDir;

/// Hermetic PATH for the SSH session. The harness prepends its own
/// `bin_dir` to this so the `cargo` and `claude` shims dropped by
/// [`install_shims`] are reachable from the bootstrap-spawned daemon.
const HERMETIC_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

#[test]
#[ignore = "slow-tier daemon E2E; runs via `just check-e2e` / `just test-e2e`"]
fn cold_start_walks_every_stage_through_real_sshd_and_handshake_completes() {
    let scratch_home = TempDir::new().expect("scratch HOME tempdir");
    let local_socket_dir = TempDir::new().expect("local-socket-dir tempdir");

    let codemuxd_bin = workspace_target_bin("codemuxd");
    let fake_bin = workspace_target_bin("fake_daemon_agent");
    let scratch_home_str = scratch_home
        .path()
        .to_str()
        .expect("scratch HOME path is utf-8");
    let codemuxd_bin_str = codemuxd_bin
        .to_str()
        .expect("codemuxd binary path is utf-8");
    let fake_bin_str = fake_bin.to_str().expect("fake_daemon_agent path is utf-8");

    let sshd = spawn_sshd(&[
        ("HOME", scratch_home_str),
        ("PATH", HERMETIC_PATH),
        ("CODEMUX_TEST_PREBUILT_DAEMON", codemuxd_bin_str),
    ]);
    install_shims(&sshd, fake_bin_str);
    assert_remote_home_overridden(&sshd, scratch_home.path());

    let stages: Rc<RefCell<Vec<Stage>>> = Rc::default();
    let cb = {
        let stages = Rc::clone(&stages);
        move |s: Stage| stages.borrow_mut().push(s)
    };

    let runner = ShimmingRunner::new(sshd.bin_dir());
    let prepared =
        prepare_remote(&runner, &cb, sshd.host_alias()).expect("prepare_remote on cold remote");
    assert!(
        prepared.binary_was_updated,
        "cold start must report binary_was_updated=true (no agent.version on disk yet)",
    );
    assert_eq!(
        prepared.remote_home,
        scratch_home.path(),
        "prepared.remote_home must reflect the HOME we injected",
    );

    let cfg = AttachConfig {
        host: sshd.host_alias().to_string(),
        agent_id: "ac003-cold".into(),
        cwd: None,
        local_socket_dir: local_socket_dir.path().to_path_buf(),
        rows: 24,
        cols: 80,
        session_id: String::new(),
        resume_session_id: None,
    };
    let transport =
        attach_agent(&runner, &cb, &prepared, &cfg).expect("attach_agent over real sshd");

    assert_eq!(
        stages.borrow().clone(),
        vec![
            Stage::VersionProbe,
            Stage::TarballStage,
            Stage::Scp,
            Stage::RemoteBuild,
            Stage::DaemonSpawn,
            Stage::SocketTunnel,
            Stage::SocketConnect,
        ],
        "cold start must fire all 7 stages in order",
    );
    assert_remote_agent_version_matches(scratch_home.path(), bootstrap_version());

    drop(transport);
    reap_daemon_pid(scratch_home.path(), "ac003-cold");
}

#[test]
#[ignore = "slow-tier daemon E2E; runs via `just check-e2e` / `just test-e2e`"]
fn warm_start_skips_install_stages_when_remote_version_marker_matches() {
    let scratch_home = TempDir::new().expect("scratch HOME tempdir");
    let local_socket_dir = TempDir::new().expect("local-socket-dir tempdir");

    let codemuxd_bin = workspace_target_bin("codemuxd");
    let fake_bin = workspace_target_bin("fake_daemon_agent");
    let scratch_home_str = scratch_home
        .path()
        .to_str()
        .expect("scratch HOME path is utf-8");
    let codemuxd_bin_str = codemuxd_bin
        .to_str()
        .expect("codemuxd binary path is utf-8");
    let fake_bin_str = fake_bin.to_str().expect("fake_daemon_agent path is utf-8");

    let sshd = spawn_sshd(&[
        ("HOME", scratch_home_str),
        ("PATH", HERMETIC_PATH),
        ("CODEMUX_TEST_PREBUILT_DAEMON", codemuxd_bin_str),
    ]);
    install_shims(&sshd, fake_bin_str);
    let runner = ShimmingRunner::new(sshd.bin_dir());

    // Cold first — stage capture is the cold-start test's job; here we
    // just need the install side-effects (agent.version on disk, daemon
    // binary copied into place) so the second prepare_remote sees a
    // version match.
    let prepared =
        prepare_remote(&runner, |_| {}, sshd.host_alias()).expect("prepare_remote on cold remote");
    let cfg = AttachConfig {
        host: sshd.host_alias().to_string(),
        agent_id: "ac003-warm".into(),
        cwd: None,
        local_socket_dir: local_socket_dir.path().to_path_buf(),
        rows: 24,
        cols: 80,
        session_id: String::new(),
        resume_session_id: None,
    };
    let first =
        attach_agent(&runner, |_| {}, &prepared, &cfg).expect("attach_agent on cold remote");
    drop(first);
    // The daemon survives the transport drop (`setsid -f`), so reap it
    // before the second attach so the warm-start assertion exercises a
    // fresh DaemonSpawn → SocketTunnel → SocketConnect rather than
    // attaching to the still-live first daemon.
    reap_daemon_pid(scratch_home.path(), "ac003-warm");

    let stages: Rc<RefCell<Vec<Stage>>> = Rc::default();
    let cb = {
        let stages = Rc::clone(&stages);
        move |s: Stage| stages.borrow_mut().push(s)
    };
    let prepared2 =
        prepare_remote(&runner, &cb, sshd.host_alias()).expect("prepare_remote on warm remote");
    assert!(
        !prepared2.binary_was_updated,
        "warm start must report binary_was_updated=false (agent.version matches)",
    );
    let second = attach_agent(&runner, &cb, &prepared2, &cfg).expect("attach_agent on warm remote");

    assert_eq!(
        stages.borrow().clone(),
        vec![
            Stage::VersionProbe,
            Stage::DaemonSpawn,
            Stage::SocketTunnel,
            Stage::SocketConnect,
        ],
        "warm start must skip TarballStage/Scp/RemoteBuild",
    );

    drop(second);
    reap_daemon_pid(scratch_home.path(), "ac003-warm");
}

/// Wraps the unit-struct [`RealRunner`] and translates the program
/// names `"ssh"` / `"scp"` into absolute paths under
/// [`SshTestHost::bin_dir`]. The bootstrap calls `Command::new("ssh")`
/// which would normally do PATH lookup against the test process's
/// env, and mutating that env globally would race with parallel
/// tests. The wrapper avoids the PATH mutation by translating program
/// names before they reach `Command::new`.
struct ShimmingRunner {
    bin_dir: PathBuf,
}

impl ShimmingRunner {
    fn new(bin_dir: &Path) -> Self {
        Self {
            bin_dir: bin_dir.to_path_buf(),
        }
    }

    fn resolve(&self, program: &str) -> String {
        match program {
            "ssh" | "scp" => self
                .bin_dir
                .join(program)
                .into_os_string()
                .into_string()
                .expect("bin_dir + program is utf-8"),
            other => other.to_string(),
        }
    }
}

impl CommandRunner for ShimmingRunner {
    fn run(&self, program: &str, args: &[&str]) -> std::io::Result<CommandOutput> {
        RealRunner.run(&self.resolve(program), args)
    }

    fn spawn_detached(&self, program: &str, args: &[&str]) -> std::io::Result<Child> {
        RealRunner.spawn_detached(&self.resolve(program), args)
    }
}

/// Drop the two test-only shims into `sshd.bin_dir()`:
/// - `claude` → symlink to the in-tree `fake_daemon_agent`. The
///   bootstrap-spawned daemon's CLI defaults to spawning `claude`, and
///   `portable_pty`'s `CommandBuilder::new("claude")` resolves via
///   PATH inside the daemon process — which inherits the SSH
///   session's PATH (with `bin_dir` prepended by `spawn_sshd`).
/// - `cargo` → shell script that recognizes the bootstrap's exact
///   `build --release --bin codemuxd` invocation and copies the
///   pre-built daemon binary into where cargo would have placed it,
///   so the bootstrap's subsequent `install -m 755 …` succeeds without
///   paying for a real cargo build.
fn install_shims(sshd: &SshTestHost, fake_bin: &str) {
    let bin = sshd.bin_dir();

    let claude = bin.join("claude");
    let _ = std::fs::remove_file(&claude);
    symlink(fake_bin, &claude).expect("symlink claude → fake_daemon_agent");

    let cargo = bin.join("cargo");
    std::fs::write(&cargo, CARGO_SHIM).expect("write cargo shim");
    std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755))
        .expect("chmod 0755 cargo shim");
}

const CARGO_SHIM: &str = r#"#!/bin/sh
# Test-only cargo shim for AC-003 cold-start E2E. The bootstrap's
# remote-build step (`remote_build` in src/lib.rs) runs
# `cargo build --release --bin codemuxd` from the unpacked source tree
# at $HOME/.cache/codemuxd/src/. We recognize that exact invocation
# and stage the pre-built binary at target/release/codemuxd so the
# subsequent `install -m 755 target/release/codemuxd ...` succeeds
# without paying for a real cargo build (~minutes from scratch). Any
# other invocation exits 0 silently.
set -eu
case "$*" in
  "build --release --bin codemuxd")
    mkdir -p target/release
    cp "$CODEMUX_TEST_PREBUILT_DAEMON" target/release/codemuxd
    chmod 0755 target/release/codemuxd
    exit 0
    ;;
  *)
    exit 0
    ;;
esac
"#;

/// Sanity check that the `HOME` env-injection actually took effect on
/// the SSH session. If `pam_env` or a shell rc file is overriding
/// HOME, the rest of the test would silently write to the developer's
/// real `~/.cache/codemuxd/` and produce confusing failures. Failing
/// fast here keeps the diagnostic close to the cause.
fn assert_remote_home_overridden(sshd: &SshTestHost, expected: &Path) {
    let out = sshd
        .ssh_command()
        .arg("printf '%s' \"$HOME\"")
        .stdin(Stdio::null())
        .output()
        .expect("ssh probe of remote $HOME");
    assert!(
        out.status.success(),
        "ssh probe of $HOME failed: status={:?} stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let got = String::from_utf8_lossy(&out.stdout);
    let got = got.trim();
    let want = expected.to_str().expect("expected HOME is utf-8");
    assert_eq!(
        got, want,
        "HOME env-injection did not take effect on the SSH session. \
         got={got:?} want={want:?}. The cold-start test depends on the SSH \
         session's HOME pointing at the per-test tempdir; investigate \
         pam_env or shell rc on this host before re-running.",
    );
}

/// After a successful `prepare_remote`, the bootstrap's remote-build
/// step writes `bootstrap_version()` to `agent.version` so subsequent
/// probes can short-circuit. Verify the file content matches.
fn assert_remote_agent_version_matches(home: &Path, expected: &str) {
    let path = home.join(".cache/codemuxd/agent.version");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read agent.version at {}: {e}", path.display()));
    assert_eq!(raw.trim(), expected);
}

/// Read the daemon's pid file (under the per-test scratch HOME) and
/// reap the orphaned process. The bootstrap launches the daemon under
/// `setsid -f` so it survives the SSH session — and the SSH transport
/// drop — by design. Drop on the test's transport handle kills the
/// tunnel, but the daemon stays alive until we explicitly SIGKILL it.
/// Skipping this step would leak one daemon per test run.
fn reap_daemon_pid(home: &Path, agent_id: &str) {
    let pid_path = home.join(format!(".cache/codemuxd/pids/{agent_id}.pid"));
    let Ok(raw) = std::fs::read_to_string(&pid_path) else {
        return;
    };
    let Ok(pid) = raw.trim().parse::<u32>() else {
        return;
    };
    reap_orphaned_daemon(pid);
}

/// Send `SIGTERM` to `pid`, wait briefly, then `SIGKILL` if still
/// alive. Mirrors `apps/daemon/tests/proto_ssh_disconnect.rs::reap_orphaned_daemon`.
fn reap_orphaned_daemon(pid: u32) {
    let _ = Command::new("/usr/bin/kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let deadline = Instant::now() + Duration::from_secs(1);
    while process_is_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if process_is_alive(pid) {
        let _ = Command::new("/usr/bin/kill")
            .arg("-KILL")
            .arg(pid.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Returns true if `pid` is a live (non-zombie) process. `kill -0`
/// alone is insufficient because it succeeds on zombies; we also
/// reject `Z` / `X` states from `/proc/<pid>/stat` field 3.
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
/// `)` before splitting on whitespace.
fn proc_state_is_dead(pid: u32) -> bool {
    let Ok(raw) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let Some((_, tail)) = raw.rsplit_once(')') else {
        return false;
    };
    let mut fields = tail.split_whitespace();
    matches!(fields.next(), Some("Z" | "X"))
}

/// Resolve `<workspace_target>/<profile>/<bin>`. `env!("CARGO_BIN_EXE_<name>")`
/// only resolves to a binary in the same package and these binaries
/// live in `apps/daemon`, not in this crate. The path-walk below uses
/// `CARGO_MANIFEST_DIR` (this crate's Cargo.toml dir, baked in at
/// compile time) plus `CARGO_TARGET_DIR` (if set) to find the workspace
/// target dir, then picks `debug` or `release` based on the test's
/// own build profile.
///
/// The binaries are produced as a side effect of
/// `cargo test --workspace --features codemuxd/test-fakes,...`
/// which is what `just check-e2e` runs. Standalone
/// `cargo test -p codemuxd-bootstrap -- --ignored` won't have built
/// the daemon binaries; the assertion below fails with a message
/// telling the developer how to fix it.
fn workspace_target_bin(bin: &str) -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .ancestors()
        .nth(2)
        .expect("walk crates/<crate> → workspace root");
    let target_dir = std::env::var_os("CARGO_TARGET_DIR")
        .map_or_else(|| workspace_root.join("target"), PathBuf::from);
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let path = target_dir.join(profile).join(bin);
    assert!(
        path.exists(),
        "binary `{bin}` not found at {}.\n\
         The cold-start test resolves daemon binaries by walking from \
         CARGO_MANIFEST_DIR to the workspace target dir; the binaries \
         are produced as a side effect of `cargo test --workspace --features \
         codemuxd/test-fakes,...`. Run `just check-e2e` (which sets \
         the right features at the workspace level) instead of \
         `cargo test -p codemuxd-bootstrap` directly.",
        path.display(),
    );
    path
}
