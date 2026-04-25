//! SSH bootstrap of `codemuxd` on a remote host.
//!
//! This crate is the **adapter** side of the SSH transport. Per the
//! workspace's hexagonal split, `crates/session` is the application
//! core (it owns the [`AgentTransport`] and its wire-protocol speakers
//! `LocalPty` / `SshDaemonPty`); this crate drives the infrastructure
//! steps (ssh, scp, file system, subprocess) that bring an
//! [`AgentTransport::SshDaemon`] into existence.
//!
//! Public surface:
//! - [`bootstrap`] — runs the 7-step pipeline and returns a connected
//!   [`UnixStream`] + the tunnel subprocess `Child`.
//! - [`establish_ssh_transport`] — convenience that bootstraps then
//!   performs the wire handshake via `SshDaemonPty::attach`, returning
//!   a ready-to-use [`AgentTransport`].
//! - [`CommandRunner`] / [`RealRunner`] — pluggable shim around
//!   `std::process::Command` so failure modes are unit-testable
//!   without touching the network.
//! - [`Error`] / [`Stage`] — the error envelope.
//!
//! # The 7 steps
//!
//! 1. **Probe**: `ssh host 'cat ~/.cache/codemuxd/agent.version'`. If
//!    the file matches our [`bootstrap_version`], skip steps 2-4.
//! 2. **Stage tarball**: write the embedded daemon source archive to
//!    a local tempfile. Cached process-wide so a second `bootstrap()`
//!    in the same TUI session reuses it.
//! 3. **scp**: copy the tarball to the remote.
//! 4. **Remote build**: untar, `cargo build --release --bin codemuxd`,
//!    move the binary into place, write `agent.version`.
//! 5. **Spawn daemon**: `ssh host 'setsid -f codemuxd ...'`. The
//!    `setsid -f` invocation detaches the daemon from the SSH session
//!    so it survives when the SSH connection closes.
//! 6. **Tunnel**: `ssh -N -L /local.sock:/remote.sock host` in a
//!    background subprocess. Uses OpenSSH unix-socket forwarding
//!    (≥6.7 on both ends). The handle is returned so the caller can
//!    kill it on Drop.
//! 7. **Connect**: `UnixStream::connect` against the local end of the
//!    tunnel, with a short retry loop while the daemon's `bind()` and
//!    the tunnel's first-packet warmup converge.
//!
//! # Mockable command execution
//!
//! All ssh/scp invocations go through a [`CommandRunner`] trait so
//! tests can inject scripted responses without touching the network.
//! [`RealRunner`] is the production implementation; the unit tests in
//! this module use a `FakeRunner` that maps `(program, args_prefix)`
//! pairs to canned [`CommandOutput`] / [`std::io::Error`] values.

pub mod error;

use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use codemux_session::{AgentTransport, SshDaemonPty};

pub use crate::error::{Error, Stage};

/// The embedded codemuxd source tarball, assembled by `build.rs`. This
/// constant is what the bootstrap scp's to remote hosts. See
/// `crates/codemuxd-bootstrap/build.rs` for the assembly logic.
const BOOTSTRAP_TARBALL: &[u8] =
    include_bytes!(concat!(env!("OUT_DIR"), "/codemuxd-bootstrap.tar.gz"));

/// How long to retry [`UnixStream::connect`] before giving up. The
/// daemon's `bind()` and the SSH tunnel's first-packet handshake both
/// take a small but non-zero time after spawn; 5 s is plenty for any
/// non-pathological path.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Polling interval inside the connect retry loop.
const CONNECT_POLL: Duration = Duration::from_millis(100);

/// SSH connect timeout for the cheap version probe (step 1). Short so
/// "host unreachable" surfaces fast — the user is waiting in the TUI.
const PROBE_TIMEOUT_SECS: u32 = 5;

/// Compute a stable cache-invalidation key for the embedded tarball.
/// The remote writes this string to `~/.cache/codemuxd/agent.version`
/// after a successful build; the next bootstrap probes that file and
/// skips steps 2-4 if it matches.
///
/// We use [`std::hash::DefaultHasher`] (`SipHash`) over the tarball
/// bytes rather than the cargo package version because the package
/// version is only meaningful when *we* remember to bump it; a hash is
/// automatic and catches any source change.
///
/// Cached after first call — the hash doesn't change at runtime.
#[must_use]
pub fn bootstrap_version() -> &'static str {
    static CACHED: OnceLock<String> = OnceLock::new();
    CACHED.get_or_init(|| {
        let mut hasher = DefaultHasher::new();
        BOOTSTRAP_TARBALL.hash(&mut hasher);
        format!("codemuxd-sip-{:016x}", hasher.finish())
    })
}

/// Output of a completed external command. Mirrors
/// [`std::process::Output`] minus the `success`/`code` distinction —
/// callers care about a single exit code, with negative values
/// reserved for "killed by signal" per UNIX convention.
#[derive(Debug)]
pub struct CommandOutput {
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Runs external commands. Implemented by [`RealRunner`] in production
/// and by tests' `FakeRunner` to script responses without spawning
/// subprocesses.
///
/// Both methods are infallible-on-the-trait-level for transport errors
/// only; an exit-1 from `ssh` is reported via [`CommandOutput::status`],
/// not via the `Result`. This matches `std::process::Command::output`'s
/// semantics and lets the bootstrap distinguish "ssh launched, host
/// rejected us" from "ssh wasn't found on local PATH".
pub trait CommandRunner: Send + Sync {
    /// Run a command to completion, capturing stdout/stderr.
    ///
    /// # Errors
    /// Returns `io::Error` when the program can't be spawned at all
    /// (e.g. `ssh` not on PATH). Subprocess exits with non-zero status
    /// are reported through `CommandOutput::status`, not via Err.
    fn run(&self, program: &str, args: &[&str]) -> std::io::Result<CommandOutput>;

    /// Spawn a long-running subprocess detached from this thread's
    /// I/O. Used for the `setsid -f` daemon launch and the `ssh -N -L`
    /// tunnel — both of which we hand back to the caller as a
    /// [`std::process::Child`] for kill-on-Drop semantics.
    ///
    /// # Errors
    /// Same envelope as [`Self::run`]: only spawn-failures bubble up
    /// here.
    fn spawn_detached(&self, program: &str, args: &[&str]) -> std::io::Result<Child>;
}

/// Production [`CommandRunner`] backed by `std::process::Command`.
/// Zero state — instantiate inline.
pub struct RealRunner;

impl CommandRunner for RealRunner {
    fn run(&self, program: &str, args: &[&str]) -> std::io::Result<CommandOutput> {
        let output = Command::new(program).args(args).output()?;
        Ok(CommandOutput {
            // ExitStatus's `code()` is None when the process was killed
            // by a signal; we encode that as -signum below in the same
            // place an `i32` exit code would live. Matches the
            // ChildExited wire-protocol convention.
            status: output.status.code().unwrap_or_else(|| {
                // On UNIX, signal-killed processes have `signal()`
                // Some. We can't pull in
                // `std::os::unix::process::ExitStatusExt` for
                // `signal()` without cfg-gating, but the daemon only
                // runs on UNIX so that's fine.
                use std::os::unix::process::ExitStatusExt;
                output.status.signal().map_or(-1, |s| -s)
            }),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }

    fn spawn_detached(&self, program: &str, args: &[&str]) -> std::io::Result<Child> {
        Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
    }
}

/// RAII guard for the `ssh -N -L` tunnel subprocess. If the bootstrap
/// fails between spawn and the final `Ok` return, the guard's `Drop`
/// kills the tunnel so we don't leak ssh processes. On success, the
/// caller calls [`Self::into_inner`] to extract the [`Child`] and
/// transfer cleanup responsibility to the [`SshDaemonPty`].
struct TunnelGuard {
    child: Option<Child>,
}

impl TunnelGuard {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    /// Disarm the guard, returning the underlying [`Child`]. Caller
    /// becomes responsible for killing it.
    fn into_inner(mut self) -> Child {
        match self.child.take() {
            Some(c) => c,
            // Construction always sets `child = Some`; `take` is only
            // called here. Reaching `None` would mean a future change
            // accidentally double-took — the unreachable! makes that
            // a fail-loud bug rather than a silent UB.
            None => unreachable!("TunnelGuard::child is always Some until into_inner"),
        }
    }
}

impl Drop for TunnelGuard {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Run the full 7-step SSH bootstrap. Returns the connected unix
/// stream the wire-protocol handshake should run over, plus the
/// tunnel subprocess (caller owns cleanup via `Child::kill`).
///
/// `cwd` is interpreted on the **remote** host — the daemon will
/// `chdir` into it before spawning `claude`.
///
/// `local_socket_dir` is where the local end of the `ssh -L` tunnel
/// binds. Production callers pass [`default_local_socket_dir`]; tests
/// pass a tempdir to avoid mutating `$HOME`.
///
/// # Errors
/// Any failure surfaces as [`Error::Bootstrap`] with the [`Stage`]
/// that tripped. The TUI uses `stage` to render an actionable message.
pub fn bootstrap(
    runner: &dyn CommandRunner,
    host: &str,
    agent_id: &str,
    cwd: &Path,
    local_socket_dir: &Path,
) -> Result<(UnixStream, Child), Error> {
    validate_agent_id(agent_id)?;

    let target_version = bootstrap_version();
    let probe = probe_remote(runner, host)?;
    if probe.installed_version.as_deref() != Some(target_version) {
        let local_tarball = stage_tarball()?;
        scp_tarball(runner, host, &local_tarball, target_version)?;
        remote_build(runner, host, target_version)?;
    }

    spawn_remote_daemon(runner, host, agent_id, cwd)?;

    let local_socket = local_socket_path(local_socket_dir, agent_id)?;
    let tunnel = open_ssh_tunnel(runner, host, agent_id, &local_socket, &probe.home)?;
    let tunnel_guard = TunnelGuard::new(tunnel);

    let stream = connect_socket(&local_socket, CONNECT_TIMEOUT)?;
    Ok((stream, tunnel_guard.into_inner()))
}

/// Bootstrap the remote daemon, then perform the
/// [`SshDaemonPty::attach`] handshake, returning a fully constructed
/// [`AgentTransport`] ready for the runtime.
///
/// This is the convenience entry point Stage 5 (TUI spawn modal) uses;
/// it composes [`bootstrap`] with the session crate's wire handshake
/// so callers don't have to know about the intermediate `(UnixStream,
/// Child)` pair.
///
/// # Errors
/// Returns [`Error::Bootstrap`] for any of the 7 bootstrap stages, or
/// [`Error::Session`] when the post-bootstrap wire handshake fails.
pub fn establish_ssh_transport(
    runner: &dyn CommandRunner,
    host: &str,
    agent_id: &str,
    cwd: &Path,
    local_socket_dir: &Path,
    rows: u16,
    cols: u16,
) -> Result<AgentTransport, Error> {
    let (stream, tunnel) = bootstrap(runner, host, agent_id, cwd, local_socket_dir)?;
    let label = format!("{host}:{agent_id}");
    SshDaemonPty::attach(stream, label, agent_id, rows, cols, Some(tunnel))
        .map(AgentTransport::SshDaemon)
        .map_err(|source| Error::Session {
            source: Box::new(source),
        })
}

/// Default `local_socket_dir` for production callers:
/// `$HOME/.cache/codemux/local-sockets/`. Per-user without needing
/// `libc::getuid` (workspace forbids `unsafe`).
///
/// # Errors
/// Returns [`Error::Bootstrap`] with stage [`Stage::SocketTunnel`] if
/// `$HOME` is unset (very unusual on a UNIX login session).
pub fn default_local_socket_dir() -> Result<PathBuf, Error> {
    let home = std::env::var_os("HOME").ok_or_else(|| Error::Bootstrap {
        stage: Stage::SocketTunnel,
        source: "HOME env var unset; cannot derive default local socket dir".into(),
    })?;
    Ok(PathBuf::from(home)
        .join(".cache")
        .join("codemux")
        .join("local-sockets"))
}

/// Reject agent ids that would shell-inject when interpolated into the
/// remote's shell command lines. The bootstrap composes ssh argv as
/// strings (single-quoted), so any embedded quote or `$` would escape.
/// Only allow ASCII alphanumerics, dash, dot, underscore.
fn validate_agent_id(agent_id: &str) -> Result<(), Error> {
    if agent_id.is_empty() || agent_id.len() > 64 {
        return Err(Error::Bootstrap {
            stage: Stage::DaemonSpawn,
            source: format!("agent_id length {} not in [1, 64]", agent_id.len()).into(),
        });
    }
    let ok = agent_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'));
    if !ok {
        return Err(Error::Bootstrap {
            stage: Stage::DaemonSpawn,
            source: format!("agent_id {agent_id:?} must be [A-Za-z0-9._-] (shell-safe)").into(),
        });
    }
    Ok(())
}

/// Result of the cheap probe step.
///
/// `home` is the remote `$HOME` (always present when ssh succeeded —
/// `echo "$HOME"` is unconditional in the probe command). It's
/// load-bearing for step 6: `ssh -L`'s remote half does not get
/// shell-expanded, so we need an absolute path to put in the forward
/// spec, and `$HOME` is the only piece we can't know in advance.
///
/// `installed_version` is the trimmed contents of the remote's
/// `agent.version` file, or `None` if the file doesn't exist (fresh
/// host with no daemon installed).
#[derive(Debug)]
struct RemoteProbe {
    home: PathBuf,
    installed_version: Option<String>,
}

/// Step 1: cheap probe. Captures the remote `$HOME` and the installed
/// daemon version (if any) in one round trip.
///
/// The probe shell command is `echo "$HOME"; cat <version> 2>/dev/null
/// || true`. The `|| true` keeps the whole command's exit status 0 in
/// the no-version-installed case, so a non-zero exit unambiguously
/// means an SSH-level failure (host unreachable, auth refused — exit
/// 255). Without a successful probe we can't compute the absolute
/// remote socket path for step 6, so SSH-level failures bubble up as
/// `Error::Bootstrap{stage: VersionProbe}` instead of being silently
/// downgraded to "no installed version".
fn probe_remote(runner: &dyn CommandRunner, host: &str) -> Result<RemoteProbe, Error> {
    let output = runner
        .run(
            "ssh",
            &[
                "-o",
                "BatchMode=yes",
                "-o",
                &format!("ConnectTimeout={PROBE_TIMEOUT_SECS}"),
                host,
                "echo \"$HOME\"; cat ~/.cache/codemuxd/agent.version 2>/dev/null || true",
            ],
        )
        .map_err(|source| Error::Bootstrap {
            stage: Stage::VersionProbe,
            source: Box::new(source),
        })?;
    if output.status != 0 {
        return Err(Error::Bootstrap {
            stage: Stage::VersionProbe,
            source: format!(
                "ssh probe failed (exit {}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim(),
            )
            .into(),
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut lines = stdout.lines();
    let home = lines
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| Error::Bootstrap {
            stage: Stage::VersionProbe,
            source: "ssh probe stdout was empty (no $HOME line)".into(),
        })?;
    let installed_version = lines
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Ok(RemoteProbe {
        home: PathBuf::from(home),
        installed_version,
    })
}

/// Step 2: write the embedded tarball to a local tempfile and return
/// its path. Cached per-process so a second `bootstrap()` in the same
/// TUI session doesn't pay for the disk write twice.
fn stage_tarball() -> Result<PathBuf, Error> {
    static STAGED: OnceLock<PathBuf> = OnceLock::new();
    if let Some(path) = STAGED.get() {
        return Ok(path.clone());
    }
    let tmp_dir = std::env::temp_dir().join("codemux-bootstrap");
    std::fs::create_dir_all(&tmp_dir).map_err(|source| Error::Bootstrap {
        stage: Stage::TarballStage,
        source: Box::new(source),
    })?;
    let path = tmp_dir.join(format!("{}.tar.gz", bootstrap_version()));
    if !path.exists() {
        let mut f = std::fs::File::create(&path).map_err(|source| Error::Bootstrap {
            stage: Stage::TarballStage,
            source: Box::new(source),
        })?;
        f.write_all(BOOTSTRAP_TARBALL)
            .map_err(|source| Error::Bootstrap {
                stage: Stage::TarballStage,
                source: Box::new(source),
            })?;
    }
    // OnceLock::set returns Err if a concurrent caller raced and won;
    // in that case use the winner's path (they wrote the same bytes).
    let _ = STAGED.set(path.clone());
    Ok(STAGED.get().cloned().unwrap_or(path))
}

/// Step 3: `scp local-path host:remote-path`. The remote path lives
/// under `~/.cache/codemuxd/src/` — the daemon's
/// [`fs_layout::Layout::src_dir`](../../../apps/daemon/src/fs_layout.rs)
/// convention.
fn scp_tarball(
    runner: &dyn CommandRunner,
    host: &str,
    local: &Path,
    version: &str,
) -> Result<(), Error> {
    let local_str = local.to_str().ok_or_else(|| Error::Bootstrap {
        stage: Stage::Scp,
        source: format!("local tarball path not UTF-8: {}", local.display()).into(),
    })?;
    let remote = format!("{host}:.cache/codemuxd/src/{version}.tar.gz");
    // Pre-create the remote dir before the scp itself; scp won't
    // create intermediate dirs for the destination.
    let mkdir_cmd = "mkdir -p ~/.cache/codemuxd/src && \
                     touch ~/.cache/codemuxd/src/.bootstrap-stamp";
    let mkdir_out = runner
        .run("ssh", &["-o", "BatchMode=yes", host, mkdir_cmd])
        .map_err(|source| Error::Bootstrap {
            stage: Stage::Scp,
            source: Box::new(source),
        })?;
    if mkdir_out.status != 0 {
        return Err(Error::Bootstrap {
            stage: Stage::Scp,
            source: format!(
                "remote mkdir failed (status {}): {}",
                mkdir_out.status,
                String::from_utf8_lossy(&mkdir_out.stderr).trim(),
            )
            .into(),
        });
    }
    let scp_out = runner
        .run("scp", &["-B", local_str, &remote])
        .map_err(|source| Error::Bootstrap {
            stage: Stage::Scp,
            source: Box::new(source),
        })?;
    if scp_out.status != 0 {
        return Err(Error::Bootstrap {
            stage: Stage::Scp,
            source: format!(
                "scp exit {}: {}",
                scp_out.status,
                String::from_utf8_lossy(&scp_out.stderr).trim(),
            )
            .into(),
        });
    }
    Ok(())
}

/// Step 4: `ssh host 'cd src && tar -xzf ... && cargo build && mv ...
/// && echo {version} > agent.version'`. Long-running. Surfaces a
/// rustup-install hint when cargo isn't on the remote PATH.
fn remote_build(runner: &dyn CommandRunner, host: &str, version: &str) -> Result<(), Error> {
    // Build steps in one ssh invocation to amortize handshake cost.
    // The `&& echo ${version}` write is last so a partial build doesn't
    // leave a misleading agent.version. `tee build.log` keeps a log
    // for diagnostics without consuming the rust output.
    let cmd = format!(
        "set -e; cd ~/.cache/codemuxd/src && \
         tar -xzf {version}.tar.gz && \
         cargo build --release --bin codemuxd 2>&1 | tee build.log && \
         mkdir -p ~/.cache/codemuxd/bin && \
         mv target/release/codemuxd ~/.cache/codemuxd/bin/codemuxd && \
         echo {version} > ~/.cache/codemuxd/agent.version"
    );
    let out = runner
        .run("ssh", &["-o", "BatchMode=yes", host, &cmd])
        .map_err(|source| Error::Bootstrap {
            stage: Stage::RemoteBuild,
            source: Box::new(source),
        })?;
    if out.status != 0 {
        let stderr_text = String::from_utf8_lossy(&out.stderr);
        let hint = if stderr_text.contains("cargo: not found")
            || stderr_text.contains("cargo: command not found")
        {
            format!(
                "`cargo` not found on {host}. Install rustup first: https://rustup.rs/\n\
                 Remote stderr: {stderr_text}"
            )
        } else {
            format!("remote build exit {}: {}", out.status, stderr_text.trim(),)
        };
        return Err(Error::Bootstrap {
            stage: Stage::RemoteBuild,
            source: hint.into(),
        });
    }
    Ok(())
}

/// Step 5: launch the daemon under `setsid -f` so it survives this
/// SSH session's exit. The daemon's exclusive pid-file acquisition
/// (apps/daemon/src/bootstrap.rs) handles the "already running" case
/// — we surface its stderr directly if spawn fails.
///
/// Stdio redirection (`</dev/null >/dev/null 2>&1`) is **load-bearing**:
/// `setsid -f` forks but doesn't reopen file descriptors, so without
/// the redirect the daemon inherits the SSH session's pipes for
/// stdin/stdout/stderr. ssh then waits for those pipes to close before
/// exiting, which never happens (the daemon outlives ssh by design).
/// The local `runner.run("ssh", ...)` call hangs forever, and the user
/// sees a perpetual "bootstrapping" placeholder. Redirecting before
/// `setsid` forks closes the pipes from the daemon's side; ssh
/// observes EOF and exits cleanly. The cost is that any pre-tracing
/// panic on the daemon side (e.g. a clap parse error before
/// `init_tracing`) goes to /dev/null instead of the local terminal.
/// That is an acceptable tradeoff — the alternative is a silent hang
/// — and a future Stage-N task will fetch the daemon's log file post-
/// failure for richer diagnostics.
fn spawn_remote_daemon(
    runner: &dyn CommandRunner,
    host: &str,
    agent_id: &str,
    cwd: &Path,
) -> Result<(), Error> {
    let cwd_str = cwd.to_str().ok_or_else(|| Error::Bootstrap {
        stage: Stage::DaemonSpawn,
        source: format!("cwd not UTF-8: {}", cwd.display()).into(),
    })?;
    if cwd_str.contains('\'') {
        return Err(Error::Bootstrap {
            stage: Stage::DaemonSpawn,
            source: format!("cwd contains a single quote, refusing to shell-escape: {cwd_str:?}")
                .into(),
        });
    }
    let cmd = format!(
        "setsid -f ~/.cache/codemuxd/bin/codemuxd \
         --socket ~/.cache/codemuxd/sockets/{agent_id}.sock \
         --pid-file ~/.cache/codemuxd/pids/{agent_id}.pid \
         --log-file ~/.cache/codemuxd/logs/{agent_id}.log \
         --agent-id {agent_id} \
         --cwd '{cwd_str}' \
         </dev/null >/dev/null 2>&1"
    );
    let out = runner
        .run("ssh", &["-o", "BatchMode=yes", host, &cmd])
        .map_err(|source| Error::Bootstrap {
            stage: Stage::DaemonSpawn,
            source: Box::new(source),
        })?;
    if out.status != 0 {
        return Err(Error::Bootstrap {
            stage: Stage::DaemonSpawn,
            source: format!(
                "daemon spawn exit {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr).trim(),
            )
            .into(),
        });
    }
    Ok(())
}

/// Local-side socket path for the `ssh -L` tunnel. The `dir` argument
/// is where the socket binds; production passes
/// [`default_local_socket_dir`], tests pass a tempdir. The directory
/// is created if missing, and any existing socket file at the target
/// path is unlinked first (a stale socket from a previous run blocks
/// `UnixStream::connect` with "Connection refused" rather than letting
/// the retry loop converge).
fn local_socket_path(dir: &Path, agent_id: &str) -> Result<PathBuf, Error> {
    std::fs::create_dir_all(dir).map_err(|source| Error::Bootstrap {
        stage: Stage::SocketTunnel,
        source: Box::new(source),
    })?;
    let path = dir.join(format!("{agent_id}.sock"));
    let _ = std::fs::remove_file(&path);
    Ok(path)
}

/// Step 6: spawn `ssh -N -L local.sock:remote.sock host` in the
/// background. Returns the [`Child`] handle so the caller (transport
/// Drop) can kill the tunnel.
///
/// `remote_home` comes from step 1's probe and is **load-bearing**:
/// `ssh -L`'s remote half is opened by the remote sshd as the literal
/// path we send — `~`, `$HOME`, and relative paths are NOT expanded.
/// A path like `.cache/codemuxd/sockets/x.sock` resolves against
/// sshd's cwd (`/`) and silently fails to find the daemon's socket.
/// The local connect *succeeds* (ssh accepts the forward), the wire
/// handshake then fails with EOF, and the user sees a confusing
/// "EOF before `HelloAck`" without any clue the path was wrong.
///
/// `ControlPath=none` and `ControlMaster=no` are **also load-bearing**:
/// if the user's `~/.ssh/config` sets `ControlMaster auto` for this
/// host (extremely common — it's the standard recipe for connection
/// multiplexing, used by every `~/.ssh/config` template at Uber and
/// most public dotfile setups), our `ssh -N -L` will be routed
/// through the existing master via mux. The slave then sends a
/// `forward` request to the master and exits with status 0, but the
/// local socket file is *never bound* (mux's handling of unix-socket
/// forwards is buggy across many OpenSSH versions — verified on
/// OpenSSH 10.2p1 / macOS Sequoia: `mux_client_request_session`
/// reports the forward but the master never creates the socket
/// file). Our retry loop in `connect_socket` then ENOENTs out for
/// 5 s and the user sees "could not connect to remote daemon socket".
/// Forcing a fresh, non-mux ssh session via these two opts sidesteps
/// the mux path entirely; verified to bind the local socket within
/// ~1 s in the same environment.
fn open_ssh_tunnel(
    runner: &dyn CommandRunner,
    host: &str,
    agent_id: &str,
    local: &Path,
    remote_home: &Path,
) -> Result<Child, Error> {
    let local_str = local.to_str().ok_or_else(|| Error::Bootstrap {
        stage: Stage::SocketTunnel,
        source: format!("local socket path not UTF-8: {}", local.display()).into(),
    })?;
    let remote_socket = remote_home
        .join(".cache/codemuxd/sockets")
        .join(format!("{agent_id}.sock"));
    let remote_str = remote_socket.to_str().ok_or_else(|| Error::Bootstrap {
        stage: Stage::SocketTunnel,
        source: format!("remote socket path not UTF-8: {}", remote_socket.display()).into(),
    })?;
    let forward = format!("{local_str}:{remote_str}");
    runner
        .spawn_detached(
            "ssh",
            &[
                "-N",
                "-o",
                "BatchMode=yes",
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "ControlPath=none",
                "-o",
                "ControlMaster=no",
                "-L",
                &forward,
                host,
            ],
        )
        .map_err(|source| Error::Bootstrap {
            stage: Stage::SocketTunnel,
            source: Box::new(source),
        })
}

/// Step 7: connect to the local end of the tunnel, retrying briefly
/// while the daemon and tunnel both warm up.
fn connect_socket(path: &Path, timeout: Duration) -> Result<UnixStream, Error> {
    let deadline = Instant::now() + timeout;
    let mut last_err: Option<std::io::Error> = None;
    while Instant::now() < deadline {
        match UnixStream::connect(path) {
            Ok(s) => return Ok(s),
            Err(e) => {
                last_err = Some(e);
                thread::sleep(CONNECT_POLL);
            }
        }
    }
    let source: Box<dyn std::error::Error + Send + Sync> = match last_err {
        Some(e) => Box::new(e),
        None => format!(
            "connect to {} timed out after {:?} with no underlying error",
            path.display(),
            timeout,
        )
        .into(),
    };
    Err(Error::Bootstrap {
        stage: Stage::SocketConnect,
        source,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::io::ErrorKind;
    use std::sync::Mutex;

    use super::*;

    /// Scripted `(program, args_prefix) -> response` lookups. Args are
    /// matched by prefix so tests can supply only the leading args
    /// they care about (e.g. `ssh host`) without spelling out the
    /// command string the bootstrap embeds.
    struct FakeRunner {
        script: Mutex<Vec<FakeCall>>,
    }

    struct FakeCall {
        program: String,
        args_prefix: Vec<String>,
        response: FakeResponse,
    }

    enum FakeResponse {
        Output(CommandOutput),
        Error(std::io::Error),
        /// Spawn a real `sleep 60` so we have a Child handle to return.
        /// The test must drop it (or kill it) before timeout.
        SpawnSleep,
    }

    impl FakeRunner {
        fn new() -> Self {
            Self {
                script: Mutex::new(Vec::new()),
            }
        }

        fn expect_run(
            &self,
            program: &str,
            args_prefix: &[&str],
            response: CommandOutput,
        ) -> &Self {
            self.script.lock().unwrap().push(FakeCall {
                program: program.to_string(),
                args_prefix: args_prefix.iter().map(|s| (*s).to_string()).collect(),
                response: FakeResponse::Output(response),
            });
            self
        }

        fn expect_error(&self, program: &str, args_prefix: &[&str], err: std::io::Error) -> &Self {
            self.script.lock().unwrap().push(FakeCall {
                program: program.to_string(),
                args_prefix: args_prefix.iter().map(|s| (*s).to_string()).collect(),
                response: FakeResponse::Error(err),
            });
            self
        }

        fn expect_spawn(&self, program: &str, args_prefix: &[&str]) -> &Self {
            self.script.lock().unwrap().push(FakeCall {
                program: program.to_string(),
                args_prefix: args_prefix.iter().map(|s| (*s).to_string()).collect(),
                response: FakeResponse::SpawnSleep,
            });
            self
        }

        fn pop_match(&self, program: &str, args: &[&str]) -> Option<FakeResponse> {
            let mut script = self.script.lock().unwrap();
            let pos = script.iter().position(|c| {
                c.program == program
                    && c.args_prefix.len() <= args.len()
                    && c.args_prefix.iter().zip(args.iter()).all(|(a, b)| a == b)
            })?;
            Some(script.remove(pos).response)
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, program: &str, args: &[&str]) -> std::io::Result<CommandOutput> {
            match self.pop_match(program, args) {
                Some(FakeResponse::Output(out)) => Ok(out),
                Some(FakeResponse::Error(e)) => Err(e),
                Some(FakeResponse::SpawnSleep) => {
                    panic!("test scripted spawn but bootstrap called run for {program} {args:?}")
                }
                None => panic!("FakeRunner: unexpected run({program}, {args:?})"),
            }
        }

        fn spawn_detached(&self, program: &str, args: &[&str]) -> std::io::Result<Child> {
            match self.pop_match(program, args) {
                Some(FakeResponse::SpawnSleep) => Command::new("sleep").arg("60").spawn(),
                Some(FakeResponse::Error(e)) => Err(e),
                Some(FakeResponse::Output(_)) => panic!(
                    "test scripted run-output but bootstrap called spawn_detached for {program} {args:?}"
                ),
                None => panic!("FakeRunner: unexpected spawn({program}, {args:?})"),
            }
        }
    }

    fn ok(stdout: &[u8]) -> CommandOutput {
        CommandOutput {
            status: 0,
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
        }
    }

    fn fail(status: i32, stderr: &[u8]) -> CommandOutput {
        CommandOutput {
            status,
            stdout: Vec::new(),
            stderr: stderr.to_vec(),
        }
    }

    /// `bootstrap_version` is stable across calls and matches the
    /// expected prefix.
    #[test]
    fn bootstrap_version_is_stable_and_well_formed() {
        let v1 = bootstrap_version();
        let v2 = bootstrap_version();
        assert_eq!(v1, v2);
        assert!(v1.starts_with("codemuxd-sip-"), "got {v1}");
        assert_eq!(v1.len(), "codemuxd-sip-".len() + 16);
    }

    /// The embedded tarball decompresses and contains the expected
    /// top-level entries.
    #[test]
    fn embedded_tarball_contains_required_files() {
        use std::io::Cursor;
        let dec = flate2::read::GzDecoder::new(Cursor::new(BOOTSTRAP_TARBALL));
        let mut tar = tar::Archive::new(dec);
        let names: Vec<String> = tar
            .entries()
            .unwrap()
            .filter_map(|e| {
                let e = e.ok()?;
                let path = e.path().ok()?;
                Some(path.display().to_string())
            })
            .collect();
        for required in [
            "Cargo.toml",
            "Cargo.lock",
            "rust-toolchain.toml",
            "apps/daemon/Cargo.toml",
            "apps/daemon/src/lib.rs",
            "crates/wire/Cargo.toml",
            "crates/wire/src/lib.rs",
        ] {
            assert!(
                names.iter().any(|n| n == required),
                "tarball missing {required}; entries: {names:?}",
            );
        }
    }

    /// `stage_tarball` writes the embedded bytes to a tempfile that
    /// matches the embedded tarball byte-for-byte.
    #[test]
    fn stage_tarball_writes_embedded_bytes() {
        let path = stage_tarball().unwrap();
        assert!(path.exists());
        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!(on_disk, BOOTSTRAP_TARBALL);
    }

    /// `validate_agent_id` accepts shell-safe ids and rejects
    /// shell-special chars.
    #[test]
    fn validate_agent_id_rejects_shell_special_chars() {
        assert!(validate_agent_id("alpha-1.2_3").is_ok());
        assert!(validate_agent_id("").is_err());
        assert!(validate_agent_id(&"x".repeat(65)).is_err());
        for bad in ["a b", "a;b", "a$b", "a/b", "a'b", "a\"b", "a`b"] {
            assert!(
                validate_agent_id(bad).is_err(),
                "agent_id {bad:?} should be rejected",
            );
        }
    }

    /// The probe step parses `$HOME` and returns version=None when the
    /// remote `agent.version` file is missing (fresh host). The `|| true`
    /// in the probe shell command keeps exit status 0, so a missing
    /// version file is distinguishable from a real ssh failure.
    #[test]
    fn probe_returns_none_version_when_agent_version_missing() {
        let runner = FakeRunner::new();
        runner.expect_run("ssh", &["-o", "BatchMode=yes"], ok(b"/home/user\n"));
        let probe = probe_remote(&runner, "host").unwrap();
        assert_eq!(probe.home, PathBuf::from("/home/user"));
        assert!(probe.installed_version.is_none());
    }

    /// The probe step bubbles up `Error::Bootstrap{VersionProbe}`
    /// when ssh itself can't be invoked.
    #[test]
    fn probe_surfaces_spawn_failure_as_bootstrap_error() {
        let runner = FakeRunner::new();
        runner.expect_error(
            "ssh",
            &["-o", "BatchMode=yes"],
            std::io::Error::new(ErrorKind::NotFound, "ssh: command not found"),
        );
        let err = probe_remote(&runner, "host").unwrap_err();
        let Error::Bootstrap { stage, .. } = err else {
            panic!("expected Error::Bootstrap, got {err:?}");
        };
        assert_eq!(stage, Stage::VersionProbe);
    }

    /// The probe step returns the trimmed remote `agent.version`
    /// contents along with `$HOME` on success.
    #[test]
    fn probe_returns_trimmed_version_on_success() {
        let runner = FakeRunner::new();
        runner.expect_run(
            "ssh",
            &["-o", "BatchMode=yes"],
            ok(b"/home/user\ncodemuxd-sip-deadbeef00000000\n"),
        );
        let probe = probe_remote(&runner, "host").unwrap();
        assert_eq!(probe.home, PathBuf::from("/home/user"));
        assert_eq!(
            probe.installed_version.as_deref(),
            Some("codemuxd-sip-deadbeef00000000"),
        );
    }

    /// SSH-level failures (e.g. exit 255 for unreachable host) now
    /// surface as `Error::Bootstrap{VersionProbe}` instead of being
    /// silently downgraded to "no installed version". Previously the
    /// downgrade meant we'd run scp/build before noticing the failure;
    /// post-fix we also need `$HOME` for the tunnel forward spec, so
    /// there's no useful fallback path here.
    #[test]
    fn probe_surfaces_ssh_connection_failure_as_bootstrap_error() {
        let runner = FakeRunner::new();
        runner.expect_run(
            "ssh",
            &["-o", "BatchMode=yes"],
            fail(255, b"ssh: Could not resolve hostname no-such-host"),
        );
        let err = probe_remote(&runner, "no-such-host").unwrap_err();
        let Error::Bootstrap { stage, source } = err else {
            panic!("expected Bootstrap, got {err:?}");
        };
        assert_eq!(stage, Stage::VersionProbe);
        assert!(
            source.to_string().contains("Could not resolve"),
            "remote stderr should surface in source, got {source}",
        );
    }

    /// Empty stdout (echo somehow returned nothing) → error rather
    /// than silently using a bogus relative path for the tunnel.
    #[test]
    fn probe_surfaces_empty_stdout_as_bootstrap_error() {
        let runner = FakeRunner::new();
        runner.expect_run("ssh", &["-o", "BatchMode=yes"], ok(b""));
        let err = probe_remote(&runner, "host").unwrap_err();
        let Error::Bootstrap { stage, source } = err else {
            panic!("expected Bootstrap, got {err:?}");
        };
        assert_eq!(stage, Stage::VersionProbe);
        assert!(
            source.to_string().contains("empty"),
            "should mention empty stdout, got {source}",
        );
    }

    /// `scp_tarball` surfaces the right stage when scp exits non-zero.
    #[test]
    fn scp_failure_carries_scp_stage() {
        let runner = FakeRunner::new();
        runner.expect_run("ssh", &["-o", "BatchMode=yes"], ok(b""));
        runner.expect_run("scp", &["-B"], fail(1, b"scp: /tmp/x: Permission denied"));
        let local = std::env::temp_dir().join("codemux-fake-tarball.tar.gz");
        std::fs::write(&local, b"x").unwrap();
        let err = scp_tarball(&runner, "host", &local, "v1").unwrap_err();
        let Error::Bootstrap { stage, .. } = err else {
            panic!("expected Bootstrap, got {err:?}");
        };
        assert_eq!(stage, Stage::Scp);
    }

    /// `remote_build` formats the `cargo not found` hint specifically.
    #[test]
    fn remote_build_surfaces_cargo_not_found_hint() {
        let runner = FakeRunner::new();
        runner.expect_run(
            "ssh",
            &["-o", "BatchMode=yes"],
            fail(127, b"bash: cargo: command not found"),
        );
        let err = remote_build(&runner, "devbox", "v1").unwrap_err();
        let Error::Bootstrap { stage, source } = err else {
            panic!("expected Bootstrap, got {err:?}");
        };
        assert_eq!(stage, Stage::RemoteBuild);
        let msg = source.to_string();
        assert!(
            msg.contains("rustup.rs"),
            "expected rustup hint in source, got {msg}",
        );
        assert!(
            msg.contains("devbox"),
            "expected host name in source, got {msg}",
        );
    }

    /// Generic `remote_build` failure (not a cargo-missing case)
    /// still carries the `RemoteBuild` stage.
    #[test]
    fn remote_build_generic_failure_carries_stage() {
        let runner = FakeRunner::new();
        runner.expect_run(
            "ssh",
            &["-o", "BatchMode=yes"],
            fail(101, b"error[E0432]: unresolved import"),
        );
        let err = remote_build(&runner, "host", "v1").unwrap_err();
        let Error::Bootstrap { stage, .. } = err else {
            panic!("expected Bootstrap, got {err:?}");
        };
        assert_eq!(stage, Stage::RemoteBuild);
    }

    /// `spawn_remote_daemon` propagates the remote stderr verbatim
    /// (so `Error::CwdNotFound` from the remote daemon is visible
    /// in the local Bootstrap source chain).
    #[test]
    fn spawn_remote_daemon_propagates_remote_stderr() {
        let runner = FakeRunner::new();
        runner.expect_run(
            "ssh",
            &["-o", "BatchMode=yes"],
            fail(2, b"Error: cwd /no/such does not exist"),
        );
        let err = spawn_remote_daemon(&runner, "host", "alpha", Path::new("/no/such")).unwrap_err();
        let Error::Bootstrap { stage, source } = err else {
            panic!("expected Bootstrap, got {err:?}");
        };
        assert_eq!(stage, Stage::DaemonSpawn);
        assert!(
            source.to_string().contains("does not exist"),
            "remote stderr should surface in source, got {source}",
        );
    }

    /// `spawn_remote_daemon` rejects a cwd with an embedded single
    /// quote rather than risk shell-escaping it incorrectly.
    #[test]
    fn spawn_remote_daemon_rejects_quote_in_cwd() {
        let runner = FakeRunner::new();
        // No script entries — should error before ssh is invoked.
        let err = spawn_remote_daemon(&runner, "host", "alpha", Path::new("/tmp/with'quote"))
            .unwrap_err();
        let Error::Bootstrap { stage, source } = err else {
            panic!("expected Bootstrap, got {err:?}");
        };
        assert_eq!(stage, Stage::DaemonSpawn);
        assert!(
            source.to_string().contains("single quote"),
            "should mention single quote, got {source}",
        );
    }

    /// Regression for the silent ssh hang: `setsid -f` forks but does
    /// not reopen file descriptors, so the daemon inherits the SSH
    /// session's pipes for stdin/stdout/stderr. Without an explicit
    /// `</dev/null >/dev/null 2>&1`, ssh waits forever for those pipes
    /// to close (the daemon outlives ssh by design), and the local
    /// `runner.run("ssh", ...)` call hangs — the user sees a perpetual
    /// "bootstrapping" placeholder. We assert the redirect is present
    /// so a future refactor can't accidentally drop it. Verified by
    /// hand: `time ssh host 'setsid -f sleep 30'` hangs ≥3s; the same
    /// command with the redirect appended returns in <100ms.
    #[test]
    fn spawn_remote_daemon_redirects_stdio_to_devnull() {
        let runner = RecordingRunner::new();
        spawn_remote_daemon(&runner, "host", "alpha", Path::new("/tmp")).unwrap();
        let cmd = runner.last_run_cmd();
        assert!(
            cmd.contains("</dev/null >/dev/null 2>&1"),
            "cmd must redirect stdio to /dev/null so setsid -f can detach \
             cleanly without ssh hanging on inherited pipes; got: {cmd}",
        );
    }

    /// Regression: the daemon writes its socket under `$HOME/.cache/codemuxd/sockets/`,
    /// so the `-L local:remote` forward must use the absolute remote path captured
    /// by `probe_remote`. A relative path would land under whatever directory the
    /// SSH session opens in, which is wrong on hosts where `pwd != $HOME`.
    #[test]
    fn open_ssh_tunnel_uses_absolute_remote_path_in_forward_spec() {
        let runner = RecordingRunner::new();
        let mut child = open_ssh_tunnel(
            &runner,
            "host.example",
            "agent-x",
            Path::new("/tmp/codemux/agent-x.sock"),
            Path::new("/home/me"),
        )
        .unwrap();
        let args = runner.last_spawn_args();
        let l_index = args.iter().position(|a| a == "-L").expect("ssh -L missing");
        let forward = &args[l_index + 1];
        assert_eq!(
            forward, "/tmp/codemux/agent-x.sock:/home/me/.cache/codemuxd/sockets/agent-x.sock",
            "forward spec must pin the absolute remote socket path",
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    /// Regression: an active OpenSSH `ControlMaster` mux on the user's machine
    /// silently swallows `-L` forwards — the second `ssh -N` reuses the existing
    /// mux connection and never installs the listener, so the local socket file
    /// is never created. We pin `ControlPath=none` + `ControlMaster=no` to bypass
    /// the mux entirely and force a fresh connection that owns the forward.
    #[test]
    fn open_ssh_tunnel_bypasses_ssh_control_master() {
        let runner = RecordingRunner::new();
        let mut child = open_ssh_tunnel(
            &runner,
            "host.example",
            "agent-x",
            Path::new("/tmp/codemux/agent-x.sock"),
            Path::new("/home/me"),
        )
        .unwrap();
        let args = runner.last_spawn_args();
        let has_pair = |k: &str, v: &str| {
            args.windows(3).any(|w| {
                w[0] == "-o" && w[1] == k && w[2] == v || w[0] == "-o" && w[1] == format!("{k}={v}")
            })
        };
        assert!(
            has_pair("ControlPath", "none"),
            "ssh tunnel must set ControlPath=none to bypass mux; got: {args:?}",
        );
        assert!(
            has_pair("ControlMaster", "no"),
            "ssh tunnel must set ControlMaster=no to bypass mux; got: {args:?}",
        );
        let _ = child.kill();
        let _ = child.wait();
    }

    /// Tiny `CommandRunner` for tests that need to inspect the actual
    /// argv produced by a single bootstrap step. Returns success for
    /// every `run` call and spawns a real `sleep 60` for every
    /// `spawn_detached` call (the test is responsible for killing the
    /// returned Child). Stores the most-recent ssh argv vectors for
    /// post-call inspection. Unlike `FakeRunner`, no script setup is
    /// needed.
    struct RecordingRunner {
        last_run_args: Mutex<Option<Vec<String>>>,
        last_spawn_args: Mutex<Option<Vec<String>>>,
    }

    impl RecordingRunner {
        fn new() -> Self {
            Self {
                last_run_args: Mutex::new(None),
                last_spawn_args: Mutex::new(None),
            }
        }

        fn last_run_cmd(&self) -> String {
            // The bootstrap composes the remote shell command as the
            // last arg of the ssh argv (`ssh ... host '<cmd>'`), so
            // grabbing args.last() is equivalent to "the command we
            // sent the remote shell".
            self.last_run_args
                .lock()
                .unwrap()
                .clone()
                .and_then(|args| args.last().cloned())
                .expect("RecordingRunner.run was never called")
        }

        fn last_spawn_args(&self) -> Vec<String> {
            self.last_spawn_args
                .lock()
                .unwrap()
                .clone()
                .expect("RecordingRunner.spawn_detached was never called")
        }
    }

    impl CommandRunner for RecordingRunner {
        fn run(&self, _program: &str, args: &[&str]) -> std::io::Result<CommandOutput> {
            *self.last_run_args.lock().unwrap() =
                Some(args.iter().map(|s| (*s).to_string()).collect());
            Ok(CommandOutput {
                status: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }

        fn spawn_detached(&self, _: &str, args: &[&str]) -> std::io::Result<Child> {
            *self.last_spawn_args.lock().unwrap() =
                Some(args.iter().map(|s| (*s).to_string()).collect());
            Command::new("sleep").arg("60").spawn()
        }
    }

    /// `connect_socket` returns a `SocketConnect` Bootstrap error when
    /// the timeout expires with no live socket on the path.
    #[test]
    fn connect_socket_times_out_with_socket_connect_stage() {
        let dir = tempfile::tempdir().unwrap();
        let nope = dir.path().join("never-binds.sock");
        let err = connect_socket(&nope, Duration::from_millis(150)).unwrap_err();
        let Error::Bootstrap { stage, .. } = err else {
            panic!("expected Bootstrap, got {err:?}");
        };
        assert_eq!(stage, Stage::SocketConnect);
    }

    /// `connect_socket` succeeds against a freshly bound socket.
    #[test]
    fn connect_socket_succeeds_against_live_socket() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("live.sock");
        let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        // Spawn an accept thread so the connect actually completes.
        let _accepter = thread::spawn(move || {
            let _ = listener.accept();
        });
        let _ = connect_socket(&sock, Duration::from_secs(2)).unwrap();
    }

    /// `RealRunner::spawn_detached` returns a Child whose `wait()`
    /// reports the natural exit. Smoke test for the spawn path that
    /// `FakeRunner` can't exercise.
    #[test]
    fn real_runner_spawn_detached_smoke() {
        let runner = RealRunner;
        let mut child = runner.spawn_detached("sh", &["-c", "exit 0"]).unwrap();
        let status = child.wait().unwrap();
        assert!(status.success());
    }

    /// `RealRunner::run` captures stdout, stderr, and exit status.
    #[test]
    fn real_runner_run_captures_output() {
        let runner = RealRunner;
        let out = runner
            .run("sh", &["-c", "printf hello; printf err 1>&2; exit 7"])
            .unwrap();
        assert_eq!(out.status, 7);
        assert_eq!(out.stdout, b"hello");
        assert_eq!(out.stderr, b"err");
    }

    /// `TunnelGuard` kills the held child on drop. We confirm via
    /// `wait` returning a non-success status (kill on macOS yields
    /// signal exit; on Linux similar).
    #[test]
    fn tunnel_guard_kills_child_on_drop() {
        let child = Command::new("sleep")
            .arg("60")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        {
            let _guard = TunnelGuard::new(child);
            // Verify alive
            let alive = Command::new("kill")
                .args(["-0", &pid.to_string()])
                .status()
                .unwrap()
                .success();
            assert!(alive, "guarded child should be alive at this point");
        }
        // After drop, the kill should have landed. Give the kernel a
        // beat to reap.
        thread::sleep(Duration::from_millis(50));
        let alive = Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(!alive, "TunnelGuard drop should have killed the child");
    }

    /// `TunnelGuard::into_inner` extracts the Child without killing
    /// it; the Child is then the caller's responsibility.
    #[test]
    fn tunnel_guard_into_inner_disarms() {
        let child = Command::new("sleep")
            .arg("0.05")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut child = TunnelGuard::new(child).into_inner();
        // Naturally exits ~50ms in. Wait reaps.
        let status = child.wait().unwrap();
        assert!(status.success());
    }

    /// End-to-end orchestration: probe miss → stage → scp → build →
    /// daemon spawn → tunnel → connect. Uses the `FakeRunner` for all
    /// network-touching steps and a delayed-bind thread to simulate
    /// `ssh -L` creating the local socket file. Confirms the
    /// orchestration order and that the connect stream is returned
    /// cleanly. Passes a tempdir as `local_socket_dir` instead of
    /// mutating `$HOME` (the workspace forbids `unsafe`, and
    /// `std::env::set_var` is unsafe in 2024 edition).
    #[test]
    fn bootstrap_full_happy_path_against_fake_runner() {
        let dir = tempfile::tempdir().unwrap();
        let socket_dir = dir.path().join("sockets");
        // We do NOT pre-bind the listener — `local_socket_path` would
        // unlink it. Instead, spawn a thread that binds shortly after
        // bootstrap starts, simulating `ssh -L` creating the socket.
        let agent_id = "happy-agent";
        let local_sock = socket_dir.join(format!("{agent_id}.sock"));
        let local_sock_for_thread = local_sock.clone();
        let _binder = thread::spawn(move || {
            // Long enough to land after `local_socket_path`'s unlink,
            // short enough to land inside the connect retry budget.
            thread::sleep(Duration::from_millis(80));
            let listener = std::os::unix::net::UnixListener::bind(&local_sock_for_thread).unwrap();
            let _ = listener.accept();
        });

        let runner = FakeRunner::new();
        runner.expect_run("ssh", &["-o", "BatchMode=yes"], ok(b"/home/fake\n"));
        runner.expect_run("ssh", &["-o", "BatchMode=yes"], ok(b""));
        runner.expect_run("scp", &["-B"], ok(b""));
        runner.expect_run("ssh", &["-o", "BatchMode=yes"], ok(b"build ok"));
        runner.expect_run("ssh", &["-o", "BatchMode=yes"], ok(b""));
        runner.expect_spawn("ssh", &["-N"]);

        let (stream, mut tunnel) = bootstrap(
            &runner,
            "fake-host",
            agent_id,
            Path::new("/some/cwd"),
            &socket_dir,
        )
        .unwrap();
        // Stream is connected to our test listener; smoke-check by
        // confirming write doesn't error.
        let _ = (&stream).write_all(b"x");
        // Cleanup the dummy ssh tunnel subprocess
        let _ = tunnel.kill();
        let _ = tunnel.wait();
    }
}
