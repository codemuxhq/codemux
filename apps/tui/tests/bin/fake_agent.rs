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

    let stdin = std::io::stdin();
    let mut line = String::new();
    loop {
        line.clear();
        match stdin.lock().read_line(&mut line) {
            // Discard input. The harness only sends keystrokes to
            // exercise the renderer; the fake is not expected to
            // react. Any non-zero byte count keeps the loop alive.
            Ok(n) if n > 0 => {}
            // Zero bytes is EOF (the harness closed the PTY master);
            // any read error on the controlling tty is unrecoverable
            // for a fixture this thin. Both lead to a clean exit.
            _ => break,
        }
    }
}
