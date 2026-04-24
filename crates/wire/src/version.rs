//! Protocol version and frame-size limits.

/// Wire protocol version. Bumped any time the framing or any message
/// payload changes shape. The local TUI controls the daemon's version
/// (it ships and deploys the matching binary), so a `VersionMismatch`
/// always means "redeploy", not "negotiate down."
pub const PROTOCOL_VERSION: u8 = 1;

/// Maximum byte count for a frame's inner contents (`type + payload`).
/// 1 MiB. A `len` larger than this is rejected before any payload is
/// read — protects against allocator `DoS` from malformed peers.
///
/// Sized so that a full PTY data chunk (8 KiB in the daemon's reader)
/// has plenty of headroom, but a deliberate or accidental gigabyte is
/// refused immediately.
pub const MAX_FRAME_LEN: usize = 1024 * 1024;
