//! Stub agent variant that prints a known URL on boot. Used by the
//! AC-041 PTY test (`pty_url_open.rs`) to drive the Ctrl+click + URL-
//! opener pipeline without depending on the user typing anything --
//! the production agent (`claude`) would print URLs in normal usage,
//! but a more deterministic fake serves the test's narrow surface.
//!
//! Behavioral contract:
//!
//! - On boot, write the literal string `FAKE_AGENT_READY>
//!   https://example.com/codemux-test ` to stdout (one line, trailing
//!   space). The URL is unique enough that the test's assertion on
//!   the recorded-open file is unambiguous, and `https://` exercises
//!   `url_scan`'s standard https path (the most common in production).
//! - Flush immediately, same rationale as `fake_agent.rs`.
//! - Read stdin to EOF and discard, same as `fake_agent.rs`.
//! - On EOF, exit 0.
//!
//! Kept as a separate `[[bin]]` rather than an argv flag on the base
//! fake so the happy-path fake's behavior contract stays "exit 0 on
//! EOF, period" -- conflating it with a URL-print flag would mean
//! every existing PTY test implicitly depends on whatever URL got
//! chosen here.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, Write};

fn main() {
    {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        // One line, trailing space so the URL sits cleanly inside its
        // own cell range -- `url_scan::find_urls_in_screen` walks the
        // visible rows and the cell-range encoding is what
        // `compute_hover` uses to hit-test a Ctrl+click. The trailing
        // space ensures the URL terminates on a real boundary, not on
        // a line wrap.
        out.write_all(b"FAKE_AGENT_READY> https://example.com/codemux-test ")
            .unwrap();
        out.flush().unwrap();
    }

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        if line.is_err() {
            break;
        }
    }
}
