//! Crashing-variant stub agent used by the slow-tier PTY harness.
//!
//! Drives AC-037 (Ready -> Crashed on non-zero PTY exit) end-to-end:
//! the runtime's `reap_dead_transports` only routes through
//! `mark_crashed` when the dying child returns a non-zero status, so
//! the happy-path `fake_agent` (which exits 0 on EOF) can't reach that
//! branch. This binary is the smallest possible delta: same boot
//! prompt, same stdin-read loop, but exits 42 when the test sends the
//! literal `QUIT` line OR when stdin EOFs.
//!
//! ## Why a separate binary instead of an argv flag on `fake_agent`
//!
//! The happy-path fake's behavioral contract -- "exit 0 on EOF" -- is
//! depended on by every existing slow-tier PTY test (lifecycle, reap,
//! nav, etc.) and the unit-test cousins documented in
//! `docs/003--acceptance-criteria.md` AC-036. Adding an `--exit-code`
//! flag would make every one of those tests implicitly depend on the
//! default flag value, and a future refactor that changed the default
//! would silently flip the contract for the entire suite. Keeping the
//! crashing behavior in its own binary preserves single-purpose
//! semantics for both fakes.
//!
//! ## Why exit code 42
//!
//! Deliberately distinctive. Real shells exit 0 on success, 1 on
//! catch-all errors, 2 on POSIX misuse, 126/127/128+N on
//! signal-and-exec failure -- using one of those would risk a false
//! match against an accidental real failure in the PTY pipeline. 42
//! sits outside that band and shows up unambiguously in the rendered
//! red banner ("exit 42") so the test can grep on the literal `42`
//! without ambiguity.
//!
//! ## Why we also exit 42 on EOF
//!
//! The QUIT-line path is the test's normal trigger (sent through
//! codemux's keymap-forward path, see `pty_crash.rs`). But the
//! harness's `Drop` closes the PTY master at teardown, which closes
//! the slave's stdin from below; if the test ever fails mid-run, the
//! `Drop` path needs to see the fake exit promptly so the codemux
//! parent reaps and the test runner doesn't hang. Returning 42 in
//! both cases keeps the binary's exit contract uniform.
//!
//! Ignoring `unwrap_used` / `expect_used` at file scope mirrors
//! `fake_agent.rs` -- failing loudly with a panic is the most useful
//! signal a test fixture can give.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, Write};

fn main() {
    {
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        out.write_all(b"FAKE_AGENT_READY> ").unwrap();
        out.flush().unwrap();
    }

    // Lock stdin once outside the loop (re-locking the global mutex on
    // every iteration is a hot-path mistake even for a fixture).
    // `BufRead::lines` strips the trailing `\n`; the extra `trim()`
    // also catches `\r` from PTY canonical-mode line-discipline
    // translation and any stray whitespace the runtime might forward
    // during the prefix-arming dance. The exact match on `QUIT` keeps
    // this from triggering on partial matches.
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim() == "QUIT" {
            std::process::exit(42);
        }
    }
    std::process::exit(42);
}
