//! Errors for the codemuxd SSH bootstrap.
//!
//! Kept separate from `codemux-session::Error` so the application core
//! does not depend on infrastructure-specific error shapes. Callers
//! that bridge a [`Bootstrap`] error into a session-level surface wrap
//! it as a boxed source.
//!
//! [`Bootstrap`]: Error::Bootstrap

use std::error::Error as StdError;

use thiserror::Error;

/// Boxed source error for infrastructure-level failures. Mirrors the
/// shape used by `codemux_session::Error::*` so callers can stitch
/// chains across crate boundaries without adapter types.
type BoxedSource = Box<dyn StdError + Send + Sync + 'static>;

/// Which step of the SSH bootstrap pipeline produced a failure. Carried
/// in [`Error::Bootstrap`] so the TUI can surface a stage-specific
/// message (each stage has a distinct actionable hint — "ssh refused",
/// "scp failed", "cargo not found on remote", etc.).
///
/// `#[non_exhaustive]` because future bootstrap variants (HTTP-based
/// installer, pre-built per-arch binaries) will add their own stages
/// without breaking match arms in callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Stage {
    /// `ssh host 'cat ~/.cache/codemuxd/agent.version'` — the cheap
    /// probe that decides whether to skip steps 2-4. Failure here
    /// usually means "ssh can't reach the host" or "auth refused".
    VersionProbe,
    /// Local-side: write the embedded tarball bytes to a tempfile we
    /// can then `scp`. Failure means a filesystem error in `$TMPDIR`.
    TarballStage,
    /// `scp local-tarball host:~/.cache/codemuxd/src/...`. Failure
    /// usually means transient network or remote disk full.
    Scp,
    /// `ssh host 'cargo build --release ...'`. Long-running. Failure
    /// usually means "cargo not on remote PATH" (actionable) or a real
    /// compilation error (rare unless the source is broken).
    RemoteBuild,
    /// `ssh host 'setsid -f ~/.cache/codemuxd/bin/codemuxd ...'`.
    /// Failure usually means the daemon CLI rejected its arguments
    /// (e.g. `cwd` not found) — surfaces as a single line of the
    /// daemon's stderr.
    DaemonSpawn,
    /// `ssh -N -L /tmp/...:~/.cache/.../sock host`. Failure usually
    /// means the OpenSSH version on either side doesn't support
    /// unix-socket forwarding (older than 6.7) or the local tunnel
    /// path is in use.
    SocketTunnel,
    /// `UnixStream::connect` against the local end of the tunnel.
    /// Failure after a short retry loop means the daemon never bound
    /// its socket — likely a remote crash mid-spawn.
    SocketConnect,
}

impl Stage {
    /// Short, user-readable label rendered next to the spinner in the
    /// spawn modal's locked path zone. Kept terse so the whole status
    /// line fits on the typical 80-100 col terminal even when the host
    /// name is long.
    ///
    /// Stays in this crate (next to the `Stage` enum it labels) so
    /// adding a new variant forces a label update in the same change
    /// — the variant + its label are a single concern. Marked `const`
    /// so callers can drop it into `&'static str` slots without runtime
    /// overhead.
    ///
    /// `Stage` is `#[non_exhaustive]` for downstream callers, but the
    /// match here is in the defining crate so the compiler enforces
    /// exhaustiveness — adding a variant is a build error until the
    /// label is wired up. That's the right pressure: a missing label
    /// would otherwise render as a confusing empty-parens in the modal.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::VersionProbe => "probing host",
            Self::TarballStage => "preparing source",
            Self::Scp => "uploading source",
            Self::RemoteBuild => "building remote daemon",
            Self::DaemonSpawn => "spawning daemon",
            Self::SocketTunnel => "opening tunnel",
            Self::SocketConnect => "connecting",
        }
    }
}

/// Errors raised by the bootstrap orchestration.
///
/// Per AD-17, marked `#[non_exhaustive]` so variants can be added
/// without breaking downstream match statements.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// SSH bootstrap failed. The `stage` field tells the caller which
    /// step of the pipeline tripped — the TUI uses this to render a
    /// stage-specific actionable message rather than a generic
    /// "ssh failed". The boxed `source` carries the underlying
    /// `io::Error` (or scripted error from a `FakeRunner` in tests).
    #[error("bootstrap failed at stage {stage:?}")]
    Bootstrap {
        stage: Stage,
        #[source]
        source: BoxedSource,
    },

    /// Wrapper around session-level errors raised from
    /// [`attach_agent`](crate::attach_agent) after the daemon is
    /// spawned and the tunnel is open (the handshake inside
    /// `SshDaemonPty::attach`).
    #[error("session error after bootstrap")]
    Session {
        #[source]
        source: BoxedSource,
    },
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::*;

    /// `Error::Bootstrap` includes the stage in its display string and
    /// preserves the underlying source for caller inspection. Each
    /// stage rendering is stable across releases (the TUI keys
    /// stage-specific messages off the Debug repr).
    #[test]
    fn bootstrap_display_includes_stage_and_preserves_source() {
        let err = Error::Bootstrap {
            stage: Stage::RemoteBuild,
            source: Box::new(io::Error::other("cargo: command not found")),
        };
        assert_eq!(err.to_string(), "bootstrap failed at stage RemoteBuild");
        let Some(source) = err.source() else {
            unreachable!("Bootstrap variant must have a source")
        };
        assert_eq!(source.to_string(), "cargo: command not found");
    }

    /// `Error::Session` preserves the wrapped session error's source
    /// chain, so TUI code can render the underlying session message
    /// (e.g. "handshake EOF") via `source().to_string()`.
    #[test]
    fn session_wrapper_preserves_source_chain() {
        let inner = io::Error::other("handshake EOF");
        let err = Error::Session {
            source: Box::new(inner),
        };
        assert_eq!(err.to_string(), "session error after bootstrap");
        let Some(source) = err.source() else {
            unreachable!("Session variant must have a source")
        };
        assert_eq!(source.to_string(), "handshake EOF");
    }
}
