//! Daemon error type. Per AD-17: per-component thiserror enum,
//! `#[non_exhaustive]`, infrastructure-level failures wrapped in a boxed
//! source so the daemon's surface never leaks specific tool types.

use std::error::Error as StdError;

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
}
