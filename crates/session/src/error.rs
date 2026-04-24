use std::error::Error as StdError;

use codemux_wire::Signal;
use thiserror::Error;

/// Boxed source error for infrastructure-level failures. Infra adapters map
/// their tool-specific errors (`rusqlite::Error`, `std::io::Error`,
/// `portable_pty::Error`) into this form before handing them across the port
/// boundary, so the application core never depends on a specific tool's types.
type BoxedSource = Box<dyn StdError + Send + Sync + 'static>;

/// Which step of the SSH bootstrap pipeline produced a failure. Carried in
/// [`Error::Bootstrap`] so the TUI can surface a stage-specific message
/// (each stage has a distinct actionable hint — "ssh refused", "scp failed",
/// "cargo not found on remote", etc.).
///
/// `#[non_exhaustive]` because future bootstrap variants (HTTP-based
/// installer, pre-built per-arch binaries) will add their own stages without
/// breaking match arms in callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum BootstrapStage {
    /// `ssh host 'cat ~/.cache/codemuxd/agent.version'` — the cheap probe
    /// that decides whether to skip steps 2-4. Failure here usually means
    /// "ssh can't reach the host" or "auth refused".
    VersionProbe,
    /// Local-side: write the embedded tarball bytes to a tempfile we can
    /// then `scp`. Failure means a filesystem error in `$TMPDIR`.
    TarballStage,
    /// `scp local-tarball host:~/.cache/codemuxd/src/...`. Failure usually
    /// means transient network or remote disk full.
    Scp,
    /// `ssh host 'cargo build --release ...'`. Long-running. Failure
    /// usually means "cargo not on remote PATH" (actionable) or a real
    /// compilation error (rare unless the source is broken).
    RemoteBuild,
    /// `ssh host 'setsid -f ~/.cache/codemuxd/bin/codemuxd ...'`. Failure
    /// usually means the daemon CLI rejected its arguments (e.g. `cwd`
    /// not found) — surfaces as a single line of the daemon's stderr.
    DaemonSpawn,
    /// `ssh -N -L /tmp/...:~/.cache/.../sock host`. Failure usually means
    /// the OpenSSH version on either side doesn't support unix-socket
    /// forwarding (older than 6.7) or the local tunnel path is in use.
    SocketTunnel,
    /// `UnixStream::connect` against the local end of the tunnel. Failure
    /// after a short retry loop means the daemon never bound its socket
    /// — likely a remote crash mid-spawn.
    SocketConnect,
    /// `Hello`/`HelloAck` exchange over the connected socket. Failure
    /// means a protocol-version mismatch or a corrupted frame.
    Handshake,
}

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
    /// signature but does not yet have a body. Returned today by
    /// `AgentTransport::spawn_ssh` and the `SshDaemonPty` method stubs;
    /// Stage 4 replaces those bodies and this variant becomes unused
    /// for that path. Kept on the enum because future seams (e.g. a
    /// remote-host transport other than SSH) will reach for the same
    /// sentinel during their own walking-skeleton stage.
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

    /// SSH bootstrap failed. The `stage` field tells the caller which step
    /// of the pipeline tripped — the TUI uses this to render a
    /// stage-specific actionable message rather than a generic "ssh
    /// failed". The boxed `source` carries the underlying `io::Error` (or
    /// scripted error from a `FakeRunner` in tests).
    #[error("bootstrap failed at stage {stage:?}")]
    Bootstrap {
        stage: BootstrapStage,
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

    /// `Error::Bootstrap` includes the stage in its display string and
    /// preserves the underlying source for caller inspection. Each
    /// stage rendering is stable across releases (the TUI keys
    /// stage-specific messages off the Debug repr).
    #[test]
    fn bootstrap_display_includes_stage_and_preserves_source() {
        let err = Error::Bootstrap {
            stage: BootstrapStage::RemoteBuild,
            source: Box::new(io::Error::other("cargo: command not found")),
        };
        assert_eq!(err.to_string(), "bootstrap failed at stage RemoteBuild");
        let Some(source) = err.source() else {
            unreachable!("Bootstrap variant must have a source")
        };
        assert_eq!(source.to_string(), "cargo: command not found");
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
