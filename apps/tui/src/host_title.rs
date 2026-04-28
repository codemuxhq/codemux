//! Emit OSC sequences that ask the surrounding terminal emulator to
//! label its window / tab with what codemux is currently showing.
//!
//! This is the inverse of [`crate::pty_title`]: that module captures
//! titles emitted *by* an inner agent PTY; this one writes titles back
//! *out* to the host terminal so Ghostty / iTerm2 / Kitty / `WezTerm` /
//! Alacritty / Terminal.app etc. can show the focused tab's name in
//! their own tab bar without the user needing a separate window
//! manager.
//!
//! AD-1 (no semantic parsing of Claude) still holds: we re-emit the
//! same OSC 0/2 payload we already accepted from the inner process,
//! optionally prefixed with the user-visible host label and repo name
//! the runtime composed itself.
//!
//! The sequences:
//!
//! - `\x1b]0;<title>\x07` — OSC 0 sets both icon name and window
//!   title. Every mainstream emulator treats this as the tab title.
//! - `\x1b[22;0t` — push the current title onto the terminal's
//!   internal stack (xterm `XTWINOPS`).
//! - `\x1b[23;0t` — pop the previously-saved title.
//!
//! Terminals that don't recognize the push/pop sequence ignore it;
//! the worst case is the user's pre-codemux title isn't restored.

use std::io::{self, Write};

/// Hard cap on emitted title length, in `char`s. Most emulators clamp
/// to ~30–80 columns in their UI anyway; capping here keeps a
/// pathological agent title (multi-kB OSC payload) from streaming
/// kilobytes of escapes through stdout on every change.
const MAX_TITLE_CHARS: usize = 200;

/// Drop ASCII control characters so an OSC payload can never be
/// terminated early by an embedded BEL / ESC and so the surrounding
/// terminal sees only printable text. The agent-side title parser in
/// [`crate::pty_title`] already does this, but inputs from `repo` /
/// `host` / fallback labels haven't been through it; one filter at
/// the emit boundary covers all of them.
fn sanitize(title: &str) -> String {
    title.chars().filter(|c| !c.is_control()).collect()
}

/// Truncate to [`MAX_TITLE_CHARS`] code points, replacing the trailing
/// glyph with `…` so the user can tell the title was cut. Operates on
/// `chars()` rather than `len()` so we never split a multibyte code
/// point.
fn truncate(title: &str) -> String {
    if title.chars().count() <= MAX_TITLE_CHARS {
        return title.to_string();
    }
    title
        .chars()
        .take(MAX_TITLE_CHARS.saturating_sub(1))
        .chain(std::iter::once('…'))
        .collect()
}

/// Emit an OSC 0 sequence asking the surrounding terminal to update
/// its window/tab title, flushing so the bytes land before the next
/// ratatui draw cycle. Mirrors `write_clipboard_to` in `runtime.rs`:
/// flush internally so a single `Result` covers both the write and
/// the flush at the call site.
///
/// Lifted to `&mut impl Write` so unit tests can capture the bytes
/// without touching `io::stdout()`.
pub fn write_set_title<W: Write>(out: &mut W, title: &str) -> io::Result<()> {
    let payload = truncate(&sanitize(title));
    write!(out, "\x1b]0;{payload}\x07")?;
    out.flush()
}

/// Push the host terminal's current title onto its internal stack so
/// it can be restored on exit. Best-effort; terminals that don't
/// implement `XTWINOPS 22 ; 0` ignore the sequence.
pub fn push_title<W: Write>(out: &mut W) -> io::Result<()> {
    write!(out, "\x1b[22;0t")?;
    out.flush()
}

/// Pop a previously-pushed title back into place. Best-effort; if the
/// stack is empty (push was ignored), the pop is a no-op.
pub fn pop_title<W: Write>(out: &mut W) -> io::Result<()> {
    write!(out, "\x1b[23;0t")?;
    out.flush()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn osc0_envelope_wraps_payload_in_set_title_sequence() {
        let mut buf = Vec::new();
        write_set_title(&mut buf, "hello world").unwrap();
        assert_eq!(buf, b"\x1b]0;hello world\x07");
    }

    #[test]
    fn embedded_bel_stripped_so_envelope_is_not_truncated() {
        // A naive emit would let the inner BEL terminate the OSC early
        // and ship "tab" instead of the full title; sanitize() guards.
        let mut buf = Vec::new();
        write_set_title(&mut buf, "tab\x07name").unwrap();
        assert_eq!(buf, b"\x1b]0;tabname\x07");
    }

    #[test]
    fn embedded_esc_stripped_so_payload_is_not_reinterpreted() {
        let mut buf = Vec::new();
        write_set_title(&mut buf, "before\x1bafter").unwrap();
        assert_eq!(buf, b"\x1b]0;beforeafter\x07");
    }

    #[test]
    fn multibyte_titles_pass_through_unchanged() {
        let mut buf = Vec::new();
        write_set_title(&mut buf, "olá 🦀 ✱").unwrap();
        let s = std::str::from_utf8(&buf).unwrap();
        assert_eq!(s, "\x1b]0;olá 🦀 ✱\x07");
    }

    #[test]
    fn long_titles_truncated_with_ellipsis_at_max_chars() {
        let mut buf = Vec::new();
        let long = "x".repeat(MAX_TITLE_CHARS + 50);
        write_set_title(&mut buf, &long).unwrap();
        let s = std::str::from_utf8(&buf).unwrap();
        let body = s
            .strip_prefix("\x1b]0;")
            .unwrap()
            .strip_suffix('\x07')
            .unwrap();
        assert_eq!(body.chars().count(), MAX_TITLE_CHARS);
        assert!(body.ends_with('…'));
    }

    #[test]
    fn titles_at_exactly_the_limit_pass_through_unmodified() {
        // Boundary: == MAX is fine, only > MAX truncates. Without this
        // assertion the off-by-one would silently chop the last char of
        // a perfectly-sized title and replace it with the ellipsis.
        let mut buf = Vec::new();
        let exact = "x".repeat(MAX_TITLE_CHARS);
        write_set_title(&mut buf, &exact).unwrap();
        let s = std::str::from_utf8(&buf).unwrap();
        let body = s
            .strip_prefix("\x1b]0;")
            .unwrap()
            .strip_suffix('\x07')
            .unwrap();
        assert_eq!(body, exact);
    }

    #[test]
    fn truncation_respects_char_boundaries_for_multibyte_inputs() {
        // Concrete regression: if truncate() used .len() instead of
        // .chars().count(), this would either over-truncate (because
        // each emoji is 4 bytes) or panic mid-codepoint.
        let mut buf = Vec::new();
        let long: String = "🦀".repeat(MAX_TITLE_CHARS + 10);
        write_set_title(&mut buf, &long).unwrap();
        let s = std::str::from_utf8(&buf).unwrap();
        let body = s
            .strip_prefix("\x1b]0;")
            .unwrap()
            .strip_suffix('\x07')
            .unwrap();
        assert_eq!(body.chars().count(), MAX_TITLE_CHARS);
    }

    #[test]
    fn empty_title_emits_empty_payload_not_no_write() {
        // An explicit empty title is a meaningful "clear" for some
        // terminals; we still want to send the envelope, not skip.
        let mut buf = Vec::new();
        write_set_title(&mut buf, "").unwrap();
        assert_eq!(buf, b"\x1b]0;\x07");
    }

    #[test]
    fn push_and_pop_emit_xtwinops_22_and_23() {
        let mut buf = Vec::new();
        push_title(&mut buf).unwrap();
        pop_title(&mut buf).unwrap();
        assert_eq!(buf, b"\x1b[22;0t\x1b[23;0t");
    }
}
