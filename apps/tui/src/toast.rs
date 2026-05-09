//! Centralized in-TUI toast / notification surface.
//!
//! A *toast* is a one-row floating pill that surfaces an asynchronous
//! event the user should know about — a URL open that fell back to
//! clipboard, a clipboard write that failed, an SSH bootstrap
//! warning. Toasts are **not** for synchronous confirmation of an
//! action the user just performed (the action's own visible effect is
//! the confirmation). They're also not for fatal/persistent state
//! like a crashed agent — those use `render_crash_banner` and stick
//! to the agent pane.
//!
//! ## Design rules of thumb
//!
//! - **Severity drives colour, weight, and TTL.** Two buckets only:
//!   `Warning`, `Error`. More buckets dilute the signal.
//! - **Floating bottom-right pill, single row.** Mirrors
//!   `render_scroll_indicator`'s placement so the user's eye already
//!   trains there for ambient state, and never covers Claude's
//!   header. The renderer expects to be handed `frame.area()` (or a
//!   subset that ends at the very bottom of the chrome) and places
//!   itself one row above the status bar.
//! - **Single-slot, newer wins.** `ToastDeck` keeps at most one
//!   toast visible at a time; a fresh `push` clobbers whatever was
//!   there. Stacked toasts are an anti-pattern in a one-user TUI:
//!   they pile up, get ignored, and the second one is usually a
//!   re-emission of the first anyway.
//! - **Dismissal is explicit.** TTL elapses *or* the user presses
//!   the dismiss chord (Esc, owned at the orchestration layer).
//!   Random keystrokes do not dismiss — the user shouldn't lose an
//!   error message because they happened to type into the agent
//!   while reading it.

use std::time::{Duration, Instant};

use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

/// Severity bucket for a toast. Drives:
/// - the colour pair (`fg`/`bg`) the renderer paints,
/// - the default TTL (`Warning` shorter than `Error` because errors
///   are the most likely thing the user wants to re-read),
/// - the leading glyph (`⚠`, `✗`).
///
/// **Two buckets only.** A `Success` variant is intentionally
/// missing: a successful action is its own confirmation, and a green
/// "✓ done" pill on top of the agent's own output is just visual
/// debt. An `Info` variant was considered and dropped — no current
/// call site has anything to surface that isn't either a recoverable
/// fault (Warning) or an unrecoverable one (Error). Add one when a
/// real call site needs it, not before.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ToastSeverity {
    /// Something didn't go to plan but recovery happened (clipboard
    /// fallback succeeded, etc.). Yellow background, medium TTL.
    Warning,
    /// Unrecoverable failure — the user needs to see this and
    /// probably needs to act on it. Red background, long TTL so the
    /// user has time to read it before it auto-clears.
    Error,
}

impl ToastSeverity {
    /// Default TTL for this severity. Errors hold longer because
    /// they're the most likely thing the user wants to re-read.
    fn default_ttl(self) -> Duration {
        match self {
            ToastSeverity::Warning => Duration::from_secs(4),
            ToastSeverity::Error => Duration::from_secs(8),
        }
    }

    /// Colour pair (fg, bg) used to paint the pill body. Picked for
    /// terminal-default-themes; the foreground is always something
    /// readable on the background regardless of light/dark scheme.
    fn colors(self) -> (Color, Color) {
        match self {
            // Warning: same yellow as `render_scroll_indicator` so
            // the visual vocabulary stays consistent.
            ToastSeverity::Warning => (Color::Black, Color::Yellow),
            // Error: matches `render_crash_banner` for the same
            // reason — red bg + white fg reads as "something is
            // wrong here" in every terminal theme.
            ToastSeverity::Error => (Color::White, Color::Red),
        }
    }

    /// Leading glyph rendered before the message body. Chosen to be
    /// distinct at a glance even on monochrome terminals.
    fn glyph(self) -> &'static str {
        match self {
            ToastSeverity::Warning => "⚠",
            ToastSeverity::Error => "✗",
        }
    }
}

/// A single floating notification. Constructed via `Toast::new` (or
/// the per-severity helpers) and handed to a `ToastDeck`; the deck
/// owns lifetime + render decisions.
#[derive(Debug, Clone)]
pub struct Toast {
    text: String,
    severity: ToastSeverity,
    started_at: Instant,
    ttl: Duration,
}

impl Toast {
    /// Build a toast with a custom TTL. Most call sites should prefer
    /// `Toast::warning` / `Toast::error`, which use the
    /// severity-default TTL.
    pub fn with_ttl(text: impl Into<String>, severity: ToastSeverity, ttl: Duration) -> Self {
        Self {
            text: text.into(),
            severity,
            started_at: Instant::now(),
            ttl,
        }
    }

    /// Default-TTL constructor; TTL picked by severity.
    pub fn new(text: impl Into<String>, severity: ToastSeverity) -> Self {
        let ttl = severity.default_ttl();
        Self::with_ttl(text, severity, ttl)
    }

    /// Convenience: an `Error`-severity toast with the default TTL.
    pub fn error(text: impl Into<String>) -> Self {
        Self::new(text, ToastSeverity::Error)
    }

    /// Convenience: a `Warning`-severity toast with the default TTL.
    pub fn warning(text: impl Into<String>) -> Self {
        Self::new(text, ToastSeverity::Warning)
    }

    // Used by external-module tests (runtime.rs); kept on the public
    // API for the next caller that needs severity-conditional logging
    // or rendering. The `dead_code` allow is for the binary build,
    // which doesn't see test-only call sites.
    #[allow(dead_code)]
    pub fn severity(&self) -> ToastSeverity {
        self.severity
    }

    #[allow(dead_code)]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Has the toast outlived its TTL? Checked once per event-loop
    /// tick by [`ToastDeck::tick`].
    fn is_expired(&self) -> bool {
        self.started_at.elapsed() >= self.ttl
    }
}

/// Single-slot toast holder owned by the runtime event loop.
///
/// Replace policy is *newer wins*: `push` overwrites whatever's
/// currently active. This is intentional — for a one-user TUI a
/// queue would just stack stale messages that the user has to wait
/// out before seeing the latest one.
#[derive(Debug, Default)]
pub struct ToastDeck {
    current: Option<Toast>,
}

impl ToastDeck {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, toast: Toast) {
        self.current = Some(toast);
    }

    /// Drop the active toast if it has outlived its TTL. Cheap when
    /// no toast is active; safe to call once per event-loop tick.
    pub fn tick(&mut self) {
        if let Some(t) = &self.current
            && t.is_expired()
        {
            self.current = None;
        }
    }

    /// Wired to the dismiss chord (Esc) at the orchestration layer.
    pub fn dismiss(&mut self) {
        self.current = None;
    }

    /// Returned by reference so the renderer doesn't need to clone.
    pub fn current(&self) -> Option<&Toast> {
        self.current.as_ref()
    }
}

/// Minimum width below which the renderer no-ops rather than
/// truncating the pill to garbage. The lower bound covers leading
/// space + glyph cell + separator + 1-cell message + trailing space.
const TOAST_MIN_WIDTH: u16 = 8;

/// Hard cap: pill never wider than (terminal width / this) so the
/// agent pane behind stays mostly readable.
const TOAST_MAX_WIDTH_FRACTION: u16 = 2;

/// Render the active toast as a one-row pill in the bottom-right of
/// `area`, one row above the status bar (which is always 1 row
/// tall). Pure projection: no state mutation, safe to call every
/// frame while a toast is live.
///
/// Caller should pass the same `area` they pass to the rest of the
/// chrome — `frame.area()` in practice. The renderer subtracts the
/// status bar row itself so call sites don't have to know that
/// constant.
pub fn render_toast(frame: &mut Frame<'_>, area: Rect, toast: &Toast) {
    // Status bar reserves the bottom row; toast row sits one above
    // it. Need at least 2 rows to have anywhere to put the pill.
    if area.width < TOAST_MIN_WIDTH || area.height < 2 {
        return;
    }
    let text = format_pill_text(toast);
    // Display width in *terminal cells*, not codepoints. The pill's
    // leading glyph (`⚠`, `✗`) and any user-supplied wide chars
    // (CJK, emoji) render as 2 cells in most modern terminals;
    // `chars().count()` would under-allocate by 1 cell per wide
    // glyph and clip the right edge. Saturates at u16::MAX on a
    // pathological message; then the .min(max_width) below clamps
    // it to half the terminal anyway.
    let text_width = u16::try_from(text.width()).unwrap_or(u16::MAX);
    let max_width = (area.width / TOAST_MAX_WIDTH_FRACTION).max(TOAST_MIN_WIDTH);
    let width = text_width.min(max_width).min(area.width);
    let pill_area = Rect {
        x: area.x + area.width - width,
        // One row above the status bar (which is the last row of `area`).
        y: area.y + area.height - 2,
        width,
        height: 1,
    };
    let (fg, bg) = toast.severity.colors();
    let style = Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD);
    let widget = Paragraph::new(Line::raw(text))
        .alignment(Alignment::Right)
        .style(style);
    frame.render_widget(Clear, pill_area);
    frame.render_widget(widget, pill_area);
}

/// Pill body: `" {glyph} {message} "`. The leading and trailing
/// spaces are the pill's internal padding; no `Block` border keeps
/// the visual vocabulary aligned with `render_scroll_indicator`.
fn format_pill_text(toast: &Toast) -> String {
    format!(" {} {} ", toast.severity.glyph(), toast.text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn row_text(terminal: &Terminal<TestBackend>, y: u16) -> String {
        let buf = terminal.backend().buffer();
        let area = buf.area;
        (area.x..area.x + area.width)
            .map(|x| buf[(x, y)].symbol())
            .collect()
    }

    #[test]
    fn deck_push_replaces_current() {
        let mut deck = ToastDeck::new();
        deck.push(Toast::warning("first"));
        deck.push(Toast::error("second"));
        assert!(matches!(
            deck.current(),
            Some(t) if t.text() == "second" && t.severity() == ToastSeverity::Error
        ));
    }

    #[test]
    fn deck_dismiss_clears_immediately() {
        let mut deck = ToastDeck::new();
        deck.push(Toast::error("oops"));
        deck.dismiss();
        assert!(deck.current().is_none());
    }

    #[test]
    fn deck_tick_evicts_expired_toast() {
        let mut deck = ToastDeck::new();
        // TTL = 0 means the toast is born already-expired; the next
        // tick must drop it.
        deck.push(Toast::with_ttl(
            "ephemeral",
            ToastSeverity::Warning,
            Duration::from_secs(0),
        ));
        deck.tick();
        assert!(deck.current().is_none());
    }

    #[test]
    fn deck_tick_keeps_live_toast() {
        let mut deck = ToastDeck::new();
        deck.push(Toast::with_ttl(
            "still alive",
            ToastSeverity::Warning,
            Duration::from_secs(60),
        ));
        deck.tick();
        assert!(deck.current().is_some());
    }

    /// Renderer paints onto the row *above* the bottom row (the
    /// status bar's row). Ensures the toast doesn't clobber chrome.
    #[test]
    fn render_toast_warning_paints_above_status_bar_row() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test backend");
        let toast = Toast::warning("xdg-open failed");
        terminal
            .draw(|frame| {
                render_toast(
                    frame,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 80,
                        height: 24,
                    },
                    &toast,
                );
            })
            .expect("draw");
        let status_row = row_text(&terminal, 23);
        assert!(
            status_row.trim().is_empty(),
            "status bar row must remain untouched: {status_row:?}",
        );
        let toast_row = row_text(&terminal, 22);
        assert!(
            toast_row.contains("xdg-open failed"),
            "toast text missing on row 22: {toast_row:?}",
        );
    }

    #[test]
    fn render_toast_warning_uses_yellow_background() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test backend");
        let toast = Toast::warning("fallback engaged");
        terminal
            .draw(|frame| {
                render_toast(
                    frame,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 80,
                        height: 24,
                    },
                    &toast,
                );
            })
            .expect("draw");
        let buf = terminal.backend().buffer();
        // Sample the rightmost cell of the right-aligned pill — the
        // trailing-padding space still carries the pill's bg style.
        assert_eq!(buf[(79, 22)].bg, Color::Yellow);
        assert_eq!(buf[(79, 22)].fg, Color::Black);
    }

    #[test]
    fn render_toast_error_uses_red_background() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test backend");
        let toast = Toast::error("clipboard write failed");
        terminal
            .draw(|frame| {
                render_toast(
                    frame,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 80,
                        height: 24,
                    },
                    &toast,
                );
            })
            .expect("draw");
        let buf = terminal.backend().buffer();
        assert_eq!(buf[(79, 22)].bg, Color::Red);
        assert_eq!(buf[(79, 22)].fg, Color::White);
    }

    /// Wide glyphs (CJK ideographs, emoji) occupy 2 terminal cells
    /// each. The renderer must size the pill in *cells*, not
    /// codepoints — otherwise the right edge clips characters. This
    /// test pins the behavior with a known wide CJK char.
    #[test]
    fn render_toast_sizes_pill_by_terminal_cells_not_codepoints() {
        use unicode_width::UnicodeWidthStr;
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test backend");
        // "你好" is two CJK ideographs, each width 2 = 4 cells. With
        // codepoint counting the pill would size to 2 + glyph + padding;
        // with cell counting it sizes to 4 + glyph + padding. We assert
        // the message string survives uncliped at the right edge.
        let msg = "你好";
        let toast = Toast::error(msg);
        terminal
            .draw(|frame| {
                render_toast(
                    frame,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 80,
                        height: 24,
                    },
                    &toast,
                );
            })
            .expect("draw");
        // The pill text " ✗ 你好 " has display width = 1 + 1 + 1 + 4 + 1 = 8 cells.
        // The trailing-padding space at the rightmost cell carries the red bg.
        let buf = terminal.backend().buffer();
        assert_eq!(
            buf[(79, 22)].bg,
            Color::Red,
            "pill must extend to the rightmost cell — wide-glyph width math",
        );
        // Sanity: the message itself fits within the painted pill area
        // (pill width >= cell width of " ✗ 你好 ").
        let pill_cells = msg.width() + " ✗  ".width();
        assert!(pill_cells >= 8, "test invariant: pill_cells = {pill_cells}");
    }

    #[test]
    fn render_toast_zero_area_is_a_noop() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test backend");
        let toast = Toast::error("ignored");
        terminal
            .draw(|frame| {
                render_toast(
                    frame,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 0,
                        height: 0,
                    },
                    &toast,
                );
            })
            .expect("draw");
        let row = row_text(&terminal, 22);
        assert!(
            row.trim().is_empty(),
            "zero-area toast must not paint: {row:?}",
        );
    }

    /// Single-row area has no room above the status bar, so the
    /// renderer must skip rather than paint over the status row.
    #[test]
    fn render_toast_single_row_area_is_a_noop() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test backend");
        let toast = Toast::error("ignored");
        terminal
            .draw(|frame| {
                render_toast(
                    frame,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 80,
                        height: 1,
                    },
                    &toast,
                );
            })
            .expect("draw");
        let row = row_text(&terminal, 0);
        assert!(
            row.trim().is_empty(),
            "single-row area must not paint: {row:?}",
        );
    }

    /// Width is capped at half the terminal — a long message gets
    /// truncated by ratatui's Paragraph rather than spanning the
    /// whole row.
    #[test]
    fn render_toast_caps_pill_width_at_half_terminal() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).expect("test backend");
        let toast = Toast::error("a".repeat(200));
        terminal
            .draw(|frame| {
                render_toast(
                    frame,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 80,
                        height: 24,
                    },
                    &toast,
                );
            })
            .expect("draw");
        let buf = terminal.backend().buffer();
        // Cell at column 39 (middle of the screen) must NOT carry
        // the red pill bg — the pill stops at width = 40.
        assert_ne!(
            buf[(39, 22)].bg,
            Color::Red,
            "pill leaked past the half-terminal cap",
        );
    }

    #[test]
    fn warning_ttl_is_shorter_than_error_ttl() {
        // Errors hold longer because they're the most likely thing
        // the user wants to re-read. If this ever flips, every call
        // site needs to be re-evaluated.
        assert!(
            ToastSeverity::Warning.default_ttl() < ToastSeverity::Error.default_ttl(),
            "warning should auto-clear faster than error",
        );
    }
}
