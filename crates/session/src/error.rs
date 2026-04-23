use std::error::Error as StdError;

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
            Error::HostNotFound { id: "laptop".into() }.to_string(),
            "host not found: laptop",
        );
    }

    #[test]
    fn source_chain_preserves_underlying_error() {
        let io_err = io::Error::other("spawn failed");
        let err = Error::Pty { source: Box::new(io_err) };

        assert_eq!(err.to_string(), "pty transport error");

        let Some(source) = err.source() else {
            unreachable!("Pty variant must have a source")
        };
        assert_eq!(source.to_string(), "spawn failed");
    }
}
