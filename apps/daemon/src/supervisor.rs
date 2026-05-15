//! Supervisor: owns the unix-socket listener and runs the accept loop.
//!
//! The supervisor's single responsibility is the runtime: accept a
//! client, attach it to the (lazily-spawned) [`Session`], handle a
//! disconnect, repeat. It owns nothing it didn't get handed by the
//! [`bootstrap`] module — [`Supervisor::new`] takes a
//! [`DaemonResources`] of already-bound listener, validated config, and
//! pid file guard.
//!
//! Sessions persist across client disconnects — the whole point of the
//! daemon is session continuity. If the child has exited between
//! attaches, the next accept respawns. Single-attach is implicit (the
//! accept loop is sequential, so a second client blocks in `connect`
//! until the first ends); a future stage may replace this with a
//! protocol-level `AlreadyAttached` rejection.
//!
//! Drop on the held [`Session`] kills the child; drop on the held
//! [`PidFile`] (inside [`DaemonResources`]) removes the pid file. Both
//! happen on a graceful supervisor shutdown.
//!
//! [`bootstrap`]: crate::bootstrap
//! [`DaemonResources`]: crate::bootstrap::DaemonResources
//! [`PidFile`]: crate::bootstrap::PidFile

use std::convert::Infallible;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;

use crate::bootstrap::{DaemonResources, PidFile};
use crate::cli::Cli;
use crate::conn;
use crate::error::Error;
use crate::session::Session;

/// What the supervisor needs to know about the child it spawns per
/// session. Built from a [`Cli`] in production, or constructed
/// directly by tests.
///
/// Pure runtime concern: paths the supervisor doesn't read at runtime
/// (pid file, socket) live in [`DaemonResources`] instead.
///
/// [`DaemonResources`]: crate::bootstrap::DaemonResources
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
    /// Held purely for its `Drop` — removes the pid file on daemon
    /// exit. Underscore prefix tells the linter we never read this;
    /// it's a liveness guard, not a value.
    _pid_file: Option<PidFile>,
}

impl Supervisor {
    /// Construct from already-prepared [`DaemonResources`]. Does no
    /// I/O. Pair with [`bootstrap::bring_up`] in production or
    /// [`bootstrap::bring_up_with`] in tests.
    ///
    /// [`bootstrap::bring_up`]: crate::bootstrap::bring_up
    /// [`bootstrap::bring_up_with`]: crate::bootstrap::bring_up_with
    #[must_use]
    pub fn new(resources: DaemonResources) -> Self {
        Self {
            listener: resources.listener,
            config: resources.config,
            session: None,
            _pid_file: resources.pid_file,
        }
    }

    /// Accept-loop forever. Each accepted connection runs to completion
    /// before the next is accepted; the underlying session persists. The
    /// `Infallible` Ok type encodes the operational invariant: this
    /// function only ever returns on error.
    pub fn serve(&mut self) -> Result<Infallible, Error> {
        loop {
            self.serve_one()?;
        }
    }

    /// Accept one connection and attach it to the (lazily-spawned)
    /// session. Returns when the conn ends. The session keeps running.
    ///
    /// AD-2 ordering: the handshake runs BEFORE the (lazy) session
    /// spawn. The client's `Hello.session_id` / `resume_session_id`
    /// drive claude argv when we have to spawn fresh; a live session
    /// is reused as-is (the new client's session-id is ignored — the
    /// supervisor does not respawn an already-running claude just
    /// because the wire fields differ). That matches the daemon's
    /// charter: continuity across attaches.
    pub fn serve_one(&mut self) -> Result<(), Error> {
        let (stream, _addr) = self
            .listener
            .accept()
            .map_err(|source| Error::Accept { source })?;
        tracing::info!("client attached");

        let mut handshake_buf = Vec::with_capacity(256);
        let hello = match conn::perform_handshake(&stream, &mut handshake_buf) {
            Ok(info) => info,
            Err(e) => {
                tracing::info!("handshake failed: {e}");
                // Best-effort socket teardown. The client side will see
                // EOF on its next read. We swallow shutdown errors —
                // the conn is already in a bad state by construction.
                let _ = stream.shutdown(std::net::Shutdown::Both);
                tracing::info!("client detached");
                return Err(e);
            }
        };

        let session = self.session_mut(&hello)?;
        let result = session.attach_post_handshake(stream, &hello, handshake_buf);

        tracing::info!("client detached");
        result
    }

    /// Return a mutable reference to a live session, spawning one (or
    /// replacing a dead one) if necessary. `Option::take` releases the
    /// borrow on `self.session` so we can either re-`insert` the same
    /// value (live) or replace it with a freshly-spawned one (dead).
    /// Without `take`, NLL still extends the original mutable borrow
    /// across the spawn branch.
    ///
    /// `hello` carries the AD-2 session-id fields; on a fresh spawn we
    /// thread them into the claude argv via [`build_child_args`]. A
    /// live session is returned untouched — the daemon's continuity
    /// invariant beats the wire-field hint.
    fn session_mut(&mut self, hello: &conn::HelloInfo) -> Result<&mut Session, Error> {
        if let Some(mut existing) = self.session.take() {
            if !existing.child_exited() {
                return Ok(self.session.insert(existing));
            }
            tracing::info!("previous session ended; spawning fresh session");
        }
        let args = build_child_args(
            &self.config.command,
            &self.config.args,
            hello.session_id.as_str(),
            hello.resume_session_id.as_deref(),
        );
        let new_session = Session::spawn(
            &self.config.command,
            &args,
            self.config.cwd.as_deref(),
            self.config.rows,
            self.config.cols,
        )?;
        Ok(self.session.insert(new_session))
    }
}

/// Build the argv passed to the child process, mixing the supervisor's
/// static argv (typically empty) with the AD-2 session-id fields
/// supplied by the client in [`HelloInfo`].
///
/// Behaviour:
/// - `command == "claude"` (the production case): appends
///   `--session-id <uuid>` when `session_id` is non-empty, OR
///   `--resume <uuid>` when `resume_session_id = Some(_)`. The two are
///   mutually exclusive on Claude Code's CLI; if both are present the
///   resume path wins (the spawn-time UUID is regenerated by the TUI
///   on the resume-failure auto-fallback, so this branch is the
///   intended order).
/// - Any other command: returned argv is the supervisor's static
///   `args` verbatim. We do not invent flags for shells the user
///   passed via `-- <cmd>` because we have no idea what their CLI
///   surface looks like; that path is the test/dev surface (`cat`,
///   `bash`, etc.) and adding `--session-id` to `cat` would just kill
///   the process.
///
/// This is THE one place in the daemon where the `--session-id` and
/// `--resume` literals live, per the AD-2 architectural boundary
/// (see `docs/004--architecture.md` and the 2026-05-15 persistence
/// spike). Keep them here — the wire crate, bootstrap crate, and
/// `session` crate must never grow these strings.
///
/// [`HelloInfo`]: crate::conn::HelloInfo
fn build_child_args(
    command: &str,
    base_args: &[String],
    session_id: &str,
    resume_session_id: Option<&str>,
) -> Vec<String> {
    if command != "claude" {
        return base_args.to_vec();
    }
    let mut args = base_args.to_vec();
    if let Some(resume) = resume_session_id
        && !resume.is_empty()
    {
        args.push("--resume".to_string());
        args.push(resume.to_string());
        return args;
    }
    if !session_id.is_empty() {
        args.push("--session-id".to_string());
        args.push(session_id.to_string());
    }
    args
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::thread;
    use std::time::{Duration, Instant};

    use codemux_wire::{self as wire, ErrorCode, Message};

    use super::*;
    use crate::bootstrap;

    fn cat_config() -> SupervisorConfig {
        SupervisorConfig {
            command: "cat".to_string(),
            args: Vec::new(),
            cwd: None,
            rows: 24,
            cols: 80,
        }
    }

    /// AD-2: a fresh-spawn Hello (`session_id` present,
    /// `resume_session_id` None) produces argv `[--session-id <uuid>]`
    /// on top of whatever static args the supervisor was launched with.
    #[test]
    fn build_child_args_fresh_spawn_appends_session_id_for_claude() {
        let uuid = "8e3c7632-f5ad-4e8c-bcbf-960c4a7d7c7d";
        let args = build_child_args("claude", &[], uuid, None);
        assert_eq!(
            args,
            vec!["--session-id".to_string(), uuid.to_string()],
            "fresh-spawn argv must end with `--session-id <uuid>`",
        );
    }

    /// AD-2: a resume Hello (`resume_session_id = Some(_)`) produces
    /// `[--resume <uuid>]` and never `[--session-id ...]` — they are
    /// mutually exclusive per Claude Code's CLI.
    #[test]
    fn build_child_args_resume_path_overrides_session_id_for_claude() {
        let fresh = "11111111-1111-4111-8111-111111111111";
        let resume = "22222222-2222-4222-8222-222222222222";
        let args = build_child_args("claude", &[], fresh, Some(resume));
        assert!(
            args.contains(&"--resume".to_string()),
            "resume argv must include `--resume`, got {args:?}",
        );
        assert!(
            args.contains(&resume.to_string()),
            "resume argv must include the resume uuid, got {args:?}",
        );
        assert!(
            !args.contains(&"--session-id".to_string()),
            "resume argv must NOT include `--session-id`, got {args:?}",
        );
    }

    /// AD-2 boundary: the supervisor only appends the claude-shaped
    /// flags when the configured command is literally `claude`. A
    /// `--` override like `bash` keeps its argv pristine so we never
    /// hand `--session-id` to a tool whose CLI doesn't accept it.
    #[test]
    fn build_child_args_non_claude_command_is_pristine() {
        let args = build_child_args(
            "bash",
            &["-l".into()],
            "8e3c7632-f5ad-4e8c-bcbf-960c4a7d7c7d",
            None,
        );
        assert_eq!(args, vec!["-l".to_string()]);
    }

    /// An empty `session_id` with no resume target produces an empty
    /// argv tail. Useful for the slow-tier test harness which speaks
    /// the wire protocol but doesn't care about the AD-2 fields.
    #[test]
    fn build_child_args_empty_session_id_no_resume_is_pristine() {
        let args = build_child_args("claude", &[], "", None);
        assert!(args.is_empty(), "no-op AD-2 fields produce no argv tail");
    }

    fn supervisor_for(socket: &Path) -> Result<Supervisor, Box<dyn std::error::Error>> {
        let resources = bootstrap::bring_up_with(socket, None, cat_config())?;
        Ok(Supervisor::new(resources))
    }

    /// End-to-end smoke for the wire protocol: handshake completes,
    /// then a typed `PtyData` frame echoes back through the cat child
    /// as `PtyData` frames. We use the "appears twice" trick (PTY echo
    /// + cat reply) on the accumulated `PtyData` payloads.
    #[test]
    fn echo_through_cat_pty() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("test.sock");
        let mut supervisor = supervisor_for(&socket)?;

        let serve = thread::spawn(move || supervisor.serve_one());

        let mut stream = wait_for_unix_socket(&socket, Duration::from_secs(2))?;
        stream.set_read_timeout(Some(Duration::from_secs(1)))?;

        let _daemon_pid = client_handshake(&mut stream, 24, 80, "test-agent")?;
        send_pty_data(&mut stream, b"hello\n")?;

        let got = drain_pty_data_until(&mut stream, b"hello", 2, Duration::from_millis(1500));
        let count = count_occurrences(&got, b"hello");
        assert!(
            count >= 2,
            "expected 'hello' to appear at least twice in PtyData payloads \
             (PTY echo + cat reply); got {count} occurrences in {got:?}",
        );

        drop(stream);

        let join_result = serve.join();
        let Ok(serve_result) = join_result else {
            panic!("serve thread panicked");
        };
        serve_result?;
        Ok(())
    }

    /// **The reason this daemon exists.** Handshake, write, disconnect.
    /// Reattach (new handshake), write again. The second `PtyData`
    /// reaching `cat` and being echoed back proves the PTY child
    /// survived the first disconnect — session continuity at the
    /// supervisor level, exercising the wire protocol re-handshake
    /// path.
    #[test]
    fn session_survives_client_disconnect_and_reattach() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("survive.sock");
        let mut supervisor = supervisor_for(&socket)?;

        let serve = thread::spawn(move || -> Result<(), Error> {
            supervisor.serve_one()?;
            supervisor.serve_one()?;
            Ok(())
        });

        {
            let mut s1 = wait_for_unix_socket(&socket, Duration::from_secs(2))?;
            s1.set_read_timeout(Some(Duration::from_secs(1)))?;
            client_handshake(&mut s1, 24, 80, "test-agent")?;
            send_pty_data(&mut s1, b"first\n")?;
            let got = drain_pty_data_until(&mut s1, b"first", 2, Duration::from_millis(1500));
            let count = count_occurrences(&got, b"first");
            assert!(
                count >= 2,
                "first attach: expected 'first' twice (echo + cat reply); \
                 got {count} occurrences in {got:?}",
            );
        }

        {
            let mut s2 = wait_for_unix_socket(&socket, Duration::from_secs(2))?;
            s2.set_read_timeout(Some(Duration::from_secs(1)))?;
            client_handshake(&mut s2, 24, 80, "test-agent")?;
            send_pty_data(&mut s2, b"second\n")?;
            let got = drain_pty_data_until(&mut s2, b"second", 2, Duration::from_millis(1500));
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

    /// **The bug this fix targets.** Without snapshot replay, a
    /// reattaching client sees a blank screen until something the child
    /// emits causes a redraw — and an idle Claude doesn't emit until
    /// the user types. This test reproduces that interactively: the
    /// first attach echoes a marker through `cat`'s PTY (so the marker
    /// is part of the screen state); the second attach immediately
    /// reads bytes WITHOUT writing anything and asserts that the
    /// marker appears in those bytes. That can only be true if the
    /// daemon serves the captured screen as the first `PtyData` frame.
    #[test]
    fn snapshot_replays_screen_state_on_reattach() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("snapshot.sock");
        let mut supervisor = supervisor_for(&socket)?;

        let serve = thread::spawn(move || -> Result<(), Error> {
            supervisor.serve_one()?;
            supervisor.serve_one()?;
            Ok(())
        });

        // First attach: write the marker, drain enough bytes to be
        // confident `cat` echoed it (line discipline echo + cat's
        // stdout reply both contain it). The daemon's parser captures
        // both occurrences as part of the screen.
        {
            let mut s1 = wait_for_unix_socket(&socket, Duration::from_secs(2))?;
            s1.set_read_timeout(Some(Duration::from_secs(1)))?;
            client_handshake(&mut s1, 24, 80, "snap-agent")?;
            // Discard the (empty) snapshot frame that gets sent first.
            // For a fresh session the snapshot is just clear-screen +
            // cursor home; it doesn't contain the marker yet.
            let _ = drain_pty_data_until(&mut s1, b"never-matches", 1, Duration::from_millis(100));
            send_pty_data(&mut s1, b"snapshot-marker\n")?;
            let got =
                drain_pty_data_until(&mut s1, b"snapshot-marker", 2, Duration::from_millis(1500));
            let count = count_occurrences(&got, b"snapshot-marker");
            assert!(
                count >= 2,
                "first attach should echo `snapshot-marker` twice (PTY echo + cat reply); \
                 got {count} occurrences in {got:?}",
            );
        }

        // Second attach: do NOT write anything. The very first PtyData
        // frame the daemon sends must already contain `snapshot-marker`
        // — that's the snapshot of the screen the previous attach left
        // behind. Without the snapshot path, this read would either
        // time out (idle child = no bytes) or return an empty payload.
        {
            let mut s2 = wait_for_unix_socket(&socket, Duration::from_secs(2))?;
            s2.set_read_timeout(Some(Duration::from_secs(1)))?;
            client_handshake(&mut s2, 24, 80, "snap-agent")?;
            let got = drain_pty_data_until(&mut s2, b"snapshot-marker", 1, Duration::from_secs(2));
            let count = count_occurrences(&got, b"snapshot-marker");
            assert!(
                count >= 1,
                "second attach should receive a snapshot frame containing the marker \
                 from the previous session's screen, without sending any input; \
                 got {count} occurrences in {got:?}",
            );
        }

        let join_result = serve.join();
        let Ok(serve_result) = join_result else {
            panic!("serve thread panicked");
        };
        serve_result?;
        Ok(())
    }

    /// Companion to `snapshot_replays_screen_state_on_reattach`: the
    /// snapshot path drains stale bytes from `rx` BEFORE sending so the
    /// snapshot isn't followed by a duplicate replay of the same data.
    /// Without the drain, anything the PTY reader buffered between
    /// attaches (which is also already in the parser) would be sent
    /// after the snapshot, double-painting the screen.
    ///
    /// We can't easily inject bytes into `rx` from here, but we can
    /// trigger `cat` to produce output during the disconnected window
    /// (write, then disconnect immediately so the bytes land in `rx`
    /// without an outbound to drain them). On reattach, the marker
    /// must appear EXACTLY in the snapshot — it must not also appear a
    /// second time as a stale `rx` replay.
    #[test]
    fn snapshot_drain_avoids_duplicate_replay_of_buffered_bytes()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("dedup.sock");
        let mut supervisor = supervisor_for(&socket)?;

        let serve = thread::spawn(move || -> Result<(), Error> {
            supervisor.serve_one()?;
            supervisor.serve_one()?;
            Ok(())
        });

        // First attach: write `dedup-marker\n`, then disconnect WITHOUT
        // draining the echoed bytes. The PTY's echo + `cat`'s reply
        // both end up in the daemon's `rx` channel after we close.
        {
            let mut s1 = wait_for_unix_socket(&socket, Duration::from_secs(2))?;
            s1.set_read_timeout(Some(Duration::from_millis(50)))?;
            client_handshake(&mut s1, 24, 80, "dedup-agent")?;
            send_pty_data(&mut s1, b"dedup-marker\n")?;
            // Sleep just enough for cat to produce output that's still
            // being read by the PTY reader thread when we disconnect.
            // We deliberately do NOT drain — the goal is to leave bytes
            // in the daemon-side channel.
            thread::sleep(Duration::from_millis(150));
        }

        // Second attach: the snapshot must contain the marker, AND it
        // must contain it the same number of times as the screen would
        // show (line discipline echo + cat reply = at most 2). What
        // would fail without `drain` is the marker appearing 3+ times:
        // once in the snapshot AND once or twice in the stale `rx`
        // replay that follows.
        {
            let mut s2 = wait_for_unix_socket(&socket, Duration::from_secs(2))?;
            s2.set_read_timeout(Some(Duration::from_millis(500)))?;
            client_handshake(&mut s2, 24, 80, "dedup-agent")?;
            let got = drain_pty_data_until(&mut s2, b"dedup-marker", 99, Duration::from_secs(1));
            let count = count_occurrences(&got, b"dedup-marker");
            assert!(
                (1..=2).contains(&count),
                "snapshot replay should appear at most twice (echo + reply) and never more \
                 — three or more would indicate a stale `rx` replay leaked through. \
                 got {count} occurrences in {got:?}",
            );
        }

        let join_result = serve.join();
        let Ok(serve_result) = join_result else {
            panic!("serve thread panicked");
        };
        serve_result?;
        Ok(())
    }

    /// A client that announces an unsupported protocol version gets an
    /// `Error{VersionMismatch}` frame from the daemon, and the
    /// supervisor's `serve_one` returns `Error::VersionMismatch` rather
    /// than treating it as a transport failure.
    #[test]
    fn handshake_version_mismatch_returns_error_frame() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("vermismatch.sock");
        let mut supervisor = supervisor_for(&socket)?;

        let serve = thread::spawn(move || supervisor.serve_one());

        let mut stream = wait_for_unix_socket(&socket, Duration::from_secs(2))?;
        stream.set_read_timeout(Some(Duration::from_secs(1)))?;

        let bogus = wire::PROTOCOL_VERSION.wrapping_add(99);
        let hello = Message::Hello {
            protocol_version: bogus,
            rows: 24,
            cols: 80,
            agent_id: String::new(),
            session_id: String::new(),
            resume_session_id: None,
        };
        stream.write_all(&hello.encode()?)?;

        let frame = read_one_frame(&mut stream, Duration::from_secs(2))?
            .ok_or("daemon closed without sending an Error frame")?;
        let Message::Error { code, .. } = frame else {
            panic!("expected Error frame, got {frame:?}");
        };
        assert_eq!(code, ErrorCode::VersionMismatch);

        let join_result = serve.join();
        let Ok(serve_result) = join_result else {
            panic!("serve thread panicked");
        };
        let Err(err) = serve_result else {
            panic!("serve_one should have errored on version mismatch");
        };
        assert!(
            matches!(err, Error::VersionMismatch { .. }),
            "expected Error::VersionMismatch, got {err:?}",
        );
        Ok(())
    }

    /// A client whose first frame is something other than `Hello`
    /// triggers `Error::HandshakeMissing` carrying the offending tag,
    /// and the supervisor surfaces it from `serve_one`.
    #[test]
    fn handshake_with_non_hello_first_frame_returns_handshake_missing()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("nonhello.sock");
        let mut supervisor = supervisor_for(&socket)?;

        let serve = thread::spawn(move || supervisor.serve_one());

        let mut stream = wait_for_unix_socket(&socket, Duration::from_secs(2))?;
        let bogus = Message::PtyData(b"i'm not Hello".to_vec()).encode()?;
        stream.write_all(&bogus)?;
        drop(stream);

        let join_result = serve.join();
        let Ok(serve_result) = join_result else {
            panic!("serve thread panicked");
        };
        let Err(err) = serve_result else {
            panic!("serve_one should have errored on missing Hello");
        };
        let Error::HandshakeMissing { got_tag } = err else {
            panic!("expected HandshakeMissing, got {err:?}");
        };
        assert_eq!(got_tag, 0x10, "tag should be PtyData (0x10)");
        Ok(())
    }

    /// A client that disconnects before sending a complete `Hello`
    /// triggers `Error::HandshakeIncomplete` rather than a generic
    /// transport error.
    #[test]
    fn handshake_with_eof_before_hello_returns_handshake_incomplete()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("eof.sock");
        let mut supervisor = supervisor_for(&socket)?;

        let serve = thread::spawn(move || supervisor.serve_one());

        let stream = wait_for_unix_socket(&socket, Duration::from_secs(2))?;
        drop(stream);

        let join_result = serve.join();
        let Ok(serve_result) = join_result else {
            panic!("serve thread panicked");
        };
        let Err(err) = serve_result else {
            panic!("serve_one should have errored on EOF before Hello");
        };
        assert!(
            matches!(err, Error::HandshakeIncomplete),
            "expected HandshakeIncomplete, got {err:?}",
        );
        Ok(())
    }

    /// Inbound non-PtyData frames must not close the connection — the next
    /// `PtyData` frame still flows. Stage 4 turned `Resize`/`Signal`/`Ping`
    /// into real handlers, but each is still designed to leave the conn
    /// alive (best-effort resize, no-op for non-Kill signals, Pong reply
    /// for Ping). This test guards that contract end-to-end.
    #[test]
    fn inbound_non_data_frames_do_not_close_connection() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("placeholders.sock");
        let mut supervisor = supervisor_for(&socket)?;

        let serve = thread::spawn(move || supervisor.serve_one());

        let mut stream = wait_for_unix_socket(&socket, Duration::from_secs(2))?;
        stream.set_read_timeout(Some(Duration::from_secs(1)))?;

        client_handshake(&mut stream, 24, 80, "test-agent")?;

        for frame in [
            Message::Resize {
                rows: 50,
                cols: 132,
            },
            Message::Signal(wire::Signal::Int),
            Message::Ping { nonce: 0xDEAD_BEEF },
            Message::Pong { nonce: 0xCAFE_BABE },
        ] {
            stream.write_all(&frame.encode()?)?;
        }
        send_pty_data(&mut stream, b"after-placeholders\n")?;

        let got = drain_pty_data_until(
            &mut stream,
            b"after-placeholders",
            2,
            Duration::from_millis(1500),
        );
        let count = count_occurrences(&got, b"after-placeholders");
        assert!(
            count >= 2,
            "connection must survive non-data frames; got {count} occurrences in {got:?}",
        );

        drop(stream);

        let join_result = serve.join();
        let Ok(serve_result) = join_result else {
            panic!("serve thread panicked");
        };
        serve_result?;
        Ok(())
    }

    /// `Resize` frames now actually reach the master PTY. We exercise this
    /// end-to-end by spawning a shell child, sending a Resize, then
    /// inspecting `$LINES x $COLUMNS` via `stty size`. Hooks into the
    /// existing handshake/echo plumbing — the `stty size\n` keystroke is
    /// echoed by the PTY (line discipline) and produces `R C` on stdout.
    #[test]
    fn inbound_resize_applies_to_master() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("resize.sock");
        // `bash -i` so $LINES/$COLUMNS stay populated when stty queries
        // them in a TTY context. Foreground the daemon (no pid file).
        let resources = bootstrap::bring_up_with(
            &socket,
            None,
            SupervisorConfig {
                command: "bash".to_string(),
                args: vec!["--noprofile".into(), "--norc".into()],
                cwd: None,
                rows: 24,
                cols: 80,
            },
        )?;
        let mut supervisor = Supervisor::new(resources);

        let serve = thread::spawn(move || supervisor.serve_one());

        let mut stream = wait_for_unix_socket(&socket, Duration::from_secs(2))?;
        stream.set_read_timeout(Some(Duration::from_secs(1)))?;

        client_handshake(&mut stream, 24, 80, "resize-agent")?;

        // Resize to a distinctive geometry, then ask the shell what it
        // thinks the size is. `stty size` prints "rows cols\n".
        stream.write_all(
            &Message::Resize {
                rows: 47,
                cols: 137,
            }
            .encode()?,
        )?;
        // Small pause to let the resize propagate to the child before
        // it queries — without this, stty can race the SIGWINCH delivery.
        thread::sleep(Duration::from_millis(100));
        send_pty_data(&mut stream, b"stty size\n")?;

        let got = drain_pty_data_until(&mut stream, b"47 137", 1, Duration::from_secs(2));
        assert!(
            got.windows(b"47 137".len()).any(|w| w == b"47 137"),
            "expected `47 137` from `stty size` after resize; got {:?}",
            String::from_utf8_lossy(&got),
        );

        drop(stream);
        let _ = serve.join();
        Ok(())
    }

    /// `Signal::Kill` reaches the child via `child.kill()`. The child is
    /// `cat` (idle on PTY input); after the kill the rx channel
    /// disconnects and the daemon emits `ChildExited` with the real
    /// exit code (the SIGKILL exit code, not the placeholder zero
    /// Stage 1/2 used).
    #[test]
    fn inbound_signal_kill_terminates_child() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("sigkill.sock");
        let mut supervisor = supervisor_for(&socket)?;

        let serve = thread::spawn(move || supervisor.serve_one());

        let mut stream = wait_for_unix_socket(&socket, Duration::from_secs(2))?;
        stream.set_read_timeout(Some(Duration::from_secs(1)))?;

        client_handshake(&mut stream, 24, 80, "kill-agent")?;
        stream.write_all(&Message::Signal(wire::Signal::Kill).encode()?)?;

        // Drain frames until ChildExited shows up. The exit code from
        // SIGKILL is platform-dependent — we just assert it's non-zero
        // (real exit, not the placeholder).
        let exit_code = await_child_exited(&mut stream, Duration::from_secs(2))?;
        assert_ne!(
            exit_code, 0,
            "SIGKILL should produce a non-zero exit code, not the Stage 1/2 placeholder",
        );

        let _ = serve.join();
        Ok(())
    }

    /// `Ping { nonce }` triggers a `Pong { nonce }` reply, with the same
    /// nonce. Locks down the inbound→socket-write path that Stage 4
    /// added (and the socket-write Mutex that serializes it with
    /// outbound's `PtyData` writes).
    #[test]
    fn inbound_ping_replies_with_pong() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("ping.sock");
        let mut supervisor = supervisor_for(&socket)?;

        let serve = thread::spawn(move || supervisor.serve_one());

        let mut stream = wait_for_unix_socket(&socket, Duration::from_secs(2))?;
        stream.set_read_timeout(Some(Duration::from_secs(1)))?;

        client_handshake(&mut stream, 24, 80, "ping-agent")?;

        let nonce = 0xCAFE_F00D_u32;
        stream.write_all(&Message::Ping { nonce }.encode()?)?;

        let pong = await_specific_frame(&mut stream, Duration::from_secs(2), |msg| {
            matches!(msg, Message::Pong { .. })
        })?;
        let Message::Pong { nonce: got_nonce } = pong else {
            panic!("expected Pong, got {pong:?}");
        };
        assert_eq!(
            got_nonce, nonce,
            "Pong nonce must echo the Ping nonce exactly",
        );

        drop(stream);
        let _ = serve.join();
        Ok(())
    }

    /// A client sending a server-only frame (`Hello`/`HelloAck`/
    /// `ChildExited`) post-handshake is a protocol violation — the
    /// daemon closes the connection cleanly without erroring
    /// `serve_one`.
    #[test]
    fn inbound_server_only_frame_closes_connection_cleanly()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("server-only.sock");
        let mut supervisor = supervisor_for(&socket)?;

        let serve = thread::spawn(move || supervisor.serve_one());

        let mut stream = wait_for_unix_socket(&socket, Duration::from_secs(2))?;
        stream.set_read_timeout(Some(Duration::from_secs(1)))?;

        client_handshake(&mut stream, 24, 80, "test-agent")?;

        let bogus = Message::Hello {
            protocol_version: wire::PROTOCOL_VERSION,
            rows: 24,
            cols: 80,
            agent_id: "agent".to_string(),
            session_id: String::new(),
            resume_session_id: None,
        }
        .encode()?;
        stream.write_all(&bogus)?;

        await_clean_close(
            &mut stream,
            Duration::from_secs(2),
            "daemon did not close conn after server-only inbound frame",
        );

        let join_result = serve.join();
        let Ok(serve_result) = join_result else {
            panic!("serve thread panicked");
        };
        serve_result?;
        Ok(())
    }

    /// A client sending an `Error` frame causes the daemon to close
    /// the connection cleanly.
    #[test]
    fn inbound_error_frame_from_client_closes_connection_cleanly()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("client-error.sock");
        let mut supervisor = supervisor_for(&socket)?;

        let serve = thread::spawn(move || supervisor.serve_one());

        let mut stream = wait_for_unix_socket(&socket, Duration::from_secs(2))?;
        stream.set_read_timeout(Some(Duration::from_secs(1)))?;

        client_handshake(&mut stream, 24, 80, "test-agent")?;

        let err_frame = Message::Error {
            code: ErrorCode::Internal,
            message: "client gave up".to_string(),
        }
        .encode()?;
        stream.write_all(&err_frame)?;

        await_clean_close(
            &mut stream,
            Duration::from_secs(2),
            "daemon did not close conn after client Error frame",
        );

        let join_result = serve.join();
        let Ok(serve_result) = join_result else {
            panic!("serve thread panicked");
        };
        serve_result?;
        Ok(())
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

    /// Send a `Hello` frame and read back a `HelloAck`. Returns the
    /// daemon's reported pid for any tests that want to assert on it.
    fn client_handshake(
        stream: &mut UnixStream,
        rows: u16,
        cols: u16,
        agent_id: &str,
    ) -> Result<u32, Box<dyn std::error::Error>> {
        let hello = Message::Hello {
            protocol_version: wire::PROTOCOL_VERSION,
            rows,
            cols,
            agent_id: agent_id.to_string(),
            session_id: String::new(),
            resume_session_id: None,
        };
        stream.write_all(&hello.encode()?)?;
        let frame = read_one_frame(stream, Duration::from_secs(2))?
            .ok_or("daemon closed before HelloAck")?;
        let Message::HelloAck { daemon_pid, .. } = frame else {
            return Err(format!("expected HelloAck, got {frame:?}").into());
        };
        Ok(daemon_pid)
    }

    fn send_pty_data(
        stream: &mut UnixStream,
        bytes: &[u8],
    ) -> Result<(), Box<dyn std::error::Error>> {
        let frame = Message::PtyData(bytes.to_vec()).encode()?;
        stream.write_all(&frame)?;
        stream.flush()?;
        Ok(())
    }

    /// Read until exactly one complete frame is available; return
    /// `Ok(None)` only on clean EOF before any frame appears.
    fn read_one_frame(
        stream: &mut UnixStream,
        timeout: Duration,
    ) -> Result<Option<Message>, Box<dyn std::error::Error>> {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 256];
        let deadline = Instant::now() + timeout;
        loop {
            if let Some((msg, _consumed)) = wire::try_decode(&buf)? {
                return Ok(Some(msg));
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "timed out waiting for frame; buffered {} bytes: {buf:?}",
                    buf.len(),
                )
                .into());
            }
            match stream.read(&mut tmp) {
                Ok(0) => return Ok(None),
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => return Err(Box::new(e)),
            }
        }
    }

    /// Read frames from `stream`, accumulate each `PtyData` payload,
    /// and stop once `needle` appears `target` times in the
    /// accumulated bytes (or the timeout / EOF hits). Non-`PtyData`
    /// frames are silently drained.
    fn drain_pty_data_until(
        stream: &mut UnixStream,
        needle: &[u8],
        target: usize,
        timeout: Duration,
    ) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut accumulated = Vec::new();
        let mut tmp = [0u8; 1024];
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            loop {
                match wire::try_decode(&buf) {
                    Ok(Some((Message::PtyData(bytes), consumed))) => {
                        buf.drain(..consumed);
                        accumulated.extend_from_slice(&bytes);
                    }
                    Ok(Some((_other, consumed))) => {
                        buf.drain(..consumed);
                    }
                    Ok(None) => break,
                    Err(_) => return accumulated,
                }
            }
            if count_occurrences(&accumulated, needle) >= target {
                break;
            }
            match stream.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    break;
                }
                Err(_) => break,
            }
        }
        accumulated
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

    /// Block until the daemon closes the stream (`read` returns 0) or
    /// the deadline elapses. Discards any frames the daemon sends
    /// meanwhile — used by tests that only care that the close happens.
    fn await_clean_close(stream: &mut UnixStream, timeout: Duration, msg: &'static str) {
        let mut sink = [0u8; 256];
        let deadline = Instant::now() + timeout;
        loop {
            assert!(Instant::now() < deadline, "{msg}");
            match stream.read(&mut sink) {
                Ok(0) => return,
                Ok(_) => {}
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(_) => return,
            }
        }
    }

    /// Drain frames until a `ChildExited` arrives, return its exit code.
    /// Errors on timeout or transport close.
    fn await_child_exited(
        stream: &mut UnixStream,
        timeout: Duration,
    ) -> Result<i32, Box<dyn std::error::Error>> {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        let deadline = Instant::now() + timeout;
        loop {
            loop {
                match wire::try_decode(&buf)? {
                    Some((Message::ChildExited { exit_code }, consumed)) => {
                        buf.drain(..consumed);
                        return Ok(exit_code);
                    }
                    Some((_, consumed)) => {
                        buf.drain(..consumed);
                    }
                    None => break,
                }
            }
            if Instant::now() >= deadline {
                return Err("timed out waiting for ChildExited".into());
            }
            match stream.read(&mut tmp) {
                Ok(0) => return Err("transport closed before ChildExited".into()),
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => return Err(Box::new(e)),
            }
        }
    }

    /// Drain frames until one matching the predicate arrives. Used by the
    /// Pong test to ignore any incidental `PtyData` / log frames.
    fn await_specific_frame(
        stream: &mut UnixStream,
        timeout: Duration,
        predicate: impl Fn(&Message) -> bool,
    ) -> Result<Message, Box<dyn std::error::Error>> {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        let deadline = Instant::now() + timeout;
        loop {
            while let Some((msg, consumed)) = wire::try_decode(&buf)? {
                buf.drain(..consumed);
                if predicate(&msg) {
                    return Ok(msg);
                }
            }
            if Instant::now() >= deadline {
                return Err("timed out waiting for matching frame".into());
            }
            match stream.read(&mut tmp) {
                Ok(0) => return Err("transport closed before matching frame".into()),
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) => return Err(Box::new(e)),
            }
        }
    }
}
