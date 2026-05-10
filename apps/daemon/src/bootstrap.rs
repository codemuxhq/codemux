//! Daemon bootstrap: validate the CLI, acquire OS-level resources, hand
//! them to [`Supervisor`].
//!
//! Extracted from `supervisor.rs` so the supervisor can stay focused on
//! its single responsibility — running the accept loop and managing the
//! [`Session`] lifecycle. The bootstrap owns all of the OS-level setup
//! the daemon does once at startup:
//!
//! 1. **`--cwd` validation.** Vision principle 6: never silently fall
//!    back. Failing here (before any side effect) leaves the filesystem
//!    untouched.
//! 2. **Pid file exclusivity** via [`PidFile`]. `O_CREAT|O_EXCL` mode
//!    0600; on contention, the held pid is checked with `kill -0`.
//!    Stale → reap and retry once; live → fail with
//!    [`Error::PidFileLocked`]. Drop removes the file.
//! 3. **Stale socket reap.** Only after we own the pid lock, since the
//!    socket is the supervisor's address-on-the-host and a live daemon
//!    might still be using it.
//! 4. **Socket bind + mode 0600.** `chmod` immediately after `bind`; on
//!    failure unlink so we never leak a too-permissive listener.
//!
//! The result, [`DaemonResources`], hands ownership of the listening
//! socket and the pid file guard to [`Supervisor::new`]; the guard's
//! `Drop` runs when the supervisor itself drops, cleaning up on a
//! graceful shutdown.
//!
//! [`Session`]: crate::session::Session
//! [`Supervisor`]: crate::supervisor::Supervisor
//! [`Supervisor::new`]: crate::supervisor::Supervisor::new

use std::fs::OpenOptions;
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::cli::Cli;
use crate::error::Error;
use crate::fs_layout;
use crate::supervisor::SupervisorConfig;

/// Output of a successful bootstrap: a listening socket, the PTY-spawn
/// config, and (optionally) the pid file guard. Pass to
/// [`Supervisor::new`] to start serving.
///
/// The pid file is `Option<PidFile>` because foreground / test runs
/// don't take the lock — the only exclusivity in those cases is `bind`
/// itself returning `EADDRINUSE`.
///
/// [`Supervisor::new`]: crate::supervisor::Supervisor::new
#[derive(Debug)]
pub struct DaemonResources {
    pub listener: UnixListener,
    pub config: SupervisorConfig,
    pub pid_file: Option<PidFile>,
}

/// Run the full bootstrap sequence from a parsed [`Cli`]. The CLI is
/// the only entry point production code needs; tests reach for
/// [`bring_up_with`] when they want to inject a custom command without
/// going through clap.
pub fn bring_up(cli: &Cli) -> Result<DaemonResources, Error> {
    bring_up_with(
        &cli.socket,
        cli.pid_file.as_deref(),
        SupervisorConfig::from_cli(cli),
    )
}

/// Bootstrap with explicit socket / pid-file paths and a prebuilt
/// [`SupervisorConfig`]. Used by tests; production calls go through
/// [`bring_up`].
///
/// Order of operations is load-bearing — see the module docs.
pub fn bring_up_with(
    socket: &Path,
    pid_file: Option<&Path>,
    mut config: SupervisorConfig,
) -> Result<DaemonResources, Error> {
    // Expand `~/` against `$HOME` (see `expand_local_tilde`) BEFORE
    // the existence check, and mutate `config.cwd` so the absolute
    // path also reaches the child PTY's chdir target — otherwise the
    // child would inherit the unexpanded literal.
    if let Some(cwd) = config.cwd.as_deref() {
        config.cwd = Some(expand_local_tilde(cwd));
    }
    if let Some(cwd) = config.cwd.as_deref()
        && !cwd.exists()
    {
        return Err(Error::CwdNotFound {
            path: cwd.to_path_buf(),
        });
    }

    let pid_file = pid_file.map(PidFile::acquire).transpose()?;

    // Production sockets land under `~/.cache/codemuxd/sockets/`; on a
    // fresh remote that directory only exists if some prior step
    // created it. Without this, the next `UnixListener::bind` returns
    // `ENOENT` and the daemon dies before the supervisor ever runs.
    // The pid-file branch creates `pids/` via `PidFile::acquire`'s
    // own `ensure_parent`; we mirror that here for `sockets/`.
    fs_layout::ensure_parent(socket).map_err(|source| Error::Bind {
        path: socket.display().to_string(),
        source,
    })?;

    reap_stale_socket(socket);

    let listener = UnixListener::bind(socket).map_err(|source| Error::Bind {
        path: socket.display().to_string(),
        source,
    })?;

    apply_socket_mode_or_cleanup(socket)?;

    tracing::info!(
        socket = %socket.display(),
        pid = std::process::id(),
        "supervisor bound",
    );

    Ok(DaemonResources {
        listener,
        config,
        pid_file,
    })
}

/// Restrict the socket file at `path` to mode 0600. On failure, unlink
/// before returning [`Error::Bind`] — never leak a too-permissive
/// listener that survived `bind` but not `chmod`.
fn apply_socket_mode_or_cleanup(path: &Path) -> Result<(), Error> {
    if let Err(source) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        let _ = std::fs::remove_file(path);
        return Err(Error::Bind {
            path: path.display().to_string(),
            source,
        });
    }
    Ok(())
}

/// Expand a leading `~/` (or bare `~`) in `path` against `$HOME`.
/// Anything else (absolute, relative, `~user`) passes through unchanged.
/// Mirrors the bootstrap-side helper in `crates/codemuxd-bootstrap`,
/// but uses the daemon's local `$HOME` rather than a remotely-probed
/// one — this is the daemon-side defense for the same class of issue
/// (literal `~/` in `--cwd` failing `Path::exists()`).
///
/// If `$HOME` is unset (very unusual on a real login session), the
/// path is returned unchanged. Downstream `cwd.exists()` will then
/// fail with `Error::CwdNotFound { path: "~/foo" }`, where the
/// surviving literal tilde is itself the diagnostic — preferable to
/// inventing a new error variant for an extremely rare environment.
fn expand_local_tilde(path: &Path) -> PathBuf {
    let Some(s) = path.to_str() else {
        return path.to_path_buf();
    };
    let Some(home) = std::env::var_os("HOME") else {
        return path.to_path_buf();
    };
    let home = PathBuf::from(home);
    if s == "~" {
        return home;
    }
    if let Some(rest) = s.strip_prefix("~/") {
        return home.join(rest);
    }
    path.to_path_buf()
}

/// Best-effort unlink of a leftover socket from a previous daemon run.
/// `bind(2)` refuses to overwrite an existing path, so a stale file from
/// an earlier crash blocks startup forever otherwise. We've already
/// taken the pid lock at this point — anyone holding the socket is dead.
///
/// `NotFound` is the common case (no prior run) and silent. Anything
/// else (typically `EACCES`) gets a `tracing::warn!` so the upcoming
/// `bind` failure isn't a mystery; we don't promote it to an error
/// because `bind` itself will surface a more accurate one in a moment.
fn reap_stale_socket(path: &Path) {
    if let Err(e) = std::fs::remove_file(path)
        && e.kind() != ErrorKind::NotFound
    {
        tracing::warn!(
            path = %path.display(),
            "could not unlink stale socket before bind: {e}",
        );
    }
}

/// RAII guard for the pid file. Created with `O_CREAT|O_EXCL` mode
/// 0600; removes the file on `Drop` so a clean exit leaves no stale
/// lock behind. A crash that skips `Drop` is the case the [`acquire`]
/// liveness check exists for.
///
/// [`acquire`]: PidFile::acquire
#[derive(Debug)]
pub struct PidFile {
    path: PathBuf,
}

impl PidFile {
    /// Acquire the pid lock at `path`. On contention, the held pid is
    /// checked with `kill -0`; a stale entry is reaped (one retry); a
    /// live one returns [`Error::PidFileLocked`] without touching the
    /// file.
    pub fn acquire(path: &Path) -> Result<Self, Error> {
        Self::acquire_with_retries(path, 1)
    }

    fn acquire_with_retries(path: &Path, retries_left: u8) -> Result<Self, Error> {
        // The default layout puts pid files under `~/.cache/codemuxd/pids/`,
        // which may not exist on a fresh host.
        fs_layout::ensure_parent(path)?;

        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
        {
            Ok(mut file) => {
                let pid = std::process::id();
                writeln!(file, "{pid}").map_err(|source| Error::Io { source })?;
                Ok(Self {
                    path: path.to_path_buf(),
                })
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists && retries_left > 0 => {
                let existing = read_pid(path)?;
                if pid_alive(existing) {
                    return Err(Error::PidFileLocked {
                        pid: existing,
                        path: path.to_path_buf(),
                    });
                }
                tracing::info!(
                    held_pid = existing,
                    path = %path.display(),
                    "reaping stale pid file",
                );
                std::fs::remove_file(path)?;
                Self::acquire_with_retries(path, retries_left - 1)
            }
            Err(source) => Err(Error::Io { source }),
        }
    }
}

impl Drop for PidFile {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.path)
            && e.kind() != ErrorKind::NotFound
        {
            tracing::debug!(
                path = %self.path.display(),
                "pid file remove on drop failed: {e}",
            );
        }
    }
}

/// Read a pid file into a `u32`. Whitespace-tolerant. Garbage payload
/// surfaces as [`Error::Io`] with a descriptive message — the file
/// exists for a reason and a human should look at it before we silently
/// reap.
fn read_pid(path: &Path) -> Result<u32, Error> {
    let raw = std::fs::read_to_string(path)?;
    raw.trim().parse::<u32>().map_err(|parse_err| Error::Io {
        source: std::io::Error::other(format!(
            "pid file {} contains unparseable pid: {parse_err}",
            path.display(),
        )),
    })
}

/// True if `pid` resolves to a live process from this user's vantage
/// point. Implemented via `kill -0 <pid>` to avoid pulling in `nix` or
/// `libc` (workspace forbids `unsafe`); `kill` is in PATH on every
/// POSIX host the daemon runs on.
///
/// Inherently racy: a process can exit between this check and the
/// retry above. The worst case is a spurious reap of a pid file that
/// just got abandoned — the original daemon is already gone in that
/// race, so re-binding is correct.
///
/// Values outside the kernel's positive `pid_t` range (`> i32::MAX`)
/// are short-circuited to `false`: they cannot name a live process on
/// any Unix the daemon supports. This guard also dodges a procps-ng
/// `kill(1)` quirk where `4294967295` parses as the signed `-1`
/// broadcast sentinel — `kill(-1, 0)` returns success for any user
/// with at least one signalable process, and we'd misread that as a
/// live holder.
fn pid_alive(pid: u32) -> bool {
    if pid > i32::MAX as u32 {
        return false;
    }
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()
        .is_some_and(|s| s.success())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use clap::Parser;

    use super::*;

    fn cat_config() -> SupervisorConfig {
        SupervisorConfig {
            command: "cat".to_string(),
            args: Vec::new(),
            cwd: None,
            rows: 24,
            cols: 80,
        }
    }

    /// `bring_up` consumes a `Cli` (clap-built) and exercises the full
    /// `from_cli` path. Foreground mode keeps the CLI minimal.
    #[test]
    fn bring_up_via_cli_succeeds() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("via-cli.sock");
        let socket_str = socket.to_string_lossy().into_owned();
        let cli = Cli::parse_from([
            "codemuxd",
            "--socket",
            &socket_str,
            "--foreground",
            "--rows",
            "30",
            "--cols",
            "100",
            "--",
            "cat",
        ]);
        let resources = bring_up(&cli)?;
        assert_eq!(resources.config.command, "cat");
        assert_eq!(resources.config.rows, 30);
        assert_eq!(resources.config.cols, 100);
        assert!(socket.exists(), "bring_up should create the socket file");
        Ok(())
    }

    /// Binding to an unwritable directory surfaces `Error::Bind` with
    /// the path embedded in the Display string.
    #[test]
    fn bring_up_to_unwritable_path_returns_bind_error() {
        let path =
            std::path::PathBuf::from("/this-directory-does-not-exist-on-any-machine/codemuxd.sock");
        let cli = Cli::parse_from([
            "codemuxd",
            "--socket",
            path.to_str().unwrap_or("/no.sock"),
            "--foreground",
        ]);
        let Err(err) = bring_up(&cli) else {
            unreachable!("bring_up to a nonexistent directory must fail");
        };
        assert!(
            matches!(err, Error::Bind { .. }),
            "expected Error::Bind, got {err:?}",
        );
    }

    /// Regression for the AD-3 bootstrap path: when the socket lives
    /// under a directory that doesn't exist yet (the `~/.cache/codemuxd/
    /// sockets/` case on a fresh remote), `bring_up` must create the
    /// parent on demand instead of letting `UnixListener::bind` fail
    /// with `ENOENT` and exit the daemon before the supervisor ever
    /// runs. Pre-fix, `agent-2.log` ended up empty on the remote and
    /// the local TUI saw a `SocketConnect` stage failure with no
    /// useful breadcrumb.
    #[test]
    fn bring_up_creates_socket_parent_on_demand() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let nested = dir.path().join("sockets");
        let socket = nested.join("agent.sock");
        assert!(
            !nested.exists(),
            "test precondition: parent dir must not exist yet",
        );

        let _resources = bring_up_with(&socket, None, cat_config())?;

        assert!(
            nested.exists(),
            "bring_up must create the socket parent directory",
        );
        assert!(socket.exists(), "bring_up must bind the socket");
        Ok(())
    }

    /// `--cwd` pointing at a non-existent directory fails fast with
    /// [`Error::CwdNotFound`] before any side effect — no socket is
    /// created, no pid file is touched. Vision principle 6 in action.
    #[test]
    fn missing_cwd_returns_cwd_not_found() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("cwd.sock");
        let missing = dir.path().join("does-not-exist-anywhere-here");
        let mut config = cat_config();
        config.cwd = Some(missing.clone());

        let result = bring_up_with(&socket, None, config);
        let Err(err) = result else {
            panic!("bring_up with missing cwd must fail");
        };
        let Error::CwdNotFound { path } = err else {
            panic!("expected Error::CwdNotFound, got {err:?}");
        };
        assert_eq!(path, missing);
        assert!(
            !socket.exists(),
            "bring_up failure must not create the socket",
        );
        Ok(())
    }

    /// `--cwd` with a leading `~/` is expanded against `$HOME` BEFORE
    /// the existence check. Without this, `Path::exists()` returns
    /// false on the literal `~/...` and the daemon dies with
    /// `Error::CwdNotFound` for what is, semantically, a valid path.
    /// The bootstrap-side helper in `crates/codemuxd-bootstrap` does
    /// this on the local side using the remote `$HOME`; this test
    /// covers the daemon-side defense for direct invocations
    /// (foreground, smoke tests, manual `cargo run`).
    ///
    /// We don't `unsafe { set_var("HOME", ...) }` — the workspace
    /// forbids unsafe — instead we rely on `$HOME` being set on every
    /// real test environment and assert against the resolved path.
    #[test]
    fn tilde_cwd_is_expanded_against_home_before_existence_check()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("tilde.sock");

        // Create a directory under $HOME that we know exists at test
        // time. Use a unique suffix so parallel test runs don't collide.
        let Some(home) = std::env::var_os("HOME") else {
            panic!("HOME unset; test cannot run in this env");
        };
        let unique = format!("codemuxd-tilde-test-{}", std::process::id());
        let real_dir = std::path::PathBuf::from(&home).join(&unique);
        std::fs::create_dir_all(&real_dir)?;

        let mut config = cat_config();
        config.cwd = Some(std::path::PathBuf::from(format!("~/{unique}")));

        let result = bring_up_with(&socket, None, config);
        // Cleanup before any assertion so a failed assert doesn't leak.
        let _ = std::fs::remove_dir_all(&real_dir);

        let resources = result?;
        // The expanded path must flow through to the supervisor config
        // so the child PTY's chdir target is the absolute path, not
        // the literal `~/...`.
        assert_eq!(
            resources.config.cwd.as_deref(),
            Some(real_dir.as_path()),
            "tilde must be expanded against $HOME and the config \
             updated so the child PTY chdir uses the absolute path",
        );
        Ok(())
    }

    /// `expand_local_tilde` direct unit coverage. The integration test
    /// above only exercises the `~/foo` branch via `bring_up_with`;
    /// this covers bare `~` and a non-tilde fallthrough so each
    /// reachable branch has explicit test coverage. Two branches
    /// remain structurally untestable in this workspace: the non-UTF-8
    /// `path.to_str() == None` branch (constructing such a path
    /// requires platform-specific bytes that round-trip through
    /// `OsString` in ways most CI environments reject) and the `$HOME`
    /// unset branch (mutating env vars requires `unsafe { set_var }`,
    /// which the workspace's `unsafe = "forbid"` blocks). Both branches
    /// are simple `return path.to_path_buf();` early-outs whose
    /// behavior is identical to the fallthrough.
    #[test]
    fn expand_local_tilde_handles_bare_tilde_and_fallthrough() {
        let Some(home_os) = std::env::var_os("HOME") else {
            panic!("HOME unset; test cannot run in this env");
        };
        let home = PathBuf::from(home_os);
        assert_eq!(expand_local_tilde(Path::new("~")), home);
        assert_eq!(expand_local_tilde(Path::new("~/foo")), home.join("foo"));
        assert_eq!(
            expand_local_tilde(Path::new("/srv/work")),
            PathBuf::from("/srv/work"),
        );
        // `~user` (no slash) is unchanged because expanding it would
        // require a `getpwnam` round trip we don't take.
        assert_eq!(expand_local_tilde(Path::new("~bob")), PathBuf::from("~bob"),);
    }

    /// A pid file holding a definitely-dead pid (`u32::MAX`) is reaped
    /// on `bring_up`: the resulting file holds *our* pid.
    #[test]
    fn stale_pid_file_is_reaped() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("stale.sock");
        let pid_path = dir.path().join("stale.pid");
        std::fs::write(&pid_path, format!("{}\n", u32::MAX))?;

        let resources = bring_up_with(&socket, Some(&pid_path), cat_config())?;

        let written: u32 = std::fs::read_to_string(&pid_path)?.trim().parse()?;
        assert_eq!(
            written,
            std::process::id(),
            "stale pid file must be replaced with the current process id",
        );

        drop(resources);
        assert!(
            !pid_path.exists(),
            "PidFile drop must remove the pid file on clean exit",
        );
        Ok(())
    }

    /// A live pid in the pid file blocks `bring_up` with
    /// [`Error::PidFileLocked`] carrying both the held pid and the file
    /// path; the live file is left intact for inspection.
    #[test]
    fn live_pid_file_blocks_bring_up() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("live.sock");
        let pid_path = dir.path().join("live.pid");

        let mut sleeper = std::process::Command::new("sleep").arg("30").spawn()?;
        let sleeper_pid = sleeper.id();
        std::fs::write(&pid_path, format!("{sleeper_pid}\n"))?;

        let result = bring_up_with(&socket, Some(&pid_path), cat_config());

        let _ = sleeper.kill();
        let _ = sleeper.wait();

        let Err(err) = result else {
            panic!("bring_up with a live pid file must fail");
        };
        let Error::PidFileLocked { pid, path } = err else {
            panic!("expected Error::PidFileLocked, got {err:?}");
        };
        assert_eq!(pid, sleeper_pid);
        assert_eq!(path, pid_path);
        assert!(
            !socket.exists(),
            "bring_up failure must not create the socket",
        );
        assert!(
            pid_path.exists(),
            "the live pid file must be left intact for inspection",
        );
        Ok(())
    }

    /// The unix socket is `chmod 0600` immediately after bind. AD-3
    /// says single-user; without this, the default mode (0666 & ~umask)
    /// would let any local user connect.
    #[test]
    fn socket_mode_is_0600() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("mode.sock");

        let _resources = bring_up_with(&socket, None, cat_config())?;

        let mode = std::fs::metadata(&socket)?.permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "socket mode should be 0600 after bind, got 0o{mode:o}",
        );
        Ok(())
    }

    /// Garbage in the pid file is treated as a hard error rather than
    /// silently reaped — the file exists for a reason.
    #[test]
    fn corrupt_pid_file_returns_io_error() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("corrupt.sock");
        let pid_path = dir.path().join("corrupt.pid");
        std::fs::write(&pid_path, "not-a-pid\n")?;

        let result = bring_up_with(&socket, Some(&pid_path), cat_config());
        let Err(err) = result else {
            panic!("bring_up with corrupt pid file must fail");
        };
        let Error::Io { source } = err else {
            panic!("expected Error::Io for corrupt pid file, got {err:?}");
        };
        let message = source.to_string();
        assert!(
            message.contains("unparseable pid"),
            "io message should mention unparseable pid, got: {message}",
        );
        assert!(
            message.contains(&pid_path.display().to_string()),
            "io message should include the path, got: {message}",
        );
        assert!(
            !socket.exists(),
            "bring_up failure must not create the socket",
        );
        Ok(())
    }

    /// `apply_socket_mode_or_cleanup` returns `Error::Bind` when chmod
    /// can't reach the target. A path inside a non-existent directory
    /// is the simplest way to make `set_permissions` fail with
    /// `NotFound`, exercising the cleanup branch.
    #[test]
    fn apply_socket_mode_returns_bind_error_when_chmod_fails()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let nonexistent = dir.path().join("missing-dir").join("ghost.sock");

        let result = apply_socket_mode_or_cleanup(&nonexistent);
        let Err(Error::Bind { path, source }) = result else {
            panic!("expected Error::Bind for unreachable path, got {result:?}");
        };
        assert_eq!(path, nonexistent.display().to_string());
        assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
        Ok(())
    }

    /// `PidFile::Drop` swallows non-NotFound removal errors via a
    /// `tracing::debug!` log. We trigger that branch by chmod'ing the
    /// parent directory to 0500 (no write) before drop, which makes
    /// `unlink` fail with `EACCES`.
    #[test]
    fn pid_file_drop_swallows_remove_failure() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let subdir = dir.path().join("guarded");
        std::fs::create_dir(&subdir)?;
        let pid_path = subdir.join("locked.pid");

        {
            let _guard = PidFile::acquire(&pid_path)?;
            std::fs::set_permissions(&subdir, std::fs::Permissions::from_mode(0o500))?;
        }

        std::fs::set_permissions(&subdir, std::fs::Permissions::from_mode(0o755))?;
        assert!(
            pid_path.exists(),
            "remove must have failed; the pid file should still be on disk",
        );
        std::fs::remove_file(&pid_path)?;
        Ok(())
    }

    /// `PidFile::acquire` surfaces a non-AlreadyExists open failure as
    /// `Error::Io`. Triggered by acquiring inside a read-only directory
    /// where `O_CREAT|O_EXCL` returns `EACCES`.
    #[test]
    fn pid_file_acquire_propagates_open_failures_as_io() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let readonly = dir.path().join("readonly");
        std::fs::create_dir(&readonly)?;
        std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o500))?;

        let pid_path = readonly.join("blocked.pid");
        let result = PidFile::acquire(&pid_path);

        std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o755))?;

        let Err(Error::Io { source }) = result else {
            panic!("expected Error::Io for unwriteable parent, got {result:?}");
        };
        assert_eq!(
            source.kind(),
            std::io::ErrorKind::PermissionDenied,
            "underlying io kind should be PermissionDenied",
        );
        Ok(())
    }
}
