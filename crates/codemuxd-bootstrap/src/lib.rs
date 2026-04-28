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
//! - [`prepare_remote`] — runs steps 1-4 (probe + install if the
//!   remote's `agent.version` mismatches our embedded tarball) and
//!   returns a [`PreparedHost`] with the remote `$HOME`. Idempotent
//!   on already-installed hosts: probe matches → returns immediately.
//! - [`attach_agent`] — runs steps 5-7 (daemon spawn + tunnel +
//!   connect) followed by the wire handshake via `SshDaemonPty::attach`,
//!   returning a ready-to-use [`AgentTransport`]. Takes a fresh `cwd`
//!   so the spawn modal can let the user pick a remote folder *between*
//!   the two phases without holding the prepare result hostage.
//! - [`RemoteFs`] — long-lived `ssh -M -N` `ControlMaster` used by the
//!   spawn modal to autocomplete remote directories cheaply between
//!   `prepare_remote` and `attach_agent`. Lives in [`remote_fs`].
//! - [`CommandRunner`] / [`RealRunner`] — pluggable shim around
//!   `std::process::Command` so failure modes are unit-testable
//!   without touching the network.
//! - [`Error`] / [`Stage`] — the error envelope.
//!
//! # The 7 steps
//!
//! Phase 1 — [`prepare_remote`]:
//! 1. **Probe**: `ssh host 'cat ~/.cache/codemuxd/agent.version'`. If
//!    the file matches our [`bootstrap_version`], skip steps 2-4.
//! 2. **Stage tarball**: write the embedded daemon source archive to
//!    a local tempfile. Cached process-wide so a second `prepare_remote()`
//!    in the same TUI session reuses it.
//! 3. **scp**: copy the tarball to the remote.
//! 4. **Remote build**: untar, `cargo build --release --bin codemuxd`,
//!    move the binary into place, write `agent.version`.
//!
//! Phase 2 — [`attach_agent`]:
//! 5. **Spawn daemon**: `ssh host 'setsid -f codemuxd ...'`. The
//!    `setsid -f` invocation detaches the daemon from the SSH session
//!    so it survives when the SSH connection closes.
//! 6. **Tunnel**: `ssh -N -L /local.sock:/remote.sock host` in a
//!    background subprocess. Uses OpenSSH unix-socket forwarding
//!    (≥6.7 on both ends). The handle is returned so the caller can
//!    kill it on Drop.
//! 7. **Connect**: `UnixStream::connect` against the local end of the
//!    tunnel, with a short retry loop while the daemon's `bind()` and
//!    the tunnel's first-packet warmup converge. Followed by the wire
//!    `Hello`/`HelloAck` handshake to produce the [`AgentTransport`].
//!
//! # Mockable command execution
//!
//! All ssh/scp invocations go through a [`CommandRunner`] trait so
//! tests can inject scripted responses without touching the network.
//! [`RealRunner`] is the production implementation; the unit tests in
//! this module use a `FakeRunner` that maps `(program, args_prefix)`
//! pairs to canned [`CommandOutput`] / [`std::io::Error`] values.

pub mod error;
pub mod remote_fs;

pub use crate::remote_fs::{DirEntry, MAX_LIST_ENTRIES, RemoteFs, RemoteFsError};

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

/// Result of [`prepare_remote`] — everything the spawn modal and
/// [`attach_agent`] need from the install/probe phase.
///
/// Deliberately minimal: the `ControlMaster` connection that powers
/// fast remote-directory autocomplete lives in [`RemoteFs`], owned
/// separately by the runtime. Coupling them here would force
/// `attach_agent` to carry a dead `ControlMaster` after the modal
/// closed (it's only used for `ls`, not for the wire transport).
#[derive(Debug, Clone)]
pub struct PreparedHost {
    /// Absolute path of the remote `$HOME`, captured by the probe step.
    /// Load-bearing for the spawn modal's remote-path picker (the base
    /// directory the wildmenu starts in) and for [`attach_agent`]'s
    /// `ssh -L` forward spec (sshd's remote half does not shell-expand
    /// `~`/`$HOME` — see notes on [`open_ssh_tunnel`]).
    pub remote_home: PathBuf,
    /// Whether the install branch ran (probe saw a version mismatch and
    /// we scp'd + built a fresh binary). When `true`, any daemon already
    /// running on the remote for this agent-id is by definition stale —
    /// it loaded the OLD binary into memory before we replaced the file
    /// on disk, and `pid_file` exclusivity will block a fresh spawn.
    /// [`attach_agent`] reads this flag and force-respawns the daemon
    /// before calling `setsid -f` so the new client doesn't get routed
    /// to the stale process.
    pub binary_was_updated: bool,
}

/// Inputs to [`attach_agent`] (and the worker entry points around it).
/// Bundled into a struct because the flat 9-arg signature was a Long
/// Parameter List smell — six of those were trivially-typed strings /
/// `u16`s and easy to swap by mistake. Owned fields (no lifetimes) so
/// the worker thread can move the whole config in a single send.
#[derive(Debug, Clone)]
pub struct AttachConfig {
    /// Hostname (as in `~/.ssh/config`); same string passed to
    /// [`prepare_remote`].
    pub host: String,
    /// Agent identifier — used in socket / pid / log filenames on the
    /// remote and as the daemon's `--agent-id` value. Must satisfy
    /// the validator (ASCII alphanumerics + `-._`, ≤ 64 chars), which
    /// runs at the start of [`attach_agent`].
    pub agent_id: String,
    /// Optional remote cwd. `None` → omit `--cwd`, daemon inherits the
    /// SSH login shell's cwd (`$HOME` on a typical login). `Some` →
    /// daemon `chdir`s before spawning `claude`; daemon refuses to
    /// bind if the path doesn't exist on the remote (vision principle
    /// 6, no silent fallback).
    pub cwd: Option<PathBuf>,
    /// Where the local end of the `ssh -L` tunnel binds. Production
    /// callers pass [`default_local_socket_dir`]; tests pass a tempdir
    /// to avoid mutating `$HOME`.
    pub local_socket_dir: PathBuf,
    /// Initial PTY geometry sent in the wire `Hello`. Resized later by
    /// the runtime if the terminal changes during the attach.
    pub rows: u16,
    pub cols: u16,
}

/// Phase 1 of the SSH bootstrap: probe the remote and (re)install
/// `codemuxd` if its installed version doesn't match the embedded
/// tarball. Idempotent on already-installed hosts — probe matches →
/// returns immediately.
///
/// Split out so the spawn modal can pause between install and daemon
/// spawn to let the user pick a remote folder. The remote `$HOME`
/// captured here flows into both the modal (for autocomplete) and
/// [`attach_agent`] (for the tunnel forward spec).
///
/// `on_stage` is invoked at the start of each install stage
/// ([`Stage::VersionProbe`] always; [`Stage::TarballStage`] /
/// [`Stage::Scp`] / [`Stage::RemoteBuild`] only if the probe shows a
/// version mismatch). Same threading rules as [`attach_agent`]'s
/// callback — non-blocking work is fine.
///
/// # Errors
/// Any failure surfaces as [`Error::Bootstrap`] with the [`Stage`] that
/// tripped.
pub fn prepare_remote(
    runner: &dyn CommandRunner,
    on_stage: impl Fn(Stage),
    host: &str,
) -> Result<PreparedHost, Error> {
    let target_version = bootstrap_version();
    on_stage(Stage::VersionProbe);
    let probe = probe_remote(runner, host)?;
    let binary_was_updated = probe.installed_version.as_deref() != Some(target_version);
    if binary_was_updated {
        on_stage(Stage::TarballStage);
        let local_tarball = stage_tarball()?;
        on_stage(Stage::Scp);
        scp_tarball(runner, host, &local_tarball, target_version)?;
        on_stage(Stage::RemoteBuild);
        remote_build(runner, host, target_version)?;
    }
    Ok(PreparedHost {
        remote_home: probe.home,
        binary_was_updated,
    })
}

/// Phase 2 of the SSH bootstrap: spawn the remote daemon at the
/// chosen `cwd`, open the tunnel, connect, and perform the
/// [`SshDaemonPty::attach`] wire handshake. Returns a fully constructed
/// [`AgentTransport`] ready for the runtime.
///
/// `prepared` carries the remote `$HOME` from [`prepare_remote`] —
/// load-bearing for the tunnel forward spec (sshd does not shell-expand
/// `~` or `$HOME` in `-L` paths). The two phases are deliberately
/// separable so the spawn modal can let the user navigate the remote
/// filesystem (via [`RemoteFs`]) between probe/install and daemon spawn.
///
/// `cfg` carries the user-facing inputs (host, agent id, cwd, socket
/// dir, geometry). See [`AttachConfig`] for field semantics.
///
/// `on_stage` is invoked at the start of stages 5-7. Same callback
/// semantics as [`prepare_remote`].
///
/// # Errors
/// Returns [`Error::Bootstrap`] for any of stages 5-7, or
/// [`Error::Session`] when the post-tunnel wire handshake fails.
pub fn attach_agent(
    runner: &dyn CommandRunner,
    on_stage: impl Fn(Stage),
    prepared: &PreparedHost,
    cfg: &AttachConfig,
) -> Result<AgentTransport, Error> {
    let (stream, tunnel) = attach_socket(runner, on_stage, prepared, cfg)?;
    let label = format!("{}:{}", cfg.host, cfg.agent_id);
    SshDaemonPty::attach(
        stream,
        label,
        &cfg.agent_id,
        cfg.rows,
        cfg.cols,
        Some(tunnel),
    )
    .map(AgentTransport::SshDaemon)
    .map_err(|source| Error::Session {
        source: Box::new(source),
    })
}

/// Stages 5-7 without the wire handshake: spawns the daemon, opens
/// the tunnel, connects. Returns the connected [`UnixStream`] plus the
/// tunnel `Child` (caller owns cleanup via `Child::kill`, typically by
/// handing it to [`SshDaemonPty::attach`]).
///
/// Private helper so the unit tests can exercise the orchestration
/// (probe → install → daemon → tunnel → connect) without needing to
/// stand up a real handshake. The public [`attach_agent`] composes
/// this with [`SshDaemonPty::attach`].
fn attach_socket(
    runner: &dyn CommandRunner,
    on_stage: impl Fn(Stage),
    prepared: &PreparedHost,
    cfg: &AttachConfig,
) -> Result<(UnixStream, Child), Error> {
    validate_agent_id(&cfg.agent_id)?;

    on_stage(Stage::DaemonSpawn);
    spawn_remote_daemon(
        runner,
        &cfg.host,
        &cfg.agent_id,
        cfg.cwd.as_deref(),
        prepared.binary_was_updated,
    )?;

    on_stage(Stage::SocketTunnel);
    let local_socket = local_socket_path(&cfg.local_socket_dir, &cfg.agent_id)?;
    let tunnel = open_ssh_tunnel(
        runner,
        &cfg.host,
        &cfg.agent_id,
        &local_socket,
        &prepared.remote_home,
    )?;
    let tunnel_guard = TunnelGuard::new(tunnel);

    on_stage(Stage::SocketConnect);
    let stream = connect_socket(&local_socket, CONNECT_TIMEOUT)?;
    Ok((stream, tunnel_guard.into_inner()))
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
/// its path. Cached per-process so a second [`prepare_remote`] in the
/// same TUI session doesn't pay for the disk write twice.
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
    // One ssh invocation amortizes handshake cost. Order is
    // load-bearing — `agent.version` is written last so a partial
    // build doesn't leave a misleading version marker on disk.
    //
    // The pre-fix recipe used `cargo build … 2>&1 | tee build.log`,
    // which silently swallows cargo's exit code: a pipeline's exit
    // status is the rightmost command's, `tee` always succeeds, so
    // `set -e` never trips. The script then marched on to
    // `mv target/release/codemuxd …` and surfaced the misleading
    // `mv: cannot stat …` instead of the real rustc diagnostic.
    //
    // Portable POSIX has no `pipefail`, so we capture cargo's exit
    // explicitly via the `cmd || rc=$?` idiom, redirect cargo's
    // output to `build.log`, and dump the log tail to stderr on
    // failure so the actual compile diagnostic reaches the user
    // (modal banner + tracing log) instead of `mv`'s downstream
    // ENOENT noise.
    let cmd = format!(
        "set -e\n\
         cd ~/.cache/codemuxd/src\n\
         tar -xzf {version}.tar.gz\n\
         cargo_status=0\n\
         cargo build --release --bin codemuxd > build.log 2>&1 || cargo_status=$?\n\
         if [ $cargo_status -ne 0 ]; then\n\
         echo '--- build.log tail (last 50 lines) ---' >&2\n\
         tail -50 build.log >&2\n\
         exit $cargo_status\n\
         fi\n\
         mkdir -p ~/.cache/codemuxd/bin\n\
         mv target/release/codemuxd ~/.cache/codemuxd/bin/codemuxd\n\
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
        // The remote shell formats "binary not on PATH" errors
        // differently per shell. Cover the three common cases:
        //   sh/dash → "sh: cargo: not found"
        //   bash    → "bash: cargo: command not found"
        //   zsh     → "zsh:N: command not found: cargo"   ← reversed order
        // The colon-anchored substrings are specific enough that
        // false positives from a real cargo build error mentioning
        // "not found" (e.g. crate not found) are unlikely.
        let cargo_missing = stderr_text.contains("cargo: not found")
            || stderr_text.contains("cargo: command not found")
            || stderr_text.contains("command not found: cargo");
        let hint = if cargo_missing {
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
/// `force_respawn` is set when [`prepare_remote`] just installed a
/// fresh binary. Any daemon already running for this agent-id is by
/// definition stale (it loaded the OLD binary into memory before we
/// replaced the file on disk). Worse, its `pid_file` exclusivity will
/// silently block the new `setsid -f codemuxd` from binding — `setsid`
/// itself succeeds (so the outer ssh exits 0), but the new daemon
/// process exits a moment later when its `bind()` fails. The tunnel
/// then connects to the old daemon, the wire handshake works, and the
/// user gets the OLD daemon's behavior on what looks like a fresh
/// install. Without this kill, every codemux upgrade strands users on
/// the previous version's daemon until they manually `pkill codemuxd`.
///
/// The kill is `kill -TERM` followed by a brief sleep, then `kill
/// -KILL` if still alive. We give SIGTERM a chance because the daemon
/// has a `Drop` cleanup path (kills its child PTY, removes pid file)
/// that SIGKILL skips. The cost: the in-flight Claude session on the
/// remote dies. That's the right tradeoff at upgrade time — a stale
/// daemon serving forever is strictly worse than a one-time session
/// loss when the user knowingly installed a new version.
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
    cwd: Option<&Path>,
    force_respawn: bool,
) -> Result<(), Error> {
    let cwd_flag = match cwd {
        None => String::new(),
        Some(p) => {
            let cwd_str = p.to_str().ok_or_else(|| Error::Bootstrap {
                stage: Stage::DaemonSpawn,
                source: format!("cwd not UTF-8: {}", p.display()).into(),
            })?;
            if cwd_str.contains('\'') {
                return Err(Error::Bootstrap {
                    stage: Stage::DaemonSpawn,
                    source: format!(
                        "cwd contains a single quote, refusing to shell-escape: {cwd_str:?}"
                    )
                    .into(),
                });
            }
            format!(" --cwd '{cwd_str}'")
        }
    };
    // When the binary was just updated, send SIGTERM to any existing
    // daemon for this agent-id, give it a second to release the pid
    // file and socket via its Drop cleanup, then SIGKILL anything still
    // alive. `sleep 1` (integer) instead of fractional seconds — POSIX
    // sleep does not accept fractions, so `sleep 0.2` would error on
    // dash/busybox shells. The extra ~800ms is invisible against the
    // multi-second cargo build that already ran on the upgrade path.
    // The `2>/dev/null` swallows "no such process" noise — both signals
    // are best-effort on a possibly-empty pid file. The `|| true` keeps
    // `set -e` quiet in the wrapping shell.
    let respawn_prelude = if force_respawn {
        format!(
            "if [ -s ~/.cache/codemuxd/pids/{agent_id}.pid ]; then \
               pid=$(cat ~/.cache/codemuxd/pids/{agent_id}.pid); \
               kill -TERM $pid 2>/dev/null || true; \
               sleep 1; \
               kill -KILL $pid 2>/dev/null || true; \
               rm -f ~/.cache/codemuxd/pids/{agent_id}.pid \
                     ~/.cache/codemuxd/sockets/{agent_id}.sock; \
             fi; "
        )
    } else {
        String::new()
    };
    // After `setsid -f` returns, poll for the daemon's socket file to
    // appear. `setsid -f` itself returns 0 as soon as its fork
    // completes — even if the spawned codemuxd then exits a millisecond
    // later because `bind()` failed, the wrong --log-file path was
    // unwritable, the cwd doesn't exist on the remote, etc. Without
    // this verification the bootstrap reports success, the local
    // tunnel connects, the wire handshake fails 5s later as a
    // `SocketConnect` timeout, and the user sees a misleading "could
    // not connect to remote daemon socket" with no diagnostic. The
    // verification keeps the failure where it belongs (`DaemonSpawn`)
    // and surfaces the daemon's log tail so the user knows WHY it
    // didn't come up.
    //
    // Polling is integer-second `sleep` (POSIX) up to 5 iterations =
    // 5 s, mirroring `connect_socket`'s budget on the local side. A
    // healthy daemon binds in milliseconds; the 5 s budget is for cold
    // starts on slow disks, never the steady state.
    //
    // The `tail -n 20 log 2>/dev/null` on failure is best-effort: if
    // the daemon couldn't even open its log file, the tail returns
    // empty and we still emit the "socket did not appear" message,
    // which is a strict improvement over silent success.
    let cmd = format!(
        "{respawn_prelude}\
         setsid -f ~/.cache/codemuxd/bin/codemuxd \
         --socket ~/.cache/codemuxd/sockets/{agent_id}.sock \
         --pid-file ~/.cache/codemuxd/pids/{agent_id}.pid \
         --log-file ~/.cache/codemuxd/logs/{agent_id}.log \
         --agent-id {agent_id}{cwd_flag} \
         </dev/null >/dev/null 2>&1\n\
         i=0\n\
         while [ ! -S ~/.cache/codemuxd/sockets/{agent_id}.sock ]; do \
           i=$((i + 1)); \
           if [ $i -ge 5 ]; then \
             echo 'daemon socket did not appear within 5s' >&2; \
             echo '--- log tail (~/.cache/codemuxd/logs/{agent_id}.log) ---' >&2; \
             tail -n 20 ~/.cache/codemuxd/logs/{agent_id}.log 2>/dev/null >&2 \
               || echo '(log file missing or unreadable)' >&2; \
             exit 1; \
           fi; \
           sleep 1; \
         done"
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
///
/// `ServerAliveInterval=15` + `ServerAliveCountMax=3` are
/// **resilience opts**: without them, ssh has no liveness probe on
/// an idle persistent tunnel. A NAT timeout, devpod hibernation, or
/// transient network drop leaves the tunnel half-open — the kernel
/// hasn't seen a RST so the socket stays valid, the framed reader
/// thread blocks indefinitely on `read()`, and the agent appears
/// frozen with no transition to `Crashed`. With these opts ssh sends
/// a probe every 15 s and exits after 3 missed responses (~45 s
/// total), at which point the tunnel `Child` dies, the framed
/// reader's socket read returns EOF, and `SshDaemonPty::try_wait`
/// flips to `Some(-1)`. 45 s is short enough to feel responsive and
/// long enough to ride through normal jitter.
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
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
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

    /// **Drift guard for the bootstrap manifest.** The remote build uses
    /// a self-contained workspace manifest (`bootstrap-root/Cargo.toml`)
    /// that mirrors the parent workspace's `[workspace.dependencies]`.
    /// If a daemon or wire crate gains a new `workspace = true` dep but
    /// the bootstrap manifest is not updated, `cargo build` on the
    /// remote fails with "no such workspace dependency", and the user
    /// sees a `RemoteBuild` error on first SSH attach. This test
    /// catches that drift at `cargo test` time so the failure mode is
    /// local, not in front of the user.
    #[test]
    fn bootstrap_manifest_mirrors_every_workspace_dep_used_by_daemon() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace_root = manifest_dir.parent().unwrap().parent().unwrap();
        let bootstrap_root = manifest_dir.join("bootstrap-root").join("Cargo.toml");
        let bootstrap_deps = workspace_dependency_names(&bootstrap_root);

        for crate_relpath in ["apps/daemon", "crates/wire"] {
            let manifest = workspace_root.join(crate_relpath).join("Cargo.toml");
            for dep in workspace_true_dependency_names(&manifest) {
                assert!(
                    bootstrap_deps.contains(&dep),
                    "drift: `{crate_relpath}` declares `{dep}.workspace = true`, but \
                     `bootstrap-root/Cargo.toml` does not list `{dep}` in \
                     [workspace.dependencies]. Mirror the dep there or the remote \
                     `cargo build` will fail to resolve it."
                );
            }
        }
    }

    /// Read a Cargo.toml, return the keys under `[workspace.dependencies]`.
    fn workspace_dependency_names(manifest: &Path) -> std::collections::HashSet<String> {
        let raw = std::fs::read_to_string(manifest)
            .unwrap_or_else(|e| panic!("read {}: {e}", manifest.display()));
        let value: toml::Value =
            toml::from_str(&raw).unwrap_or_else(|e| panic!("parse {}: {e}", manifest.display()));
        value
            .get("workspace")
            .and_then(|w| w.get("dependencies"))
            .and_then(|d| d.as_table())
            .map(|t| t.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// Read a Cargo.toml, return the keys under `[dependencies]` that
    /// are declared with `workspace = true` (i.e. inherit version from
    /// the workspace root). Bare-string deps and explicitly-versioned
    /// deps are excluded — they don't need to be mirrored.
    fn workspace_true_dependency_names(manifest: &Path) -> Vec<String> {
        let raw = std::fs::read_to_string(manifest)
            .unwrap_or_else(|e| panic!("read {}: {e}", manifest.display()));
        let value: toml::Value =
            toml::from_str(&raw).unwrap_or_else(|e| panic!("parse {}: {e}", manifest.display()));
        let Some(deps) = value.get("dependencies").and_then(|d| d.as_table()) else {
            return Vec::new();
        };
        deps.iter()
            .filter_map(|(name, spec)| {
                let workspace_flag = spec
                    .as_table()
                    .and_then(|t| t.get("workspace"))
                    .and_then(toml::Value::as_bool)
                    .unwrap_or(false);
                workspace_flag.then(|| name.clone())
            })
            .collect()
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

    /// zsh formats the missing-binary error as
    /// `zsh:N: command not found: cargo` — note the reversed word
    /// order vs bash. The detector must handle this so users on
    /// zsh-default boxes (devpods, macOS hosts) get the rustup hint
    /// rather than the generic compile-error fallback.
    #[test]
    fn remote_build_surfaces_cargo_not_found_hint_on_zsh() {
        let runner = FakeRunner::new();
        runner.expect_run(
            "ssh",
            &["-o", "BatchMode=yes"],
            fail(
                127,
                b"--- build.log tail (last 50 lines) ---\nzsh:5: command not found: cargo\n",
            ),
        );
        let err = remote_build(&runner, "devpod", "v1").unwrap_err();
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
            msg.contains("devpod"),
            "expected host name in source, got {msg}",
        );
    }

    /// sh/dash formats the missing-binary error as
    /// `sh: cargo: not found` (no "command" word). Locked in so
    /// the OR chain doesn't accidentally drop this case during a
    /// future cleanup.
    #[test]
    fn remote_build_surfaces_cargo_not_found_hint_on_sh() {
        let runner = FakeRunner::new();
        runner.expect_run(
            "ssh",
            &["-o", "BatchMode=yes"],
            fail(127, b"sh: 1: cargo: not found"),
        );
        let err = remote_build(&runner, "alpine", "v1").unwrap_err();
        let Error::Bootstrap { stage, source } = err else {
            panic!("expected Bootstrap, got {err:?}");
        };
        assert_eq!(stage, Stage::RemoteBuild);
        let msg = source.to_string();
        assert!(
            msg.contains("rustup.rs"),
            "expected rustup hint in source, got {msg}",
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
        let err = spawn_remote_daemon(&runner, "host", "alpha", Some(Path::new("/no/such")), false)
            .unwrap_err();
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
        let err = spawn_remote_daemon(
            &runner,
            "host",
            "alpha",
            Some(Path::new("/tmp/with'quote")),
            false,
        )
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
        spawn_remote_daemon(&runner, "host", "alpha", Some(Path::new("/tmp")), false).unwrap();
        let cmd = runner.last_run_cmd();
        assert!(
            cmd.contains("</dev/null >/dev/null 2>&1"),
            "cmd must redirect stdio to /dev/null so setsid -f can detach \
             cleanly without ssh hanging on inherited pipes; got: {cmd}",
        );
    }

    /// `cwd: None` ⇒ omit the `--cwd` flag from the daemon command line so the
    /// daemon falls back to the SSH login shell's cwd ($HOME). Pre-fix the TUI
    /// always sent its local cwd verbatim, which tripped the daemon's
    /// `cwd.exists()` check on the remote and surfaced as "EOF before `HelloAck`".
    #[test]
    fn spawn_remote_daemon_omits_cwd_flag_when_none() {
        let runner = RecordingRunner::new();
        spawn_remote_daemon(&runner, "host", "alpha", None, false).unwrap();
        let cmd = runner.last_run_cmd();
        assert!(
            !cmd.contains("--cwd"),
            "cmd must not include --cwd when cwd is None; got: {cmd}",
        );
    }

    /// `cwd: Some(p)` ⇒ include `--cwd '<p>'` in the daemon command line. Pin
    /// the exact format so a future refactor can't silently drop the daemon's
    /// chdir target.
    #[test]
    fn spawn_remote_daemon_includes_cwd_flag_when_some() {
        let runner = RecordingRunner::new();
        spawn_remote_daemon(
            &runner,
            "host",
            "alpha",
            Some(Path::new("/srv/work")),
            false,
        )
        .unwrap();
        let cmd = runner.last_run_cmd();
        assert!(
            cmd.contains("--cwd '/srv/work'"),
            "cmd must include --cwd '/srv/work' when cwd is Some; got: {cmd}",
        );
    }

    /// `force_respawn = false` keeps the prior recipe verbatim — no kill
    /// prelude, no rm -f. Reattaching to a same-version daemon must
    /// preserve the existing process (session continuity, AD-26).
    #[test]
    fn spawn_remote_daemon_omits_kill_prelude_when_not_force_respawn() {
        let runner = RecordingRunner::new();
        spawn_remote_daemon(&runner, "host", "alpha", None, false).unwrap();
        let cmd = runner.last_run_cmd();
        assert!(
            !cmd.contains("kill"),
            "non-force-respawn must not kill any running daemon; got: {cmd}",
        );
        assert!(
            !cmd.contains("rm -f"),
            "non-force-respawn must not remove pid/socket files; got: {cmd}",
        );
    }

    /// `force_respawn = true` ⇒ kill prelude precedes the `setsid` line.
    /// Ordering is load-bearing: SIGTERM first (so the old daemon's Drop
    /// runs and removes the pid file cleanly), brief sleep, then SIGKILL
    /// for anything that ignored TERM, then rm -f the pid+socket files
    /// in case the daemon couldn't clean up. Without this, a stale
    /// daemon from a previous version keeps its pid lock and the new
    /// `setsid -f codemuxd` silently fails (`setsid` itself succeeds so
    /// ssh exits 0). The user then connects to the OLD daemon over the
    /// pre-existing socket, with no diagnostic that anything went wrong.
    #[test]
    fn spawn_remote_daemon_runs_kill_prelude_when_force_respawn() {
        let runner = RecordingRunner::new();
        spawn_remote_daemon(&runner, "host", "alpha", None, true).unwrap();
        let cmd = runner.last_run_cmd();
        // Both signals must appear and target the pid file by path.
        assert!(
            cmd.contains("kill -TERM"),
            "force-respawn must SIGTERM existing daemon; got: {cmd}",
        );
        assert!(
            cmd.contains("kill -KILL"),
            "force-respawn must SIGKILL post-grace-period; got: {cmd}",
        );
        assert!(
            cmd.contains("~/.cache/codemuxd/pids/alpha.pid"),
            "kill must target the agent-id's pid file; got: {cmd}",
        );
        // Pre-spawn cleanup of pid file and socket so the new daemon
        // gets a clean acquire path even if SIGKILL raced with cleanup.
        assert!(
            cmd.contains("rm -f"),
            "force-respawn must rm pid+socket post-kill; got: {cmd}",
        );
        // Kill prelude precedes the setsid invocation in the same
        // ssh shell (one round-trip) — assert the textual order so a
        // refactor can't accidentally interleave them.
        let kill_pos = cmd.find("kill -TERM").unwrap();
        let setsid_pos = cmd.find("setsid -f").unwrap();
        assert!(
            kill_pos < setsid_pos,
            "kill prelude must precede setsid in the same shell; got: {cmd}",
        );
    }

    /// Spawn verification: after `setsid -f` returns, the shell polls
    /// for the daemon's socket file to appear and exits non-zero with
    /// a log tail if it doesn't. This catches "setsid forked
    /// successfully but codemuxd then crashed during `bind()`" — without
    /// it, the bootstrap reports success and the user sees a
    /// `SocketConnect` timeout 5 s later with no clue why. Polling
    /// must use POSIX `sleep <integer>`, not fractional seconds, so
    /// dash/busybox shells don't reject the script.
    #[test]
    fn spawn_remote_daemon_verifies_socket_appearance_post_spawn() {
        let runner = RecordingRunner::new();
        spawn_remote_daemon(&runner, "host", "alpha", None, false).unwrap();
        let cmd = runner.last_run_cmd();
        // Verification loop must check for the agent's socket path,
        // bail with a diagnostic if it doesn't appear, and tail the
        // daemon log so the failure surfaces somewhere actionable.
        assert!(
            cmd.contains("[ ! -S ~/.cache/codemuxd/sockets/alpha.sock ]"),
            "verification must check the agent's socket path exists; got: {cmd}",
        );
        assert!(
            cmd.contains("daemon socket did not appear"),
            "verification must emit a recognizable failure message; got: {cmd}",
        );
        assert!(
            cmd.contains("tail -n") && cmd.contains("alpha.log"),
            "verification failure must tail the daemon's log file so the user \
             sees WHY the socket didn't come up; got: {cmd}",
        );
        // POSIX `sleep` accepts only integers. The earlier 0.2-s
        // version errored on dash; pin the integer form.
        assert!(
            cmd.contains("sleep 1"),
            "verification poll must sleep an integer (POSIX); got: {cmd}",
        );
        // Verification block must come AFTER the setsid invocation
        // (you can't poll for a socket the daemon hasn't been asked
        // to bind yet).
        let setsid_pos = cmd.find("setsid -f").unwrap();
        let verify_pos = cmd
            .find("[ ! -S ~/.cache/codemuxd/sockets/alpha.sock ]")
            .unwrap();
        assert!(
            setsid_pos < verify_pos,
            "verification must run after setsid; got: {cmd}",
        );
    }

    /// Regression: the daemon writes its socket under `$HOME/.cache/codemuxd/sockets/`,
    /// so the `-L local:remote` forward must use the absolute remote path captured
    /// by `probe_remote`. A relative path would land under whatever directory the
    /// SSH session opens in, which is wrong on hosts where `pwd != $HOME`.
    #[test]
    fn open_ssh_tunnel_uses_absolute_remote_path_in_forward_spec() {
        let runner = RecordingRunner::new();
        let _guard = SpawnedChildGuard::new(
            open_ssh_tunnel(
                &runner,
                "host.example",
                "agent-x",
                Path::new("/tmp/codemux/agent-x.sock"),
                Path::new("/home/me"),
            )
            .unwrap(),
        );
        let args = runner.last_spawn_args();
        let l_index = args.iter().position(|a| a == "-L").expect("ssh -L missing");
        let forward = &args[l_index + 1];
        assert_eq!(
            forward, "/tmp/codemux/agent-x.sock:/home/me/.cache/codemuxd/sockets/agent-x.sock",
            "forward spec must pin the absolute remote socket path",
        );
    }

    /// Regression: an active OpenSSH `ControlMaster` mux on the user's machine
    /// silently swallows `-L` forwards — the second `ssh -N` reuses the existing
    /// mux connection and never installs the listener, so the local socket file
    /// is never created. We pin `ControlPath=none` + `ControlMaster=no` to bypass
    /// the mux entirely and force a fresh connection that owns the forward.
    #[test]
    fn open_ssh_tunnel_bypasses_ssh_control_master() {
        let runner = RecordingRunner::new();
        let _guard = SpawnedChildGuard::new(
            open_ssh_tunnel(
                &runner,
                "host.example",
                "agent-x",
                Path::new("/tmp/codemux/agent-x.sock"),
                Path::new("/home/me"),
            )
            .unwrap(),
        );
        let args = runner.last_spawn_args();
        assert!(
            ssh_arg_pair_present(&args, "ControlPath", "none"),
            "ssh tunnel must set ControlPath=none to bypass mux; got: {args:?}",
        );
        assert!(
            ssh_arg_pair_present(&args, "ControlMaster", "no"),
            "ssh tunnel must set ControlMaster=no to bypass mux; got: {args:?}",
        );
    }

    /// Regression: without `ServerAlive` opts, a silent network drop (NAT
    /// timeout, devpod hibernation) leaves the tunnel half-open and the
    /// agent appears frozen — the framed reader never sees EOF and
    /// `try_wait` never flips. With these opts ssh probes every 15 s
    /// and exits after 3 missed responses (~45 s), which gives the TUI
    /// a clean transition to `Crashed` instead of a silent hang.
    #[test]
    fn open_ssh_tunnel_sets_server_alive_opts() {
        let runner = RecordingRunner::new();
        let _guard = SpawnedChildGuard::new(
            open_ssh_tunnel(
                &runner,
                "host.example",
                "agent-x",
                Path::new("/tmp/codemux/agent-x.sock"),
                Path::new("/home/me"),
            )
            .unwrap(),
        );
        let args = runner.last_spawn_args();
        assert!(
            ssh_arg_pair_present(&args, "ServerAliveInterval", "15"),
            "ssh tunnel must set ServerAliveInterval=15 for liveness; got: {args:?}",
        );
        assert!(
            ssh_arg_pair_present(&args, "ServerAliveCountMax", "3"),
            "ssh tunnel must set ServerAliveCountMax=3 for liveness; got: {args:?}",
        );
    }

    /// RAII guard for a spawned ssh `Child` in tests. Kills and reaps
    /// the child on drop, including when the test panics partway
    /// through (which `let _ = child.kill();` after the asserts would
    /// silently leak as a zombie).
    struct SpawnedChildGuard(Child);

    impl SpawnedChildGuard {
        fn new(child: Child) -> Self {
            Self(child)
        }
    }

    impl Drop for SpawnedChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// Returns true if the ssh `args` slice contains an `-o key=value`
    /// pair, accepting both the split form (`-o`, `key`, `value`) and
    /// the joined form (`-o`, `key=value`). Used by every
    /// argv-inspection test in this module so each can stay declarative
    /// instead of redefining the same scan.
    fn ssh_arg_pair_present(args: &[String], key: &str, value: &str) -> bool {
        args.windows(3).any(|w| {
            w[0] == "-o" && w[1] == key && w[2] == value
                || w[0] == "-o" && w[1] == format!("{key}={value}")
        })
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

    /// `prepare_remote` happy path on a fresh host: probe miss → stage
    /// → scp → build, returning a `PreparedHost` with the remote
    /// `$HOME` from the probe. No socket / tunnel work — that's
    /// `attach_socket`'s job.
    #[test]
    fn prepare_remote_happy_path_on_fresh_host() {
        let runner = FakeRunner::new();
        runner.expect_run("ssh", &["-o", "BatchMode=yes"], ok(b"/home/fake\n"));
        runner.expect_run("ssh", &["-o", "BatchMode=yes"], ok(b""));
        runner.expect_run("scp", &["-B"], ok(b""));
        runner.expect_run("ssh", &["-o", "BatchMode=yes"], ok(b"build ok"));

        let prepared = prepare_remote(&runner, |_| {}, "fake-host").unwrap();
        assert_eq!(prepared.remote_home, PathBuf::from("/home/fake"));
        assert!(
            prepared.binary_was_updated,
            "fresh host runs the install branch; flag must signal that to attach_agent",
        );
    }

    /// `prepare_remote` skip-rebuild path: probe matches the embedded
    /// version → stage/scp/build are skipped entirely. Confirms the
    /// idempotent re-call shape (a second prepare against the same
    /// already-installed host returns immediately).
    #[test]
    fn prepare_remote_skips_install_when_version_matches() {
        let runner = FakeRunner::new();
        let probe_stdout = format!("/home/fake\n{}\n", bootstrap_version());
        runner.expect_run("ssh", &["-o", "BatchMode=yes"], ok(probe_stdout.as_bytes()));
        // No further script entries — if the test calls scp/build the
        // FakeRunner will panic with "unexpected" matches.

        let prepared = prepare_remote(&runner, |_| {}, "fake-host").unwrap();
        assert_eq!(prepared.remote_home, PathBuf::from("/home/fake"));
        assert!(
            !prepared.binary_was_updated,
            "version-match path must not signal an upgrade — that would force \
             a daemon kill and break session continuity for unchanged builds",
        );
    }

    /// `attach_socket` happy path: daemon spawn → tunnel → connect,
    /// returning the connected `UnixStream` + tunnel `Child`. Uses a
    /// `PreparedHost` constructed in the test (skipping the prepare
    /// phase) and a delayed-bind thread to simulate `ssh -L` creating
    /// the local socket file. Passes a tempdir as `local_socket_dir`
    /// instead of mutating `$HOME` (the workspace forbids `unsafe`,
    /// and `std::env::set_var` is unsafe in 2024 edition).
    #[test]
    fn attach_socket_happy_path_against_fake_runner() {
        let dir = tempfile::tempdir().unwrap();
        let socket_dir = dir.path().join("sockets");
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
        runner.expect_run("ssh", &["-o", "BatchMode=yes"], ok(b""));
        runner.expect_spawn("ssh", &["-N"]);

        let prepared = PreparedHost {
            remote_home: PathBuf::from("/home/fake"),
            binary_was_updated: false,
        };
        let cfg = AttachConfig {
            host: "fake-host".into(),
            agent_id: agent_id.into(),
            cwd: Some(PathBuf::from("/some/cwd")),
            local_socket_dir: socket_dir,
            rows: 24,
            cols: 80,
        };
        let (stream, mut tunnel) = attach_socket(&runner, |_| {}, &prepared, &cfg).unwrap();
        let _ = (&stream).write_all(b"x");
        let _ = tunnel.kill();
        let _ = tunnel.wait();
    }
}
