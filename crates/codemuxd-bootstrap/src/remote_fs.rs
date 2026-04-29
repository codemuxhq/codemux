//! `ssh -M -N` `ControlMaster` + cheap remote `ls` for the spawn modal's
//! remote-path autocomplete.
//!
//! [`RemoteFs::open`] spawns a long-lived ssh master subprocess that
//! binds a unix socket; subsequent [`RemoteFs::list_dir`] calls reuse
//! the master via `ssh -S {socket}`, which skips the TCP handshake +
//! key exchange + auth and lands sub-100 ms even on slow links. The
//! plain alternative — one fresh ssh per keystroke — pays the full
//! handshake cost on every keystroke and feels janky.
//!
//! Lifecycle:
//! - **Open** during the spawn modal's prepare phase (after
//!   [`prepare_remote`](crate::prepare_remote) returns, before the
//!   user starts typing the remote path).
//! - **Use** for each path-zone keystroke that crosses a directory
//!   boundary; the runtime caches results within a directory so
//!   prefix-narrowing keystrokes don't re-shell.
//! - **Drop** when the modal closes (success, cancel, or attach
//!   completes) — kills the master subprocess and removes the socket.
//!
//! Failure mode: [`RemoteFs::open`] returning `Err` is non-fatal at
//! the call site — the modal degrades to literal-path mode (no
//! autocomplete, type the path, hit Enter). Per workspace principle 6,
//! no silent fallback: the wildmenu shows a `(no remote autocomplete:
//! {error})` hint so the user knows why.
//!
//! Why a separate module / type from the bootstrap pipeline:
//! - Different lifecycle (modal-scoped, not session-scoped).
//! - Different fault model ([`RemoteFsError`] is distinct from
//!   [`crate::Error`] — list failures are not bootstrap stage failures
//!   and shouldn't be confused at the surface).
//! - Bootstrap deliberately bypasses any `ControlMaster` (uses
//!   `ControlPath=none, ControlMaster=no` for the tunnel — see notes
//!   in `lib.rs::open_ssh_tunnel`); this module is the only place we
//!   *want* a master, and using a dedicated socket name (`{host}.cm.sock`)
//!   keeps it cleanly separate from the per-agent tunnel sockets.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use thiserror::Error;

use crate::{CommandRunner, default_local_socket_dir};

/// How long to wait for the `ssh -M -N` master to bind its socket
/// before giving up. A fresh SSH connection (DNS, TCP, key exchange,
/// auth) can take a second or two on a high-latency link; 3 s leaves
/// headroom without making cancellation feel laggy.
const OPEN_TIMEOUT: Duration = Duration::from_secs(3);

/// Polling interval inside the open-socket retry loop.
const OPEN_POLL: Duration = Duration::from_millis(50);

/// Cap the number of entries returned by [`RemoteFs::list_dir`]. A
/// remote `/usr/lib` can have thousands of entries; rendering them
/// all in the wildmenu (which only shows ~3 rows anyway) is wasted
/// work. Matches `apps/tui/src/spawn.rs::MAX_SCAN_ENTRIES`.
pub const MAX_LIST_ENTRIES: usize = 1024;

/// One entry in a remote directory listing.
///
/// `is_dir` is derived from `ls -p`'s trailing-slash convention: any
/// entry that ends with `/` is a directory, everything else is a file
/// (or symlink to a file — `ls` resolves the link target's type when
/// `-L` is in play; we don't pass `-L`, so symlinks-to-dirs render as
/// files, which matches local `read_dir`'s default behavior in
/// `spawn.rs::scan_dir`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub is_dir: bool,
}

/// Errors raised by [`RemoteFs`].
///
/// Distinct from [`crate::Error`] because list/listing failures are a
/// presentation-layer concern (the modal degrades gracefully) rather
/// than a bootstrap-stage failure (which is a hard error rendered in
/// the placeholder pane).
///
/// `#[non_exhaustive]` per AD-17 — variants can be added without
/// breaking downstream `match` statements.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RemoteFsError {
    /// Failed to spawn the `ssh -M -N` master subprocess. Usually
    /// means `ssh` is not on the local PATH.
    #[error("failed to spawn ssh master")]
    SpawnMaster {
        #[source]
        source: std::io::Error,
    },

    /// The master subprocess started but the control socket file
    /// never appeared within [`OPEN_TIMEOUT`]. Most often: the host is
    /// unreachable, auth was refused (e.g. agent not loaded), or the
    /// remote sshd is configured to refuse master connections. The
    /// caller should treat this as "no remote autocomplete available"
    /// rather than a fatal modal error.
    #[error("ssh control socket {socket} did not appear within {timeout:?}")]
    OpenTimeout { socket: PathBuf, timeout: Duration },

    /// Failed to derive the local socket directory (typically `$HOME`
    /// is unset). Mirrors [`crate::default_local_socket_dir`]'s error.
    #[error("could not derive local socket dir")]
    SocketDir {
        #[source]
        source: Box<dyn std::error::Error + Send + Sync + 'static>,
    },

    /// The path passed to [`RemoteFs::list_dir`] contains a character
    /// that the shell-escape policy rejects. Currently a single quote
    /// — the path is wrapped in single quotes when forwarded to ssh,
    /// and an embedded `'` would prematurely terminate the quoted
    /// string. Validating up-front is safer than escaping (the caller
    /// can substitute or fall back to literal-path mode).
    #[error("path {path:?} contains an unsafe character ({reason})")]
    UnsafePath { path: PathBuf, reason: &'static str },

    /// Failed to invoke `ssh -S {socket} ... ls ...` for a list. Most
    /// often means `ssh` was removed mid-session. The master process
    /// itself going away surfaces as `ListExit { status: 255 }`.
    #[error("ssh ls invocation failed")]
    ListSpawn {
        #[source]
        source: std::io::Error,
    },

    /// `ssh ... ls ...` exited non-zero. Common causes: the directory
    /// doesn't exist (`status=2`), permission denied (`status=1`), or
    /// the master died and ssh fell back to a fresh connection that
    /// failed (`status=255`). Stderr is included for diagnostics.
    #[error("ssh ls exited with status {status}: {stderr}")]
    ListExit { status: i32, stderr: String },

    /// Failed to invoke `ssh -S {socket} ... mkdir -p ...`. Same shape
    /// as [`Self::ListSpawn`] but for the [`RemoteFs::mkdir_p`] path —
    /// kept distinct so the runtime can surface a different
    /// diagnostic without parsing strings.
    #[error("ssh mkdir invocation failed")]
    MkdirSpawn {
        #[source]
        source: std::io::Error,
    },

    /// `ssh ... mkdir -p ...` exited non-zero. With `mkdir -p`, the
    /// only realistic causes are permission denied (`status=1`) or
    /// the master died and ssh fell back to a fresh connection that
    /// failed (`status=255`). Stderr is included for diagnostics.
    #[error("ssh mkdir exited with status {status}: {stderr}")]
    MkdirExit { status: i32, stderr: String },
}

/// Long-lived `ssh -M -N` `ControlMaster` connection used to amortize
/// remote `ls` calls during spawn-modal autocomplete.
///
/// `Drop` kills the master subprocess and unlinks the socket file.
/// Best-effort: failures during cleanup are logged via `tracing` but
/// don't propagate (drop semantics).
pub struct RemoteFs {
    /// Local end of the ssh control socket. Removed on Drop.
    socket: PathBuf,
    /// Hostname (as it appears in the user's `~/.ssh/config`). Stored
    /// so subsequent [`Self::list_dir`] calls can pass it as the ssh
    /// destination — `ssh -S {socket} {host} ...`.
    host: String,
    /// The master subprocess. Killed on Drop. `Option` so `Drop` can
    /// `take()` it; the field is always `Some` between construction
    /// and Drop.
    child: Option<Child>,
}

impl std::fmt::Debug for RemoteFs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteFs")
            .field("socket", &self.socket)
            .field("host", &self.host)
            .field("child_alive", &self.child.is_some())
            .finish()
    }
}

impl RemoteFs {
    /// Spawn a `ssh -M -N -S {socket} {host}` master subprocess and
    /// wait for the socket file to appear (up to [`OPEN_TIMEOUT`]).
    ///
    /// `host` is the destination as it would appear on the ssh command
    /// line — typically a `~/.ssh/config` `Host` entry. The socket is
    /// placed at `{default_local_socket_dir}/{sanitized_host}.cm.sock`
    /// to keep it visually distinct from the per-agent tunnel sockets
    /// (which use the same dir and are named `{agent_id}.sock`).
    ///
    /// On failure, kills the master (if started) and removes the
    /// socket file — leaves no leaked subprocess or stale socket.
    ///
    /// # Errors
    /// - [`RemoteFsError::SocketDir`] if `$HOME` can't be resolved.
    /// - [`RemoteFsError::SpawnMaster`] if `ssh` can't be invoked.
    /// - [`RemoteFsError::OpenTimeout`] if the master doesn't bind
    ///   the socket within the timeout (host unreachable, auth
    ///   refused, etc.).
    pub fn open(host: &str) -> Result<Self, RemoteFsError> {
        let dir = default_local_socket_dir().map_err(|e| RemoteFsError::SocketDir {
            source: Box::new(e),
        })?;
        std::fs::create_dir_all(&dir).map_err(|e| RemoteFsError::SocketDir {
            source: Box::new(e),
        })?;
        let socket = dir.join(format!("{}.cm.sock", sanitize_host_for_filename(host)));
        // A stale socket from a prior session would block bind. Best-effort.
        let _ = std::fs::remove_file(&socket);

        let socket_str = socket.to_string_lossy();
        let child = Command::new("ssh")
            .args([
                "-M",
                "-N",
                "-S",
                socket_str.as_ref(),
                "-o",
                "BatchMode=yes",
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "ServerAliveInterval=30",
                host,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|source| RemoteFsError::SpawnMaster { source })?;

        // The master forks and binds the socket asynchronously. Poll
        // for its existence — the moment it appears we're ready to
        // multiplex `ls` calls through it.
        let mut me = Self {
            socket: socket.clone(),
            host: host.to_string(),
            child: Some(child),
        };
        let deadline = Instant::now() + OPEN_TIMEOUT;
        while Instant::now() < deadline {
            if me.socket.exists() {
                tracing::debug!(
                    host = %me.host,
                    socket = %me.socket.display(),
                    "RemoteFs control master ready",
                );
                return Ok(me);
            }
            // If the master subprocess died early (auth failed, host
            // unreachable, key denied) the socket will never appear.
            // Bail immediately rather than waiting the full timeout.
            if let Some(child) = me.child.as_mut()
                && let Ok(Some(status)) = child.try_wait()
            {
                tracing::debug!(
                    host = %me.host,
                    exit = ?status.code(),
                    "ssh control master exited before binding socket",
                );
                // Take the child so Drop doesn't try to kill it again.
                me.child.take();
                return Err(RemoteFsError::OpenTimeout {
                    socket: me.socket.clone(),
                    timeout: OPEN_TIMEOUT,
                });
            }
            thread::sleep(OPEN_POLL);
        }
        Err(RemoteFsError::OpenTimeout {
            socket: me.socket.clone(),
            timeout: OPEN_TIMEOUT,
        })
    }

    /// Path of the local control socket. Useful for diagnostics.
    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket
    }

    /// Hostname this master is bound to.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Construct a [`RemoteFs`] without spawning the ssh master.
    /// Test-only — production paths must use [`Self::open`].
    ///
    /// The returned value behaves like a real `RemoteFs` for
    /// [`Self::list_dir`] purposes (the `host` and `socket` end up in
    /// the ssh argv) but [`Drop`] short-circuits because there's no
    /// child to kill. Pair with a scripted `CommandRunner` so the
    /// `ssh` invocation is intercepted and never runs.
    ///
    /// `#[doc(hidden)]` because this leaks the internal field shape;
    /// not a stable API. The TUI's spawn-modal tests (in a different
    /// crate) need this entry point — `#[cfg(test)]` would limit it
    /// to the defining crate, hence the visibility hack.
    #[doc(hidden)]
    #[must_use]
    pub fn for_test(host: String, socket: PathBuf) -> Self {
        Self {
            socket,
            host,
            child: None,
        }
    }

    /// List a remote directory, returning entries (names + is-dir
    /// flags). Reuses the master via `ssh -S {socket}` so each call
    /// pays only the master's per-message overhead (sub-100 ms typ).
    ///
    /// `path` must be UTF-8 and must not contain `'` (the shell
    /// escape policy). The runtime is responsible for substituting
    /// `~` with the [`PreparedHost::remote_home`](crate::PreparedHost)
    /// before calling — this function does not shell-expand.
    ///
    /// `runner` lets tests script the `ssh ... ls ...` invocation
    /// without spawning a real subprocess. Production callers pass
    /// `&RealRunner`. (The runner is *not* used for the master itself;
    /// [`Self::open`] always calls `ssh` directly because the master
    /// lifecycle is fundamentally stateful.)
    ///
    /// # Errors
    /// - [`RemoteFsError::UnsafePath`] for paths containing `'`.
    /// - [`RemoteFsError::ListSpawn`] if the runner can't invoke ssh.
    /// - [`RemoteFsError::ListExit`] if `ls` exited non-zero (no such
    ///   directory, permission denied, master died, etc.).
    pub fn list_dir(
        &self,
        runner: &dyn CommandRunner,
        path: &Path,
    ) -> Result<Vec<DirEntry>, RemoteFsError> {
        let path_str = path.to_str().ok_or_else(|| RemoteFsError::UnsafePath {
            path: path.to_path_buf(),
            reason: "path is not valid UTF-8",
        })?;
        if path_str.contains('\'') {
            return Err(RemoteFsError::UnsafePath {
                path: path.to_path_buf(),
                reason: "single quote",
            });
        }

        // `ls -1pA`: one entry per line (-1), trailing slash on dirs (-p),
        // include dotfiles minus `.` and `..` (-A). The remote shell
        // sees `--` so any future `ls` flag added to the path can't
        // be misinterpreted as a flag (`ls -- '-rf'`).
        let socket_str = self.socket.to_string_lossy();
        let quoted_path = format!("'{path_str}'");
        let remote_cmd = format!("ls -1pA -- {quoted_path}");
        let output = runner
            .run(
                "ssh",
                &[
                    "-S",
                    socket_str.as_ref(),
                    "-o",
                    "BatchMode=yes",
                    &self.host,
                    "--",
                    &remote_cmd,
                ],
            )
            .map_err(|source| RemoteFsError::ListSpawn { source })?;

        if output.status != 0 {
            return Err(RemoteFsError::ListExit {
                status: output.status,
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let mut entries: Vec<DirEntry> = stdout
            .lines()
            .filter(|line| !line.is_empty())
            .take(MAX_LIST_ENTRIES)
            .map(|line| {
                if let Some(stripped) = line.strip_suffix('/') {
                    DirEntry {
                        name: stripped.to_string(),
                        is_dir: true,
                    }
                } else {
                    DirEntry {
                        name: line.to_string(),
                        is_dir: false,
                    }
                }
            })
            .collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(entries)
    }

    /// Create `path` and any missing parent directories on the remote
    /// host (`mkdir -p`). Idempotent — succeeds if the directory
    /// already exists.
    ///
    /// Used by the spawn modal's "scratch" fallback so the user-
    /// configured scratch dir (default `~/.codemux/scratch`, tilde
    /// pre-expanded against the remote `$HOME`) exists before the
    /// daemon's `--cwd` validation runs. The runtime calls this
    /// while it still holds the prepare phase's [`RemoteFs`] —
    /// piggybacking on the live `ControlMaster` keeps the SSH
    /// round-trip cost identical to a single `list_dir`.
    ///
    /// Same shell-escape policy as [`Self::list_dir`]: the path is
    /// wrapped in single quotes when forwarded to ssh, so an
    /// embedded `'` is rejected up-front. UTF-8 paths only.
    ///
    /// `runner` lets tests script the `ssh ... mkdir ...` invocation
    /// without spawning a real subprocess. Production callers pass
    /// `&RealRunner`.
    ///
    /// # Errors
    /// - [`RemoteFsError::UnsafePath`] for paths containing `'` or
    ///   non-UTF-8 bytes.
    /// - [`RemoteFsError::MkdirSpawn`] if the runner can't invoke ssh.
    /// - [`RemoteFsError::MkdirExit`] if `mkdir -p` exited non-zero
    ///   (typically permission denied or the master died).
    pub fn mkdir_p(&self, runner: &dyn CommandRunner, path: &Path) -> Result<(), RemoteFsError> {
        let path_str = path.to_str().ok_or_else(|| RemoteFsError::UnsafePath {
            path: path.to_path_buf(),
            reason: "path is not valid UTF-8",
        })?;
        if path_str.contains('\'') {
            return Err(RemoteFsError::UnsafePath {
                path: path.to_path_buf(),
                reason: "single quote",
            });
        }

        let socket_str = self.socket.to_string_lossy();
        let quoted_path = format!("'{path_str}'");
        // `mkdir -p --` so any future flag added to the path can't
        // be misinterpreted as a flag (`mkdir -p -- '-rf'`).
        let remote_cmd = format!("mkdir -p -- {quoted_path}");
        let output = runner
            .run(
                "ssh",
                &[
                    "-S",
                    socket_str.as_ref(),
                    "-o",
                    "BatchMode=yes",
                    &self.host,
                    "--",
                    &remote_cmd,
                ],
            )
            .map_err(|source| RemoteFsError::MkdirSpawn { source })?;

        if output.status != 0 {
            return Err(RemoteFsError::MkdirExit {
                status: output.status,
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        Ok(())
    }
}

impl Drop for RemoteFs {
    fn drop(&mut self) {
        // Best-effort cleanup. Surface failures via tracing only —
        // Drop must not panic, and the user's terminal may already be
        // restored.
        if let Some(mut child) = self.child.take() {
            if let Err(e) = child.kill() {
                tracing::debug!(error = %e, "RemoteFs: failed to kill ssh master");
            }
            if let Err(e) = child.wait() {
                tracing::debug!(error = %e, "RemoteFs: failed to wait ssh master");
            }
        }
        if self.socket.exists()
            && let Err(e) = std::fs::remove_file(&self.socket)
        {
            tracing::debug!(error = %e, "RemoteFs: failed to remove control socket");
        }
    }
}

/// Sanitize a host string into a filename-safe form. Replaces any char
/// that isn't `[A-Za-z0-9_-]` with `_`. Used only for the control
/// socket filename so it doesn't conflict with the per-agent tunnel
/// sockets (which assume `agent_id` is already shell-safe per
/// `lib.rs::validate_agent_id`).
fn sanitize_host_for_filename(host: &str) -> String {
    host.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::CommandOutput;

    /// Tiny `CommandRunner` for testing `list_dir` parsing without
    /// spawning real ssh. Records the args of the most-recent `run`
    /// call and returns a scripted `CommandOutput`.
    struct ScriptedRunner {
        last_args: Mutex<Option<Vec<String>>>,
        response: Mutex<Option<std::io::Result<CommandOutput>>>,
    }

    impl ScriptedRunner {
        fn ok(stdout: &[u8]) -> Self {
            Self {
                last_args: Mutex::new(None),
                response: Mutex::new(Some(Ok(CommandOutput {
                    status: 0,
                    stdout: stdout.to_vec(),
                    stderr: Vec::new(),
                }))),
            }
        }

        fn fail(status: i32, stderr: &[u8]) -> Self {
            Self {
                last_args: Mutex::new(None),
                response: Mutex::new(Some(Ok(CommandOutput {
                    status,
                    stdout: Vec::new(),
                    stderr: stderr.to_vec(),
                }))),
            }
        }

        fn last_args(&self) -> Vec<String> {
            self.last_args
                .lock()
                .unwrap()
                .clone()
                .expect("ScriptedRunner.run was not called")
        }
    }

    impl CommandRunner for ScriptedRunner {
        fn run(&self, _program: &str, args: &[&str]) -> std::io::Result<CommandOutput> {
            *self.last_args.lock().unwrap() = Some(args.iter().map(|s| (*s).to_string()).collect());
            self.response
                .lock()
                .unwrap()
                .take()
                .expect("ScriptedRunner.run called twice but only one response was scripted")
        }

        fn spawn_detached(&self, _: &str, _: &[&str]) -> std::io::Result<std::process::Child> {
            unreachable!("RemoteFs::list_dir does not spawn detached subprocesses")
        }
    }

    /// Build a `RemoteFs` without spawning a real ssh master. Used by
    /// tests that only exercise `list_dir`. The fake socket path is
    /// meaningful (it ends up in the ssh argv) but isn't required to
    /// exist on disk — the `ScriptedRunner` doesn't actually invoke ssh.
    fn fake_remote_fs(host: &str, socket: &Path) -> RemoteFs {
        RemoteFs {
            socket: socket.to_path_buf(),
            host: host.to_string(),
            // No child — Drop will skip the kill/wait branch.
            child: None,
        }
    }

    /// `ls -1pA` output with a mix of files, directories, and a
    /// dotfile parses cleanly: trailing slash → `is_dir: true`, no
    /// slash → file, hidden files included.
    #[test]
    fn list_dir_parses_mixed_files_and_dirs() {
        let fs = fake_remote_fs("host.example", Path::new("/tmp/host.cm.sock"));
        let runner = ScriptedRunner::ok(b".bashrc\nbin/\nfile.txt\nsrc/\n");
        let entries = fs.list_dir(&runner, Path::new("/home/me")).unwrap();
        assert_eq!(
            entries,
            vec![
                DirEntry {
                    name: ".bashrc".into(),
                    is_dir: false,
                },
                DirEntry {
                    name: "bin".into(),
                    is_dir: true,
                },
                DirEntry {
                    name: "file.txt".into(),
                    is_dir: false,
                },
                DirEntry {
                    name: "src".into(),
                    is_dir: true,
                },
            ],
        );
    }

    /// `list_dir` invokes ssh with the `-S {socket}` mux flag, the
    /// host as destination, and the `--` argument separator before the
    /// shell command (so a future `ls` flag added to the path can't
    /// be misinterpreted).
    #[test]
    fn list_dir_invokes_ssh_with_correct_flags() {
        let fs = fake_remote_fs("host.example", Path::new("/tmp/host.cm.sock"));
        let runner = ScriptedRunner::ok(b"file/\n");
        let _ = fs.list_dir(&runner, Path::new("/srv/work")).unwrap();
        let args = runner.last_args();
        // -S must be followed by the socket path
        let s_idx = args.iter().position(|a| a == "-S").expect("ssh -S missing");
        assert_eq!(args[s_idx + 1], "/tmp/host.cm.sock");
        // Host must appear unquoted as a literal arg
        assert!(args.contains(&"host.example".to_string()));
        // Argument separator + ls command must be present
        let sep_idx = args.iter().position(|a| a == "--").expect("-- missing");
        let cmd = &args[sep_idx + 1];
        assert!(
            cmd.contains("ls -1pA --") && cmd.contains("'/srv/work'"),
            "remote command should be ls -1pA -- '<path>'; got {cmd:?}",
        );
    }

    /// `list_dir` includes BatchMode=yes so a stale auth doesn't
    /// trigger an interactive password prompt that hangs the modal.
    #[test]
    fn list_dir_includes_batchmode_yes() {
        let fs = fake_remote_fs("host.example", Path::new("/tmp/host.cm.sock"));
        let runner = ScriptedRunner::ok(b"");
        let _ = fs.list_dir(&runner, Path::new("/")).unwrap();
        let args = runner.last_args();
        // -o BatchMode=yes appears as two adjacent args ("-o", "BatchMode=yes")
        let pair_present = args
            .windows(2)
            .any(|w| w[0] == "-o" && w[1] == "BatchMode=yes");
        assert!(pair_present, "BatchMode=yes missing from {args:?}");
    }

    /// Empty `ls` output (empty directory, or directory with only `.`/`..`
    /// which `-A` skips) yields an empty `Vec`, not an error.
    #[test]
    fn list_dir_empty_output_yields_empty_vec() {
        let fs = fake_remote_fs("host.example", Path::new("/tmp/host.cm.sock"));
        let runner = ScriptedRunner::ok(b"");
        let entries = fs.list_dir(&runner, Path::new("/empty")).unwrap();
        assert!(entries.is_empty());
    }

    /// Non-zero exit status surfaces as `ListExit` with the stderr
    /// trimmed and included for diagnostics.
    #[test]
    fn list_dir_non_zero_exit_returns_list_exit() {
        let fs = fake_remote_fs("host.example", Path::new("/tmp/host.cm.sock"));
        let runner = ScriptedRunner::fail(2, b"ls: /no/such: No such file or directory\n");
        let err = fs.list_dir(&runner, Path::new("/no/such")).unwrap_err();
        let RemoteFsError::ListExit { status, stderr } = err else {
            panic!("expected ListExit, got {err:?}");
        };
        assert_eq!(status, 2);
        assert!(stderr.contains("No such file"));
    }

    /// Paths with embedded `'` are rejected up front — single quotes
    /// would prematurely terminate the shell-quoted string we send
    /// over ssh, and escaping is more error-prone than rejecting.
    #[test]
    fn list_dir_rejects_path_with_single_quote() {
        let fs = fake_remote_fs("host.example", Path::new("/tmp/host.cm.sock"));
        let runner = ScriptedRunner::ok(b"");
        let err = fs
            .list_dir(&runner, Path::new("/tmp/with'quote"))
            .unwrap_err();
        let RemoteFsError::UnsafePath { reason, .. } = err else {
            panic!("expected UnsafePath, got {err:?}");
        };
        assert_eq!(reason, "single quote");
    }

    /// Output longer than `MAX_LIST_ENTRIES` is truncated rather than
    /// blowing up the wildmenu render budget.
    #[test]
    fn list_dir_truncates_at_max_list_entries() {
        let mut buf = Vec::new();
        for i in 0..(MAX_LIST_ENTRIES + 50) {
            buf.extend_from_slice(format!("file{i}\n").as_bytes());
        }
        let fs = fake_remote_fs("host.example", Path::new("/tmp/host.cm.sock"));
        let runner = ScriptedRunner::ok(&buf);
        let entries = fs.list_dir(&runner, Path::new("/")).unwrap();
        assert_eq!(entries.len(), MAX_LIST_ENTRIES);
    }

    // ── mkdir_p ──────────────────────────────────────────────────
    //
    // Same shell-escape policy as list_dir: paths are wrapped in
    // single quotes when forwarded to ssh. The mkdir_p path is hit
    // by the spawn modal's scratch fallback, so a regression here
    // would silently fail SSH spawns into the configured scratch
    // dir.

    /// Happy path: `mkdir -p` succeeds, ssh exits 0, the call returns
    /// `Ok(())`.
    #[test]
    fn mkdir_p_returns_ok_on_success() {
        let fs = fake_remote_fs("host.example", Path::new("/tmp/host.cm.sock"));
        let runner = ScriptedRunner::ok(b"");
        fs.mkdir_p(&runner, Path::new("/home/me/.codemux/scratch"))
            .unwrap();
    }

    /// `mkdir_p` invokes ssh with the same `-S {socket} -o BatchMode=yes
    /// {host} -- mkdir -p '<path>'` shape as `list_dir`. Pinning each
    /// piece guards against accidental shell-escape bugs (the path
    /// must be quoted, the `--` separator must be present so a future
    /// path starting with `-` can't be misparsed as a flag).
    #[test]
    fn mkdir_p_invokes_ssh_with_correct_flags() {
        let fs = fake_remote_fs("host.example", Path::new("/tmp/host.cm.sock"));
        let runner = ScriptedRunner::ok(b"");
        fs.mkdir_p(&runner, Path::new("/srv/scratch")).unwrap();
        let args = runner.last_args();
        let s_idx = args.iter().position(|a| a == "-S").expect("ssh -S missing");
        assert_eq!(args[s_idx + 1], "/tmp/host.cm.sock");
        assert!(args.contains(&"host.example".to_string()));
        let pair_present = args
            .windows(2)
            .any(|w| w[0] == "-o" && w[1] == "BatchMode=yes");
        assert!(pair_present, "BatchMode=yes missing from {args:?}");
        let sep_idx = args.iter().position(|a| a == "--").expect("-- missing");
        let cmd = &args[sep_idx + 1];
        assert!(
            cmd.contains("mkdir -p --") && cmd.contains("'/srv/scratch'"),
            "remote command should be mkdir -p -- '<path>'; got {cmd:?}",
        );
    }

    /// Non-zero exit (typically permission denied) surfaces as
    /// `MkdirExit` with the stderr trimmed and included so the
    /// runtime can render a useful diagnostic banner.
    #[test]
    fn mkdir_p_non_zero_exit_returns_mkdir_exit() {
        let fs = fake_remote_fs("host.example", Path::new("/tmp/host.cm.sock"));
        let runner =
            ScriptedRunner::fail(1, b"mkdir: cannot create directory: Permission denied\n");
        let err = fs
            .mkdir_p(&runner, Path::new("/no/perms/scratch"))
            .unwrap_err();
        let RemoteFsError::MkdirExit { status, stderr } = err else {
            panic!("expected MkdirExit, got {err:?}");
        };
        assert_eq!(status, 1);
        assert!(stderr.contains("Permission denied"));
    }

    /// Mirrors `list_dir_rejects_path_with_single_quote` — the same
    /// shell-escape policy applies to `mkdir_p`.
    #[test]
    fn mkdir_p_rejects_path_with_single_quote() {
        let fs = fake_remote_fs("host.example", Path::new("/tmp/host.cm.sock"));
        let runner = ScriptedRunner::ok(b"");
        let err = fs
            .mkdir_p(&runner, Path::new("/tmp/with'quote"))
            .unwrap_err();
        let RemoteFsError::UnsafePath { reason, .. } = err else {
            panic!("expected UnsafePath, got {err:?}");
        };
        assert_eq!(reason, "single quote");
    }

    /// `sanitize_host_for_filename` keeps alphanumerics + `_-`,
    /// replaces everything else with `_`. Round-trips for normal
    /// hostnames; sanitizes user@host:port-style targets so they
    /// don't try to use a `:` in a filename (illegal on some FSes).
    #[test]
    fn sanitize_host_keeps_safe_chars() {
        assert_eq!(sanitize_host_for_filename("devpod-1"), "devpod-1");
        assert_eq!(sanitize_host_for_filename("alice_box"), "alice_box");
    }

    #[test]
    fn sanitize_host_replaces_unsafe_chars() {
        assert_eq!(sanitize_host_for_filename("user@host"), "user_host");
        assert_eq!(sanitize_host_for_filename("host:22"), "host_22");
        assert_eq!(sanitize_host_for_filename("a/b"), "a_b");
    }

    /// `Drop` on a `RemoteFs` with no child (the test fixture) doesn't
    /// panic and silently skips kill/wait. Mirrors the production
    /// path's "best-effort cleanup" semantics.
    #[test]
    fn drop_with_no_child_is_noop() {
        let fs = fake_remote_fs("host.example", Path::new("/tmp/never-existed.sock"));
        drop(fs);
    }

    /// `Debug` impl redacts the child handle (no PID leak) but
    /// surfaces socket + host for diagnostics.
    #[test]
    fn debug_impl_includes_socket_and_host() {
        let fs = fake_remote_fs("devpod-1", Path::new("/tmp/devpod-1.cm.sock"));
        let s = format!("{fs:?}");
        assert!(s.contains("devpod-1"));
        assert!(s.contains("/tmp/devpod-1.cm.sock"));
        assert!(s.contains("child_alive"));
    }

    /// End-to-end Drop test: spawn a real subprocess (`sleep 60`),
    /// stash it in a `RemoteFs` along with a real socket file, drop
    /// the `RemoteFs`, confirm both the subprocess and the socket
    /// file are gone. Exercises the production cleanup path that the
    /// test fixture (`fake_remote_fs`, `child: None`) skips.
    #[test]
    fn drop_kills_child_and_removes_socket() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("test.cm.sock");
        // Touch the socket so Drop's remove_file has something to remove.
        std::fs::write(&socket, b"").unwrap();
        let child = Command::new("sleep")
            .arg("60")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let fs = RemoteFs {
            socket: socket.clone(),
            host: "host.example".into(),
            child: Some(child),
        };
        drop(fs);
        // Give the kernel a beat to reap the subprocess.
        thread::sleep(Duration::from_millis(50));
        // Socket should be gone.
        assert!(!socket.exists(), "socket should be unlinked after Drop");
        // Subprocess should be dead. `kill -0 {pid}` returns nonzero
        // (no such process) when the process has been reaped.
        let alive = Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(!alive, "ssh master subprocess should be killed by Drop");
    }
}
