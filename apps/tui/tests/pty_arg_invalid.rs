//! AC-031 (invalid `[PATH]` arg / `--nav <invalid>` fails loud before
//! raw mode): pin two runtime-level claims that unit tests can't
//! reach.
//!
//! 1. `apps/tui/src/main.rs::resolve_cwd_errors_when_path_is_missing` and
//!    `resolve_cwd_errors_when_path_is_a_file` cover that
//!    `resolve_cwd` rejects bad paths -- but they do not prove the
//!    rejection happens before `enable_raw_mode`.
//! 2. The `--nav <invalid>` failure is a clap-level parse error
//!    (clap derives `NavStyle: ValueEnum`); no Rust unit test
//!    exercises clap's own error path against the assembled `Cli`,
//!    and clap parsing also happens before `runtime::run`.
//!
//! ## What this pins
//!
//! Both paths bail out of `main` BEFORE `runtime::run`, so
//! `enable_raw_mode()` / `EnterAlternateScreen` -- the only emitters
//! of `\x1b[?1049h` in the codebase -- never run. A regression that
//! moved arg-validation after raw-mode entry would surface as a
//! `\x1b[?1049h` byte in the master stream; both tests assert on its
//! *absence* after the child has exited.
//!
//! ## How we trigger each case
//!
//! - **Invalid path**: pass `/nonexistent/...` as the positional
//!   `[PATH]`. `Cli::parse()` accepts it (clap only checks the type is
//!   `PathBuf`, no on-disk validation), then `resolve_cwd` in `main`
//!   calls `fs::canonicalize` -- which fails with `ENOENT` -- and the
//!   `?` propagates a `color_eyre` error. Exit code is 1 (the runtime
//!   error path).
//! - **Invalid `--nav`**: pass `--nav=wat`. Clap's `ValueEnum` machinery
//!   rejects the value at `Cli::parse()` time; clap prints
//!   `error: invalid value '<x>' for '--nav <NAV>'` on stderr and
//!   exits 2 (clap's standard usage-error code). `main` never runs
//!   past `Cli::parse()`.
//!
//! Gating mirrors the rest of the slow tier: `test-fakes` feature,
//! `#[ignore]`, `#[serial]`.

#![cfg(feature = "test-fakes")]

#[allow(dead_code)]
mod common;

use std::time::Duration;

use serial_test::serial;

use common::{master_bytes_snapshot, spawn_codemux_with_args, wait_for_exit};

#[test]
#[ignore = "slow-tier PTY E2E; runs via `just check-e2e` / `just test-e2e`"]
#[serial]
fn invalid_path_arg_exits_before_raw_mode() {
    let agent_bin = env!("CARGO_BIN_EXE_fake_agent");
    // Absolute path that cannot exist on a test host. `--` separates
    // any future clap flags from the positional argument so the
    // path is unambiguously the `[PATH]` arg.
    let mut handle = spawn_codemux_with_args(
        agent_bin,
        "",
        &["--", "/nonexistent/codemux-invalid-path-arg-test"],
    );

    let status = wait_for_exit(&mut handle, Duration::from_secs(5))
        .expect("codemux did not exit within 5s of boot with an invalid path arg");
    assert!(
        !status.success(),
        "expected non-zero exit when [PATH] is invalid; got {status:?}"
    );

    let bytes = master_bytes_snapshot(&mut handle);
    assert!(
        byte_index_of(&bytes, b"\x1b[?1049h").is_none(),
        "AC-031: expected NO alt-screen-on escape (`\\x1b[?1049h`) on the master \
         byte stream after an invalid [PATH] arg -- raw mode must never have engaged.\n\
         Raw bytes (lossy utf-8): {}",
        String::from_utf8_lossy(&bytes),
    );
}

#[test]
#[ignore = "slow-tier PTY E2E; runs via `just check-e2e` / `just test-e2e`"]
#[serial]
fn invalid_nav_arg_exits_at_clap_parse_time() {
    let agent_bin = env!("CARGO_BIN_EXE_fake_agent");
    // `wat` is not in the `NavStyle` enum's `[left-pane, popup]`
    // possible values. Clap rejects it during `Cli::parse()` and
    // exits 2 before `main`'s body runs.
    let mut handle = spawn_codemux_with_args(agent_bin, "", &["--nav=wat"]);

    let status = wait_for_exit(&mut handle, Duration::from_secs(5))
        .expect("codemux did not exit within 5s of boot with an invalid --nav arg");
    assert!(
        !status.success(),
        "expected non-zero exit when --nav is invalid; got {status:?}"
    );

    let bytes = master_bytes_snapshot(&mut handle);
    assert!(
        byte_index_of(&bytes, b"\x1b[?1049h").is_none(),
        "AC-031: expected NO alt-screen-on escape (`\\x1b[?1049h`) on the master \
         byte stream after a clap-rejected --nav -- raw mode must never have engaged.\n\
         Raw bytes (lossy utf-8): {}",
        String::from_utf8_lossy(&bytes),
    );
    // Belt-and-suspenders: clap's error rendering includes the literal
    // `invalid value` phrase. portable-pty's `spawn_command` routes
    // both stdout and stderr through the slave, so clap's stderr-bound
    // usage line lands on the master byte stream the harness reads.
    // If a future regression silenced the clap error (e.g. by wrapping
    // `Cli::parse()` in a custom catcher) the exit-code assert above
    // would still hold, but the user would lose the diagnostic; this
    // check is the canary.
    assert!(
        byte_index_of(&bytes, b"invalid value").is_some(),
        "AC-031: expected clap's `invalid value` message on the master byte stream.\n\
         Raw bytes (lossy utf-8): {}",
        String::from_utf8_lossy(&bytes),
    );
}

/// Find the byte offset of the first occurrence of `needle` in
/// `haystack`. Same shape as the helper in `pty_panic.rs`; duplicated
/// here so each PTY-test file stays self-contained.
fn byte_index_of(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}
