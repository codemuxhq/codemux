//! Stub agent variant that prints a long stream of numbered lines
//! before the steady-state prompt. Used by the scroll-mode PTY tests
//! (AC-017, AC-018, AC-039) to populate the focused agent's vt100
//! scrollback so a mouse-wheel-up actually has history to scroll
//! into.
//!
//! Behavioral contract:
//!
//! - On boot, write 200 lines of `HISTORY <N>` (N from 0 through 199),
//!   each terminated by `\r\n`. 200 lines is enough that even after
//!   the visible 24-row grid takes the bottom slice, ~176 rows of
//!   history remain in the scrollback buffer -- room for several
//!   wheel ticks and a `g` (jump-to-top) gesture to land somewhere
//!   meaningful.
//! - Then write the literal `FAKE_AGENT_READY> ` prompt (same as the
//!   base `fake_agent`, no trailing newline). Tests can wait for the
//!   prompt to confirm the history has fully flushed -- the prompt
//!   is the LAST byte written, so seeing it on screen means
//!   `HISTORY 199` has already left the parser.
//! - Flush after every write so the bytes actually reach the master.
//! - Read stdin to EOF and discard.
//! - On EOF, exit 0.
//!
//! Single-purpose, no env vars, no clap. Same `test-fakes` gate as the
//! other fakes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, Write};

const HISTORY_LINES: u32 = 200;

fn main() {
    {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        for i in 0..HISTORY_LINES {
            // `\r\n` -- raw stdout in a PTY follows the kernel's
            // canonical line discipline, so the explicit CR keeps the
            // cursor from drifting. Real Claude does the same.
            writeln!(out, "HISTORY {i}\r").unwrap();
        }
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
