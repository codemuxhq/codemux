//! `AgentTransport` — the seam between the runtime and the per-agent PTY.
//!
//! The runtime renders bytes and forwards keystrokes; whether those bytes
//! come from a child spawned in a *local* PTY or tunneled from a
//! *remote* `codemuxd` is the transport's concern, not the runtime's.
//! Stage 3 introduces this enum and ports the local variant — Stage 4
//! lights up the SSH variant in place.
//!
//! ## Why an enum and not a trait
//!
//! Every transport has the same six operations
//! (`try_read`/`write`/`resize`/`signal`/`try_wait`/`kill`) and there
//! will only ever be a small, closed set of variants (`Local`,
//! `SshDaemon`, and conceivably one or two others). A
//! `Box<dyn Transport>` would buy us nothing and cost us a vtable
//! indirection on every PTY byte. The enum keeps the dispatch local,
//! exhaustive, and inlinable.
//!
//! The enum is `#[non_exhaustive]` so external callers must add a
//! catch-all arm when matching. Both variants are declared from day
//! one — even though `SshDaemonPty` is unconstructible in Stage 3 —
//! so Stage 4 grows a body without forcing every match site to
//! re-check exhaustiveness.

use std::io::{Read, Write};
use std::path::Path;
use std::thread;

use codemux_wire::Signal;
use crossbeam_channel::{Receiver, unbounded};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

use crate::error::Error;

/// PTY read chunk size. Mirrors `apps/daemon/src/pty.rs::READ_BUFFER_SIZE`
/// — 8 KiB balances syscall overhead against burst output from a
/// terminal-mode child.
const READ_BUFFER_SIZE: usize = 8 * 1024;

/// Where an agent's PTY actually lives. The runtime owns one of these
/// per agent and only ever talks to it through the inherent methods on
/// this enum.
#[non_exhaustive]
pub enum AgentTransport {
    /// Child process spawned in a local PTY on the same host as the
    /// runtime. The default and only Stage 3 variant.
    Local(LocalPty),
    /// Tunneled connection to a `codemuxd` running on a remote host.
    /// Stage 3 stub; Stage 4 implements bootstrap + tunnel.
    SshDaemon(SshDaemonPty),
}

impl AgentTransport {
    /// Spawn the agent in a local PTY. Hardcodes `claude` as the agent
    /// binary — that's the product purpose. Tests construct
    /// [`LocalPty`] directly with a different command.
    ///
    /// `label` is purely advisory; it appears in tracing breadcrumbs so
    /// a multi-agent log is easy to follow.
    pub fn spawn_local(
        label: String,
        cwd: Option<&Path>,
        rows: u16,
        cols: u16,
    ) -> Result<Self, Error> {
        LocalPty::spawn("claude", &[], label, cwd, rows, cols).map(Self::Local)
    }

    /// Spawn the agent against a `codemuxd` reachable over SSH. **Stage 3
    /// stub** — bootstrap + tunnel land in Stage 4. The signature is
    /// fixed now so the spawn-modal call site (Stage 5) compiles
    /// against the final shape; the body just returns
    /// [`Error::NotImplemented`].
    pub fn spawn_ssh(
        _host: &str,
        _agent_id: &str,
        _cwd: &Path,
        _rows: u16,
        _cols: u16,
    ) -> Result<Self, Error> {
        Err(Error::NotImplemented {
            feature: "SSH agent transport",
        })
    }

    /// Drain whatever bytes the transport has buffered since the last
    /// call. Returns immediately; an empty `Vec` means "no new output
    /// right now", **not** "transport closed". Use [`Self::try_wait`] to
    /// detect liveness.
    ///
    /// Each chunk preserves the boundary the reader thread observed,
    /// which doesn't matter for the vt100 parser (it processes bytes
    /// incrementally) but does keep memory ownership contiguous with
    /// what the channel produced — saves a concat copy.
    pub fn try_read(&mut self) -> Vec<Vec<u8>> {
        match self {
            Self::Local(p) => p.try_read(),
            Self::SshDaemon(p) => p.try_read(),
        }
    }

    pub fn write(&mut self, data: &[u8]) -> Result<(), Error> {
        match self {
            Self::Local(p) => p.write(data),
            Self::SshDaemon(p) => p.write(data),
        }
    }

    /// Apply a new PTY geometry. Best-effort by convention: an `Err`
    /// here means the child sees a stale size until the next resize, a
    /// harmless cosmetic glitch (claude re-lays-out on the next paint).
    /// The runtime currently logs and continues — see
    /// `apps/tui/src/runtime.rs::resize_agents`.
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<(), Error> {
        match self {
            Self::Local(p) => p.resize(rows, cols),
            Self::SshDaemon(p) => p.resize(rows, cols),
        }
    }

    pub fn signal(&mut self, sig: Signal) -> Result<(), Error> {
        match self {
            Self::Local(p) => p.signal(sig),
            Self::SshDaemon(p) => p.signal(sig),
        }
    }

    /// Liveness check. `None` = still alive, `Some(code)` = exited
    /// with the given status. Best-effort: an underlying `try_wait`
    /// I/O error is reported as `None` so the runtime keeps the agent
    /// in the navigator until the next poll cycle (matches the existing
    /// retain-on-exit behaviour).
    pub fn try_wait(&mut self) -> Option<i32> {
        match self {
            Self::Local(p) => p.try_wait(),
            Self::SshDaemon(p) => p.try_wait(),
        }
    }

    pub fn kill(&mut self) -> Result<(), Error> {
        match self {
            Self::Local(p) => p.kill(),
            Self::SshDaemon(p) => p.kill(),
        }
    }
}

/// A child process spawned inside a local PTY.
///
/// Owns the same shape the previous `RuntimeAgent` had inline
/// (`master + writer + child + rx`). The `_master` is held only to keep
/// the master fd open — closing it would make the child see EOF on its
/// tty and exit immediately. Same invariant as
/// `apps/daemon/src/session.rs::Session._master`; if you find yourself
/// tempted to drop it earlier, don't.
pub struct LocalPty {
    label: String,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    rx: Receiver<Vec<u8>>,
    master: Box<dyn MasterPty + Send>,
}

impl LocalPty {
    /// Spawn `command args...` inside a fresh PTY of size `rows x cols`,
    /// optionally with `cwd` as the working directory.
    ///
    /// Public so tests can spawn `cat` instead of the production
    /// `claude` binary; the runtime always reaches this through
    /// [`AgentTransport::spawn_local`].
    pub fn spawn(
        command: &str,
        args: &[String],
        label: String,
        cwd: Option<&Path>,
        rows: u16,
        cols: u16,
    ) -> Result<Self, Error> {
        tracing::debug!(label, command, ?cwd, rows, cols, "spawning local PTY");

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| Error::Pty {
                source: Box::new(std::io::Error::other(format!("open pty: {e}"))),
            })?;

        let mut cmd = CommandBuilder::new(command);
        cmd.args(args);
        if let Some(cwd) = cwd {
            cmd.cwd(cwd);
        }
        let child = pair.slave.spawn_command(cmd).map_err(|e| Error::Spawn {
            command: command.to_string(),
            source: Box::new(std::io::Error::other(format!(
                "spawn `{command}` (is it on PATH?): {e}"
            ))),
        })?;
        // Closing the slave fd on the parent side is required so the
        // child sees EOF on its tty when it exits — without this, the
        // master read never returns EOF and the reader thread spins.
        drop(pair.slave);

        let writer = pair.master.take_writer().map_err(|e| Error::Pty {
            source: Box::new(std::io::Error::other(format!("take pty writer: {e}"))),
        })?;
        let reader = pair.master.try_clone_reader().map_err(|e| Error::Pty {
            source: Box::new(std::io::Error::other(format!("clone pty reader: {e}"))),
        })?;
        let master = pair.master;
        let rx = spawn_reader_thread(reader);
        Ok(Self {
            label,
            writer,
            child,
            rx,
            master,
        })
    }

    fn try_read(&mut self) -> Vec<Vec<u8>> {
        let mut chunks = Vec::new();
        while let Ok(bytes) = self.rx.try_recv() {
            chunks.push(bytes);
        }
        chunks
    }

    fn write(&mut self, data: &[u8]) -> Result<(), Error> {
        self.writer.write_all(data).map_err(|e| Error::Pty {
            source: Box::new(e),
        })
    }

    fn resize(&mut self, rows: u16, cols: u16) -> Result<(), Error> {
        self.master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| Error::Pty {
                source: Box::new(std::io::Error::other(format!("resize: {e}"))),
            })
    }

    fn signal(&mut self, sig: Signal) -> Result<(), Error> {
        // `portable-pty`'s `Child` only exposes `kill()` (SIGKILL).
        // Other signals would need `unsafe libc::kill` or a `nix` dep —
        // both deliberately out of scope here. The runtime delivers
        // Ctrl-C as the byte `0x03` through `write` instead, which is
        // the right interactive-terminal semantics anyway. The SSH
        // variant tunnels signals to the daemon, where the same
        // constraint applies on the remote side.
        if sig == Signal::Kill {
            return self.kill();
        }
        Err(Error::SignalNotSupported { signal: sig })
    }

    fn try_wait(&mut self) -> Option<i32> {
        // `portable-pty` exit codes are u32 (it folds Unix signal-killed
        // children into 128 + signum); cast saturating to i32 so a
        // hypothetical >2^31 code doesn't wrap into a negative — that
        // would lie to callers who treat negative as "killed by signal".
        match self.child.try_wait() {
            Ok(Some(status)) => Some(i32::try_from(status.exit_code()).unwrap_or(i32::MAX)),
            // Both `Ok(None)` (still running) and `Err(_)` (transient
            // wait failure) get reported as "alive" — the runtime
            // re-polls on the next tick and will reap on a future call.
            _ => None,
        }
    }

    fn kill(&mut self) -> Result<(), Error> {
        self.child.kill().map_err(|e| Error::Pty {
            source: Box::new(e),
        })
    }
}

impl Drop for LocalPty {
    fn drop(&mut self) {
        // Best-effort cleanup matching `apps/daemon/src/session.rs`:
        // log at debug so a stuck-zombie investigation has a
        // breadcrumb, but never panic from drop. `child.kill` /
        // `child.wait` may fail because the child is already gone
        // (EOF, SIGCHLD already reaped) — that's normal, not an error
        // worth surfacing.
        if let Err(e) = self.child.kill() {
            tracing::debug!(label = %self.label, "drop: child.kill failed: {e}");
        }
        if let Err(e) = self.child.wait() {
            tracing::debug!(label = %self.label, "drop: child.wait failed: {e}");
        }
    }
}

/// Stage 3 placeholder for the SSH transport. Stage 4 grows the body
/// (bootstrap, scp, remote build, daemon spawn, socket tunnel,
/// `Hello`/`HelloAck` handshake).
///
/// The struct is unconstructible from outside this module — its only
/// private field forbids `SshDaemonPty { ... }` syntax — and
/// [`AgentTransport::spawn_ssh`] returns [`Error::NotImplemented`]
/// before any code can reach the methods below. They exist to make
/// the enum's match arms compile, not as runtime behaviour.
pub struct SshDaemonPty {
    _private: (),
}

impl SshDaemonPty {
    // `unused_self`: Stage 4 grows real fields here (the SSH child,
    // tunnel handle, framed wire-protocol reader) and these methods
    // start using them. Quieting the lint at the impl level keeps the
    // stub's signatures honest about what Stage 4 will look like.
    #[allow(clippy::unused_self)]
    fn try_read(&mut self) -> Vec<Vec<u8>> {
        Vec::new()
    }

    #[allow(clippy::unused_self)]
    fn write(&mut self, _data: &[u8]) -> Result<(), Error> {
        Err(Error::NotImplemented {
            feature: "SSH agent transport",
        })
    }

    #[allow(clippy::unused_self)]
    fn resize(&mut self, _rows: u16, _cols: u16) -> Result<(), Error> {
        Err(Error::NotImplemented {
            feature: "SSH agent transport",
        })
    }

    #[allow(clippy::unused_self)]
    fn signal(&mut self, _sig: Signal) -> Result<(), Error> {
        Err(Error::NotImplemented {
            feature: "SSH agent transport",
        })
    }

    #[allow(clippy::unused_self)]
    fn try_wait(&mut self) -> Option<i32> {
        None
    }

    #[allow(clippy::unused_self)]
    fn kill(&mut self) -> Result<(), Error> {
        Err(Error::NotImplemented {
            feature: "SSH agent transport",
        })
    }
}

/// Background reader: drains the PTY master and pushes chunks into a
/// crossbeam channel. Exits on EOF or read error (including the master
/// being dropped). Same shape as `apps/daemon/src/pty.rs` and the prior
/// `apps/tui/src/runtime.rs::spawn_reader_thread`.
fn spawn_reader_thread(mut reader: Box<dyn Read + Send>) -> Receiver<Vec<u8>> {
    let (tx, rx) = unbounded::<Vec<u8>>();
    thread::spawn(move || {
        let mut buf = vec![0u8; READ_BUFFER_SIZE];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    rx
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    /// Round-trip the full `LocalPty` surface that the runtime depends on:
    /// write some bytes, drain the echoed output, resize, kill. `cat`
    /// echoes stdin to stdout so we don't need the production `claude`
    /// binary on `PATH`.
    ///
    /// Models `apps/daemon/src/pty.rs::tests::spawn_with_cwd_sets_child_working_directory`.
    #[test]
    fn local_transport_round_trips_write_read_resize_kill() -> Result<(), Box<dyn std::error::Error>>
    {
        let pty = LocalPty::spawn("cat", &[], "test-cat".into(), None, 24, 80)?;
        let mut transport = AgentTransport::Local(pty);

        // `cat` is line-buffered when its stdin is a TTY, so the carriage
        // return triggers an echo of the line back through the master.
        transport.write(b"hello\r")?;

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut got = Vec::new();
        loop {
            for chunk in transport.try_read() {
                got.extend_from_slice(&chunk);
            }
            if String::from_utf8_lossy(&got).contains("hello") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "expected `cat` to echo `hello` within 2s, got {:?}",
                String::from_utf8_lossy(&got),
            );
            std::thread::sleep(Duration::from_millis(20));
        }

        // Resize succeeds while the child is still alive; the geometry
        // change is invisible to a `cat` child but the syscall path is
        // exercised end-to-end.
        transport.resize(40, 120)?;

        // Verify try_wait reports alive before kill, dead after.
        assert!(
            transport.try_wait().is_none(),
            "freshly-spawned cat should still be alive",
        );

        transport.kill()?;

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if transport.try_wait().is_some() {
                return Ok(());
            }
            assert!(
                Instant::now() < deadline,
                "cat did not die within 2s of kill()",
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// SSH transport is a stub in Stage 3; constructing one must report
    /// `NotImplemented` rather than silently succeeding. Catches the
    /// regression where Stage 4's wiring partially lands without the
    /// body — the spawn-modal call site (Stage 5) keys off this Err to
    /// keep emitting `tracing::warn`.
    #[test]
    fn ssh_transport_spawn_returns_not_implemented() {
        let result = AgentTransport::spawn_ssh(
            "devpod.example",
            "agent-1",
            Path::new("/home/me/repo"),
            24,
            80,
        );
        let Err(err) = result else {
            unreachable!("ssh transport must error in Stage 3");
        };
        assert!(
            matches!(
                err,
                Error::NotImplemented {
                    feature: "SSH agent transport"
                }
            ),
            "expected NotImplemented, got {err:?}",
        );
    }

    /// Local transport rejects non-Kill signals with a precise variant
    /// rather than silently dropping them. Future work (stage where
    /// `nix` arrives or unsafe is permitted) will replace this with
    /// real signal delivery.
    #[test]
    fn local_signal_only_supports_kill() -> Result<(), Box<dyn std::error::Error>> {
        let pty = LocalPty::spawn("cat", &[], "test-sig".into(), None, 24, 80)?;
        let mut transport = AgentTransport::Local(pty);

        for sig in [Signal::Hup, Signal::Int, Signal::Term] {
            let Err(err) = transport.signal(sig) else {
                unreachable!("non-Kill signal {sig:?} must error on local transport");
            };
            assert!(
                matches!(err, Error::SignalNotSupported { signal: s } if s == sig),
                "expected SignalNotSupported({sig:?}), got {err:?}",
            );
        }

        // Kill is the one signal portable-pty exposes — verify it
        // actually reaps the child.
        transport.signal(Signal::Kill)?;

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if transport.try_wait().is_some() {
                return Ok(());
            }
            assert!(
                Instant::now() < deadline,
                "cat did not die within 2s of signal(Kill)",
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
