//! Message variants, tags, and encode/decode logic.
//!
//! The wire format is hand-coded: no `serde`, no derives. Reasons:
//! (a) fewer moving parts in a load-bearing protocol; (b) the byte layout
//! is documented in the source rather than emerging from derive macros;
//! (c) keeps the dependency surface tiny so the daemon can be built on a
//! fresh remote host without pulling half of crates.io.

use crate::error::Error;
use crate::version::MAX_FRAME_LEN;

/// Frame tags. Grouped so the high nibble hints at the category:
/// `0x0_` handshake, `0x1_` PTY I/O, `0x2_` lifecycle, `0x3_` keep-alive,
/// `0xF_` errors. Stable across protocol revisions of the same major
/// version — never renumber a tag, only add new ones.
mod tag {
    pub const HELLO: u8 = 0x01;
    pub const HELLO_ACK: u8 = 0x02;
    pub const PTY_DATA: u8 = 0x10;
    pub const RESIZE: u8 = 0x11;
    pub const SIGNAL: u8 = 0x12;
    pub const CHILD_EXITED: u8 = 0x20;
    pub const PING: u8 = 0x30;
    pub const PONG: u8 = 0x31;
    pub const ERROR: u8 = 0xFF;
}

/// All wire message variants.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Message {
    /// Client → daemon. First frame after connect; carries the version
    /// the client speaks plus the initial PTY geometry and the agent
    /// identifier the client wants to attach to.
    ///
    /// `session_id` and `resume_session_id` were added for AD-2
    /// resume-on-focus. The wire layer treats them as opaque text — the
    /// daemon's argv builder turns them into `--session-id <uuid>` /
    /// `--resume <uuid>`, the literals never appear in this crate. Empty
    /// `session_id` means "this client does not care to pin a session
    /// id" (older clients before the AD-2 work, or test harnesses that
    /// only exercise the framing); the daemon falls back to whichever
    /// argv it was started with in that case. `resume_session_id =
    /// Some(...)` is the resume-attempt marker — the daemon spawns
    /// `claude --resume <id>` instead of fresh.
    Hello {
        protocol_version: u8,
        rows: u16,
        cols: u16,
        agent_id: String,
        session_id: String,
        resume_session_id: Option<String>,
    },
    /// Daemon → client, in response to `Hello`. Confirms the daemon
    /// speaks a compatible version and returns its pid (useful for
    /// diagnostics — `kill -0 daemon_pid` from the client's host probes
    /// liveness without an extra round-trip).
    HelloAck {
        protocol_version: u8,
        daemon_pid: u32,
    },
    /// Bidirectional. Raw bytes for the PTY: client → daemon is input
    /// (typed keystrokes), daemon → client is output (terminal frames).
    PtyData(Vec<u8>),
    /// Client → daemon. New PTY geometry. The daemon applies it via
    /// `master.resize()`. Stage 2 wires the actual resize call.
    Resize { rows: u16, cols: u16 },
    /// Client → daemon. Forward this signal to the child process.
    Signal(Signal),
    /// Daemon → client. The PTY child has exited; the conn will close
    /// next.
    ChildExited { exit_code: i32 },
    /// Either direction. Liveness probe. Receiver should reply with
    /// `Pong` carrying the same nonce.
    Ping { nonce: u32 },
    /// Either direction. Reply to `Ping`.
    Pong { nonce: u32 },
    /// Either direction. Protocol or runtime error; the sender will
    /// typically close the connection immediately after.
    Error { code: ErrorCode, message: String },
}

/// UNIX signals the client may forward to the daemon's child. Restricted
/// to the small set that's meaningful for an interactive terminal session
/// — arbitrary signal numbers would let a hostile peer SIGSEGV the
/// child, which is not a feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
#[repr(u8)]
pub enum Signal {
    Hup = 1,
    Int = 2,
    Kill = 9,
    Term = 15,
}

impl Signal {
    #[must_use]
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// Decode a signal byte into one of the recognised UNIX signals.
    ///
    /// # Errors
    /// Returns [`Error::UnknownSignal`] if `byte` is not one of the four
    /// signals enumerated in [`Signal`].
    pub fn from_u8(byte: u8) -> Result<Self, Error> {
        match byte {
            1 => Ok(Self::Hup),
            2 => Ok(Self::Int),
            9 => Ok(Self::Kill),
            15 => Ok(Self::Term),
            _ => Err(Error::UnknownSignal { byte }),
        }
    }
}

/// Error codes carried in `Message::Error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
#[repr(u16)]
pub enum ErrorCode {
    VersionMismatch = 0x0001,
    UnknownAgent = 0x0002,
    AlreadyAttached = 0x0003,
    ChildSpawnFailed = 0x0004,
    BadFrame = 0x0005,
    Internal = 0xFFFF,
}

impl ErrorCode {
    #[must_use]
    pub fn as_u16(self) -> u16 {
        self as u16
    }

    /// Decode a 16-bit error code into one of the recognised
    /// [`ErrorCode`] variants.
    ///
    /// # Errors
    /// Returns [`Error::UnknownErrorCode`] if `code` is not one of the
    /// six codes enumerated in [`ErrorCode`].
    pub fn from_u16(code: u16) -> Result<Self, Error> {
        match code {
            0x0001 => Ok(Self::VersionMismatch),
            0x0002 => Ok(Self::UnknownAgent),
            0x0003 => Ok(Self::AlreadyAttached),
            0x0004 => Ok(Self::ChildSpawnFailed),
            0x0005 => Ok(Self::BadFrame),
            0xFFFF => Ok(Self::Internal),
            _ => Err(Error::UnknownErrorCode { code }),
        }
    }
}

impl Message {
    /// The frame tag byte used to encode this variant. Exposed so that
    /// non-wire code (notably the daemon's diagnostic paths) can identify
    /// a frame by tag without re-implementing the table.
    #[must_use]
    pub fn tag(&self) -> u8 {
        match self {
            Self::Hello { .. } => tag::HELLO,
            Self::HelloAck { .. } => tag::HELLO_ACK,
            Self::PtyData(_) => tag::PTY_DATA,
            Self::Resize { .. } => tag::RESIZE,
            Self::Signal(_) => tag::SIGNAL,
            Self::ChildExited { .. } => tag::CHILD_EXITED,
            Self::Ping { .. } => tag::PING,
            Self::Pong { .. } => tag::PONG,
            Self::Error { .. } => tag::ERROR,
        }
    }

    /// Encode this message into a fresh `Vec<u8>` containing the full
    /// frame envelope (length prefix + tag + payload).
    ///
    /// # Errors
    /// Returns [`Error::Oversized`] if the encoded inner length exceeds
    /// [`MAX_FRAME_LEN`].
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        let mut out = Vec::new();
        self.encode_to(&mut out)?;
        Ok(out)
    }

    /// Encode this message into the end of `out`. Useful for callers
    /// that batch frames into a single write buffer.
    ///
    /// On error, `out` is rolled back to its prior length so callers can
    /// reuse the buffer for the next frame.
    ///
    /// # Errors
    /// Returns [`Error::Oversized`] if the encoded inner length exceeds
    /// [`MAX_FRAME_LEN`].
    pub fn encode_to(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        // Reserve length prefix (filled in at the end once payload size
        // is known). Sticking the placeholder in now keeps the writes
        // sequential and avoids a second pass to compute total size.
        let len_offset = out.len();
        out.extend_from_slice(&[0; 4]);
        out.push(self.tag());
        self.encode_payload(out);

        // inner_len = bytes after the 4-byte length prefix
        let inner_len = out.len() - len_offset - 4;
        if inner_len > MAX_FRAME_LEN {
            // Roll back what we wrote so callers can reuse `out`.
            out.truncate(len_offset);
            return Err(Error::Oversized { len: inner_len });
        }
        // Cast: inner_len <= MAX_FRAME_LEN < u32::MAX, so this is lossless.
        #[allow(clippy::cast_possible_truncation)]
        let inner_len_u32 = inner_len as u32;
        out[len_offset..len_offset + 4].copy_from_slice(&inner_len_u32.to_be_bytes());
        Ok(())
    }

    fn encode_payload(&self, out: &mut Vec<u8>) {
        match self {
            Self::Hello {
                protocol_version,
                rows,
                cols,
                agent_id,
                session_id,
                resume_session_id,
            } => {
                out.push(*protocol_version);
                out.extend_from_slice(&rows.to_be_bytes());
                out.extend_from_slice(&cols.to_be_bytes());
                // Length-prefixed UTF-8 string. Order matters: any new
                // string field is appended AFTER the existing ones so
                // older encode paths (which omit them) decode as empty
                // / absent under the additive-field rule.
                encode_lp_string(out, agent_id);
                encode_lp_string(out, session_id);
                // resume_session_id is optional. 1-byte tag: 0 = None,
                // 1 = Some(<lp-string>).
                match resume_session_id {
                    None => out.push(0),
                    Some(id) => {
                        out.push(1);
                        encode_lp_string(out, id);
                    }
                }
            }
            Self::HelloAck {
                protocol_version,
                daemon_pid,
            } => {
                out.push(*protocol_version);
                out.extend_from_slice(&daemon_pid.to_be_bytes());
            }
            Self::PtyData(bytes) => {
                out.extend_from_slice(bytes);
            }
            Self::Resize { rows, cols } => {
                out.extend_from_slice(&rows.to_be_bytes());
                out.extend_from_slice(&cols.to_be_bytes());
            }
            Self::Signal(sig) => {
                out.push(sig.as_u8());
            }
            Self::ChildExited { exit_code } => {
                out.extend_from_slice(&exit_code.to_be_bytes());
            }
            Self::Ping { nonce } | Self::Pong { nonce } => {
                out.extend_from_slice(&nonce.to_be_bytes());
            }
            Self::Error { code, message } => {
                out.extend_from_slice(&code.as_u16().to_be_bytes());
                let msg_bytes = message.as_bytes();
                #[allow(clippy::cast_possible_truncation)]
                let msg_len = msg_bytes.len() as u32;
                out.extend_from_slice(&msg_len.to_be_bytes());
                out.extend_from_slice(msg_bytes);
            }
        }
    }
}

/// Try to decode a single frame from the start of `buf`.
///
/// Returns:
/// - `Ok(Some((message, consumed)))` when a complete frame was decoded;
///   `consumed` is the number of bytes that should be drained from the
///   start of `buf` before the next call.
/// - `Ok(None)` when more bytes are needed (`buf` does not yet contain a
///   complete frame). Caller should read more bytes and try again.
/// - `Err(_)` when the buffered bytes are malformed. The caller should
///   close the connection — there's no way to resync mid-stream.
///
/// # Errors
/// Returns [`Error`] for any malformed frame: oversized length, unknown
/// tag, malformed payload, invalid UTF-8, or unknown signal/error code.
pub fn try_decode(buf: &[u8]) -> Result<Option<(Message, usize)>, Error> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let inner_len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if inner_len > MAX_FRAME_LEN {
        return Err(Error::Oversized { len: inner_len });
    }
    if inner_len == 0 {
        // A frame must contain at least the tag byte.
        return Err(Error::PayloadTooShort {
            message_type: "frame envelope",
            have: 0,
            need: 1,
        });
    }
    let total = 4 + inner_len;
    if buf.len() < total {
        return Ok(None);
    }
    let tag = buf[4];
    let payload = &buf[5..total];
    let msg = decode_payload(tag, payload)?;
    Ok(Some((msg, total)))
}

fn decode_payload(tag: u8, payload: &[u8]) -> Result<Message, Error> {
    match tag {
        tag::HELLO => decode_hello(payload),
        tag::HELLO_ACK => decode_hello_ack(payload),
        tag::PTY_DATA => Ok(Message::PtyData(payload.to_vec())),
        tag::RESIZE => decode_resize(payload),
        tag::SIGNAL => decode_signal(payload),
        tag::CHILD_EXITED => decode_child_exited(payload),
        tag::PING => decode_ping(payload).map(|nonce| Message::Ping { nonce }),
        tag::PONG => decode_ping(payload).map(|nonce| Message::Pong { nonce }),
        tag::ERROR => decode_error(payload),
        other => Err(Error::UnknownMessageTag { tag: other }),
    }
}

fn decode_hello(payload: &[u8]) -> Result<Message, Error> {
    // Fixed prefix: 1 (version) + 2 (rows) + 2 (cols) = 5; the trailing
    // length-prefixed strings (agent_id, session_id) and the optional
    // resume tag are decoded incrementally below so the post-AD-2 wire
    // layout can grow without renumbering this constant.
    const FIXED: usize = 5;
    if payload.len() < FIXED {
        return Err(Error::PayloadTooShort {
            message_type: "Hello",
            have: payload.len(),
            need: FIXED,
        });
    }
    let protocol_version = payload[0];
    let rows = u16::from_be_bytes([payload[1], payload[2]]);
    let cols = u16::from_be_bytes([payload[3], payload[4]]);

    let mut cursor = FIXED;
    let agent_id = decode_lp_string(payload, &mut cursor, "agent_id")?;
    let session_id = decode_lp_string(payload, &mut cursor, "session_id")?;
    let resume_session_id = decode_optional_lp_string(payload, &mut cursor, "resume_session_id")?;
    if cursor != payload.len() {
        return Err(Error::PayloadLengthMismatch {
            claimed: cursor,
            available: payload.len(),
        });
    }
    Ok(Message::Hello {
        protocol_version,
        rows,
        cols,
        agent_id,
        session_id,
        resume_session_id,
    })
}

/// Append a 4-byte length prefix followed by the string's UTF-8 bytes.
/// Used by [`Message::Hello`] for `agent_id`, `session_id`, and the
/// `Some(_)` arm of `resume_session_id`. Keeping the helper local to
/// this module so the wire-layout invariants stay co-located with the
/// matching decode helper below.
fn encode_lp_string(out: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    // Cast: any string carried in a Hello frame is bounded by
    // MAX_FRAME_LEN minus header bytes; encode_to rolls back the buffer
    // if the total exceeds MAX_FRAME_LEN, so this u32 cast is safe in
    // practice.
    #[allow(clippy::cast_possible_truncation)]
    let len = bytes.len() as u32;
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
}

/// Read a `u32` length prefix and the following UTF-8 bytes from
/// `payload` at `*cursor`, advancing the cursor past the consumed
/// bytes. `field` names the field for the [`Error::InvalidUtf8`]
/// variant — pinned `&'static str` so the error type stays cheap to
/// construct in hot paths.
fn decode_lp_string(
    payload: &[u8],
    cursor: &mut usize,
    field: &'static str,
) -> Result<String, Error> {
    if payload.len() < *cursor + 4 {
        return Err(Error::PayloadTooShort {
            message_type: "Hello",
            have: payload.len(),
            need: *cursor + 4,
        });
    }
    let len = u32::from_be_bytes([
        payload[*cursor],
        payload[*cursor + 1],
        payload[*cursor + 2],
        payload[*cursor + 3],
    ]) as usize;
    *cursor += 4;
    if payload.len() < *cursor + len {
        return Err(Error::PayloadLengthMismatch {
            claimed: *cursor + len,
            available: payload.len(),
        });
    }
    let s = std::str::from_utf8(&payload[*cursor..*cursor + len])
        .map_err(|_| Error::InvalidUtf8 { field })?
        .to_string();
    *cursor += len;
    Ok(s)
}

/// Read a 1-byte tag (0 = None, 1 = Some) and conditionally a
/// length-prefixed UTF-8 string. Mirrors the optional-string protocol
/// pattern used by the AD-2 fields on [`Message::Hello`].
fn decode_optional_lp_string(
    payload: &[u8],
    cursor: &mut usize,
    field: &'static str,
) -> Result<Option<String>, Error> {
    if payload.len() < *cursor + 1 {
        return Err(Error::PayloadTooShort {
            message_type: "Hello",
            have: payload.len(),
            need: *cursor + 1,
        });
    }
    let tag = payload[*cursor];
    *cursor += 1;
    match tag {
        0 => Ok(None),
        1 => Ok(Some(decode_lp_string(payload, cursor, field)?)),
        other => Err(Error::UnknownMessageTag { tag: other }),
    }
}

fn decode_hello_ack(payload: &[u8]) -> Result<Message, Error> {
    const NEED: usize = 5;
    if payload.len() != NEED {
        return Err(Error::PayloadTooShort {
            message_type: "HelloAck",
            have: payload.len(),
            need: NEED,
        });
    }
    let protocol_version = payload[0];
    let daemon_pid = u32::from_be_bytes([payload[1], payload[2], payload[3], payload[4]]);
    Ok(Message::HelloAck {
        protocol_version,
        daemon_pid,
    })
}

fn decode_resize(payload: &[u8]) -> Result<Message, Error> {
    const NEED: usize = 4;
    if payload.len() != NEED {
        return Err(Error::PayloadTooShort {
            message_type: "Resize",
            have: payload.len(),
            need: NEED,
        });
    }
    let rows = u16::from_be_bytes([payload[0], payload[1]]);
    let cols = u16::from_be_bytes([payload[2], payload[3]]);
    Ok(Message::Resize { rows, cols })
}

fn decode_signal(payload: &[u8]) -> Result<Message, Error> {
    if payload.len() != 1 {
        return Err(Error::PayloadTooShort {
            message_type: "Signal",
            have: payload.len(),
            need: 1,
        });
    }
    Signal::from_u8(payload[0]).map(Message::Signal)
}

fn decode_child_exited(payload: &[u8]) -> Result<Message, Error> {
    const NEED: usize = 4;
    if payload.len() != NEED {
        return Err(Error::PayloadTooShort {
            message_type: "ChildExited",
            have: payload.len(),
            need: NEED,
        });
    }
    let exit_code = i32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
    Ok(Message::ChildExited { exit_code })
}

fn decode_ping(payload: &[u8]) -> Result<u32, Error> {
    const NEED: usize = 4;
    if payload.len() != NEED {
        return Err(Error::PayloadTooShort {
            message_type: "Ping/Pong",
            have: payload.len(),
            need: NEED,
        });
    }
    Ok(u32::from_be_bytes([
        payload[0], payload[1], payload[2], payload[3],
    ]))
}

fn decode_error(payload: &[u8]) -> Result<Message, Error> {
    // Fixed prefix: 2 (code) + 4 (message_len) = 6
    const FIXED: usize = 6;
    if payload.len() < FIXED {
        return Err(Error::PayloadTooShort {
            message_type: "Error",
            have: payload.len(),
            need: FIXED,
        });
    }
    let code_raw = u16::from_be_bytes([payload[0], payload[1]]);
    let code = ErrorCode::from_u16(code_raw)?;
    let msg_len = u32::from_be_bytes([payload[2], payload[3], payload[4], payload[5]]) as usize;
    let msg_bytes = &payload[FIXED..];
    if msg_len != msg_bytes.len() {
        return Err(Error::PayloadLengthMismatch {
            claimed: msg_len,
            available: msg_bytes.len(),
        });
    }
    let message = std::str::from_utf8(msg_bytes)
        .map_err(|_| Error::InvalidUtf8 {
            field: "error message",
        })?
        .to_string();
    Ok(Message::Error { code, message })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip helper: encode the message, decode it from the
    /// resulting bytes, and assert the decoded value matches.
    fn roundtrip(msg: &Message) -> Result<(), Error> {
        let bytes = msg.encode()?;
        let Some((decoded, consumed)) = try_decode(&bytes)? else {
            panic!("complete frame should decode but got Ok(None)");
        };
        assert_eq!(consumed, bytes.len(), "consumed != frame length");
        assert_eq!(&decoded, msg, "round-trip mismatch");
        Ok(())
    }

    #[test]
    fn roundtrip_hello() -> Result<(), Error> {
        roundtrip(&Message::Hello {
            protocol_version: 1,
            rows: 24,
            cols: 80,
            agent_id: "agent-abc-123".into(),
            session_id: "8e3c7632-f5ad-4e8c-bcbf-960c4a7d7c7d".into(),
            resume_session_id: None,
        })
    }

    #[test]
    fn roundtrip_hello_empty_agent_id() -> Result<(), Error> {
        roundtrip(&Message::Hello {
            protocol_version: 1,
            rows: 24,
            cols: 80,
            agent_id: String::new(),
            session_id: String::new(),
            resume_session_id: None,
        })
    }

    /// AD-2: the resume-on-focus path encodes `resume_session_id =
    /// Some(<uuid>)`. The wire layer treats it as opaque text; this
    /// just exercises the `Some` arm of the optional-string codec.
    #[test]
    fn roundtrip_hello_with_resume_session_id() -> Result<(), Error> {
        roundtrip(&Message::Hello {
            protocol_version: 1,
            rows: 24,
            cols: 80,
            agent_id: "agent-1".into(),
            session_id: "8e3c7632-f5ad-4e8c-bcbf-960c4a7d7c7d".into(),
            resume_session_id: Some("8e3c7632-f5ad-4e8c-bcbf-960c4a7d7c7d".into()),
        })
    }

    #[test]
    fn roundtrip_hello_ack() -> Result<(), Error> {
        roundtrip(&Message::HelloAck {
            protocol_version: 1,
            daemon_pid: 12345,
        })
    }

    #[test]
    fn roundtrip_pty_data() -> Result<(), Error> {
        roundtrip(&Message::PtyData(b"hello\r\n\x1b[31mred\x1b[0m".to_vec()))
    }

    #[test]
    fn roundtrip_pty_data_empty() -> Result<(), Error> {
        roundtrip(&Message::PtyData(Vec::new()))
    }

    #[test]
    fn roundtrip_resize() -> Result<(), Error> {
        roundtrip(&Message::Resize {
            rows: 50,
            cols: 200,
        })
    }

    #[test]
    fn roundtrip_signal_each_variant() -> Result<(), Error> {
        for sig in [Signal::Hup, Signal::Int, Signal::Kill, Signal::Term] {
            roundtrip(&Message::Signal(sig))?;
        }
        Ok(())
    }

    #[test]
    fn roundtrip_child_exited_negative() -> Result<(), Error> {
        // Negative exit codes encode signal-killed children on UNIX.
        roundtrip(&Message::ChildExited { exit_code: -15 })
    }

    #[test]
    fn roundtrip_child_exited_zero() -> Result<(), Error> {
        roundtrip(&Message::ChildExited { exit_code: 0 })
    }

    #[test]
    fn roundtrip_ping_pong() -> Result<(), Error> {
        roundtrip(&Message::Ping { nonce: 0xDEAD_BEEF })?;
        roundtrip(&Message::Pong { nonce: 0 })
    }

    #[test]
    fn roundtrip_error_each_code() -> Result<(), Error> {
        for code in [
            ErrorCode::VersionMismatch,
            ErrorCode::UnknownAgent,
            ErrorCode::AlreadyAttached,
            ErrorCode::ChildSpawnFailed,
            ErrorCode::BadFrame,
            ErrorCode::Internal,
        ] {
            roundtrip(&Message::Error {
                code,
                message: format!("test message for {code:?}"),
            })?;
        }
        Ok(())
    }

    /// A buffer shorter than the 4-byte length prefix yields `Ok(None)`.
    #[test]
    fn truncated_below_length_prefix_is_need_more() -> Result<(), Error> {
        for len in 0..4 {
            let buf = vec![0u8; len];
            assert!(try_decode(&buf)?.is_none(), "expected None for len={len}",);
        }
        Ok(())
    }

    /// A buffer with a complete length prefix but missing payload bytes
    /// yields `Ok(None)`.
    #[test]
    fn truncated_below_full_payload_is_need_more() -> Result<(), Error> {
        let bytes = Message::Hello {
            protocol_version: 1,
            rows: 24,
            cols: 80,
            agent_id: "abc".into(),
            session_id: String::new(),
            resume_session_id: None,
        }
        .encode()?;
        for n in 4..bytes.len() {
            assert!(
                try_decode(&bytes[..n])?.is_none(),
                "expected None at len={n}",
            );
        }
        Ok(())
    }

    /// A frame whose advertised inner length exceeds [`MAX_FRAME_LEN`]
    /// is rejected before any payload is consumed.
    #[test]
    fn oversized_frame_errors_immediately() {
        #[allow(clippy::cast_possible_truncation)]
        let bogus_len = (MAX_FRAME_LEN + 1) as u32;
        let mut buf = bogus_len.to_be_bytes().to_vec();
        buf.push(0x10);
        let Err(err) = try_decode(&buf) else {
            panic!("oversize must error");
        };
        assert!(matches!(err, Error::Oversized { .. }), "got {err:?}");
    }

    /// `PtyData` of exactly `MAX_FRAME_LEN - 1` bytes encodes successfully
    /// (1 byte for tag + payload). One byte more should error.
    #[test]
    fn pty_data_at_max_payload_succeeds_one_over_errors() -> Result<(), Error> {
        let max_payload = MAX_FRAME_LEN - 1;
        let ok = Message::PtyData(vec![0xAB; max_payload]).encode()?;
        let Some((decoded, consumed)) = try_decode(&ok)? else {
            panic!("max-sized PtyData should decode");
        };
        assert_eq!(consumed, ok.len());
        assert_eq!(decoded, Message::PtyData(vec![0xAB; max_payload]));

        let Err(err) = Message::PtyData(vec![0xAB; max_payload + 1]).encode() else {
            panic!("payload one byte too large must error");
        };
        assert!(matches!(err, Error::Oversized { .. }), "got {err:?}");
        Ok(())
    }

    /// An unknown message tag triggers an error rather than silently
    /// becoming a no-op. Future tag additions get explicit attention.
    #[test]
    fn unknown_message_tag_errors() {
        let mut buf = 1u32.to_be_bytes().to_vec();
        buf.push(0x77);
        let Err(err) = try_decode(&buf) else {
            panic!("unknown tag must error");
        };
        assert!(
            matches!(err, Error::UnknownMessageTag { tag: 0x77 }),
            "got {err:?}",
        );
    }

    #[test]
    fn unknown_signal_byte_errors() {
        let mut buf = 2u32.to_be_bytes().to_vec();
        buf.push(0x12);
        buf.push(0xAB);
        let Err(err) = try_decode(&buf) else {
            panic!("unknown signal must error");
        };
        assert!(
            matches!(err, Error::UnknownSignal { byte: 0xAB }),
            "got {err:?}",
        );
    }

    #[test]
    fn unknown_error_code_errors() {
        let mut buf = 7u32.to_be_bytes().to_vec();
        buf.push(0xFF);
        buf.extend_from_slice(&0xABCDu16.to_be_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes());
        let Err(err) = try_decode(&buf) else {
            panic!("unknown code must error");
        };
        assert!(
            matches!(err, Error::UnknownErrorCode { code: 0xABCD }),
            "got {err:?}",
        );
    }

    #[test]
    fn non_utf8_agent_id_errors() {
        // Inner len: tag(1) + version(1) + rows(2) + cols(2)
        //          + agent_id_len(4) + agent_id_bytes(2)
        //          + session_id_len(4) + session_id_bytes(0)
        //          + resume_tag(1)
        let inner_len: u32 = 1 + 1 + 2 + 2 + 4 + 2 + 4 + 1;
        let mut buf = inner_len.to_be_bytes().to_vec();
        buf.push(0x01);
        buf.push(1);
        buf.extend_from_slice(&24u16.to_be_bytes());
        buf.extend_from_slice(&80u16.to_be_bytes());
        buf.extend_from_slice(&2u32.to_be_bytes());
        buf.extend_from_slice(&[0xC3, 0x28]);
        // Empty session_id and resume_session_id = None so the decoder
        // reaches the agent_id UTF-8 check before any unrelated failure.
        buf.extend_from_slice(&0u32.to_be_bytes());
        buf.push(0);
        let Err(err) = try_decode(&buf) else {
            panic!("non-utf8 must error");
        };
        assert!(
            matches!(err, Error::InvalidUtf8 { field: "agent_id" }),
            "got {err:?}",
        );
    }

    #[test]
    fn resize_with_wrong_payload_length_errors() {
        let mut buf = 4u32.to_be_bytes().to_vec();
        buf.push(0x11);
        buf.extend_from_slice(&[0, 0, 0]);
        let Err(err) = try_decode(&buf) else {
            panic!("short Resize must error");
        };
        assert!(
            matches!(
                err,
                Error::PayloadTooShort {
                    message_type: "Resize",
                    ..
                }
            ),
            "got {err:?}",
        );
    }

    /// `try_decode` returns the consumed byte count so callers can drain
    /// a streaming buffer correctly even when extra bytes follow.
    #[test]
    fn extra_bytes_after_frame_are_left_alone() -> Result<(), Error> {
        let frame = Message::Ping { nonce: 42 }.encode()?;
        let mut buf = frame.clone();
        buf.extend_from_slice(b"extra bytes that should NOT be consumed");
        let Some((msg, consumed)) = try_decode(&buf)? else {
            panic!("complete frame should decode");
        };
        assert_eq!(msg, Message::Ping { nonce: 42 });
        assert_eq!(consumed, frame.len());
        Ok(())
    }

    /// Encode failure on oversize must leave the output buffer
    /// unchanged so callers can recover.
    #[test]
    fn oversized_encode_rolls_back_buffer() {
        let mut buf = b"prior contents".to_vec();
        let prior = buf.clone();
        let Err(err) = Message::PtyData(vec![0; MAX_FRAME_LEN]).encode_to(&mut buf) else {
            panic!("oversize must error");
        };
        assert!(matches!(err, Error::Oversized { .. }));
        assert_eq!(buf, prior, "output buffer must be unchanged on error");
    }
}
