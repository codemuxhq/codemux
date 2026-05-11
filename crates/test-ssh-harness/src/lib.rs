//! Real-`sshd`-as-subprocess harness shared by the SSH-leg integration
//! tests across the workspace.
//!
//! Consumers:
//! - `apps/daemon/tests/proto_ssh_disconnect.rs` (AC-043, daemon survives
//!   SSH `ControlMaster` kill via `setsid -f`)
//! - `crates/codemuxd-bootstrap/tests/proto_ssh_cold_start.rs` (AC-003,
//!   real `prepare_remote` + `attach_agent` over real `sshd`)
//!
//! Lives in its own workspace crate so the dependency direction matches
//! production: the bootstrap crate's integration tests depend on this
//! harness, not on `apps/daemon`. Otherwise the natural placement
//! (`apps/daemon/tests/common/`) would force `crates/codemuxd-bootstrap`
//! to depend on `apps/daemon` for tests, inverting the production graph
//! (`apps/tui → codemuxd-bootstrap`, `crates/codemuxd-bootstrap →
//! crates/session`, `apps/daemon → crates/wire`).
//!
//! Requirements (probed at module load): `sshd`, `ssh-keygen`, `ssh` on
//! disk. `sshd` typically lives in `/usr/sbin/sshd` and is not on the
//! login-user `PATH` on Debian; we look for it explicitly there.
//!
//! ## Why real sshd
//!
//! The T3 e2e plan originally deferred this with a stub-first decision
//! (see `docs/plans/2026-05-10--e2e-testing.md` Open Decisions). The
//! reversal: a stub for `ssh`/`scp` would have to re-implement enough of
//! the channel mux + `ControlMaster` semantics for AC-043's
//! "kill the master, observe the daemon" gesture to exercise the actual
//! survival mechanism. Standing up a real `sshd` subprocess is
//! considerably less code than re-implementing those semantics, and
//! pins the production behaviour rather than a model of it.
//!
//! ## Wiring
//!
//! Each test that wants an SSH transport calls [`spawn_sshd`] and
//! receives an [`SshTestHost`]. The host owns:
//!
//! - A `sshd` subprocess listening on `127.0.0.1:<random-port>`.
//! - A tempdir holding the host key, the test user's keypair, the
//!   sshd config, and an `authorized_keys` file pinned to the test key.
//! - A tempdir under which `ssh`/`scp` shim binaries live. Any
//!   subprocess that prepends [`SshTestHost::bin_dir`] to its `PATH`
//!   sees these shims first; the shims `exec`s the real `ssh`/`scp`
//!   with `-F <test-ssh-config>`, so bootstrap-style `ssh <host>`
//!   invocations resolve through the test's per-host config without
//!   touching the developer's real `~/.ssh/config`. This is the only
//!   currently-feasible way to route the bootstrap's SSH calls to a
//!   non-default port and key — the production bootstrap intentionally
//!   does not expose `-F` / `-p` / `-i` flags.
//!
//! ## Drop semantics
//!
//! [`SshTestHost::drop`] sends `SIGTERM` to the `sshd` subprocess and
//! then `SIGKILL` after a short bounded wait. The tempdirs unlink
//! their contents on drop. Nothing inside `Drop` panics — leaks are
//! preferred over a panic-in-drop that masks the original test failure.

// Test-harness setup panics on environment failure; the messages it
// produces are how a missing-binary or bad-permission diagnostic
// reaches the developer. Allow at crate scope.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// Where Debian ships `sshd`. We look here explicitly because `sshd` is
/// usually not on a login-user `PATH`; falling back to `which sshd`
/// would silently skip the test in that very common case.
const SSHD_DEFAULT_PATH: &str = "/usr/sbin/sshd";

/// Upper bound on Drop's wait for `sshd` to exit after `SIGTERM`. The
/// process is a single accept loop with no flushes; one second is
/// plenty.
const SSHD_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

/// How long [`spawn_sshd`] waits for the listening port to become
/// connectable after launch. `sshd` binds before backgrounding so this
/// is mostly fork/exec latency.
const SSHD_READY_TIMEOUT: Duration = Duration::from_secs(3);

/// Cadence inside the readiness poll.
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Owns a running `sshd` subprocess plus every disk artifact the
/// subprocess and any client need.
///
/// Fields are private; the only public surface is the accessor methods
/// below and [`Self::bin_dir`] for `PATH` injection.
pub struct SshTestHost {
    /// Random local port the sshd is listening on. Chosen by binding
    /// `127.0.0.1:0`, releasing the listener, and passing the port to
    /// sshd's config — the kernel does not reuse the port immediately,
    /// so this is race-tolerant in practice for our serial-test model.
    port: u16,
    /// Tempdir holding host key, user key, sshd config, authorized
    /// keys file. Held so its Drop runs after [`Self::sshd_child`]
    /// is reaped.
    _config_dir: TempDir,
    /// Tempdir holding the `ssh` / `scp` PATH shims that bake the
    /// per-test `ssh_config` into every invocation. Held for Drop.
    _shim_dir: TempDir,
    /// Absolute path to the `bin` subdir of `_shim_dir`. Tests prepend
    /// this to `PATH` so bootstrap-style `ssh`/`scp` calls hit the
    /// shims.
    bin_dir: PathBuf,
    /// Absolute path to the test user's private key. The `ssh` shim
    /// uses this via the test `ssh_config`'s `IdentityFile`.
    user_key_path: PathBuf,
    /// Hostname alias the shim's `ssh_config` routes to `127.0.0.1:port`.
    /// Tests pass this string wherever the production bootstrap takes
    /// a `host: &str`.
    host_alias: String,
    /// sshd process. `Option` so `Drop` can move it out.
    sshd_child: Option<Child>,
}

impl SshTestHost {
    /// TCP port the sshd is listening on (always `127.0.0.1`-bound).
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Hostname alias the test should pass to the bootstrap. The shim
    /// rewrites it to the real `127.0.0.1:<port>` via `-F` + the
    /// per-test `ssh_config`.
    #[must_use]
    pub fn host_alias(&self) -> &str {
        &self.host_alias
    }

    /// Path to the shim `bin` directory. Prepend to a child's `PATH`
    /// so it picks up our `ssh`/`scp` shims before the system binaries.
    #[must_use]
    pub fn bin_dir(&self) -> &Path {
        &self.bin_dir
    }

    /// Absolute path to the test user's private key. Tests that issue
    /// `ssh` directly (not through a shim) pass this with `-i`.
    #[must_use]
    pub fn user_key_path(&self) -> &Path {
        &self.user_key_path
    }

    /// Build a `Command` that runs the real `ssh` binary against this
    /// test host with all the flags a hermetic test needs: identity
    /// file, `BatchMode=yes`, no host-key checking, no known-hosts
    /// pollution. Callers add their remote command via `.arg(...)`.
    ///
    /// Use this when the test wants to talk to the sshd directly —
    /// e.g. to verify a daemon's pid is still alive after the
    /// `ControlMaster` exits. The shim approach (via [`Self::bin_dir`])
    /// is for tests that drive the production bootstrap, which calls
    /// `ssh` by name.
    #[must_use]
    pub fn ssh_command(&self) -> Command {
        let mut cmd = Command::new("/usr/bin/ssh");
        cmd.arg("-p")
            .arg(self.port.to_string())
            .arg("-i")
            .arg(&self.user_key_path)
            .arg("-o")
            .arg("StrictHostKeyChecking=no")
            .arg("-o")
            .arg("UserKnownHostsFile=/dev/null")
            .arg("-o")
            .arg("IdentitiesOnly=yes")
            .arg("-o")
            .arg("BatchMode=yes")
            .arg(format!("{user}@127.0.0.1", user = current_user()));
        cmd
    }
}

impl Drop for SshTestHost {
    fn drop(&mut self) {
        if let Some(mut child) = self.sshd_child.take() {
            // SIGTERM first. `Child::kill` sends SIGKILL on Unix; we
            // want a polite termination so sshd closes listening
            // sockets cleanly. The workspace's `unsafe_code = "forbid"`
            // lint blocks `libc::kill` directly, so shell out to
            // `/usr/bin/kill` (matches the daemon's own kill prelude).
            let pid = child.id();
            let _ = Command::new("/usr/bin/kill")
                .arg("-TERM")
                .arg(pid.to_string())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();

            let deadline = Instant::now() + SSHD_SHUTDOWN_TIMEOUT;
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(POLL_INTERVAL);
                    }
                    _ => {
                        // SIGTERM didn't take — fall back to SIGKILL
                        // and reap. Best-effort: a stuck Drop here
                        // would mask the real test failure.
                        child.kill().ok();
                        child.wait().ok();
                        break;
                    }
                }
            }
        }
    }
}

/// Spawn an isolated `sshd` subprocess listening on a random port on
/// `127.0.0.1`. Returns a handle whose `Drop` kills the subprocess.
///
/// Test-only env injection: any `key=value` pair in `env` is added to
/// the test `authorized_keys` line as `environment="key=value"` so
/// `sshd` exports it into the connecting session (the test sshd is
/// configured with `PermitUserEnvironment yes`). Use this to thread a
/// value like `CODEMUX_AGENT_BIN=/abs/path` through to the remote
/// daemon launched over ssh, where the production CLI does not yet
/// expose a flag.
///
/// `PATH` opt-in: if (and only if) the caller passes a `("PATH", ...)`
/// entry, this helper prepends [`SshTestHost::bin_dir`] to that PATH
/// so test-only shims dropped into `bin_dir` (e.g. AC-003's `cargo`
/// shim or `claude` symlink) are reachable from any command run via
/// `ssh <host> '...'`. Callers that don't need the shim path can omit
/// the PATH key entirely and inherit sshd's default session PATH —
/// existing absolute-path callers (`proto_ssh_disconnect.rs`,
/// `proto_setsid.rs`) hit this branch.
///
/// # Panics
///
/// Panics if any of `sshd`, `ssh-keygen`, or the real `ssh` binary is
/// missing from the well-known locations the harness probes, if a
/// random port can't be acquired, or if the subprocess does not accept
/// a TCP connection within [`SSHD_READY_TIMEOUT`]. All three are
/// environment errors at this layer; a panic gives the clearest
/// possible failure message before any test logic runs.
#[must_use]
pub fn spawn_sshd(env: &[(&str, &str)]) -> SshTestHost {
    require_binary(SSHD_DEFAULT_PATH);
    require_binary("/usr/bin/ssh-keygen");
    require_binary("/usr/bin/ssh");

    let config_dir = TempDir::new().expect("tempdir for sshd config");
    let host_key = config_dir.path().join("host_key");
    let user_key = config_dir.path().join("user_key");
    let authorized_keys = config_dir.path().join("authorized_keys");
    let sshd_config = config_dir.path().join("sshd_config");

    keygen_ed25519(&host_key);
    keygen_ed25519(&user_key);

    let user_pub = std::fs::read_to_string(format!("{}.pub", user_key.display()))
        .expect("read generated user public key");
    let user_pub_trimmed = user_pub.trim();

    // Compute the shim `bin_dir` path early so it can be baked into the
    // SSH session's PATH via env-injection. The actual `mkdir` plus
    // ssh/scp shim install happens after sshd is up; the remote PATH is
    // only consulted when an SSH command exec's, by which point
    // everything is in place.
    let shim_dir = TempDir::new().expect("tempdir for ssh shims");
    let bin_dir = shim_dir.path().join("bin");

    let env_prefix = build_env_prefix(env, &bin_dir);
    write_file_mode(
        &authorized_keys,
        &format!("{env_prefix}{user_pub_trimmed}\n"),
        0o600,
    );

    let port = pick_random_local_port();
    let config = build_sshd_config(port, &host_key, &authorized_keys);
    write_file_mode(&sshd_config, &config, 0o600);

    let validate = Command::new(SSHD_DEFAULT_PATH)
        .arg("-t")
        .arg("-f")
        .arg(&sshd_config)
        .output()
        .expect("run sshd -t for config validation");
    assert!(
        validate.status.success(),
        "test sshd config rejected by `sshd -t`: stderr={}",
        String::from_utf8_lossy(&validate.stderr),
    );

    // `-D` keeps sshd in the foreground (no fork), `-e` writes to
    // stderr instead of syslog — the no-syslog bit is necessary in
    // environments where syslog isn't available (containers, CI).
    let child = Command::new(SSHD_DEFAULT_PATH)
        .arg("-D")
        .arg("-e")
        .arg("-f")
        .arg(&sshd_config)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sshd subprocess");

    wait_for_listening(port, SSHD_READY_TIMEOUT);

    std::fs::create_dir(&bin_dir).expect("mkdir shim bin dir");
    let host_alias = "codemux-test-host".to_string();
    let shim_ssh_config = shim_dir.path().join("ssh_config");
    write_ssh_config(
        &shim_ssh_config,
        &host_alias,
        port,
        &user_key,
        &current_user(),
    );
    write_shim(&bin_dir.join("ssh"), "/usr/bin/ssh", &shim_ssh_config);
    write_shim(&bin_dir.join("scp"), "/usr/bin/scp", &shim_ssh_config);

    SshTestHost {
        port,
        _config_dir: config_dir,
        _shim_dir: shim_dir,
        bin_dir,
        user_key_path: user_key,
        host_alias,
        sshd_child: Some(child),
    }
}

/// Build the `environment="..."` prefix for the test user's
/// `authorized_keys` line. PATH injection is opt-in: only callers that
/// pass a `("PATH", ...)` entry get `bin_dir` prepended onto their
/// PATH. Callers that omit the PATH key get no PATH env at all,
/// preserving sshd's default session PATH (the behavior existing
/// callers like `proto_ssh_disconnect.rs` and `proto_setsid.rs`
/// have always relied on, since they invoke binaries by absolute
/// path).
fn build_env_prefix(env: &[(&str, &str)], bin_dir: &Path) -> String {
    if env.is_empty() {
        return String::new();
    }
    let bin_dir_str = bin_dir.display().to_string();
    assert!(
        !bin_dir_str.contains('"') && !bin_dir_str.contains(','),
        "shim bin_dir path must not contain double quote or comma (got {bin_dir_str:?})",
    );
    let pairs: Vec<String> = env
        .iter()
        .map(|(k, v)| {
            assert!(
                !k.contains('"') && !v.contains('"'),
                "test env var keys/values may not contain double quotes: {k:?}={v:?}",
            );
            assert!(
                !k.contains(',') && !v.contains(','),
                "test env var keys/values may not contain commas (ssh authorized_keys \
                 option separator): {k:?}={v:?}",
            );
            if *k == "PATH" {
                format!("environment=\"PATH={bin_dir_str}:{v}\"")
            } else {
                format!("environment=\"{k}={v}\"")
            }
        })
        .collect();
    format!("{} ", pairs.join(","))
}

/// Write a config file for `sshd -f`. Keeps the surface minimal: pubkey
/// auth only, no password, no PAM, no motd, no agent forwarding, no
/// X11, no system-integration surface that would need root or pam.d.
///
/// `PermitUserEnvironment yes` lets the test inject env vars via
/// `authorized_keys`. Without it, `environment="..."` lines are
/// silently ignored.
fn build_sshd_config(port: u16, host_key: &Path, authorized_keys: &Path) -> String {
    format!(
        "Port {port}\n\
         ListenAddress 127.0.0.1\n\
         HostKey {host_key}\n\
         AuthorizedKeysFile {auth}\n\
         PubkeyAuthentication yes\n\
         PasswordAuthentication no\n\
         KbdInteractiveAuthentication no\n\
         PermitRootLogin no\n\
         UsePAM no\n\
         StrictModes no\n\
         PrintMotd no\n\
         PrintLastLog no\n\
         X11Forwarding no\n\
         AllowAgentForwarding no\n\
         PermitUserEnvironment yes\n\
         AcceptEnv *\n\
         AllowStreamLocalForwarding yes\n\
         LogLevel ERROR\n",
        host_key = host_key.display(),
        auth = authorized_keys.display(),
    )
}

/// Write the per-test `ssh_config` the shims pass to `ssh -F`. The
/// `Host *` block applies to any alias the test uses (we expose one
/// here, but the production bootstrap may add more in future tests);
/// the `Host <alias>` block routes that alias to `127.0.0.1:<port>`.
fn write_ssh_config(path: &Path, alias: &str, port: u16, user_key: &Path, user: &str) {
    let body = format!(
        "Host {alias}\n\
         \tHostName 127.0.0.1\n\
         \tPort {port}\n\
         \tUser {user}\n\
         \tIdentityFile {key}\n\
         \tIdentitiesOnly yes\n\
         \tStrictHostKeyChecking no\n\
         \tUserKnownHostsFile /dev/null\n\
         \tLogLevel ERROR\n\
         \tForwardAgent no\n\
         \tForwardX11 no\n",
        alias = alias,
        port = port,
        user = user,
        key = user_key.display(),
    );
    write_file_mode(path, &body, 0o600);
}

/// Write an executable shim script at `path` that `exec`s the real
/// binary with `-F <ssh_config>` prepended to its argv. The pattern
/// works for both `ssh` and `scp` — both honor `-F`.
fn write_shim(path: &Path, real: &str, ssh_config: &Path) {
    let body = format!(
        "#!/bin/sh\nexec {real} -F {cfg} \"$@\"\n",
        real = real,
        cfg = ssh_config.display(),
    );
    write_file_mode(path, &body, 0o755);
}

/// Run `ssh-keygen -t ed25519 -f <path> -N ""` to materialize a fresh
/// keypair. Quiet on success; panic with stderr on failure.
fn keygen_ed25519(path: &Path) {
    let output = Command::new("/usr/bin/ssh-keygen")
        .arg("-q")
        .arg("-t")
        .arg("ed25519")
        .arg("-f")
        .arg(path)
        .arg("-N")
        .arg("")
        .output()
        .expect("spawn ssh-keygen");
    assert!(
        output.status.success(),
        "ssh-keygen failed: stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );
}

/// Pick a port by binding `127.0.0.1:0`, reading the assigned port,
/// then closing the listener. There is a small TOCTOU race between
/// the close and sshd's bind — in practice the kernel does not reuse
/// the ephemeral port within milliseconds, and the test runner is
/// single-process so cross-test collisions can't happen either.
fn pick_random_local_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0 to pick port");
    let port = listener.local_addr().expect("read local_addr").port();
    drop(listener);
    port
}

/// Poll `127.0.0.1:<port>` until a TCP connect succeeds or the
/// deadline fires. Used after spawning sshd to gate the test on actual
/// listen readiness rather than just `Command::spawn` returning.
fn wait_for_listening(port: u16, timeout: Duration) {
    let addr = format!("127.0.0.1:{port}");
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if std::net::TcpStream::connect_timeout(&addr.parse().expect("parse addr"), POLL_INTERVAL)
            .is_ok()
        {
            return;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    panic!("sshd did not start listening on {addr} within {timeout:?}");
}

/// Write `body` to `path` and chmod to `mode`. Panics on either I/O
/// error — these are setup failures, not test logic.
fn write_file_mode(path: &Path, body: &str, mode: u32) {
    let mut f =
        std::fs::File::create(path).unwrap_or_else(|e| panic!("create {}: {e}", path.display()));
    f.write_all(body.as_bytes())
        .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    let perms = std::fs::Permissions::from_mode(mode);
    std::fs::set_permissions(path, perms)
        .unwrap_or_else(|e| panic!("chmod {} -> {mode:o}: {e}", path.display()));
}

/// Panic with a clear message if `path` does not exist or is not
/// executable. Catches the "host doesn't have openssh-server installed"
/// case before sshd's exec fails with a less obvious diagnostic.
fn require_binary(path: &str) {
    let meta = std::fs::metadata(path).unwrap_or_else(|e| {
        panic!("required binary {path} not present (install openssh-server / openssh-client): {e}")
    });
    let mode = meta.permissions().mode();
    assert!(
        mode & 0o111 != 0,
        "required binary {path} exists but is not executable (mode={mode:o})",
    );
}

/// Current Unix user name, read from the `USER` env var. The test sshd
/// is configured to accept this user; the `ssh_config` routes the alias
/// to `<user>@127.0.0.1`. Panic if unset — every Unix shell sets
/// `USER`, so an empty value is an environment bug.
fn current_user() -> String {
    std::env::var("USER").expect("USER env var must be set for the SSH test harness")
}
