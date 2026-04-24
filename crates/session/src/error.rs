use std::error::Error as StdError;

use codemux_wire::Signal;
use thiserror::Error;

/// Boxed source error for infrastructure-level failures. Infra adapters map
/// their tool-specific errors (`rusqlite::Error`, `std::io::Error`,
/// `portable_pty::Error`) into this form before handing them across the port
/// boundary, so the application core never depends on a specific tool's types.
type BoxedSource = Box<dyn StdError + Send + Sync + 'static>;

/// Errors raised by the session bounded context.
///
/// Per AD-17, marked `#[non_exhaustive]` so variants can be added without
/// breaking downstream match statements.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    #[error("agent not found: {id}")]
    AgentNotFound { id: String },

    #[error("host not found: {id}")]
    HostNotFound { id: String },

    #[error("pty transport error")]
    Pty {
        #[source]
        source: BoxedSource,
    },

    #[error("storage error")]
    Storage {
        #[source]
        source: BoxedSource,
    },

    #[error("ssh transport error")]
    Ssh {
        #[source]
        source: BoxedSource,
    },

    /// Failed to spawn an agent's child process. Distinct from
    /// [`Error::Pty`] (which covers PTY-system failures like `openpty`)
    /// so callers can tell "the binary is missing / `claude` not on PATH"
    /// apart from "the kernel ran out of pty fds".
    #[error("spawn child {command}")]
    Spawn {
        command: String,
        #[source]
        source: BoxedSource,
    },

    /// Stage 3 sentinel for transport surface that exists in the type
    /// signature but does not yet have a body. Returned today by the
    /// `SshDaemonPty` method stubs that haven't been wired; Stage 4
    /// retired the SSH-spawn path's use of this variant. Kept on the
    /// enum because future seams (e.g. a remote-host transport other
    /// than SSH) will reach for the same sentinel during their own
    /// walking-skeleton stage.
    #[error("not yet implemented: {feature}")]
    NotImplemented { feature: &'static str },

    /// The local PTY transport accepts the full [`Signal`] surface for
    /// symmetry with the SSH path, but only [`Signal::Kill`] can be
    /// delivered without unsafe libc calls (`Child::kill` is the only
    /// signal `portable-pty` exposes). Other signals on a local
    /// transport surface this error rather than silently misroute —
    /// the runtime tunnels Ctrl-C as the byte `0x03` instead, which is
    /// the right interactive-terminal semantics anyway.
    #[error("signal {signal:?} is not supported on local transport")]
    SignalNotSupported { signal: Signal },

    /// `Hello`/`HelloAck` exchange over an established socket failed —
    /// timeout, oversized frame, version mismatch, or framing error.
    /// Raised by [`SshDaemonPty::attach`](crate::transport::SshDaemonPty::attach).
    /// Bootstrap-stage errors (probe, scp, build, etc.) live in the
    /// `codemuxd-bootstrap` crate's own error type; this is the
    /// session-side handshake failure that follows a successful
    /// transport bring-up.
    #[error("handshake failed")]
    Handshake {
        #[source]
        source: BoxedSource,
    },

    /// Wire-protocol encode/decode failure. Wraps the structured
    /// [`codemux_wire::Error`] so callers can inspect the kind without
    /// string parsing. Reachable from the SSH transport's handshake and
    /// frame-reader paths.
    #[error("wire frame error")]
    Wire {
        #[from]
        source: codemux_wire::Error,
    },
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    #[test]
    fn display_messages_are_stable() {
        assert_eq!(
            Error::AgentNotFound { id: "alpha".into() }.to_string(),
            "agent not found: alpha",
        );
        assert_eq!(
            Error::HostNotFound {
                id: "laptop".into()
            }
            .to_string(),
            "host not found: laptop",
        );
    }

    #[test]
    fn source_chain_preserves_underlying_error() {
        let io_err = io::Error::other("spawn failed");
        let err = Error::Pty {
            source: Box::new(io_err),
        };

        assert_eq!(err.to_string(), "pty transport error");

        let Some(source) = err.source() else {
            unreachable!("Pty variant must have a source")
        };
        assert_eq!(source.to_string(), "spawn failed");
    }

    #[test]
    fn spawn_display_includes_command_and_preserves_source() {
        let err = Error::Spawn {
            command: "claude".into(),
            source: Box::new(io::Error::other("not found on PATH")),
        };
        assert_eq!(err.to_string(), "spawn child claude");
        let Some(source) = err.source() else {
            unreachable!("Spawn variant must have a source")
        };
        assert_eq!(source.to_string(), "not found on PATH");
    }

    #[test]
    fn not_implemented_display_includes_feature() {
        let err = Error::NotImplemented {
            feature: "SSH agent transport",
        };
        assert_eq!(err.to_string(), "not yet implemented: SSH agent transport",);
    }

    #[test]
    fn signal_not_supported_display_includes_signal_variant() {
        let err = Error::SignalNotSupported {
            signal: Signal::Int,
        };
        assert_eq!(
            err.to_string(),
            "signal Int is not supported on local transport",
        );
    }

    /// `Error::Handshake` carries the underlying source (timeout,
    /// EOF-before-HelloAck, oversized frame) for caller inspection.
    #[test]
    fn handshake_display_and_source_chain() {
        let err = Error::Handshake {
            source: Box::new(io::Error::other("EOF before HelloAck")),
        };
        assert_eq!(err.to_string(), "handshake failed");
        let Some(source) = err.source() else {
            unreachable!("Handshake variant must have a source")
        };
        assert_eq!(source.to_string(), "EOF before HelloAck");
    }

    /// `Error::Wire` converts from `codemux_wire::Error` via `#[from]` so
    /// the SSH transport's handshake path can use `?` directly on the
    /// wire decoder.
    #[test]
    fn wire_converts_from_codemux_wire_error() {
        let wire_err = codemux_wire::Error::Oversized { len: 99_999_999 };
        let display = wire_err.to_string();
        let err: Error = wire_err.into();
        assert_eq!(err.to_string(), "wire frame error");
        let Some(source) = err.source() else {
            unreachable!("Wire variant must have a source")
        };
        assert_eq!(source.to_string(), display);
    }
}
