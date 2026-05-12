//! Stub agent used by the T4 daemon E2E harness.
//!
//! Behavioral contract — narrow on purpose so the harness has a
//! deterministic surface to assert against:
//!
//! - On boot, write `FAKE_AGENT_READY> ` to stdout (no trailing newline).
//! - Flush immediately so the prompt lands in the daemon's vt100 mirror
//!   before any client could attach.
//! - Read stdin line-by-line until EOF and discard everything. The
//!   process must stay alive long enough for the harness to drive the
//!   wire protocol and observe state through the daemon's mirror.
//! - On EOF, exit 0.
//!
//! Mirrors the TUI's `fake_agent.rs` byte-for-byte by contract, but
//! lives under `apps/daemon` so the daemon package is self-contained
//! (NLM-flagged: workspace crates should not reach across `tests/bin/`
//! boundaries via `path =` references — duplicating ~50 lines is the
//! idiomatic trade-off).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, Write};

fn main() {
    {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        out.write_all(b"FAKE_AGENT_READY> ").unwrap();
        out.flush().unwrap();
    }

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        if line.is_err() {
            break;
        }
    }
}
