//! Nested-PTY rendering via `tui-term` into a ratatui Rect.
//!
//! Consumes bytes from a PTY's output buffer, feeds them to a `vt100::Parser`,
//! renders the cell grid through `tui_term::widget::PseudoTerminal`.

// TODO(P1): feed output bytes of the focused agent's PTY into a vt100::Parser,
// render via tui_term::widget::PseudoTerminal into a ratatui Rect.
