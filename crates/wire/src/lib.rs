//! `codemux-wire` — the on-the-wire protocol between `codemuxd` (host-side
//! daemon) and the `codemux` TUI (local).
//!
//! Per AD-3, the wire protocol is the artifact to design carefully; the
//! daemon and TUI implementations are replaceable around it.
//!
//! # Frame envelope
//!
//! Length-prefixed binary frames. All multi-byte integers are big-endian.
//!
//! ```text
//! +--------+--------+--------+--------+--------+========+
//! | len (u32 BE)                      | type   | payload|
//! +--------+--------+--------+--------+--------+========+
//! ```
//!
//! `len` is the byte count of `type + payload` (i.e. excludes the 4-byte
//! length prefix itself). Total wire bytes per frame = `4 + len`.
//!
//! `MAX_FRAME_LEN = 1 MiB`. Frames advertising a larger inner length are
//! rejected before any payload is read.
//!
//! # Versioning
//!
//! Version negotiation is exactly once, at `Hello`/`HelloAck`. The frame
//! envelope itself is unversioned: post-handshake, both peers know the
//! version and there's no scenario where individual frames could
//! disagree. Per-frame version bytes are deliberately omitted.

#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod error;
pub mod messages;
pub mod version;

pub use error::Error;
pub use messages::{ErrorCode, Message, Signal, try_decode};
pub use version::{MAX_FRAME_LEN, PROTOCOL_VERSION};
