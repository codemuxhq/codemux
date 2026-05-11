//! AC-030 (invalid config fails loud before raw mode): pin the
//! runtime-level proof that a malformed `config.toml` fails BEFORE
//! `enable_raw_mode` / `EnterAlternateScreen` ever runs. Unit tests in
//! `apps/tui/src/config.rs` already cover that the deserializer rejects
//! the malformed shapes (`invalid_chord_in_config_propagates_as_a_parse_error`,
//! `host_colors_unknown_name_is_an_error`, `spawn_unknown_default_mode_is_an_error`);
//! none of them prove that the failure happens before the terminal
//! switches to raw mode.
//!
//! ## What this pins
//!
//! `main.rs` calls `config::load()?` before `runtime::run`, and
//! `runtime::run` is the only call site of `enable_raw_mode()` /
//! `EnterAlternateScreen` (see `runtime.rs::1252-1253`). A regression
//! that reordered config-load to AFTER raw-mode entry would leave the
//! user's terminal in alt-screen + raw-mode state when the error
//! bailed; this test would flip immediately because `\x1b[?1049h`
//! (the DEC alt-screen-on sequence crossterm emits as part of
//! `EnterAlternateScreen`) would show up in the master byte stream.
//!
//! ## How we assert "before"
//!
//! After waiting for the child to exit non-zero, we drain the master
//! byte stream once and assert that `\x1b[?1049h` is NOT present.
//! `wait_for_exit` drains during its poll loop, so by the time we
//! read the snapshot every byte the child ever emitted is in the log.
//! Asserting on the byte's *absence* post-exit is the cleanest "raw
//! mode never engaged" signal.
//!
//! Gating mirrors the rest of the slow tier: `test-fakes` feature,
//! `#[ignore]`, `#[serial]`.

#![cfg(feature = "test-fakes")]

#[allow(dead_code)]
mod common;

use std::time::Duration;

use serial_test::serial;

use common::{master_bytes_snapshot, spawn_codemux_with_config, wait_for_exit};

#[test]
#[ignore = "slow-tier PTY E2E; runs via `just check-e2e` / `just test-e2e`"]
#[serial]
fn invalid_config_exits_before_raw_mode() {
    // Same invalid shape the config unit test pins: a malformed chord
    // on a binding key. `config::load` deserializes through
    // `toml::from_str`, hits the chord parser, and returns Err
    // wrapped with `parse config at <path>`. The `?` in `main` then
    // propagates the error past `main`'s return boundary -- color_eyre
    // formats it on stderr and the process exits non-zero. Crucially,
    // this happens BEFORE `runtime::run` is called, so
    // `enable_raw_mode` / `EnterAlternateScreen` never run.
    let mut handle = spawn_codemux_with_config("[bindings.on_prefix]\nquit = \"ctrl+nonsense\"\n");

    let status = wait_for_exit(&mut handle, Duration::from_secs(5))
        .expect("codemux did not exit within 5s of boot with an invalid config");
    assert!(
        !status.success(),
        "expected non-zero exit when config is invalid; got {status:?}"
    );

    // The child has exited, so the master byte stream is closed and
    // finite. One drain-and-clone is enough to see every byte it ever
    // emitted; no polling needed.
    let bytes = master_bytes_snapshot(&mut handle);

    assert!(
        byte_index_of(&bytes, b"\x1b[?1049h").is_none(),
        "AC-030: expected NO alt-screen-on escape (`\\x1b[?1049h`) on the master \
         byte stream after an invalid config -- raw mode must never have engaged.\n\
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
