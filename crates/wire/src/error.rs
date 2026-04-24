//! Errors raised by encode/decode. Decode errors are *malformed-frame*
//! errors; "need more bytes" is signalled by `Ok(None)` from `try_decode`,
//! not via this enum.

use thiserror::Error;

use crate::version::MAX_FRAME_LEN;

#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// A frame's advertised inner length exceeds [`MAX_FRAME_LEN`].
    #[error("frame inner length {len} exceeds max {MAX_FRAME_LEN}")]
    Oversized { len: usize },

    /// A message payload claims a length that exceeds the encoded
    /// envelope. Most often a bug on the encoding side.
    #[error("payload length field {claimed} exceeds remaining {available} bytes")]
    PayloadLengthMismatch { claimed: usize, available: usize },

    /// The frame envelope was complete but smaller than the minimum a
    /// message of its tag requires (e.g. a Resize frame with no body).
    #[error("payload too short for {message_type}: have {have}, need at least {need}")]
    PayloadTooShort {
        message_type: &'static str,
        have: usize,
        need: usize,
    },

    /// The frame's tag byte does not match any known message variant.
    #[error("unknown message tag: 0x{tag:02X}")]
    UnknownMessageTag { tag: u8 },

    /// A `Signal` payload byte is not one of the recognised UNIX signals.
    #[error("unknown signal byte: {byte}")]
    UnknownSignal { byte: u8 },

    /// An `Error` payload's code field is not one of the recognised
    /// codes. The receiving side should typically map this to
    /// `ErrorCode::Internal` rather than reject the frame entirely.
    #[error("unknown error code: 0x{code:04X}")]
    UnknownErrorCode { code: u16 },

    /// A UTF-8 string field in a payload contained invalid UTF-8.
    #[error("invalid utf-8 in {field}")]
    InvalidUtf8 { field: &'static str },
}
