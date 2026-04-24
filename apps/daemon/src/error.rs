//! Daemon error type. Per AD-17: per-component thiserror enum,
//! `#[non_exhaustive]`, infrastructure-level failures wrapped in a boxed
//! source so the daemon's surface never leaks specific tool types.

use std::error::Error as StdError;
use std::path::PathBuf;

use thiserror::Error;

type BoxedSource = Box<dyn StdError + Send + Sync + 'static>;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    #[error("bind unix socket {path}")]
    Bind {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("accept connection")]
    Accept {
        #[source]
        source: std::io::Error,
    },

    #[error("spawn child {command}")]
    Spawn {
        command: String,
        #[source]
        source: BoxedSource,
    },

    #[error("pty operation")]
    Pty {
        #[source]
        source: BoxedSource,
    },

    /// A second client tried to attach while one was already connected.
    /// Per AD-3, single-attach is enforced; the late client is rejected.
    #[error("agent already attached")]
    AlreadyAttached,

    /// Another live daemon already holds the pid file. Stage 2 promotes
    /// supervisor exclusivity from "first to bind wins" (Stage 0) to
    /// "first to acquire the pid file wins". The held-pid is included so
    /// users can inspect or kill the offending process without grepping.
    #[error("pid file {} already locked by live process pid {pid}", path.display())]
    PidFileLocked { pid: u32, path: PathBuf },

    /// `--cwd` was supplied but the directory does not exist on this
    /// host. Surfaced before any side effects (no socket, no pid file)
    /// per vision principle 6: never silently fall back. Stage 4's
    /// bootstrap wraps this as `Bootstrap { stage: DaemonSpawn, .. }`.
    #[error("cwd {} does not exist", path.display())]
    CwdNotFound { path: PathBuf },

    /// The peer announced a wire-protocol version this daemon does not
    /// speak. The daemon sends an `Error{VersionMismatch}` frame and
    /// closes the connection; the client redeploys.
    #[error("wire version mismatch: client sent v{client}, daemon speaks v{daemon}")]
    VersionMismatch { client: u8, daemon: u8 },

    /// The peer's first frame after connect was not `Hello`. The
    /// handshake is mandatory before any other traffic.
    #[error("expected Hello as first frame, got tag 0x{got_tag:02X}")]
    HandshakeMissing { got_tag: u8 },

    /// The peer closed the socket before sending a complete `Hello`.
    #[error("peer disconnected before completing handshake")]
    HandshakeIncomplete,

    /// Wire-level encode/decode failure. Wraps the structured
    /// [`codemux_wire::Error`] so callers can inspect the kind without
    /// string parsing.
    #[error("wire frame error")]
    Wire {
        #[from]
        source: codemux_wire::Error,
    },

    #[error("io")]
    Io {
        #[from]
        source: std::io::Error,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn already_attached_display() {
        assert_eq!(Error::AlreadyAttached.to_string(), "agent already attached");
    }

    #[test]
    fn bind_display_includes_path() {
        let err = Error::Bind {
            path: "/tmp/x.sock".into(),
            source: std::io::Error::other("perm"),
        };
        assert_eq!(err.to_string(), "bind unix socket /tmp/x.sock");
    }

    #[test]
    fn accept_display() {
        let err = Error::Accept {
            source: std::io::Error::other("eof"),
        };
        assert_eq!(err.to_string(), "accept connection");
    }

    #[test]
    fn spawn_display_includes_command() {
        let err = Error::Spawn {
            command: "claude".into(),
            source: Box::new(std::io::Error::other("not found")),
        };
        assert_eq!(err.to_string(), "spawn child claude");
    }

    #[test]
    fn pty_display() {
        let err = Error::Pty {
            source: Box::new(std::io::Error::other("openpty failed")),
        };
        assert_eq!(err.to_string(), "pty operation");
    }

    #[test]
    fn io_conversion_preserves_source() {
        let io_err = std::io::Error::other("boom");
        let err: Error = io_err.into();
        assert_eq!(err.to_string(), "io");
        let Some(source) = err.source() else {
            unreachable!("Io variant must have a source")
        };
        assert_eq!(source.to_string(), "boom");
    }

    #[test]
    fn pid_file_locked_display_includes_pid_and_path() {
        let err = Error::PidFileLocked {
            pid: 12345,
            path: PathBuf::from("/tmp/codemuxd/agent.pid"),
        };
        assert_eq!(
            err.to_string(),
            "pid file /tmp/codemuxd/agent.pid already locked by live process pid 12345",
        );
    }

    #[test]
    fn cwd_not_found_display_includes_path() {
        let err = Error::CwdNotFound {
            path: PathBuf::from("/no/such/dir"),
        };
        assert_eq!(err.to_string(), "cwd /no/such/dir does not exist");
    }
}
