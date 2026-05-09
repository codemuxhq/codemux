//! Capture the OSC 0 / OSC 2 window-title sequence each agent's PTY
//! emits, so the navigator can label tabs with whatever the foreground
//! process — typically Claude Code — declares as the current tab name.
//!
//! AD-1 forbids semantically parsing Claude Code's UI; reading the
//! terminal title is the standard cross-program escape hatch (every
//! terminal emulator does it) and falls cleanly outside that ban: we
//! consume an industry-standard OSC sequence, not Claude's prompt
//! shape, conversation state, or session contents.
//!
//! `vt100` exposes title updates via the [`Callbacks`] trait rather
//! than via a getter on `Screen`, so we plug a small [`TitleCapture`]
//! into `Parser<TitleCapture>` and read `parser.callbacks().title()`
//! each render cycle.
//!
//! Two pieces of state live here. [`title`](TitleCapture::title)
//! returns the sanitized label string (status glyphs and surrounding
//! whitespace stripped) so the navigator stays readable. The separate
//! [`is_working`](TitleCapture::is_working) flag remembers whether
//! the *raw* title started with one of those status glyphs, which the
//! runtime treats as Claude's "I'm in the middle of a turn" signal —
//! used to drive the per-tab spinner animation and the
//! finished-while-unfocused blink. Detecting the glyph here (before
//! sanitize strips it) keeps that signal cheap and avoids the
//! navigator from having to reach back into raw bytes.

use vt100::Callbacks;
use vt100::Screen;

/// A `vt100::Callbacks` implementation that stashes both the most
/// recent window title set by the PTY and a derived "is the
/// foreground process currently busy" hint. Reset on each
/// [`set_window_title`] invocation; we do not buffer history.
#[derive(Debug, Default)]
pub struct TitleCapture {
    title: Option<String>,
    working: bool,
}

impl TitleCapture {
    /// Latest title the foreground process has declared, after
    /// sanitization. `None` means the process has not emitted an OSC
    /// 0 / OSC 2 sequence yet; callers fall back to the static label
    /// the runtime assigned at spawn.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Whether the most recent title carried a leading "in progress"
    /// glyph (any Dingbats asterisk/star Claude rotates through, or a
    /// Braille spinner frame). Cleared when the next title arrives
    /// without one. The runtime uses this to drive the navigator
    /// spinner and to detect working→idle transitions for the
    /// "finished while unfocused" blink.
    pub fn is_working(&self) -> bool {
        self.working
    }
}

impl Callbacks for TitleCapture {
    fn set_window_title(&mut self, _: &mut Screen, title: &[u8]) {
        // Lossy decode keeps us robust against the occasional
        // non-UTF-8 byte some shells emit; a `?` glyph in the label
        // is preferable to dropping the entire title.
        let raw = String::from_utf8_lossy(title);
        // Decide working state from the raw bytes: look at the first
        // non-whitespace, non-control character. If it's a status
        // glyph we treat the agent as busy. Doing this *before*
        // sanitize means we still get the signal even though the
        // user-visible label has the glyph stripped out.
        self.working = raw
            .chars()
            .find(|c| !c.is_control() && !c.is_whitespace())
            .is_some_and(is_working_glyph);
        let cleaned = sanitize(&raw);
        self.title = (!cleaned.is_empty()).then_some(cleaned);
    }
}

/// Strip leading status glyphs and surrounding whitespace from a raw
/// title. Claude Code (and several other agentic CLIs) prefix the
/// title with a star or a Braille spinner frame to indicate liveness
/// — useful in a terminal tab bar, distracting in our navigator,
/// and the spinner mutates several times a second which would
/// otherwise make the label visibly flicker. Trailing whitespace is
/// trimmed too, and embedded control characters are dropped.
fn sanitize(raw: &str) -> String {
    let no_ctl: String = raw.chars().filter(|c| !c.is_control()).collect();
    no_ctl.trim_start_matches(is_decoration).trim().to_string()
}

/// Characters we treat as "decoration" at the start of a title:
/// whitespace, the asterisk/star spinner glyphs Claude Code cycles
/// through, and the Braille spinner glyphs (`U+2800..=U+28FF`) most
/// other TUI spinners draw from. Used only by [`sanitize`] — the
/// working-state detector restricts itself to [`is_working_glyph`] so
/// a leading space alone never flips an agent into "working".
fn is_decoration(c: char) -> bool {
    c.is_whitespace() || is_decoration_glyph(c)
}

/// Glyphs to strip from the start of a title so the user-visible
/// label stays readable. Broader than [`is_working_glyph`] because
/// Claude Code prefixes its idle title with a static star too — we
/// want all of those gone from the label, even though only the
/// rotating subset means "the agent is busy."
// The matched glyphs are the four Dingbats asterisks observed in
// Claude Code 2.x's titles: ✱ (HEAVY ASTERISK),
// ✳ (EIGHT SPOKED ASTERISK), ✶ (SIX POINTED BLACK STAR), and
// ✻ (TEARDROP-SPOKED ASTERISK). Add a glyph here only when a real
// agent emits one — we deliberately avoid pre-matching the rest of
// the Dingbats block so a legitimate title that happens to start
// with an unrelated star (e.g. a project named `✦ infra`) isn't
// silently stripped.
fn is_decoration_glyph(c: char) -> bool {
    matches!(
        c,
        '\u{2731}' | '\u{2733}' | '\u{2736}' | '\u{273B}' | '\u{2800}'..='\u{28FF}'
    )
}

/// Glyphs whose presence at the start of a title means "this process
/// is actively in the middle of a turn." The runtime uses this to
/// drive the per-tab Braille overlay and the finished-while-unfocused
/// blink — false positives leave the spinner stuck on forever.
// Strict subset of [`is_decoration_glyph`]: Claude Code's idle title
// also carries a leading Dingbats star (a static brand prefix, not a
// rotating spinner frame). Treating that idle prefix as "working"
// pinned every tab into a permanent spinner. We match only glyphs
// that have been confirmed to appear ONLY mid-turn — currently
// ✱ (U+2731) and the Braille block. Add more here only after
// observing them rotate, never just because they look spinner-y.
fn is_working_glyph(c: char) -> bool {
    matches!(c, '\u{2731}' | '\u{2800}'..='\u{28FF}')
}

#[cfg(test)]
mod tests {
    use super::*;
    use vt100::Parser;

    fn parse(bytes: &[u8]) -> Parser<TitleCapture> {
        let mut p = Parser::new_with_callbacks(24, 80, 0, TitleCapture::default());
        p.process(bytes);
        p
    }

    #[test]
    fn osc_2_sets_title() {
        let p = parse(b"\x1b]2;hello world\x07");
        assert_eq!(p.callbacks().title(), Some("hello world"));
    }

    #[test]
    fn osc_0_sets_title() {
        // OSC 0 sets both icon name and title; vt100 fires
        // set_window_title for it, which is what we capture.
        let p = parse(b"\x1b]0;both fields\x07");
        assert_eq!(p.callbacks().title(), Some("both fields"));
    }

    #[test]
    fn later_title_overwrites_earlier() {
        let p = parse(b"\x1b]2;first\x07\x1b]2;second\x07");
        assert_eq!(p.callbacks().title(), Some("second"));
    }

    #[test]
    fn leading_star_glyph_is_stripped() {
        let p = parse("\x1b]2;✱ Add tab names\x07".as_bytes());
        assert_eq!(p.callbacks().title(), Some("Add tab names"));
    }

    #[test]
    fn leading_dingbats_star_variants_are_stripped() {
        // Pin every Dingbats star variant the strip set covers beyond
        // the original ✱: if is_decoration_glyph drops one, the tab
        // label keeps the noise.
        for glyph in ['✳', '✶', '✻'] {
            let raw = format!("\x1b]2;{glyph} Thinking\x07");
            let p = parse(raw.as_bytes());
            assert_eq!(
                p.callbacks().title(),
                Some("Thinking"),
                "glyph {glyph:?} should be stripped"
            );
        }
    }

    #[test]
    fn idle_dingbats_star_prefix_does_not_flip_working() {
        // Regression guard. Claude Code prefixes its IDLE title with
        // a static Dingbats star too (not a rotating spinner frame).
        // If the working detector matches that prefix, every tab gets
        // pinned into a permanent spinner. Working detection must stay
        // narrower than decoration stripping — only glyphs Claude
        // actually rotates through count.
        for glyph in ['✳', '✶', '✻'] {
            let raw = format!("\x1b]2;{glyph} ProjectName\x07");
            let p = parse(raw.as_bytes());
            assert!(
                !p.callbacks().is_working(),
                "glyph {glyph:?} is also used as Claude's idle prefix; \
                 must not trigger working state"
            );
        }
    }

    #[test]
    fn leading_braille_spinner_is_stripped() {
        let p = parse("\x1b]2;⠋ working\x07".as_bytes());
        assert_eq!(p.callbacks().title(), Some("working"));
    }

    #[test]
    fn surrounding_whitespace_is_trimmed() {
        let p = parse(b"\x1b]2;   padded   \x07");
        assert_eq!(p.callbacks().title(), Some("padded"));
    }

    #[test]
    fn empty_title_after_sanitize_is_none() {
        // Claude occasionally emits a bare spinner with no body
        // mid-update; we'd rather show the fallback label than a
        // single character.
        let p = parse("\x1b]2;⠋\x07".as_bytes());
        assert_eq!(p.callbacks().title(), None);
    }

    #[test]
    fn no_osc_means_no_title() {
        let p = parse(b"plain text only");
        assert_eq!(p.callbacks().title(), None);
    }

    #[test]
    fn embedded_control_chars_are_stripped() {
        let p = parse(b"\x1b]2;tab\tname\x07");
        assert_eq!(p.callbacks().title(), Some("tabname"));
    }

    // The runtime drives the per-tab spinner and the
    // finished-while-unfocused blink off `is_working()`. These tests
    // pin the contract: leading status glyph (Claude's spinner / star)
    // → working; plain title → idle; transitions update on each title.

    #[test]
    fn star_glyph_marks_working() {
        let p = parse("\x1b]2;✱ Thinking\x07".as_bytes());
        assert!(p.callbacks().is_working());
    }

    #[test]
    fn braille_spinner_marks_working() {
        let p = parse("\x1b]2;⠋ Compiling\x07".as_bytes());
        assert!(p.callbacks().is_working());
    }

    #[test]
    fn plain_title_is_not_working() {
        let p = parse(b"\x1b]2;Done\x07");
        assert!(!p.callbacks().is_working());
    }

    #[test]
    fn working_clears_when_next_title_lacks_glyph() {
        // Working → idle is the transition that triggers the blink.
        // Must update on every title, not just the first.
        let p = parse("\x1b]2;⠋ Working\x07\x1b]2;Done\x07".as_bytes());
        assert!(!p.callbacks().is_working());
        assert_eq!(p.callbacks().title(), Some("Done"));
    }

    #[test]
    fn working_sets_when_next_title_carries_glyph() {
        // Idle → working is also valid; the second title arrives with
        // a spinner so we must flip back on.
        let p = parse("\x1b]2;ready\x07\x1b]2;⠋ thinking\x07".as_bytes());
        assert!(p.callbacks().is_working());
    }

    #[test]
    fn leading_whitespace_alone_is_not_working() {
        // Some shells pad titles. Whitespace is decoration but not a
        // status signal — only the deliberate spinner glyphs are.
        let p = parse(b"\x1b]2;   padded\x07");
        assert!(!p.callbacks().is_working());
    }

    #[test]
    fn no_title_at_all_is_not_working() {
        let p = parse(b"hello");
        assert!(!p.callbacks().is_working());
    }
}
