//! Supervisor: owns the unix-socket listener and runs the accept loop.
//!
//! Stage 0: at most one `Session` per supervisor, lazily spawned on first
//! accept. Sessions persist across client disconnects — the whole point
//! of the daemon is session continuity. If the child has exited between
//! attaches, the next accept respawns. Single-attach is implicit (the
//! accept loop is sequential, so a second client blocks in `connect`
//! until the first ends); Stage 1 will replace this with a protocol-level
//! `AlreadyAttached` rejection.
//!
//! Drop on the `Session` (which happens when the supervisor is dropped at
//! daemon shutdown) kills the child. Without that, the child would become
//! a zombie outliving the daemon.

use std::os::unix::net::UnixListener;
use std::path::PathBuf;

use crate::cli::Cli;
use crate::error::Error;
use crate::session::Session;

/// What the supervisor needs to know about the child it spawns per
/// session. Extracted from `Cli` so callers (notably tests) can build it
/// without going through clap.
#[derive(Clone, Debug)]
pub struct SupervisorConfig {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub rows: u16,
    pub cols: u16,
}

impl SupervisorConfig {
    #[must_use]
    pub fn from_cli(cli: &Cli) -> Self {
        let (command, args) = cli.child_command();
        Self {
            command,
            args,
            cwd: cli.cwd.clone(),
            rows: cli.rows,
            cols: cli.cols,
        }
    }
}

pub struct Supervisor {
    listener: UnixListener,
    config: SupervisorConfig,
    session: Option<Session>,
}

impl Supervisor {
    /// Bind the unix-socket listener at `cli.socket` and prepare a
    /// supervisor that will spawn a `Session` lazily on first accept.
    ///
    /// If a stale socket file exists at the path, it is removed before
    /// binding. Stage 2 will replace this with a real liveness check (read
    /// the pid file, send signal 0, only unlink if the process is gone).
    pub fn bind(cli: &Cli) -> Result<Self, Error> {
        let _ = std::fs::remove_file(&cli.socket);
        let listener = UnixListener::bind(&cli.socket).map_err(|e| Error::Bind {
            path: cli.socket.display().to_string(),
            source: e,
        })?;
        Ok(Self {
            listener,
            config: SupervisorConfig::from_cli(cli),
            session: None,
        })
    }

    /// Bind with an explicit `SupervisorConfig`. Used by tests that want
    /// to supply a custom command without constructing a full `Cli`.
    pub fn bind_with(socket: &std::path::Path, config: SupervisorConfig) -> Result<Self, Error> {
        let _ = std::fs::remove_file(socket);
        let listener = UnixListener::bind(socket).map_err(|e| Error::Bind {
            path: socket.display().to_string(),
            source: e,
        })?;
        Ok(Self {
            listener,
            config,
            session: None,
        })
    }

    /// Accept-loop forever. Each accepted connection runs to completion
    /// before the next is accepted; the underlying session persists.
    pub fn serve(&mut self) -> Result<(), Error> {
        loop {
            self.serve_one()?;
        }
    }

    /// Accept one connection and attach it to the (lazily-spawned)
    /// session. Returns when the conn ends. The session keeps running.
    pub fn serve_one(&mut self) -> Result<(), Error> {
        let (stream, _addr) = self
            .listener
            .accept()
            .map_err(|source| Error::Accept { source })?;
        tracing::info!("client attached");

        let session = self.session_mut()?;
        let result = session.attach(stream);

        tracing::info!("client detached");
        result
    }

    /// Return a mutable reference to a live session, spawning one (or
    /// replacing a dead one) if necessary. Computing `dead` separately
    /// keeps the borrow checker happy when we reassign `self.session`.
    fn session_mut(&mut self) -> Result<&mut Session, Error> {
        let dead = match &mut self.session {
            Some(s) => s.child_exited(),
            None => true,
        };
        if dead {
            if self.session.is_some() {
                tracing::info!("previous session ended; spawning fresh session");
            }
            let new_session = Session::spawn(
                &self.config.command,
                &self.config.args,
                self.config.cwd.as_deref(),
                self.config.rows,
                self.config.cols,
            )?;
            return Ok(self.session.insert(new_session));
        }
        // Else: session is Some and alive (we just checked). Anything
        // other than Some here is a logic bug, not a runtime error.
        match self.session.as_mut() {
            Some(s) => Ok(s),
            None => unreachable!("session must be Some when not respawning"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::thread;
    use std::time::{Duration, Instant};

    use clap::Parser;

    use super::*;

    /// End-to-end smoke for Stage 0: the supervisor binds, accepts one
    /// client, lazily spawns a `Session` running `cat`, shuttles bytes
    /// both ways, and returns cleanly when the client disconnects.
    ///
    /// Why `cat`: it stays alive across multiple writes (no exit on its
    /// own), and a default PTY echoes input back BEFORE delivering it to
    /// the canonical-mode reader, so writing `"hello\n"` produces
    /// `"hello"` twice on output (terminal echo + cat reply, both with
    /// ONLCR `\n`→`\r\n`). We use that "twice" property as the assertion
    /// — it's a stronger signal than "any output".
    #[test]
    fn echo_through_cat_pty() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("test.sock");

        let config = SupervisorConfig {
            command: "cat".to_string(),
            args: Vec::new(),
            cwd: None,
            rows: 24,
            cols: 80,
        };
        let mut supervisor = Supervisor::bind_with(&socket, config)?;

        let serve = thread::spawn(move || supervisor.serve_one());

        let mut stream = wait_for_unix_socket(&socket, Duration::from_secs(2))?;
        stream.set_read_timeout(Some(Duration::from_secs(1)))?;

        stream.write_all(b"hello\n")?;
        let got = drain_until_count(&mut stream, b"hello", 2, Duration::from_millis(1500));

        let count = count_occurrences(&got, b"hello");
        assert!(
            count >= 2,
            "expected 'hello' to appear at least twice (PTY echo + cat reply); \
             got {count} occurrences in {got:?}",
        );

        drop(stream);

        let join_result = serve.join();
        let Ok(serve_result) = join_result else {
            panic!("serve thread panicked");
        };
        serve_result?;
        Ok(())
    }

    /// **The reason this daemon exists.** Attach, write, disconnect.
    /// Reattach, write again. The second write reaching `cat` and being
    /// echoed back proves the PTY child survived the first disconnect —
    /// session continuity at the supervisor level.
    #[test]
    fn session_survives_client_disconnect_and_reattach() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("survive.sock");

        let config = SupervisorConfig {
            command: "cat".to_string(),
            args: Vec::new(),
            cwd: None,
            rows: 24,
            cols: 80,
        };
        let mut supervisor = Supervisor::bind_with(&socket, config)?;

        let serve = thread::spawn(move || -> Result<(), Error> {
            supervisor.serve_one()?;
            supervisor.serve_one()?;
            Ok(())
        });

        // First attach.
        {
            let mut s1 = wait_for_unix_socket(&socket, Duration::from_secs(2))?;
            s1.set_read_timeout(Some(Duration::from_secs(1)))?;
            s1.write_all(b"first\n")?;
            let got = drain_until_count(&mut s1, b"first", 2, Duration::from_millis(1500));
            let count = count_occurrences(&got, b"first");
            assert!(
                count >= 2,
                "first attach: expected 'first' twice (echo + cat reply); \
                 got {count} occurrences in {got:?}",
            );
        }

        // Second attach: cat should still be the same process. Its echo
        // reply to "second\n" is the proof.
        {
            let mut s2 = wait_for_unix_socket(&socket, Duration::from_secs(2))?;
            s2.set_read_timeout(Some(Duration::from_secs(1)))?;
            s2.write_all(b"second\n")?;
            let got = drain_until_count(&mut s2, b"second", 2, Duration::from_millis(1500));
            let count = count_occurrences(&got, b"second");
            assert!(
                count >= 2,
                "second attach: expected 'second' twice (proves session survived \
                 the first disconnect); got {count} occurrences in {got:?}",
            );
        }

        let join_result = serve.join();
        let Ok(serve_result) = join_result else {
            panic!("serve thread panicked");
        };
        serve_result?;
        Ok(())
    }

    /// `bind` consumes a `Cli` (clap-built) and exercises the full
    /// `from_cli` path that `bind_with` skips.
    #[test]
    fn bind_via_cli_succeeds_and_exposes_config() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("via-cli.sock");
        let socket_str = socket.to_string_lossy().into_owned();
        let cli = Cli::parse_from([
            "codemuxd",
            "--socket",
            &socket_str,
            "--rows",
            "30",
            "--cols",
            "100",
            "--",
            "cat",
        ]);
        let supervisor = Supervisor::bind(&cli)?;
        assert_eq!(supervisor.config.command, "cat");
        assert_eq!(supervisor.config.rows, 30);
        assert_eq!(supervisor.config.cols, 100);
        assert!(
            supervisor.session.is_none(),
            "session must be lazily spawned"
        );
        assert!(socket.exists(), "bind should create the socket file");
        Ok(())
    }

    /// Binding to an unwritable directory surfaces `Error::Bind` with the
    /// path embedded in the Display string.
    #[test]
    fn bind_to_unwritable_path_returns_bind_error() {
        let path =
            std::path::PathBuf::from("/this-directory-does-not-exist-on-any-machine/codemuxd.sock");
        let cli = Cli::parse_from(["codemuxd", "--socket", path.to_str().unwrap_or("/no.sock")]);
        let Err(err) = Supervisor::bind(&cli) else {
            unreachable!("bind to a nonexistent directory must fail");
        };
        assert!(
            matches!(err, Error::Bind { .. }),
            "expected Error::Bind, got {err:?}",
        );
    }

    fn wait_for_unix_socket(
        path: &Path,
        timeout: Duration,
    ) -> Result<UnixStream, Box<dyn std::error::Error>> {
        let deadline = Instant::now() + timeout;
        loop {
            match UnixStream::connect(path) {
                Ok(s) => return Ok(s),
                Err(_) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(e) => return Err(Box::new(e)),
            }
        }
    }

    /// Drain `stream` until `needle` appears `target` times or the
    /// timeout / EOF hits. Returns whatever was read.
    fn drain_until_count(
        stream: &mut UnixStream,
        needle: &[u8],
        target: usize,
        timeout: Duration,
    ) -> Vec<u8> {
        let mut got = Vec::new();
        let mut buf = [0u8; 256];
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    break;
                }
                Err(_) => break,
            }
            if count_occurrences(&got, needle) >= target {
                break;
            }
        }
        got
    }

    fn count_occurrences(haystack: &[u8], needle: &[u8]) -> usize {
        if needle.is_empty() || haystack.len() < needle.len() {
            return 0;
        }
        haystack
            .windows(needle.len())
            .filter(|w| *w == needle)
            .count()
    }
}
