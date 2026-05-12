//! Stub agent used by the slow-tier PTY harness.
//!
//! Behavioral contract — kept narrow on purpose so the harness has a
//! deterministic surface to assert against:
//!
//! - On boot, write the literal string `FAKE_AGENT_READY> ` to stdout
//!   (no trailing newline; the trailing space mimics a prompt the user
//!   would type into).
//! - Flush immediately. PTYs line-buffer by default and an unflushed
//!   prompt would never appear in `vt100`'s cell grid until the next
//!   write — defeating `screen_eventually`'s very first assertion.
//! - Read stdin line-by-line until EOF and discard everything. This
//!   keeps the process alive long enough for the harness to send keys
//!   and observe state without the child exiting under it.
//! - On EOF, exit 0. The harness's `Drop` closes the PTY master, which
//!   causes the slave's stdin to hit EOF cleanly.
//!
//! No CLI args, no env vars, no `clap`, no `tokio` — `std` only. Any
//! incoming argv (e.g. the `--settings <json>` codemux passes to real
//! Claude) is ignored without parsing so we don't accidentally couple
//! the fake to the production agent's argv shape.
//!
//! Ignoring `unwrap_used` / `expect_used` at file scope is the
//! established pattern for test fixtures — failing loudly with a
//! panic is the most useful signal a test fixture can give.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, Write};

fn main() {
    {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        out.write_all(b"FAKE_AGENT_READY> ").unwrap();
        out.flush().unwrap();
    }

    // Lock stdin once outside the loop; re-locking the global stdin
    // mutex on every iteration would acquire and release it for each
    // line the harness sends. Discard input — the harness only feeds
    // keystrokes to exercise the renderer, not to drive the fake. An
    // `Err` item (controlling tty gone) is unrecoverable here and ends
    // the loop; iterator exhaustion is the EOF signal.
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        if line.is_err() {
            break;
        }
    }
}
