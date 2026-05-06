//! P0 + P1.1 + P1.2 + P1.3: multi-agent runtime with two togglable navigator
//! styles and a config-driven keymap.
//!
//! The keymap (`crate::keymap`) is the single source of truth for which key
//! triggers which action. The runtime consults the appropriate `Bindings`
//! struct per scope and translates the matched action into a state mutation.
//! `Ctrl-B ?` (default) opens a help popup that lists every binding for every
//! scope, generated from the same Bindings POD.
//!
//! Per-agent PTY ownership lives behind [`AgentTransport`] in the
//! `codemux-session` crate; the runtime holds only the renderable
//! [`Parser`] alongside it. Stage 3 of the codemuxd build-out introduced
//! that seam — see `docs/codemuxd-stages.md`.
//!
//! The SSH spawn flow runs *inside* the spawn modal: the path zone
//! locks with a per-stage spinner while [`crate::bootstrap_worker`]
//! drives prepare and attach off-thread (see Stage 6 of
//! `docs/codemuxd-stages.md`). The runtime tracks the in-flight
//! prepare phase as a single [`PendingPrepare`] (the modal can only
//! prepare one host at a time) and tracks each in-flight attach as a
//! [`PendingAttach`] entry so the user can fire-and-forget multiple
//! spawns in quick succession. When an attach completes the runtime
//! pushes a `Ready` agent into the navigator; on failure it pushes a
//! `Failed` agent so the bootstrap error has a render surface even
//! after the modal closes.

use std::collections::HashMap;
use std::io;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::ValueEnum;
use codemux_session::AgentTransport;
use codemux_shared_kernel::AgentId;
use codemuxd_bootstrap::{PreparedHost, RealRunner, RemoteFs};
use color_eyre::Result;
use color_eyre::eyre::WrapErr;
use crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, DisableMouseCapture, EnableBracketedPaste,
    EnableFocusChange, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    KeyboardEnhancementFlags, ModifierKeyCode, MouseButton, MouseEvent, MouseEventKind,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use tui_term::widget::PseudoTerminal;
use unicode_width::UnicodeWidthStr;
use vt100::Parser;

use crate::agent_meta_worker::{AgentMetaWorker, MetaEvent};
use crate::bootstrap_worker::{
    AttachEvent, AttachHandle, PrepareEvent, PrepareHandle, PrepareSuccess, start_attach,
    start_prepare,
};
use crate::config::{Config, MouseUrlModifier, SpawnConfig};
use crate::fuzzy_worker::FuzzyWorker;
use crate::host_title;
use crate::index_manager::IndexManager;
use crate::index_worker::IndexState;
use crate::keymap::{Bindings, DirectAction, ModalAction, PopupAction, PrefixAction, ScrollAction};
use crate::log_tail::LogTail;
use crate::pty_title::TitleCapture;
use crate::repo_name;
use crate::spawn::{DirLister, HOST_PLACEHOLDER, ModalOutcome, SpawnMinibuffer};
use crate::status_bar::{self, SegmentCtx, StatusSegment, render_segments};

const FRAME_POLL: Duration = Duration::from_millis(50);
const NAV_PANE_WIDTH: u16 = 25;
const STATUS_BAR_HEIGHT: u16 = 1;
/// Height of the bottom log strip rendered when `--log` is passed.
/// Currently 1 row (the user's chosen UX is "show only the latest
/// line"); a future scrollable overlay could be N rows behind a
/// keybinding without changing this constant.
const LOG_STRIP_HEIGHT: u16 = 1;
/// Lines moved per wheel-tick. Three matches "feels right" in tmux /
/// most terminal scrollers; one is too granular, five overshoots when
/// chasing a specific line. Page-mode scrolling uses the focused
/// agent's row count instead.
const WHEEL_STEP: i32 = 3;
/// Maximum width of the floating scroll indicator badge, in cells.
/// 24 fits ` ↑ scroll 999999 · esc ` with room to grow; clamps to the
/// actual pane width when the user runs in a very narrow terminal.
const SCROLL_INDICATOR_WIDTH: u16 = 24;

// Each bool tracks an independent terminal capability we may have
// failed to enable (so we can skip the matching disable on drop).
// Modeling them as a state machine or pair of two-variant enums
// would just be ceremony — they're orthogonal flags.
#[allow(clippy::struct_excessive_bools)]
struct TerminalGuard {
    enhanced_keyboard: bool,
    mouse_captured: bool,
    bracketed_paste: bool,
    /// Whether we attempted to push the host terminal's title via
    /// `XTWINOPS 22 ; 0`. We always issue the matching pop on drop —
    /// terminals that ignored the push also ignore the pop, so the
    /// flag only exists so a future "skip on bad terminal" branch
    /// has somewhere to live.
    host_title_pushed: bool,
    /// Whether we asked the terminal to deliver focus-change events.
    /// Used so the URL-modifier yield logic can reclaim mouse capture
    /// if the user alt-tabs away while still holding the modifier.
    focus_changes: bool,
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Best-effort cleanup. Failures here are unrecoverable (we are mid-drop
        // and may be on a panic path); the user's terminal may already be in
        // a degraded state, and surfacing an error would clobber whatever the
        // panic backtrace was about to say.
        //
        // Drop order is the reverse of acquisition: focus changes, bracketed
        // paste, mouse capture, then keyboard enhancement, then leave-alt-
        // screen + raw-mode. Each is an independent escape sequence; mirroring
        // acquisition order is the safe discipline — and skipping the matching
        // disable when the matching enable failed avoids generating spurious
        // sequences the terminal never opted into.
        if self.focus_changes {
            let _ = execute!(io::stdout(), DisableFocusChange);
        }
        if self.bracketed_paste {
            let _ = execute!(io::stdout(), DisableBracketedPaste);
        }
        if self.mouse_captured {
            let _ = execute!(io::stdout(), DisableMouseCapture);
        }
        // Reset the host mouse pointer in case we left it as a hand
        // (in-app Ctrl+hover sets OSC 22 `pointer`; if the user kills
        // codemux mid-hover the host inherits whatever shape we last
        // sent). OSC 22 `default` is a no-op on terminals that don't
        // implement the sequence.
        let _ = io::stdout().write_all(b"\x1b]22;default\x1b\\");
        if self.enhanced_keyboard {
            let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
        }
        // Pop the host terminal title before leaving the alt-screen so
        // the user sees their original title back on the primary
        // screen as soon as we exit. Done before raw-mode disable so
        // the bytes flush through the same stdout the rest of the
        // teardown uses.
        if self.host_title_pushed {
            let _ = host_title::pop_title(&mut io::stdout());
        }
        // Drain any input bytes still buffered in stdin before raw-mode
        // is disabled. Under KKP `REPORT_EVENT_TYPES` (enabled when the
        // URL-modifier feature is active) the terminal emits a
        // key-release event for the quit chord — typically `q` after
        // `<prefix> q` — as a kitty escape sequence such as
        // `\x1b[113;1:3u`. Codemux exits on the *press*, so the release
        // bytes are still sitting in stdin when raw mode is turned
        // off. The shell that launched codemux then reads them as raw
        // input: zsh's emacs bindings see the leading `\x1b` as Meta
        // and the user lands at an Alt-X "execute: " prompt with no
        // way out short of `Ctrl-G`. Reading and discarding all
        // pending events here closes the gap. Done after `Pop`-ing
        // KKP so the terminal has stopped generating new release
        // events by the time we drain.
        while event::poll(Duration::ZERO).unwrap_or(false) {
            if event::read().is_err() {
                break;
            }
        }
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum PrefixState {
    #[default]
    Idle,
    AwaitingCommand,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum NavStyle {
    LeftPane,
    Popup,
}

impl NavStyle {
    fn toggle(self) -> Self {
        match self {
            Self::LeftPane => Self::Popup,
            Self::Popup => Self::LeftPane,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum PopupState {
    #[default]
    Closed,
    Open {
        selection: usize,
    },
}

/// Bundle of runtime navigation state that always travels together:
/// the agent vector, the focused index into it, the bounce slot for
/// `<prefix> Tab`, and the agent-switcher popup state.
///
/// Pre-`NavState` these were four separate `let mut` locals in
/// [`event_loop`]. Operations like `dismiss_focused` had to take
/// four parallel `&mut` references — flagged in architecture review
/// as a data clump / anaemic-domain-model smell. Bundling them into
/// a single struct lets the mutation invariants (clamp `focused`
/// after a remove, clear stale `previous_focused`, clamp the popup
/// selection) live as `&mut self` methods on one boundary.
///
/// Field-public is intentional: the central event loop reads each
/// field directly in dozens of places and a getter wall would only
/// add noise. The methods below own the *non-trivial* invariants
/// that affect more than one field at once.
struct NavState {
    agents: Vec<RuntimeAgent>,
    /// Index into `agents` for the focused tab. Always valid as long
    /// as `agents` is non-empty; methods that shrink the Vec are
    /// responsible for clamping.
    focused: usize,
    /// Last agent the user was focused on, before the most recent
    /// switch. Lets `<prefix> Tab` (`FocusLast`) bounce between two
    /// agents — the canonical alt-tab move when juggling a couple of
    /// workspaces. `None` until the first switch happens; cleared if
    /// the agent it points to is reaped (so the bounce never lands
    /// on a stale slot).
    previous_focused: Option<usize>,
    /// Switcher overlay state.
    popup_state: PopupState,
}

/// Snapshot captured by [`NavState::peek_finish_transitions`] and
/// consumed by [`NavState::apply_finish_transitions`]. Carries the
/// indices that just transitioned working → idle plus the current
/// `is_working()` poll for every agent (so `apply` doesn't re-poll
/// and risk seeing a different value).
///
/// Splitting peek + apply this way honors Command-Query Separation:
/// the query (does anyone need attention?) is answered without
/// implicitly committing the side effects (`needs_attention`,
/// `last_working` rotation).
struct FinishTransitions {
    /// Indices of agents that transitioned working → idle this tick.
    /// Empty when nothing changed.
    finished: Vec<usize>,
    /// Current `is_working()` for every agent, indexed parallel to
    /// `NavState.agents`. Apply writes this back to `last_working`.
    new_working: Vec<bool>,
}

impl FinishTransitions {
    /// True iff at least one agent (focused or not) finished a turn
    /// this tick. Drives the host-terminal BEL — the focused-finish
    /// case counts because the host signal matters whenever the user
    /// is in another window, regardless of which internal tab finished.
    fn any(&self) -> bool {
        !self.finished.is_empty()
    }
}

impl NavState {
    /// Build a fresh state from an initial agent vector. The first
    /// agent (index 0) is focused.
    #[must_use]
    fn new(agents: Vec<RuntimeAgent>) -> Self {
        Self {
            agents,
            focused: 0,
            previous_focused: None,
            popup_state: PopupState::default(),
        }
    }

    /// Walk the agent list and react to any `Ready` agent whose
    /// transport has died. Two outcomes:
    ///
    /// - **Exit code `0`** — clean exit (user typed `/quit` in claude,
    ///   `Ctrl-D`, etc.). Remove the slot silently. The user
    ///   initiated this; surfacing a dismiss-this-banner step would
    ///   be friction the prior banner-keeps-the-slot policy never
    ///   intended to add for clean exits — that policy was meant for
    ///   *crashes* that the user might otherwise miss.
    /// - **Anything else** — non-zero process exit, or the SSH
    ///   `-1` sentinel for a dropped tunnel / dead daemon. Transition
    ///   to [`AgentState::Crashed`] in place. The slot stays on
    ///   screen with the last frame plus a red banner; the user
    ///   dismisses it explicitly with `<prefix> d`.
    ///
    /// Called once per tick after the read loop so the parser has
    /// already absorbed any final bytes the dying child wrote
    /// before EOF. Indices into `clean_exits` are processed
    /// highest-first so earlier removals don't shift later ones,
    /// and `remove_at` handles all surrounding focus / popup
    /// clamping.
    fn reap_dead_transports(&mut self) {
        let mut clean_exits: Vec<usize> = Vec::new();
        for (i, agent) in self.agents.iter_mut().enumerate() {
            if let AgentState::Ready { transport, .. } = &mut agent.state
                && let Some(code) = transport.try_wait()
            {
                if code == 0 {
                    clean_exits.push(i);
                } else {
                    agent.mark_crashed(code);
                }
            }
        }
        for i in clean_exits.into_iter().rev() {
            self.remove_at(i);
        }
    }

    /// Pure query. Captures every agent's current `is_working()` plus
    /// the indices that just transitioned working → idle since the
    /// previous tick. Does **not** mutate any agent — the caller
    /// commits the side effects (and the `last_working` rotation)
    /// with a paired [`Self::apply_finish_transitions`] call.
    ///
    /// Splitting the query (this) from the command (`apply`) honors
    /// Command-Query Separation: the call site can read whether
    /// anything finished (via [`FinishTransitions::any`]) without
    /// implicitly committing the consequences. Both calls are
    /// expected to happen back-to-back on the same `NavState`; the
    /// `is_working()` snapshot captured here informs both the
    /// transition set and the next-tick baseline (no double poll).
    fn peek_finish_transitions(&self) -> FinishTransitions {
        let mut finished = Vec::new();
        let mut new_working = Vec::with_capacity(self.agents.len());
        for (i, agent) in self.agents.iter().enumerate() {
            let cur = agent.is_working();
            if agent.last_working && !cur {
                finished.push(i);
            }
            new_working.push(cur);
        }
        FinishTransitions {
            finished,
            new_working,
        }
    }

    /// Pure command. Applies a previously-peeked snapshot:
    ///
    /// - **Unfocused agents** that finished get `needs_attention =
    ///   true` so the navigator renders the slow-blink attention cue.
    ///   The currently-focused agent is skipped — the user is already
    ///   looking; nothing to alert about inside codemux — and any
    ///   agent caught in this window has its blink cleared on the next
    ///   focus change via [`Self::change_focus`].
    /// - **`last_working` is rotated** to the captured snapshot for
    ///   every agent so the next tick has a fresh baseline.
    ///
    /// Pairs with [`Self::peek_finish_transitions`] called immediately
    /// before. Indices outside the current agent vector are silently
    /// skipped — defensive against any future call site that mutates
    /// `agents` between peek and apply.
    fn apply_finish_transitions(&mut self, transitions: &FinishTransitions) {
        for &i in &transitions.finished {
            if i != self.focused
                && let Some(agent) = self.agents.get_mut(i)
            {
                agent.needs_attention = true;
            }
        }
        for (agent, &w) in self.agents.iter_mut().zip(transitions.new_working.iter()) {
            agent.last_working = w;
        }
    }

    /// Move focus to `new`, recording the prior focus index for
    /// `<prefix> Tab` (alt-tab) bouncing. No-op if focus is already
    /// on `new` — that keeps a double-tap of the same direct-bind
    /// from clobbering the bounce slot. Centralized here so the six
    /// focus-mutation sites in the event loop don't each open-code
    /// the bounce-slot bookkeeping.
    ///
    /// Also clears [`RuntimeAgent::needs_attention`] on the newly-
    /// focused agent so the slow-blink dismisses the moment the user
    /// actually looks at it.
    ///
    /// Bounds are the caller's responsibility — every event-loop site
    /// already checks `new < agents.len()` (`FocusNext`/`FocusPrev` wrap
    /// via modulo, `FocusAt` and the spawn handler check explicitly,
    /// the popup confirm clamps via `selection.min(agents.len()-1)`).
    /// Adding a defensive bounds check here would also reject the
    /// semantic test cases that exercise the bookkeeping with an
    /// empty agent vec.
    fn change_focus(&mut self, new: usize) {
        if new == self.focused {
            return;
        }
        self.previous_focused = Some(self.focused);
        self.focused = new;
        if let Some(a) = self.agents.get_mut(new) {
            a.needs_attention = false;
        }
    }

    /// Remove the focused agent if it's in a terminal state
    /// (`Failed` or `Crashed`) and clamp the surrounding navigation
    /// state. No-op on a `Ready` agent so a fat-finger of
    /// `<prefix> d` can't close a live session — the more aggressive
    /// [`Self::kill_focused`] is the chord that punches through.
    ///
    /// Returns `true` when an agent was actually removed, so the
    /// caller can react if needed.
    fn dismiss_focused(&mut self) -> bool {
        let is_dismissable = self.agents.get(self.focused).is_some_and(|a| {
            matches!(
                a.state,
                AgentState::Failed { .. } | AgentState::Crashed { .. }
            )
        });
        if !is_dismissable {
            return false;
        }
        self.remove_at(self.focused);
        true
    }

    /// Force-close the focused agent regardless of state. The
    /// `<prefix> x` chord. Drop semantics on the underlying
    /// `AgentTransport` (`LocalPty::drop` calls `child.kill`;
    /// `SshDaemonPty::drop` kills the tunnel) take care of reaping
    /// the child / tunnel — no explicit `kill()` call needed here.
    ///
    /// Returns `true` when an agent was actually removed, `false`
    /// when called against an empty Vec.
    fn kill_focused(&mut self) -> bool {
        if self.focused >= self.agents.len() {
            return false;
        }
        self.remove_at(self.focused);
        true
    }

    /// Remove the agent at `idx` and clamp every surrounding
    /// navigation index that the removal could invalidate. Called by
    /// every site that shrinks the agent Vec — `dismiss_focused`,
    /// `kill_focused`, and `reap_dead_transports`'s clean-exit path —
    /// so the four-fold index bookkeeping has a single home.
    ///
    /// Mutates four pieces of state in concert:
    /// - `agents` — entry removed via `Vec::remove` to preserve tab
    ///   order. `swap_remove` would be O(1) but would silently
    ///   reshuffle tabs, which reads as a bug.
    /// - `focused` — decremented when `idx < focused` so the same
    ///   tab keeps focus across an upstream removal; clamped to the
    ///   new last index when the removed slot *was* focused.
    /// - `previous_focused` — decremented when `> idx`, cleared when
    ///   `== idx` (stale pointer) or when it now collides with
    ///   `focused` (bouncing onto self is a no-op).
    /// - `popup_state` — selection decremented when `idx < selection`
    ///   so the popup keeps highlighting the same agent; clamped to
    ///   the new last index when the removed slot was the selection.
    ///
    /// No-op when `idx` is out of bounds.
    fn remove_at(&mut self, idx: usize) {
        if idx >= self.agents.len() {
            return;
        }
        self.agents.remove(idx);
        if !self.agents.is_empty() {
            if idx < self.focused {
                self.focused -= 1;
            } else if idx == self.focused {
                self.focused = self.focused.min(self.agents.len() - 1);
            }
        }
        if let Some(prev) = self.previous_focused {
            if prev == idx {
                self.previous_focused = None;
            } else if prev > idx {
                self.previous_focused = Some(prev - 1);
            }
        }
        if self.previous_focused == Some(self.focused) {
            self.previous_focused = None;
        }
        if let PopupState::Open { selection } = self.popup_state {
            if self.agents.is_empty() {
                self.popup_state = PopupState::Closed;
            } else {
                let new_selection = if idx < selection {
                    selection - 1
                } else {
                    selection
                };
                self.popup_state = PopupState::Open {
                    selection: new_selection.min(self.agents.len() - 1),
                };
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum HelpState {
    #[default]
    Closed,
    Open,
}

/// One clickable region recorded by the tab-strip / nav renderer. The
/// `agent_id` is the stable identity (not a `Vec` index) so the
/// gesture survives reorders, reaps, and resizes between Down and Up:
/// the event loop resolves `agent_id → index` at the moment of the
/// state mutation, returning gracefully if the agent is gone.
#[derive(Clone, Debug)]
struct Hitbox {
    rect: Rect,
    agent_id: AgentId,
}

impl Hitbox {
    fn contains(&self, col: u16, row: u16) -> bool {
        col >= self.rect.x
            && col < self.rect.x.saturating_add(self.rect.width)
            && row >= self.rect.y
            && row < self.rect.y.saturating_add(self.rect.height)
    }
}

/// The single agent pane visible this frame. Recorded by
/// [`render_agent_pane`] (via [`render_left_pane`] / [`render_popup_style`])
/// and consumed by the mouse handler to decide whether a `Down(Left)`
/// arms a selection. Lives in `event_loop`'s state alongside
/// [`TabHitboxes`] and is cleared at the top of every [`render_frame`]
/// for the same stale-rect reason.
///
/// `Option` because there's no pane to record before the first frame
/// renders, and because a Failed-only navigator (every agent is in the
/// `Failed` state) deliberately skips the agent-pane render path.
#[derive(Default)]
struct PaneHitbox {
    rect: Option<Rect>,
    agent_id: Option<AgentId>,
}

impl PaneHitbox {
    fn clear(&mut self) {
        self.rect = None;
        self.agent_id = None;
    }

    fn record(&mut self, rect: Rect, agent_id: AgentId) {
        if rect.width == 0 || rect.height == 0 {
            return;
        }
        self.rect = Some(rect);
        self.agent_id = Some(agent_id);
    }

    /// Read-only accessor for the post-draw OSC 8 hyperlink painter.
    /// Returns `None` when the focused agent's pane wasn't recorded
    /// this frame (e.g. all agents are in `Failed` state, or layout
    /// has zero area for the pane).
    fn rect(&self) -> Option<Rect> {
        self.rect
    }

    /// Translate a screen-cell click into a pane-relative cell, or
    /// return `None` if the click landed outside the recorded rect.
    /// Pane-relative means `(0, 0)` is the top-left of the agent's
    /// PTY area, which is what `vt100::Screen::contents_between`
    /// expects (it walks `visible_rows()` from row 0).
    fn cell_at(&self, col: u16, row: u16) -> Option<(AgentId, CellPos)> {
        let rect = self.rect?;
        let id = self.agent_id.clone()?;
        if col < rect.x
            || col >= rect.x.saturating_add(rect.width)
            || row < rect.y
            || row >= rect.y.saturating_add(rect.height)
        {
            return None;
        }
        Some((
            id,
            CellPos {
                col: col - rect.x,
                row: row - rect.y,
            },
        ))
    }

    /// Clamp a screen-cell coordinate (e.g. drag continuing past the
    /// pane edge) into a pane-relative cell on the nearest edge.
    /// Returns `None` only when the pane wasn't recorded this frame.
    fn clamped_cell_at(&self, col: u16, row: u16) -> Option<CellPos> {
        let rect = self.rect?;
        let last_col = rect.x.saturating_add(rect.width).saturating_sub(1);
        let last_row = rect.y.saturating_add(rect.height).saturating_sub(1);
        let clamped_col = col.clamp(rect.x, last_col);
        let clamped_row = row.clamp(rect.y, last_row);
        Some(CellPos {
            col: clamped_col - rect.x,
            row: clamped_row - rect.y,
        })
    }
}

/// A cell coordinate inside the agent pane (`(0, 0)` is the top-left
/// of the PTY area, *not* the screen). Stored pane-relative so the
/// selection survives a renderer reshuffle that moves the pane around
/// — only a resize / scrollback shift can invalidate the relationship
/// to the underlying content, and we clear on resize for that reason.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
struct CellPos {
    row: u16,
    col: u16,
}

/// Active drag-to-select gesture. `anchor` is the cell of the original
/// `Down(Left)`; `head` is updated on every `Drag(Left)` event. Bound
/// to a stable [`AgentId`] so a tab switch / agent reap mid-gesture
/// cancels gracefully — the lookup at commit time will return `None`
/// and we just clear without emitting OSC 52.
#[derive(Clone, Eq, PartialEq, Debug)]
struct Selection {
    agent: AgentId,
    anchor: CellPos,
    head: CellPos,
}

/// A URL the user is hovering over with Ctrl held. Stored pane-relative
/// (same as [`Selection`]) and bound to a stable [`AgentId`] so a tab
/// switch or reap clears it without lookup ambiguity. The renderer
/// underlines `cols` on `row`; the click handler reads `url` to dispatch
/// the OS opener. Single-row only — see [`crate::url_scan`] for why.
#[derive(Clone, Eq, PartialEq, Debug)]
struct HoverUrl {
    agent: AgentId,
    row: u16,
    cols: std::ops::Range<u16>,
    url: String,
}

/// Pane-overlay state passed to the render layer: read-only references
/// to the user's drag-to-select selection and Ctrl-hover URL highlight.
/// Bundled to keep render-fn signatures from gaining a parameter every
/// time a new overlay lands — flagged in architecture review as
/// viscosity / tramp data when threaded as separate args.
#[derive(Clone, Copy, Default)]
struct PaneOverlay<'a> {
    selection: Option<&'a Selection>,
    hover: Option<&'a HoverUrl>,
}

/// Returns `(start, end)` ordered top-left-first regardless of which
/// direction the user dragged. `vt100::Screen::contents_between`
/// requires the start cell to be earlier in reading order than the
/// end cell, so this normalization is mandatory before extraction.
fn normalized_range(a: CellPos, b: CellPos) -> (CellPos, CellPos) {
    if (a.row, a.col) <= (b.row, b.col) {
        (a, b)
    } else {
        (b, a)
    }
}

/// Cell-column bounds for `row` in a multi-row selection. Returns
/// `(col_lo, col_hi_excl)` so the caller can iterate `col_lo..col_hi`.
///
/// Three cases collapse here:
/// - **Single-row selection** (`start.row == end.row`): `start.col..end.col + 1`.
/// - **Top row of a multi-row selection**: `start.col..pane_width`.
/// - **Middle row**: `0..pane_width`.
/// - **Bottom row**: `0..end.col + 1`.
///
/// `pane_width` is exclusive (matches `Rect::width`). The `+ 1` on
/// the trailing column is because selection bounds are inclusive at
/// both ends, but iteration is half-open.
fn row_bounds(start: CellPos, end: CellPos, row: u16, pane_width: u16) -> (u16, u16) {
    if start.row == end.row {
        return (start.col, end.col.saturating_add(1).min(pane_width));
    }
    if row == start.row {
        (start.col, pane_width)
    } else if row == end.row {
        (0, end.col.saturating_add(1).min(pane_width))
    } else {
        (0, pane_width)
    }
}

/// Per-frame mouse hitboxes for the tab strip (`Popup` mode) and nav
/// rows (`LeftPane` mode). The two leaf renderers (`render_status_bar`,
/// `render_left_pane`) populate this; the event loop reads it on
/// `MouseEventKind::Down`/`Up(Left)` to translate screen coordinates
/// to an agent identity.
///
/// The struct is owned by `event_loop` and cleared at the top of every
/// `render_frame` so a stale frame's rects can never bleed into the
/// next event hit-test if the layout changed (e.g. terminal resize,
/// nav-style toggle).
#[derive(Default)]
struct TabHitboxes {
    rects: Vec<Hitbox>,
}

impl TabHitboxes {
    fn clear(&mut self) {
        self.rects.clear();
    }

    fn record(&mut self, rect: Rect, agent_id: AgentId) {
        if rect.width == 0 || rect.height == 0 {
            return;
        }
        self.rects.push(Hitbox { rect, agent_id });
    }

    /// Return the agent id whose hitbox contains the given screen
    /// cell, or `None` if the cell is outside every recorded rect
    /// (the buddy tail, the right-aligned hint, the agent pane, etc).
    fn at(&self, col: u16, row: u16) -> Option<AgentId> {
        self.rects
            .iter()
            .find(|h| h.contains(col, row))
            .map(|h| h.agent_id.clone())
    }
}

/// What the prefix-key dispatcher tells the event loop to do. Distinct from
/// `keymap::PrefixAction` because some dispatches (forwarding bytes,
/// addressing an agent by index) carry payload that the binding itself does
/// not encode.
#[derive(Debug, Eq, PartialEq)]
enum KeyDispatch {
    Forward(Vec<u8>),
    Consume,
    Exit,
    SpawnAgent,
    FocusNext,
    FocusPrev,
    FocusLast,
    FocusAt(usize),
    ToggleNav,
    OpenPopup,
    OpenHelp,
    DismissAgent,
    KillAgent,
}

struct RuntimeAgent {
    /// Stable identity for this agent — invariant for the agent's
    /// entire lifetime, regardless of its position in the navigator
    /// `Vec`. The renderer records hitboxes by `id`, the mouse handler
    /// stores `id` in `mouse_press`, and the click/drag-reorder
    /// dispatcher returns `id`s. Resolving `id → index` happens at the
    /// last possible moment so a reap or reorder between Down and Up
    /// can't silently retarget the gesture (the resolution returns
    /// `None` and the gesture cancels gracefully).
    id: AgentId,
    /// Static fallback shown when the foreground process hasn't yet
    /// emitted an OSC title (and as a debugging breadcrumb in
    /// tracing). User-visible labels render via [`agent_label_spans`]
    /// using the live title when available.
    label: String,
    /// Repo name for this agent: git root basename when local
    /// resolution found a `.git`, otherwise the cwd basename. `None`
    /// if neither could be determined (e.g. local agent spawned with
    /// no cwd, or remote spawn that defaulted to `$HOME` with an
    /// empty path); the renderer falls back to `label` in that case.
    repo: Option<String>,
    /// Working directory the agent was spawned in (local agents only).
    /// Stored separately from [`Self::repo`] because the status-bar
    /// `BranchSegment` needs the original cwd to compare its basename
    /// against `repo` (worktree vs. plain checkout) and the
    /// `agent_meta_worker` needs it to read `.git/HEAD` for the
    /// branch lookup. `None` for SSH agents (cwd is remote —
    /// different type, not handled in v1) and for local agents
    /// spawned without an explicit cwd.
    cwd: Option<PathBuf>,
    /// Hostname for SSH-backed agents (`Some` for both Ready and
    /// Failed SSH agents, `None` for local). The single source of
    /// truth — the renderer derives the dim/gray prefix from this,
    /// and the failure pane reads it for the "✗ bootstrap of {host}
    /// failed" line.
    host: Option<String>,
    /// Most-recent model alias + reasoning effort reported by the
    /// [`crate::agent_meta_worker`]. Updates when the user runs
    /// `/model` inside any agent (the source is the global
    /// `~/.claude/settings.json`, not a per-session transcript).
    /// `None` until the worker's first successful read of that file,
    /// and for SSH agents (worker only handles local in v1). Held as
    /// a single struct rather than two flat fields so the segment
    /// can never render a torn pair (a fresh model with a stale
    /// effort, or vice versa).
    model_effort: Option<crate::agent_meta_worker::ModelEffort>,
    /// Most-recent git branch reported by the [`crate::agent_meta_worker`]
    /// for the focused agent's cwd. `None` outside a git repo, on
    /// HEAD-parse failures, and for SSH agents.
    branch: Option<String>,
    /// Working state observed on the previous frame. The runtime
    /// compares this to `parser.callbacks().is_working()` each tick;
    /// a `true → false` transition while the agent is *not* focused
    /// flips [`needs_attention`](Self::needs_attention) on. Stored
    /// per-agent rather than as a side-table so reaping a transport
    /// also drops the state — no stale slot to clean up.
    last_working: bool,
    /// True after the agent went working→idle while unfocused. The
    /// renderer pulses the tab body in `DarkGray` with a small `●`
    /// prefix so the user notices something completed without yelling
    /// in yellow / red. Cleared the moment the user focuses this tab
    /// (see [`change_focus`]).
    needs_attention: bool,
    /// Cell width/height the agent's pane is currently allocated.
    /// Tracked separately so a `Failed` agent (no transport) still has
    /// the right geometry if the surrounding logic ever needs it, and
    /// so the next `Ready` agent we spawn can be sized at the
    /// terminal's *current* dimensions rather than whatever was true
    /// at TUI start.
    rows: u16,
    cols: u16,
    state: AgentState,
}

/// Per-agent state. The lifecycle has two terminal states reached
/// from different entry points:
///
/// - `(spawn modal) → Ready → Crashed → (dismissed)` when a previously
///   live agent's transport dies (local claude exit, SSH tunnel drop,
///   remote daemon death). The parser is preserved so the user can
///   read what was on screen at the moment of death.
/// - `(spawn modal) → Failed → (dismissed)` when the SSH bootstrap
///   itself errors out before there's ever a live transport.
///
/// Both terminal states share the same `<prefix> d` dismiss UX. The
/// runtime never auto-reaps a Crashed or Failed agent — silence on
/// crash was the prior behavior and made the TUI feel buggy ("did I
/// do that?"); the user now decides when to close the tab.
///
/// In-flight SSH bootstraps live in the spawn modal (see
/// [`crate::spawn::SpawnMinibuffer::lock_for_bootstrap`]) rather than
/// in this enum: the user picks a remote folder *between* prepare and
/// attach, so the in-flight phase has UX that doesn't fit a per-agent
/// pane.
enum AgentState {
    /// Bootstrap returned an error. The dead handle has already been
    /// dropped; the variant carries the structured
    /// [`codemuxd_bootstrap::Error`] so the renderer can format it
    /// (single-line summary today, "Caused by:" cascade later) without
    /// the runtime baking in a stringification policy here. The host
    /// itself lives on [`RuntimeAgent::host`] (single source of truth
    /// across `Ready` and `Failed`).
    Failed { error: codemuxd_bootstrap::Error },
    /// Live agent with an attached PTY (local or SSH-tunneled).
    Ready {
        /// Boxed because `vt100::Parser` carries a screen-sized cell
        /// grid (~720 bytes), which dwarfs the `Failed` variant.
        /// Without the box, every `RuntimeAgent` pays the
        /// `Ready`-sized footprint regardless of state, and clippy
        /// fires `large_enum_variant`. The pointer indirection is
        /// invisible against the per-frame parser/render work.
        ///
        /// Parameterised on [`TitleCapture`] so OSC 0 / OSC 2 titles
        /// the foreground process emits land in
        /// `parser.callbacks().title()` and feed the smart-label
        /// renderer.
        parser: Box<Parser<TitleCapture>>,
        transport: AgentTransport,
    },
    /// Transport died after the agent had been `Ready`. The parser
    /// is preserved (boxed for the same reason as in `Ready`) so the
    /// renderer can still draw the last screen content under the
    /// crash banner — that's usually the most useful diagnostic for
    /// the user. `exit_code` distinguishes:
    ///
    /// - `0` — clean exit (e.g. user typed `/quit` in claude)
    /// - `> 0` — non-zero process exit
    /// - `-1` — `SshDaemonPty` sentinel for socket-level failures
    ///   (tunnel drop, daemon death, framed-reader I/O error)
    ///
    /// The renderer chooses banner color and copy from this code.
    Crashed {
        parser: Box<Parser<TitleCapture>>,
        exit_code: i32,
    },
}

impl AgentState {
    /// Live or last-frozen vt100 screen for this agent. `Some` for
    /// `Ready` (live PTY) and `Crashed` (frozen at moment of death so
    /// the user can still scroll back through what claude was doing
    /// pre-crash); `None` for `Failed` because no parser was ever
    /// constructed. Centralised here so call sites that need a screen
    /// (selection commit, hover lookup, post-draw OSC 8 painter) don't
    /// each repeat the same `Ready | Crashed => Some(...), Failed =>
    /// None` match against the variant shape.
    fn screen(&self) -> Option<&vt100::Screen> {
        match self {
            AgentState::Ready { parser, .. } | AgentState::Crashed { parser, .. } => {
                Some(parser.screen())
            }
            AgentState::Failed { .. } => None,
        }
    }
}

impl RuntimeAgent {
    // 9 args after the agent_meta_worker landing; identity, label,
    // working dir, host, transport, geometry, and scrollback budget
    // are all distinct concerns that the single call site sets at
    // once. A builder would add code without making any of these
    // optional or composable.
    #[allow(clippy::too_many_arguments)]
    fn ready(
        id: AgentId,
        label: String,
        repo: Option<String>,
        cwd: Option<PathBuf>,
        host: Option<String>,
        transport: AgentTransport,
        rows: u16,
        cols: u16,
        scrollback_len: usize,
    ) -> Self {
        Self {
            id,
            label,
            repo,
            cwd,
            host,
            model_effort: None,
            branch: None,
            last_working: false,
            needs_attention: false,
            rows,
            cols,
            state: AgentState::Ready {
                parser: Box::new(Parser::new_with_callbacks(
                    rows,
                    cols,
                    scrollback_len,
                    TitleCapture::default(),
                )),
                transport,
            },
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn failed(
        id: AgentId,
        label: String,
        repo: Option<String>,
        cwd: Option<PathBuf>,
        host: String,
        error: codemuxd_bootstrap::Error,
        rows: u16,
        cols: u16,
    ) -> Self {
        Self {
            id,
            label,
            repo,
            cwd,
            host: Some(host),
            model_effort: None,
            branch: None,
            last_working: false,
            needs_attention: false,
            rows,
            cols,
            state: AgentState::Failed { error },
        }
    }

    /// Current scrollback offset (`0` = live view). Returns `0` for a
    /// `Failed` agent so callers can check "is this agent in scroll
    /// mode" without matching on the state. `Crashed` agents keep
    /// their parser, so scrollback is meaningful even though the
    /// transport is gone — the user can still page back through what
    /// claude was doing pre-crash.
    fn scrollback_offset(&self) -> usize {
        self.state.screen().map_or(0, vt100::Screen::scrollback)
    }

    /// Adjust the scrollback offset by `delta` (positive scrolls back
    /// into history, negative toward the live view). Saturates at zero
    /// on the bottom; `vt100::Screen::set_scrollback` clamps the top to
    /// the buffer length, so we don't need to know the cap. No-op for
    /// `Failed` agents — they have no parser. `Crashed` agents do
    /// have one, so they scroll normally.
    fn nudge_scrollback(&mut self, delta: i32) {
        if let AgentState::Ready { parser, .. } | AgentState::Crashed { parser, .. } =
            &mut self.state
        {
            let screen = parser.screen_mut();
            let next = screen.scrollback().saturating_add_signed(delta as isize);
            screen.set_scrollback(next);
        }
    }

    /// Snap back to the live view (offset = 0). Used by the
    /// `ScrollAction::ExitScroll` path and by the non-sticky "any
    /// forwarded keystroke snaps" rule. Crashed agents have no live
    /// view per se but the same call resets scrollback to the bottom
    /// so the crash banner and last frame are aligned.
    fn snap_to_live(&mut self) {
        if let AgentState::Ready { parser, .. } | AgentState::Crashed { parser, .. } =
            &mut self.state
        {
            parser.screen_mut().set_scrollback(0);
        }
    }

    /// Jump to the top of the buffer. `vt100::Screen::set_scrollback`
    /// clamps to the buffer length, so passing `usize::MAX` reaches the
    /// top regardless of the configured `scrollback_len` — no need to
    /// thread the cap through.
    fn jump_to_top(&mut self) {
        if let AgentState::Ready { parser, .. } | AgentState::Crashed { parser, .. } =
            &mut self.state
        {
            parser.screen_mut().set_scrollback(usize::MAX);
        }
    }

    /// Live OSC title from the agent's parser, if any. `None` for
    /// `Failed` agents (no parser) and for agents whose foreground
    /// process never emitted a title. Crashed agents keep returning
    /// their last title — the renderer dims/strikes-through the tab
    /// label separately based on state.
    fn title(&self) -> Option<&str> {
        match &self.state {
            AgentState::Ready { parser, .. } | AgentState::Crashed { parser, .. } => {
                parser.callbacks().title()
            }
            AgentState::Failed { .. } => None,
        }
    }

    /// Whether the foreground process is currently in a working
    /// state per its OSC title. `false` for `Failed` and `Crashed`
    /// agents — Crashed has no foreground process anymore, regardless
    /// of what the title was at the moment of death — and `false`
    /// for Ready agents whose title doesn't carry a status glyph.
    fn is_working(&self) -> bool {
        match &self.state {
            AgentState::Ready { parser, .. } => parser.callbacks().is_working(),
            AgentState::Failed { .. } | AgentState::Crashed { .. } => false,
        }
    }

    /// Transition `Ready → Crashed` in-place, preserving the parser
    /// so the renderer can still draw the last screen. No-op for
    /// agents already in a terminal state.
    ///
    /// Implementation note: `parser` lives inside the `Ready`
    /// variant, so we can't move it out behind a `&mut self`
    /// borrow without first swapping the state. We `mem::replace`
    /// with a placeholder `Crashed` carrying a tiny throwaway
    /// parser at the agent's current geometry, then immediately
    /// overwrite once we own the prior `Ready`. The placeholder
    /// allocation is paid once per crash event — invisible against
    /// the per-frame parser/render work.
    fn mark_crashed(&mut self, exit_code: i32) {
        let placeholder = AgentState::Crashed {
            parser: Box::new(Parser::new_with_callbacks(
                self.rows.max(1),
                self.cols.max(1),
                0,
                TitleCapture::default(),
            )),
            exit_code,
        };
        let prior = std::mem::replace(&mut self.state, placeholder);
        match prior {
            AgentState::Ready { parser, .. } => {
                self.state = AgentState::Crashed { parser, exit_code };
            }
            other => {
                // Already terminal — restore. The placeholder we
                // installed above gets dropped here, costing only the
                // throwaway parser allocation.
                self.state = other;
            }
        }
    }
}

/// In-flight prepare worker for an SSH host. Owned by the runtime
/// across the modal's locked-during-bootstrap state. Replacing this
/// slot cancels the previous worker via `PrepareHandle::Drop`. On
/// `Done(Ok(_))` the runtime stashes the result for the subsequent
/// attach phase; on `Done(Err)` it surfaces the error to the modal
/// and unlocks back to the host zone.
struct PendingPrepare {
    host: String,
    handle: PrepareHandle,
    /// Set after prepare reports `Done(Ok(_))`. Holds the remote
    /// `$HOME` so the runtime can pass it to
    /// `unlock_for_remote_path` and, later, build the `PreparedHost`
    /// the attach worker needs.
    prepared: Option<PreparedHost>,
    /// `Some` if the worker's `RemoteFs::open` call succeeded. Held
    /// on the runtime side so the `ControlMaster`'s `Drop` cleans up
    /// when the prepare slot is replaced or cancelled. The runtime
    /// hands a `&fs` / `&runner` pair to the modal per keystroke via
    /// `DirLister`.
    remote_fs: Option<RemoteFs>,
    /// `Some(path)` when this prepare was triggered by a host-bound
    /// named project (`ModalOutcome::PrepareHostThenSpawn`). On
    /// `Done(Ok)` the runtime dismisses the modal and synthesizes
    /// the SSH spawn against this path instead of unlocking the
    /// modal for user path entry. `None` for the regular
    /// `PrepareHost` flow (host typed at the modal, user picks the
    /// remote path manually after prepare).
    pending_project_path: Option<String>,
}

/// In-flight attach worker. The modal usually stays locked watching
/// this attach via `modal_owner = true`; on Done we push a Ready or
/// Failed agent and close the modal. `attaches: Vec<_>` rather than
/// `Option<_>` so a future flow where the user dismisses the modal
/// during attach can leave the handle running in the background.
struct PendingAttach {
    /// Stable identity for the agent that this attach will produce.
    /// Plumbed through here so the eventual `RuntimeAgent::ready` /
    /// `RuntimeAgent::failed` constructor — which fires on the
    /// daemon-bootstrap thread's response — uses the same id the
    /// spawn site already chose, rather than rederiving one from a
    /// possibly-stale `spawn_counter`.
    agent_id: AgentId,
    label: String,
    host: String,
    /// Repo name resolved from the user-typed remote cwd (basename).
    /// Stored here rather than recomputed on Done because the cwd
    /// itself isn't carried past attach kickoff and the modal might
    /// have been replaced by then.
    repo: Option<String>,
    rows: u16,
    cols: u16,
    handle: AttachHandle,
    /// `true` if the spawn modal is currently locked watching this
    /// attach — at most one `PendingAttach` has this set at a time.
    /// Stored per-attach so removing a finished entry from the Vec
    /// doesn't shift indices and break a separate `modal_attach_idx`.
    modal_owner: bool,
}

pub fn run(
    nav_style: NavStyle,
    config: &Config,
    initial_cwd: &Path,
    log_tail: Option<&LogTail>,
) -> Result<()> {
    tracing::info!(?initial_cwd, "codemux starting (nav={nav_style:?})");

    let (term_cols, term_rows) = crossterm::terminal::size().wrap_err("read terminal size")?;
    let (pty_rows, pty_cols) = pty_size_for(nav_style, term_rows, term_cols, log_tail.is_some());

    let initial = spawn_local_agent(
        AgentId::new("agent-1"),
        "agent-1".into(),
        Some(initial_cwd),
        pty_rows,
        pty_cols,
        config.scrollback_len,
    )?;
    let agents = vec![initial];

    enable_raw_mode().wrap_err("enable raw mode")?;
    execute!(io::stdout(), EnterAlternateScreen).wrap_err("enter alt screen")?;

    // Mouse capture: needed for `MouseEventKind::ScrollUp/Down` to reach
    // us as `Event::Mouse`. Without it, terminals running on the
    // alternate screen translate the wheel into ↑ / ↓ arrow keys via
    // their `alternateScroll` feature — which Claude Code interprets as
    // "cycle prompt history," not what the user wants. Apple Terminal is
    // the documented exception (no SGR mouse support); on every other
    // mainstream terminal `?1006h` arrives via crossterm's
    // `EnableMouseCapture`. Cost: native click-and-drag selection now
    // requires holding ⌥/Alt to bypass capture; the help screen
    // documents this. See AD-25.
    let mouse_captured = execute!(io::stdout(), EnableMouseCapture).is_ok();
    if !mouse_captured {
        tracing::warn!("EnableMouseCapture failed; scrollback wheel will not work");
    }

    // Bracketed paste: tells the terminal to wrap pasted content in
    // `\x1b[200~ ... \x1b[201~` so crossterm can deliver it as a single
    // `Event::Paste` instead of streaming each character as a `KeyEvent`.
    // Without this, embedded newlines in the paste arrive as
    // `KeyCode::Enter`, which `key_to_bytes` maps to `\r` — and Claude
    // submits the message on every line. The paste handler re-wraps the
    // text in `\x1b[200~ ... \x1b[201~` before forwarding to the inner
    // PTY because Claude advertises `?2004h` and we are the host
    // terminal from its perspective.
    let bracketed_paste = execute!(io::stdout(), EnableBracketedPaste).is_ok();
    if !bracketed_paste {
        tracing::warn!("EnableBracketedPaste failed; multi-line pastes may submit early");
    }

    // Auto-detect: enable the Kitty Keyboard Protocol when the user has
    // bound something to a SUPER (Cmd / Win) chord OR when the URL-modifier
    // yield feature is active (the latter needs bare-modifier press/release
    // events, which are off by default and require the
    // `REPORT_ALL_KEYS_AS_ESCAPE_CODES` + `REPORT_EVENT_TYPES` sub-modes).
    // Without this, terminals that support the protocol (Ghostty, Kitty,
    // WezTerm, recent Alacritty, Foot) cannot deliver Cmd events at all,
    // and the modifier-yield logic never sees the press/release pair.
    // Terminals that do not understand the negotiation simply ignore it.
    let needs_super_keys = config.bindings.uses_super_modifier();
    let needs_modifier_events = !matches!(config.mouse_url_modifier, MouseUrlModifier::None);
    let mut keyboard_flags = KeyboardEnhancementFlags::empty();
    if needs_super_keys || needs_modifier_events {
        keyboard_flags |= KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES;
    }
    if needs_modifier_events {
        keyboard_flags |= KeyboardEnhancementFlags::REPORT_EVENT_TYPES;
        keyboard_flags |= KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES;
        // Also push REPORT_ALTERNATE_KEYS so the terminal includes the
        // shifted form alongside the base form for keys like Shift+1.
        // Without this flag, REPORT_ALL_KEYS_AS_ESCAPE_CODES makes
        // crossterm receive `KeyCode::Char('1') + SHIFT` and our wire
        // encoder writes "1" to the PTY instead of "!" — Shift-typed
        // symbols get the wrong character. With this flag, crossterm
        // resolves to `KeyCode::Char('!')` directly.
        keyboard_flags |= KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS;
    }
    let enhanced_keyboard = !keyboard_flags.is_empty()
        && execute!(io::stdout(), PushKeyboardEnhancementFlags(keyboard_flags)).is_ok();
    if enhanced_keyboard {
        tracing::debug!(
            ?keyboard_flags,
            "Kitty Keyboard Protocol enabled (super-bindings or url-modifier)",
        );
    }

    // Focus-change events let us reclaim mouse capture if the user alt-tabs
    // away while still holding the URL modifier. Only useful when the yield
    // feature is on; for a `None` modifier we never yield so we never need
    // to reclaim on focus loss.
    let focus_changes = needs_modifier_events && execute!(io::stdout(), EnableFocusChange).is_ok();

    // Save the user's pre-codemux terminal title onto the emulator's
    // internal stack so the matching pop in `TerminalGuard::drop`
    // restores it. Tracked separately on the guard for symmetry with
    // the other reverse-on-drop sequences. Any I/O failure here is
    // benign: it just means we won't restore the title on exit.
    let host_title_pushed = host_title::push_title(&mut io::stdout()).is_ok();

    let _guard = TerminalGuard {
        enhanced_keyboard,
        mouse_captured,
        bracketed_paste,
        host_title_pushed,
        focus_changes,
    };

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).wrap_err("construct ratatui terminal")?;

    let chrome = ChromeStyle::from_ui(&config.ui);
    let segments = status_bar::build_segments(&config.ui.status_bar_segments, &config.ui.segments);
    let ctx = RuntimeContext {
        bindings: &config.bindings,
        chrome: &chrome,
        spawn_config: &config.spawn,
        scrollback_len: config.scrollback_len,
        host_bell_on_finish: config.ui.host_bell_on_finish,
        mouse_captured,
        mouse_url_modifier: config.mouse_url_modifier,
        mouse_yield_on_failed: config.mouse_yield_on_failed,
        segments: &segments,
    };
    event_loop(
        &mut terminal,
        agents,
        nav_style,
        log_tail,
        initial_cwd,
        &ctx,
    )
}

fn pty_size_for(style: NavStyle, term_rows: u16, term_cols: u16, log_strip: bool) -> (u16, u16) {
    let log_rows = if log_strip { LOG_STRIP_HEIGHT } else { 0 };
    match style {
        NavStyle::LeftPane => (
            term_rows.saturating_sub(log_rows),
            term_cols.saturating_sub(NAV_PANE_WIDTH),
        ),
        NavStyle::Popup => (
            term_rows.saturating_sub(STATUS_BAR_HEIGHT + log_rows),
            term_cols,
        ),
    }
}

/// Daemon-facing `agent_id` for an SSH spawn. The TUI's pid namespaces
/// the id so a relaunch can never collide with a still-live remote
/// daemon from a previous codemux invocation — the bug being fixed
/// here was the bootstrap silently re-attaching to the surviving
/// daemon's socket and replaying its captured Claude PTY snapshot.
/// See the call site in `event_loop` for the full rationale.
fn daemon_agent_id_for(tui_pid: u32, spawn_counter: usize) -> String {
    format!("agent-{tui_pid}-{spawn_counter}")
}

/// One frame of fuzzy-worker bookkeeping for the spawn modal:
/// drain any results the worker has produced and apply them to the
/// modal, then dispatch fresh `SetIndex` / `Query` messages when the
/// per-host index generation or the modal's query string have moved
/// on since the last dispatch. Cheap when the modal is closed
/// (drain-and-drop) or not in fuzzy mode (no dispatch).
///
/// Memoization tables are passed by `&mut` so the helper can be
/// called from both the per-tick spot at the top of the loop AND the
/// post-keystroke spot inside the event arm without losing state
/// between calls. Both call sites need the same drain-then-dispatch
/// ordering — see the doc on [`crate::fuzzy_worker`] for the race
/// the ordering exists to handle.
fn tick_fuzzy_dispatch(
    spawn_ui: Option<&mut SpawnMinibuffer>,
    fuzzy_worker: &FuzzyWorker,
    index_mgr: &IndexManager,
    last_pushed_index_gen: &mut HashMap<String, u64>,
    last_pushed_query: &mut HashMap<String, String>,
) {
    // Drain first regardless — even if the modal is closed, results
    // from a just-cancelled query may still be in flight; drop them
    // so the channel doesn't grow unbounded.
    let drained = fuzzy_worker.drain();
    let Some(ui) = spawn_ui else {
        return;
    };
    for r in drained {
        ui.set_fuzzy_results(r);
    }
    // Active-host gate + empty-query short-circuit: the modal returns
    // `None` outside Fuzzy + Path mode and for empty queries. Both
    // cases mean "no fuzzy work to dispatch."
    let Some(req) = ui.fuzzy_dispatch_request() else {
        return;
    };
    let host = req.host.to_string();
    let query = req.query.to_string();
    // SetIndex memoization: only push when the per-host generation
    // counter (bumped by `IndexCatalog` mutations and the in-place
    // `dirs.extend` in `IndexManager::drain_one_host`) differs from
    // the last-pushed value. This is the line that turns ~50 ms-per-
    // frame `score_fuzzy` calls into "once per index transition."
    let current_gen = index_mgr.state_generation_for(&host);
    let pushed_gen = last_pushed_index_gen.get(&host).copied();
    if current_gen != pushed_gen
        && let Some(dirs) = index_mgr.state_for(&host).and_then(IndexState::cached_dirs)
    {
        let named = ui.named_projects().to_vec();
        fuzzy_worker.set_index(host.clone(), dirs.to_vec(), named);
        if let Some(g) = current_gen {
            last_pushed_index_gen.insert(host.clone(), g);
        }
        // Force the Query branch below to re-dispatch so the
        // worker scores the *current* query against the *new*
        // index in the same drain batch — without this, a query
        // that landed before the index was ready would never
        // produce a result.
        last_pushed_query.remove(&host);
    }
    // Query memoization: dispatch on every distinct value, not on
    // every frame. Combined with the SetIndex memoization above, an
    // idle modal with a static query produces zero worker traffic.
    if last_pushed_query.get(&host).map(String::as_str) != Some(query.as_str()) {
        fuzzy_worker.query(host.clone(), query.clone());
        last_pushed_query.insert(host, query);
    }
}

/// Construct a [`RuntimeAgent`] backed by a local PTY. The transport
/// owns the PTY shape (master + child + reader thread); the runtime
/// keeps the renderable [`Parser`] alongside, since rendering is the
/// runtime's job (AD-1) and not the transport's.
fn spawn_local_agent(
    id: AgentId,
    label: String,
    cwd: Option<&Path>,
    rows: u16,
    cols: u16,
    scrollback_len: usize,
) -> Result<RuntimeAgent> {
    let transport = AgentTransport::spawn_local(label.clone(), cwd, rows, cols)
        .wrap_err("spawn local agent")?;
    let repo = cwd.and_then(repo_name::resolve_local);
    Ok(RuntimeAgent::ready(
        id,
        label,
        repo,
        cwd.map(Path::to_path_buf),
        None,
        transport,
        rows,
        cols,
        scrollback_len,
    ))
}

/// Build the [`PendingAttach`] for an SSH spawn. Shared by the user-
/// driven `Spawn { host, path }` path (modal stays open watching the
/// attach with `modal_owner = true`) and the auto-spawn-after-prepare
/// path triggered by `PrepareHostThenSpawn` (modal is dismissed before
/// the call so `modal_owner = false`).
///
/// Modal state (lock vs dismiss) is the caller's job — both flows
/// agree on how the attach is launched, but disagree on what the modal
/// should look like while it runs.
///
/// Mirrors [`spawn_local_agent`] in shape: do the PTY/transport-shaped
/// work in one place, return a value the caller threads into the
/// runtime's collections.
///
/// `attach_factory` is the function that turns the prepared host plus
/// attach-shaped args into an [`AttachHandle`]. Production passes
/// [`start_attach`] directly (function items coerce to `FnOnce`). Tests
/// pass a closure that returns a scripted handle via
/// [`AttachHandle::from_events`], avoiding any real worker thread.
//
// Nine args (vs clippy's default 7): each is a distinct fact the helper
// needs and has no natural pairing — bundling `(rows, cols)` or
// `(tui_pid, spawn_counter)` into a struct would only push the noise
// to the call site. The added `attach_factory` is the test-injection
// seam (mirrors the `start_attach` / `start_attach_with_runner` split
// in `bootstrap_worker.rs`).
#[allow(clippy::too_many_arguments)]
fn build_remote_attach<F>(
    prepared: PreparedHost,
    host: String,
    path: &str,
    tui_pid: u32,
    spawn_counter: usize,
    rows: u16,
    cols: u16,
    modal_owner: bool,
    attach_factory: F,
) -> PendingAttach
where
    F: FnOnce(PreparedHost, String, String, Option<PathBuf>, u16, u16) -> AttachHandle,
{
    let label = format!("{host}:agent-{spawn_counter}");
    let runtime_id = AgentId::new(format!("agent-{spawn_counter}"));
    // Daemon-facing id is namespaced by the TUI's pid so a relaunch
    // never collides with a still-live remote daemon from a previous
    // codemux invocation. Without the prefix the bootstrap silently
    // re-attaches to the old socket (the new daemon's bind fails on
    // the held pid file, but the surviving socket is what the poll
    // loop sees). The user-visible label and in-process AgentId stay
    // short intentionally — the prefix is for the remote filesystem,
    // not for humans.
    let daemon_agent_id = daemon_agent_id_for(tui_pid, spawn_counter);
    // Empty path → None: omit `--cwd` on the remote daemon and let
    // it inherit the remote shell's login cwd ($HOME). A local path
    // here would otherwise be sent verbatim to the remote, fail
    // `cwd.exists()`, and exit the daemon before it ever bound the
    // socket — the user-visible "EOF before HelloAck" failure mode.
    let cwd_path = if path.is_empty() {
        None
    } else {
        Some(PathBuf::from(path))
    };
    // Repo name shown in the navigator for this agent. We can't probe
    // the remote filesystem from here without a second ssh round-trip
    // (the prepare's `RemoteFs` was already dropped), so we settle
    // for the basename of whatever the user typed. `None` for empty
    // paths — the renderer falls back to the static label.
    let repo = if path.is_empty() {
        None
    } else {
        repo_name::resolve_remote(path)
    };
    let handle = attach_factory(
        prepared,
        host.clone(),
        daemon_agent_id,
        cwd_path,
        rows,
        cols,
    );
    tracing::info!(%host, label = %label, "started SSH attach worker");
    PendingAttach {
        agent_id: runtime_id,
        label,
        host,
        repo,
        rows,
        cols,
        handle,
        modal_owner,
    }
}

/// Bundle of refs/values the [`drain_prepare_events`] state machine
/// needs from `event_loop`'s locals. Exists only to keep the function
/// signature within the project's argument-count convention (mirrors
/// the [`RuntimeContext`] / [`NavState`] pattern: bundle at the
/// boundary, destructure inside).
///
/// `'a` is the borrow lifetime of all the `&mut` refs into
/// `event_loop`'s locals; `F` is the attach-handle factory closure
/// (production passes [`start_attach`], tests pass a closure
/// returning [`crate::bootstrap_worker::AttachHandle::from_events`]).
struct PrepareDrainCtx<'a, F>
where
    F: FnOnce(PreparedHost, String, String, Option<PathBuf>, u16, u16) -> AttachHandle,
{
    prepare: &'a mut Option<PendingPrepare>,
    spawn_ui: &'a mut Option<SpawnMinibuffer>,
    attaches: &'a mut Vec<PendingAttach>,
    index_mgr: &'a mut IndexManager,
    spawn_counter: &'a mut usize,
    spawn_config: &'a SpawnConfig,
    pty_geom: (u16, u16),
    tui_pid: u32,
    attach_factory: F,
}

/// Drain one frame's worth of prepare events from the in-flight
/// bootstrap worker (if any), advancing the modal/attach state machine
/// accordingly. Returns once the channel is empty for this frame OR
/// the worker reported `Done(_)` and the resulting transition has
/// been applied.
///
/// Three outcomes encoded in the slot's resting state after the call:
///
/// 1. **Still in flight** — `*prepare` is `Some(_)`, the channel had
///    only `Stage(_)` events (or none). The modal's stage indicator was
///    updated; nothing else changed. Re-runs next frame.
///
/// 2. **Success → user picks remote folder** (`pending_project_path:
///    None`) — `*prepare` is `Some(_)` with `prepared` and `remote_fs`
///    populated; the modal is unlocked into `PathMode::Remote`. The
///    subsequent user-driven `ModalOutcome::Spawn { host, path }`
///    consumes the slot.
///
/// 3. **Success → auto-spawn** (`pending_project_path: Some(path)`,
///    set by `ModalOutcome::PrepareHostThenSpawn`) — the modal is
///    dismissed, an attach is built via `build_remote_attach` against
///    the stashed path, and pushed onto `attaches`. `*prepare` is
///    `None` — the slot is consumed.
///
/// Plus the failure branch: prepare reported `Done(Err)`, modal goes
/// back to host zone with the structured error visible, `*prepare` is
/// `None`. Failure path takes precedence over auto-spawn.
///
/// `attach_factory` is the function that produces the [`AttachHandle`]
/// for the auto-spawn flow. Production passes [`start_attach`]
/// directly; tests pass a closure returning
/// [`crate::bootstrap_worker::AttachHandle::from_events`] so the call
/// doesn't spawn a worker thread or touch the network.
//
// The args bundle (mutable runtime state + read-only inputs + the
// attach factory) is heterogeneous enough that there's no natural
// pairing — the bundle exists for the boundary, not as a domain
// abstraction. The function body destructures it back into bare
// locals so the inline reads/writes stay readable.
fn drain_prepare_events<F>(ctx: PrepareDrainCtx<'_, F>)
where
    F: FnOnce(PreparedHost, String, String, Option<PathBuf>, u16, u16) -> AttachHandle,
{
    let PrepareDrainCtx {
        prepare,
        spawn_ui,
        attaches,
        index_mgr,
        spawn_config,
        pty_geom,
        tui_pid,
        spawn_counter,
        attach_factory,
    } = ctx;
    // Take the slot out for the duration of this drain. Each branch
    // either re-stashes it (still in flight, or success-without-pending)
    // or drops it (failure, or auto-spawn success). Owning the slot
    // here means `*prepare = None` at the end of the function is
    // implicit — drop semantics — and there's no `&mut prepare` /
    // `&mut p` borrow conflict to dance around like the previous
    // inlined version had.
    let Some(mut slot) = prepare.take() else {
        return;
    };
    let mut completion = None;
    while let Some(event) = slot.handle.try_recv() {
        match event {
            PrepareEvent::Stage(stage) => {
                if let Some(ui) = spawn_ui.as_mut() {
                    ui.set_bootstrap_stage(stage);
                }
            }
            PrepareEvent::Done(result) => {
                completion = Some(result);
                break;
            }
        }
    }
    let Some(completion) = completion else {
        // Channel emptied without a Done — worker still in flight.
        // Re-stash so the next frame keeps polling.
        *prepare = Some(slot);
        return;
    };
    match completion {
        Ok(PrepareSuccess { prepared, fs }) => {
            // SWR for the SSH host: hydrate from the remote disk
            // cache if present, then start a fresh walk in the
            // background. Skip cleanly when `fs` is `None` (the modal
            // would degrade to literal-path mode anyway). Run in both
            // pending-path branches — the remote index is useful next
            // time the modal opens against this host, even when the
            // current spawn doesn't need the user to pick a path.
            if let Some(rfs) = fs.as_ref() {
                let host_roots = spawn_config.ssh_search_roots(&slot.host);
                let outcome = index_mgr.request_remote_swr(
                    &slot.host,
                    rfs.socket_path(),
                    &prepared.remote_home,
                    &host_roots,
                    &spawn_config.project_markers,
                );
                tracing::debug!(?outcome, host = %slot.host, "remote fuzzy index: SWR start");
            }
            match slot.pending_project_path.take() {
                None => {
                    // Regular PrepareHost flow: hand the modal back to
                    // the user for path entry. The worker opened the
                    // ssh `ControlMaster` for us (see
                    // `start_prepare_with_runner`) so the main thread
                    // doesn't block on a synchronous `RemoteFs::open`
                    // poll while the spinner is locked. `fs == None`
                    // means the open failed; the modal degrades to
                    // literal-path mode (logged in the worker).
                    if let Some(ui) = spawn_ui.as_mut() {
                        let runner = RealRunner;
                        let mut lister = match fs.as_ref() {
                            Some(rfs) => DirLister::Remote {
                                fs: rfs,
                                runner: &runner,
                            },
                            None => DirLister::Local,
                        };
                        ui.unlock_for_remote_path(
                            slot.host.clone(),
                            prepared.remote_home.clone(),
                            &mut lister,
                        );
                    }
                    slot.remote_fs = fs;
                    slot.prepared = Some(prepared);
                    *prepare = Some(slot);
                }
                Some(path) => {
                    // PrepareHostThenSpawn flow: dismiss the modal and
                    // launch the SSH attach against the stashed path.
                    // Slot is consumed (not re-stashed) — the prepare
                    // phase's purpose is fulfilled.
                    *spawn_counter += 1;
                    let (rows, cols) = pty_geom;
                    let attach = build_remote_attach(
                        prepared,
                        slot.host,
                        &path,
                        tui_pid,
                        *spawn_counter,
                        rows,
                        cols,
                        /* modal_owner */ false,
                        attach_factory,
                    );
                    *spawn_ui = None;
                    attaches.push(attach);
                }
            }
        }
        Err(e) => {
            tracing::error!(host = %slot.host, "prepare failed: {e}");
            if let Some(ui) = spawn_ui.as_mut() {
                // Pass the structured error; the modal formats it via
                // `user_message()` at render time.
                ui.unlock_back_to_host(&mut DirLister::Local, Some(e));
            }
            // `slot` dropped here — failure path takes precedence
            // over the auto-spawn `pending_project_path` (which is
            // also dropped with the slot). Errors always surface.
        }
    }
}

/// Resolve the configured `[spawn].scratch_dir` against the captured
/// remote `$HOME` and `mkdir -p` it over the prepare's `RemoteFs`.
/// Returns the resolved absolute remote path on success, `None` on
/// any failure so the caller can fall back to today's "use the remote
/// shell's default cwd" semantics.
///
/// Lives next to `spawn_local_agent` because both shapes (local
/// scratch via `std::fs::create_dir_all`, remote scratch via this
/// helper) are spawn-time concerns the runtime owns. Extracted from
/// the `SpawnScratch` arm because nesting four levels of `match` /
/// `Option` inside a 100+-line match arm trips clippy's
/// `single_match_else` lint and makes the arm hard to read.
fn resolve_remote_scratch_cwd(
    spawn_config: &SpawnConfig,
    remote_home: &Path,
    remote_fs: Option<&RemoteFs>,
    runner: &dyn codemuxd_bootstrap::CommandRunner,
) -> Option<PathBuf> {
    let dir = spawn_config.remote_scratch_dir(remote_home)?;
    let Some(fs) = remote_fs else {
        // No live `ControlMaster` to mkdir through. The directory
        // probably already exists (this is the user's habitual
        // scratch dir), so still send `--cwd` and let the daemon
        // validate. If validation fails the user sees the standard
        // SSH-spawn error, not a silent fallback to $HOME.
        tracing::warn!(
            scratch_dir = %dir.display(),
            "no live RemoteFs to mkdir scratch; sending --cwd anyway \
             and letting the daemon validate",
        );
        return Some(dir);
    };
    if let Err(e) = fs.mkdir_p(runner, &dir) {
        // Graceful degradation — the agent still spawns, just at
        // remote $HOME instead of the configured scratch. Warn
        // (not error) per the project logging policy: an end-user-
        // surfaced runbook is the right level for "secondary
        // infrastructure failed but primary use case continues".
        tracing::warn!(
            scratch_dir = %dir.display(),
            "remote scratch mkdir failed: {e}; falling back to remote $HOME",
        );
        return None;
    }
    Some(dir)
}

/// Local twin of [`resolve_remote_scratch_cwd`]. Resolves the
/// configured `[spawn].scratch_dir` against the local `$HOME` and
/// `mkdir -p`s it. Returns the resolved absolute local path on
/// success, `None` on any failure so the caller can fall back to
/// today's "spawn at the TUI's cwd" semantics.
///
/// Extracted for symmetry with the remote helper — both branches
/// of the `SpawnScratch` arm now read identically (`let cwd =
/// resolve_*_scratch_cwd(...);`) instead of one being a flat call
/// and the other a nested `match`.
fn resolve_local_scratch_cwd(spawn_config: &SpawnConfig) -> Option<PathBuf> {
    let dir = spawn_config.local_scratch_dir()?;
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(
            scratch_dir = %dir.display(),
            "local scratch mkdir failed: {e}; falling back to TUI cwd",
        );
        return None;
    }
    Some(dir)
}

/// Sync the meta worker's polling target with the currently-focused
/// agent. Sends `set_target` for a local agent (one with a known cwd)
/// and `clear_target` for an SSH agent / failed agent / no agent.
/// Tracks the last-sent id in `last_sent` so we only push to the
/// worker when focus actually changes — same-agent re-focuses are
/// no-ops.
///
/// This sits at the top of every event-loop tick. Cheap (one
/// reference compare per frame; clones happen only on the focus-
/// change path) and keeps the worker control logic in one place
/// rather than scattered across every `change_focus` call site.
fn sync_meta_worker_target(
    worker: &AgentMetaWorker,
    nav: &NavState,
    last_sent: &mut Option<AgentId>,
) {
    // Yield references rather than clones — we'll only clone on the
    // SetTarget path that actually moves data into the worker.
    // Only locally-spawned agents are polled in v1: SSH agents have
    // a remote cwd that the worker can't read. Detect them by the
    // absence of `cwd` (set only by `spawn_local_agent`).
    let focused_local = nav
        .agents
        .get(nav.focused)
        .and_then(|a| a.cwd.as_ref().map(|cwd| (&a.id, cwd)));
    match (focused_local, last_sent.as_ref()) {
        (Some((id, _)), Some(prev)) if *id == *prev => {
            // Same focused agent. Worker still polling it; no clones,
            // no channel sends.
        }
        (Some((id, cwd)), _) => {
            worker.set_target(id.clone(), cwd.clone());
            *last_sent = Some(id.clone());
        }
        (None, Some(_)) => {
            worker.clear_target();
            *last_sent = None;
        }
        (None, None) => {}
    }
}

/// Apply a batch of `MetaEvent`s drained from the worker. Each event
/// names an `AgentId`; we resolve it to an index in `agents` before
/// mutating, so a focus change or reorder mid-poll can never
/// misroute an update onto the wrong agent.
fn apply_meta_events(agents: &mut [RuntimeAgent], events: Vec<MetaEvent>) {
    for ev in events {
        let target_id = match &ev {
            MetaEvent::Branch { agent_id, .. } | MetaEvent::Model { agent_id, .. } => agent_id,
        };
        let Some(agent) = agents.iter_mut().find(|a| &a.id == target_id) else {
            continue;
        };
        match ev {
            MetaEvent::Branch { value, .. } => agent.branch = value,
            MetaEvent::Model { value, .. } => agent.model_effort = value,
        }
    }
}

fn resize_agents(agents: &mut [RuntimeAgent], rows: u16, cols: u16) {
    for a in agents {
        // Stash the geometry on every agent — even Failed ones — so a
        // future resize-while-failed doesn't grow stale and so any
        // agent we promote to Ready in the future is sized at the
        // current terminal dimensions.
        a.rows = rows;
        a.cols = cols;
        match &mut a.state {
            AgentState::Ready { parser, transport } => {
                // PTY resize is best-effort: failure here means the child
                // sees a stale size until next resize, which is a harmless
                // cosmetic glitch (claude re-lays-out on the next paint
                // cycle). Surfacing as an error would force callers to
                // handle a non-actionable failure.
                let _ = transport.resize(rows, cols);
                parser.screen_mut().set_size(rows, cols);
            }
            AgentState::Crashed { parser, .. } => {
                // No transport to notify, but the parser screen still
                // needs to track the pane size so the last frame draws
                // correctly under the crash banner after a window resize.
                parser.screen_mut().set_size(rows, cols);
            }
            AgentState::Failed { .. } => {}
        }
    }
}

/// Cancel and remove the at-most-one [`PendingAttach`] currently owned
/// by the spawn modal. Used by both `Cancel` and `CancelBootstrap` so
/// dismissing the modal in the middle of an attach takes the worker
/// down with it. Other attaches in the Vec are left running — there
/// aren't any in the current flow, but the data shape supports it.
fn cancel_modal_owned_attach(attaches: &mut Vec<PendingAttach>) {
    let Some(idx) = attaches.iter().position(|a| a.modal_owner) else {
        return;
    };
    // `swap_remove` is fine: we don't care about ordering, just that
    // the slot is gone and its `Drop` (cancels the worker) fires.
    attaches.swap_remove(idx);
}

/// Move the agent at `from` to position `to`, sliding the agents in
/// between by one slot. Browser-tab semantics — drag tab 1 onto slot 4
/// inserts at slot 4 (it does NOT swap with slot 4). No-op if either
/// index is out of range or `from == to`.
///
/// Caller is responsible for re-deriving `focused` and `previous_focused`
/// via [`shift_index`] after the move so the same agent stays focused
/// (and the alt-tab buddy still points at the same agent) across the
/// reorder.
fn reorder_agents(agents: &mut Vec<RuntimeAgent>, from: usize, to: usize) {
    if from == to || from >= agents.len() || to >= agents.len() {
        return;
    }
    let agent = agents.remove(from);
    agents.insert(to, agent);
}

/// Compute the new index of an existing slot after a `remove(from) +
/// insert(to)` reorder. The four cases:
///
/// - `i == from`: this is the moved slot; it lands at `to`.
/// - moved right (`from < to`) and `from < i <= to`: each in-between
///   slot shifted left by one to fill the gap.
/// - moved left (`from > to`) and `to <= i < from`: each in-between
///   slot shifted right by one to make room.
/// - otherwise: untouched.
///
/// Applied to both `focused` and `previous_focused` so the same agent
/// remains focused (and the alt-tab buddy stays on the same agent)
/// across a reorder.
fn shift_index(i: usize, from: usize, to: usize) -> usize {
    if i == from {
        to
    } else if from < to && i > from && i <= to {
        i - 1
    } else if from > to && i >= to && i < from {
        i + 1
    } else {
        i
    }
}

/// Outcome of a left-button mouse event over the tab strip / nav rows.
/// The event loop translates each variant into the matching state
/// mutation; pulling the decision into a return value keeps the wiring
/// pure-functional and unit-testable without an event-loop harness.
/// Returned wrapped in `Option` so the dispatcher can signal "nothing
/// to do" (wheel, motion, drag, right/middle button, stray release)
/// without a dedicated `None` enum variant.
#[derive(Clone, Debug, Eq, PartialEq)]
enum TabMouseDispatch {
    /// Left press over a tab — the loop should record the agent id
    /// in `mouse_press` so the eventual release knows what was grabbed.
    PressTab(AgentId),
    /// Left release over the same tab the press grabbed: focus it.
    Click(AgentId),
    /// Left release over a different tab from the press: reorder by
    /// moving `from` to `to`, then re-derive `focused` /
    /// `previous_focused` via [`shift_index`]. Identities, not slot
    /// indices — the loop resolves both to current `Vec` positions
    /// at the moment of the mutation, so a reap or background reorder
    /// between Down and Up cancels gracefully instead of mis-targeting.
    Reorder { from: AgentId, to: AgentId },
    /// Left release outside any tab while a press was active: cancel
    /// the gesture (no focus, no reorder). The loop should clear
    /// `mouse_press`.
    Cancel,
}

/// Resolve a left-button mouse event against the recorded tab
/// hitboxes. Crossterm only fires `Drag` on motion, so a same-cell
/// down→up generates a clean `Down`/`Up` pair with no intervening
/// drag — the click and reorder gestures share this dispatcher.
///
/// `mouse_press` is the agent id grabbed on the most recent
/// `Down(Left)`, or `None` if the user isn't currently holding the
/// mouse over a tab. Storing the *id* (not coords or index) at press
/// time means a terminal resize, agent reap, or background reorder
/// between Down and Up still resolves the gesture to the same agent
/// — its hitbox may have moved cells, its slot may have shifted, but
/// the press's identity is preserved (and a reap turns into a clean
/// no-op at apply time when `position` returns `None`).
fn tab_mouse_dispatch(
    kind: MouseEventKind,
    column: u16,
    row: u16,
    hitboxes: &TabHitboxes,
    mouse_press: Option<&AgentId>,
) -> Option<TabMouseDispatch> {
    match kind {
        MouseEventKind::Down(MouseButton::Left) => {
            hitboxes.at(column, row).map(TabMouseDispatch::PressTab)
        }
        MouseEventKind::Up(MouseButton::Left) => match (mouse_press, hitboxes.at(column, row)) {
            (Some(from), Some(to)) if &to == from => Some(TabMouseDispatch::Click(to)),
            (Some(from), Some(to)) => Some(TabMouseDispatch::Reorder {
                from: from.clone(),
                to,
            }),
            (Some(_), None) => Some(TabMouseDispatch::Cancel),
            (None, _) => None,
        },
        // Drag and other kinds (motion, right/middle, side scroll):
        // no-op. Native copy-and-paste in iTerm2 / Alacritty / Ghostty
        // / WezTerm / Kitty requires holding Alt/Option to bypass
        // mouse capture — this is documented in the help screen.
        _ => None,
    }
}

/// Outcome of a left-button mouse event over the live agent pane.
/// Same wiring shape as [`TabMouseDispatch`]: pure-functional
/// translation, the loop owns the state mutation. Returned wrapped in
/// `Option` so the dispatcher can signal "not for me — let
/// `tab_mouse_dispatch` look at it" without a dedicated `Skip` variant.
#[derive(Clone, Eq, PartialEq, Debug)]
enum PaneMouseDispatch {
    /// Left press inside the pane: arm a fresh selection at the
    /// translated cell. Any prior selection is dropped (the loop
    /// overwrites unconditionally).
    Arm { agent: AgentId, cell: CellPos },
    /// Drag while a selection is active: extend the head to the new
    /// cell. The cell is already pane-clamped — drags continuing past
    /// the pane edge land on the nearest edge cell, so selecting "to
    /// the bottom" by overshooting still works.
    Extend(CellPos),
    /// Left release while a selection is active: extract text and
    /// write OSC 52, then clear. The loop performs the side effect
    /// because it owns `&nav.agents` (and thus the vt100 parser).
    Commit,
}

/// Resolve a left-button mouse event against the recorded pane hitbox.
///
/// Returns `None` for events that don't concern the pane: wheel (handled
/// in the wheel arm above), right / middle button, motion without drag,
/// or any event whose position is outside the recorded pane rect *and*
/// no active selection exists. When `None`, the loop falls through to
/// [`tab_mouse_dispatch`] so chrome clicks still work.
///
/// `Drag` and `Up` events outside the pane DO produce a dispatch when
/// a selection is active — drags get clamped (`PaneHitbox::clamped_cell_at`)
/// so the user can release outside the pane and still get the
/// selection they visibly drew.
fn pane_mouse_dispatch(
    kind: MouseEventKind,
    column: u16,
    row: u16,
    pane_hitbox: &PaneHitbox,
    selection: Option<&Selection>,
) -> Option<PaneMouseDispatch> {
    match kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let (agent, cell) = pane_hitbox.cell_at(column, row)?;
            Some(PaneMouseDispatch::Arm { agent, cell })
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            // Only relevant if a selection is in flight. Without an
            // anchor, a stray Drag (e.g. drag started in chrome) has
            // nothing to extend and we let it fall through.
            selection?;
            let cell = pane_hitbox.clamped_cell_at(column, row)?;
            Some(PaneMouseDispatch::Extend(cell))
        }
        MouseEventKind::Up(MouseButton::Left) => selection.map(|_| PaneMouseDispatch::Commit),
        // Right / middle buttons, side-scroll, motion-without-button:
        // no-op. Right-click context menu is a deliberate non-goal in
        // v1 (matches the existing AD-25 minimal-mouse posture).
        _ => None,
    }
}

/// Pull text from the focused agent's pane for the given selection
/// range and write it to the system clipboard via OSC 52.
///
/// Lookup is by stable [`AgentId`], not by index, so a tab switch /
/// agent reap between Down and Up cancels gracefully — `position`
/// returns `None` and we silently bail without writing.
///
/// Two extraction paths share a single OSC 52 write tail:
/// - `Ready` / `Crashed`: `vt100::Screen::contents_between` walks
///   `visible_rows()`, so the selection respects the parser's current
///   scrollback offset (selection while scrolled-back yields
///   scrollback text, not live text).
/// - `Failed`: [`failure_text_in_range`] mirrors the same cell-range
///   semantics over the centered failure layout. The pane area is
///   read off `pane_hitbox` because the Failed branch has no parser
///   that owns the dimensions.
///
/// Empty selections (zero-width or all-whitespace cells in trimmed
/// regions) produce empty strings; we skip the OSC 52 write in that
/// case to avoid clobbering whatever was on the clipboard before.
fn commit_selection(sel: &Selection, agents: &[RuntimeAgent], pane_hitbox: &PaneHitbox) {
    let Some(agent) = agents.iter().find(|a| a.id == sel.agent) else {
        return;
    };
    let (start, end) = normalized_range(sel.anchor, sel.head);
    let text = match &agent.state {
        AgentState::Ready { parser, .. } | AgentState::Crashed { parser, .. } => parser
            .screen()
            .contents_between(start.row, start.col, end.row, end.col.saturating_add(1)),
        AgentState::Failed { error } => {
            // Pane area is required to recompute the centered layout
            // — without it the cells the user clicked don't map back
            // to chars. Bail silently if the hitbox wasn't recorded
            // (e.g. layout had zero pane area this frame).
            let Some(area) = pane_hitbox.rect() else {
                return;
            };
            let host = agent.host.as_deref().unwrap_or("");
            failure_text_in_range(host, &error.user_message(), area, start, end)
        }
    };
    if text.is_empty() {
        return;
    }
    if let Err(err) = write_clipboard_to(&mut io::stdout(), &text) {
        tracing::debug!(?err, "OSC 52 clipboard write failed");
    }
}

/// Translate a screen-cell mouse coordinate into a [`HoverUrl`] when
/// the cell sits over a URL in the focused agent's pane. Used by both
/// the Ctrl-hover highlight (drives [`paint_hover_url_if_active`]) and
/// the Ctrl-click open dispatch (the URL string is what's handed to
/// `open` / `xdg-open`).
///
/// Returns `None` when:
/// - the click landed outside the recorded pane rect,
/// - the focused agent isn't `Ready` (no live parser to scan),
/// - or the cell isn't inside any URL on its row.
///
/// Single-row scope matches `find_url_at` — multi-row URLs are a
/// follow-up; in practice Claude doesn't wrap them.
fn compute_hover(
    pane_hitbox: &PaneHitbox,
    agents: &[RuntimeAgent],
    column: u16,
    row: u16,
) -> Option<HoverUrl> {
    let (agent_id, cell) = pane_hitbox.cell_at(column, row)?;
    let agent = agents.iter().find(|a| a.id == agent_id)?;
    let span = crate::url_scan::find_url_at(agent.state.screen()?, cell.row, cell.col)?;
    Some(HoverUrl {
        agent: agent_id,
        row: cell.row,
        cols: span.cols,
        url: span.url,
    })
}

/// Abstraction over the OS-level "open this URL" call. The event loop
/// holds a `&dyn UrlOpener` rather than calling the platform-specific
/// command directly — surfaced in arch review as a Dependency Inversion
/// concern about hardcoded `Command::new("open")` inside the runtime
/// crate. Production wires [`OsUrlOpener`]; tests can swap in a mock
/// that records the URLs without spawning a browser.
///
/// Returns `io::Result<()>` rather than swallowing internally so the
/// caller decides whether to log or surface the failure — the runtime
/// loop logs via `tracing::debug!` and continues; a future test could
/// assert on the error.
trait UrlOpener {
    fn open(&self, url: &str) -> io::Result<()>;
}

/// Production [`UrlOpener`]: spawns the host OS's URL handler.
///
/// macOS: `open <url>`. Other Unix: `xdg-open <url>`. Windows: `cmd /C
/// start "" <url>` (the empty quoted string is `start`'s "no window
/// title" sentinel; without it the URL would be parsed as the title).
///
/// Best-effort: spawn-only, no wait, no output capture. The URL is
/// passed as a single argv entry — never through a shell — so a
/// quote/space/`;` in the URL can't escape into the user's environment.
/// Same threat model as iTerm2's Cmd+Click.
struct OsUrlOpener;

impl UrlOpener for OsUrlOpener {
    fn open(&self, url: &str) -> io::Result<()> {
        use std::process::{Command, Stdio};
        let mut command = if cfg!(target_os = "macos") {
            let mut c = Command::new("open");
            c.arg(url);
            c
        } else if cfg!(target_os = "windows") {
            let mut c = Command::new("cmd");
            c.args(["/C", "start", "", url]);
            c
        } else {
            let mut c = Command::new("xdg-open");
            c.arg(url);
            c
        };
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(|_child| ())
    }
}

/// Tracks whether codemux has temporarily yielded mouse capture to the
/// host terminal, and the independent reasons that may demand a yield.
/// Capture is yielded iff at least one reason is active and reclaimed
/// only when every reason has cleared.
///
/// Yield reasons:
/// - **URL-modifier hold**: user is holding Cmd / Ctrl (configurable
///   via [`MouseUrlModifier`]) so the host terminal's native URL
///   hover/click UX can run. Driven by KKP bare-modifier press/release
///   events.
/// - **Focused-Failed pane**: the focused agent is in
///   [`AgentState::Failed`] and has no live PTY — yielding lets the
///   user use their terminal's native click-drag-copy on the error
///   text as a fallback for terminals where OSC 52 (the in-app
///   selection's clipboard write path) is not honored.
///
/// Why this is needed at all: any DEC mouse capture mode silences
/// Ghostty's URL hover detector (verified empirically — see the test
/// trail in commit history). The SGR mouse encoding cannot deliver
/// Super/Cmd, so the only path to native Cmd-click is to step out of
/// capture entirely while the modifier is held. The Failed-pane reason
/// extends the same machinery to the case where the in-app text-grid
/// has no rows the user might want to copy.
///
/// Modeled as a `mode` + `reasons` pair (rather than a flat enum) so
/// the two reasons stay independent: a mid-hold focus change to a
/// non-Failed agent must NOT clobber the URL-modifier reason. Idempotent
/// on every setter — repeated calls (sticky modifier, redundant focus
/// syncs from per-iteration recomputation) don't over-emit escape
/// sequences. Terminals that refused our initial `EnableMouseCapture`
/// stay in [`CaptureMode::Disabled`] forever and every setter is a
/// pure no-op.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MouseCaptureState {
    mode: CaptureMode,
    reasons: YieldReasons,
}

/// What the host terminal's mouse-capture mode is currently set to.
/// Distinguishes "we never had capture" from "capture is held" from
/// "capture was held and we've since yielded" — the renderer never
/// looks at this; only the per-iteration sync compares it against
/// `reasons.any()` to decide whether to flip.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CaptureMode {
    /// Initial `EnableMouseCapture` failed or was never attempted.
    /// Yield/reclaim are no-ops; the host terminal already owns mouse
    /// events.
    Disabled,
    /// Capture is active — codemux is receiving mouse events via the
    /// SGR mouse encoding and the host terminal's native URL handler
    /// is silenced.
    Captured,
    /// Capture was temporarily released. Mouse events go to the host
    /// terminal so its native UX (URL hover, click-drag selection) can
    /// run; reclaimed when the last yield reason clears.
    Yielded,
}

/// The set of independent reasons that demand a mouse-capture yield.
/// Capture is yielded iff [`Self::any`] is true. Setters in
/// [`MouseCaptureState`] update one bit at a time so a focus change
/// during a modifier hold leaves the modifier bit intact (and vice
/// versa).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct YieldReasons {
    /// User is holding the configured URL modifier (Cmd / Ctrl /
    /// Alt / Shift per [`MouseUrlModifier`]). Driven by KKP bare-
    /// modifier press/release events; cleared on terminal focus loss
    /// because the OS swallows the release when the window changes.
    url_modifier_held: bool,
    /// Focused agent is in `Failed` state. Driven by a per-iteration
    /// sync against `nav.focused`; not cleared on focus loss because
    /// alt-tabbing back to a Failed pane should re-yield without the
    /// user needing to click into the pane first.
    focused_failed: bool,
}

impl YieldReasons {
    fn any(self) -> bool {
        self.url_modifier_held || self.focused_failed
    }
}

impl MouseCaptureState {
    fn new(mode_active: bool) -> Self {
        Self {
            mode: if mode_active {
                CaptureMode::Captured
            } else {
                CaptureMode::Disabled
            },
            reasons: YieldReasons::default(),
        }
    }

    /// Update the URL-modifier reason and re-sync the OS mode. Called
    /// from the bare-modifier press/release arms of the input loop.
    fn set_url_modifier_held(&mut self, held: bool) {
        if self.reasons.url_modifier_held == held {
            return;
        }
        self.reasons.url_modifier_held = held;
        self.sync();
    }

    /// Update the focused-Failed reason and re-sync the OS mode. The
    /// per-iteration sync in `event_loop` calls this with the current
    /// focused agent's state; idempotent so a stable focused-Failed
    /// across many iterations doesn't churn.
    fn set_focused_failed(&mut self, failed: bool) {
        if self.reasons.focused_failed == failed {
            return;
        }
        self.reasons.focused_failed = failed;
        self.sync();
    }

    /// Terminal focus loss: clear the URL-modifier bit (the OS won't
    /// deliver the release when the window changes) and re-sync. The
    /// focused-Failed bit is left alone — the focused agent didn't
    /// move, only the host window's keyboard focus did, and re-entering
    /// the codemux window with focus on a Failed pane should still
    /// land in the yielded state.
    fn lose_focus(&mut self) {
        self.set_url_modifier_held(false);
    }

    /// Reconcile [`Self::mode`] with [`Self::reasons`]: yield if any
    /// reason is active and we're currently `Captured`; reclaim if no
    /// reason is active and we're currently `Yielded`. `Disabled` is a
    /// no-op (terminal never gave us capture, so we have nothing to
    /// flip). Failure to write the escape sequence leaves `mode`
    /// unchanged so the next sync retries.
    fn sync(&mut self) {
        match (self.mode, self.reasons.any()) {
            (CaptureMode::Captured, true) => {
                if execute!(io::stdout(), DisableMouseCapture).is_ok() {
                    self.mode = CaptureMode::Yielded;
                }
            }
            (CaptureMode::Yielded, false) => {
                if execute!(io::stdout(), EnableMouseCapture).is_ok() {
                    self.mode = CaptureMode::Captured;
                }
            }
            // (Captured, false) — already in the right state.
            // (Yielded, true) — already yielded for at least one reason.
            // (Disabled, _) — terminal refused capture, nothing to do.
            _ => {}
        }
    }
}

/// True when `mk` is the modifier key the user has chosen to drive the
/// URL-yield. Both left- and right-side variants count, so the user can
/// hold either physical key. `Cmd` matches Super (macOS Command) and
/// Meta (some X11 keymaps map the Meta key to the Command key);
/// platforms differ enough here that being permissive is safer than
/// strict.
fn matches_url_modifier(mk: ModifierKeyCode, url_mod: MouseUrlModifier) -> bool {
    match url_mod {
        MouseUrlModifier::None => false,
        MouseUrlModifier::Cmd => matches!(
            mk,
            ModifierKeyCode::LeftSuper
                | ModifierKeyCode::RightSuper
                | ModifierKeyCode::LeftMeta
                | ModifierKeyCode::RightMeta
        ),
        MouseUrlModifier::Ctrl => matches!(
            mk,
            ModifierKeyCode::LeftControl | ModifierKeyCode::RightControl
        ),
        MouseUrlModifier::Alt => {
            matches!(mk, ModifierKeyCode::LeftAlt | ModifierKeyCode::RightAlt)
        }
        MouseUrlModifier::Shift => {
            matches!(mk, ModifierKeyCode::LeftShift | ModifierKeyCode::RightShift)
        }
    }
}

/// terminal (iTerm2 / Ghostty / Kitty / `WezTerm` / Alacritty / tmux
/// passthrough) decodes the base64 and writes to the system clipboard.
///
/// Apple Terminal does not implement OSC 52; this is a documented
/// non-goal consistent with the existing AD-25 mouse-capture story.
/// Best-effort: the caller logs at debug on failure and otherwise
/// ignores it — losing a clipboard write is a UX wart, not a crash
/// surface.
///
/// Lifted to take `&mut impl Write` so unit tests can capture the
/// emitted bytes without touching `io::stdout()`.
fn write_clipboard_to<W: io::Write>(out: &mut W, text: &str) -> io::Result<()> {
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    let payload = STANDARD.encode(text.as_bytes());
    write!(out, "\x1b]52;c;{payload}\x07")?;
    out.flush()
}

/// True when no overlay is open: spawn modal closed, agent-switcher
/// popup closed, help screen closed. Mouse-wheel events that should
/// scroll the focused agent are gated on this — wheel-while-popup-up
/// would otherwise scroll the agent buried beneath the popup, which is
/// confusing and undocumented behavior.
fn no_overlay_active(
    spawn_ui: Option<&SpawnMinibuffer>,
    popup_state: PopupState,
    help_state: HelpState,
) -> bool {
    spawn_ui.is_none()
        && matches!(popup_state, PopupState::Closed)
        && matches!(help_state, HelpState::Closed)
}

/// Static-for-the-duration-of-the-event-loop knobs and styling
/// references. Bundled into one parameter to keep `event_loop`'s
/// signature manageable as more presentation / behavior knobs land —
/// a previous `event_loop` carrying nine positional arguments was
/// flagged as a Data Clump in code review.
///
/// All fields are `Copy`, so the body can destructure into named
/// locals once at the top and continue using bare names (no
/// per-site `ctx.` rewrites). Lifetime `'a` is the borrow scope of
/// the surrounding `Config` — `RuntimeContext` does not own anything
/// it points to.
#[derive(Clone, Copy)]
struct RuntimeContext<'a> {
    /// User key bindings (prefix + on-prefix + on-modal tables).
    bindings: &'a Bindings,
    /// Pre-computed chrome styles + per-host accent map.
    chrome: &'a ChromeStyle,
    /// Spawn-modal config (search roots, named projects, project
    /// markers, per-host SSH search-roots).
    spawn_config: &'a SpawnConfig,
    /// Per-agent vt100 scrollback budget, in rows.
    scrollback_len: usize,
    /// When true, the event loop emits a host-terminal BEL on every
    /// agent's working → idle transition. See `[ui]
    /// host_bell_on_finish` in `Ui` for the user-facing knob.
    host_bell_on_finish: bool,
    /// True iff the initial `EnableMouseCapture` succeeded. The yield/
    /// reclaim state machine uses this so it doesn't try to disable
    /// capture that was never enabled.
    mouse_captured: bool,
    /// Modifier key the user holds to make codemux yield mouse capture
    /// for the host's native URL hover/click UX. See [`MouseUrlModifier`].
    mouse_url_modifier: MouseUrlModifier,
    /// When `true`, codemux yields mouse capture whenever the focused
    /// agent is in `Failed` state — opt-in trade where the user gets the
    /// host terminal's native I-beam cursor and click-drag-copy on the
    /// failure pane in exchange for losing tab clicks / scroll wheel /
    /// the in-app drag-to-select overlay while focus stays there. Default
    /// `false`: in-app selection covers Failed panes via `commit_selection`'s
    /// Failed branch, so this knob is for users who specifically prefer
    /// the native gesture and accept the keyboard-only tab switching
    /// while on a Failed pane.
    mouse_yield_on_failed: bool,
    /// Status-bar right-side segments, built from
    /// `config.ui.status_bar_segments`. Owned by the caller of
    /// `event_loop`; threaded through to `render_status_bar` so
    /// segments stay in user-defined order across frames.
    segments: &'a [Box<dyn StatusSegment>],
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    agents: Vec<RuntimeAgent>,
    mut nav_style: NavStyle,
    log_tail: Option<&LogTail>,
    initial_cwd: &Path,
    ctx: &RuntimeContext<'_>,
) -> Result<()> {
    // Long, but it is the central event loop and breaks naturally into
    // sequential phases (drain / reap / render / dispatch). Pulling each
    // arm into its own helper would require threading >5 mutable references
    // through the helper and gain little.
    #![allow(clippy::too_many_lines)]
    // Destructure into bare locals so the body — which accesses each
    // of these in dozens of places — keeps reading naturally instead
    // of carrying `ctx.` everywhere. All fields are `Copy`.
    let RuntimeContext {
        bindings,
        chrome,
        spawn_config,
        scrollback_len,
        host_bell_on_finish,
        mouse_captured,
        mouse_url_modifier,
        mouse_yield_on_failed,
        segments,
    } = *ctx;
    let mut prefix_state = PrefixState::default();
    let mut help_state = HelpState::default();
    let mut spawn_ui: Option<SpawnMinibuffer> = None;
    // In-flight SSH bootstrap state. The modal owns the per-stage UX
    // via `lock_for_bootstrap` / `set_bootstrap_stage`, but the worker
    // handles + the live `RemoteFs` ControlMaster live here so their
    // `Drop` semantics are tied to the runtime's exit (or to an
    // explicit cancel/finish event), not to the modal's open/close
    // lifecycle.
    let mut prepare: Option<PendingPrepare> = None;
    let mut attaches: Vec<PendingAttach> = Vec::new();
    // Per-host fuzzy directory index. The manager owns the catalog,
    // disk hydration, walker spawns, drain loop, and disk-save
    // dispatch — runtime forwards lifecycle requests
    // (request_local_swr / request_remote_swr / force_rebuild_*)
    // and queries `state_for(host)` to drive the modal's render.
    let mut index_mgr = IndexManager::new();
    // Always kick off the local index in the background, regardless
    // of `default_mode`. Indexing is cheap (background thread,
    // ignore-aware walker) and starting unconditionally means a
    // mid-session toggle from Precise to Fuzzy lands on a populated
    // index instead of a cold "indexing…" sentinel. The `Building`
    // state grows its searchable `dirs` list incrementally so the
    // first user query can score against the partial index even on
    // first run.
    {
        let outcome =
            index_mgr.request_local_swr(&spawn_config.search_roots, &spawn_config.project_markers);
        tracing::debug!(?outcome, "fuzzy index: build started at session start");
    }
    // Per-focused-agent meta worker: reads `~/.claude/settings.json`
    // for `model`+`effortLevel` and re-reads `<cwd>/.git/HEAD` for
    // `branch`, then posts MetaEvents back to the runtime which
    // caches the values on RuntimeAgent.{model,effort,branch} for
    // the status bar to render. Single thread, focused-agent only —
    // see [`agent_meta_worker`].
    let meta_worker = AgentMetaWorker::start();
    // Background fuzzy scoring for the spawn modal. Declared after
    // `meta_worker` so reverse-declaration drop order tears it down
    // first — neither worker depends on the other, but keeping a
    // consistent shutdown order makes the trace logs predictable.
    // The runtime memoizes via `last_pushed_index_gen` /
    // `last_pushed_query` so the worker only sees one dispatch per
    // distinct (host, gen) and (host, query) — typing fast collapses
    // to the latest state inside the worker's drain loop.
    let fuzzy_worker = FuzzyWorker::start();
    let mut last_pushed_index_gen: HashMap<String, u64> = HashMap::new();
    let mut last_pushed_query: HashMap<String, String> = HashMap::new();
    let initial_count = agents.len();
    // Bundle the four navigation locals (`agents`, `focused`,
    // `previous_focused`, `popup_state`) into a single owned struct.
    // Pre-`NavState` they were four separate `let mut` locals here
    // and the helpers (`dismiss_focused`, `change_focus`, etc.) had
    // to take a parallel cluster of `&mut` refs — flagged in
    // architecture review as a data clump. The methods now own their
    // mutation invariants behind `&mut self`.
    let mut nav = NavState::new(agents);
    // Per-frame click hitboxes for the tab strip / nav rows. Populated
    // by the leaf renderers, consumed by the mouse handler. Cleared at
    // the top of every `render_frame` so a stale frame's geometry can
    // never bleed into a fresh event hit-test.
    let mut tab_hitboxes = TabHitboxes::default();
    // The currently-painted agent pane, recorded by render_agent_pane
    // and consumed by the mouse handler when arming / extending a
    // drag-to-select gesture. Like `tab_hitboxes`, cleared at the top
    // of every render_frame so a stale rect can never satisfy a
    // hit-test against a layout that no longer exists.
    let mut pane_hitbox = PaneHitbox::default();
    // Tab grabbed on `MouseEventKind::Down(Left)` — by stable
    // `AgentId`, not by index, so a reap or background reorder between
    // Down and Up still resolves to the same agent (or returns `None`
    // and the gesture cancels gracefully).
    let mut mouse_press: Option<AgentId> = None;
    // Active drag-to-select gesture inside an agent pane (separate from
    // `mouse_press`, which is for the tab strip). Anchored to a stable
    // AgentId so a tab switch / agent reap mid-gesture cancels cleanly
    // — the lookup at commit time will return `None` and we just clear
    // without writing to the clipboard. See AD-25's selection follow-up.
    let mut selection: Option<Selection> = None;
    // URL the user is currently hovering with Ctrl held. Drives the
    // underline overlay (in the renderer) and Ctrl+Click open dispatch
    // (in the mouse handler). Cleared on resize, focused-agent change,
    // and any motion event that arrives without Ctrl held.
    let mut hover: Option<HoverUrl> = None;
    // Tracks whether mouse capture has been temporarily yielded to the
    // host terminal so the user's URL modifier can drive Ghostty's
    // (or iTerm2's, Kitty's, ...) native URL hover/click handler. See
    // the type doc for why this exists at all.
    let mut mouse_capture_state = MouseCaptureState::new(mouse_captured);
    // Production URL opener: spawns `open` / `xdg-open` / `cmd start`.
    // Held behind the [`UrlOpener`] trait so a future test can swap in
    // a recording mock without spawning a real browser.
    let url_opener: &dyn UrlOpener = &OsUrlOpener;
    let mut spawn_counter: usize = initial_count;
    // PID of this codemux invocation, used to namespace SSH daemon
    // agent_ids so a relaunch can never collide with a still-running
    // remote daemon from a previous launch. Without this, the SSH
    // bootstrap re-attaches to the surviving daemon and replays its
    // captured PTY snapshot — the user's last Claude session "kinda"
    // resumes when they wanted a fresh one. `setsid -f codemuxd`
    // outlives the SSH session by design (session continuity is the
    // daemon's whole point), so the namespace has to come from the
    // client side. Session restoring will be a separate explicit flow.
    let tui_pid = std::process::id();
    // Captured once at loop entry so per-tab spinner / blink phases
    // are derived from a stable monotonic origin. The 50 ms event
    // poll below already redraws on each tick when nothing else
    // happens, so the wall-clock derivation is enough — no extra
    // wakeup machinery needed.
    let start = Instant::now();
    // Last title we emitted to the surrounding terminal emulator via
    // OSC 0. The render tick computes the desired title from the
    // focused agent and only emits when it actually changed — so a
    // working spinner ticking through Braille frames inside the agent
    // (which the title parser strips out before storage) doesn't
    // produce a per-frame escape stream.
    let mut last_emitted_host_title: Option<String> = None;
    // Track the agent the meta-worker is currently polling so we only
    // send a control message when focus actually changes (the worker
    // dedupes internally too, but a per-frame send floods the channel
    // and adds noise to the trace logs). `None` represents "worker
    // has been told to clear" — distinct from "we haven't told it
    // anything yet" which we model by initialising to `None` on
    // start and unconditionally pushing the initial focus below.
    let mut meta_worker_target: Option<AgentId> = None;
    sync_meta_worker_target(&meta_worker, &nav, &mut meta_worker_target);

    loop {
        // Drain in-flight index events for every host (local + each
        // SSH host the user has spawned to), update states, and
        // dispatch any completed walks to a detached disk-save
        // thread. The manager owns this whole pipeline; the runtime
        // just yields control once per frame.
        index_mgr.tick();
        // Apply any model/branch updates the meta worker queued and
        // make sure it's polling the currently-focused agent. Both
        // are no-ops when nothing changed (worker idle / focus
        // stable), so the per-frame cost is a `try_recv` and a
        // single comparison.
        apply_meta_events(&mut nav.agents, meta_worker.drain());
        sync_meta_worker_target(&meta_worker, &nav, &mut meta_worker_target);
        // Drain background fuzzy results into the modal and dispatch
        // any new SetIndex / Query messages — see `tick_fuzzy_dispatch`
        // for the memoization and ordering rules. Cheap when the
        // modal is closed or not in fuzzy mode.
        tick_fuzzy_dispatch(
            spawn_ui.as_mut(),
            &fuzzy_worker,
            &index_mgr,
            &mut last_pushed_index_gen,
            &mut last_pushed_query,
        );

        // Drain prepare events first: the modal should reflect the
        // worker's progress on the same frame the events arrive,
        // before any keystroke handling. On `Done` we either unlock
        // the modal for a remote-folder pick (success), auto-spawn
        // when a project alias stashed a path, or unlock back to the
        // host zone with the error visible (failure). Extracted into
        // a free fn so the SSH state machine can be exercised by unit
        // tests without driving the full event loop.
        let pty_geom_for_drain = {
            let (term_cols, term_rows) =
                crossterm::terminal::size().wrap_err("read terminal size")?;
            pty_size_for(nav_style, term_rows, term_cols, log_tail.is_some())
        };
        drain_prepare_events(PrepareDrainCtx {
            prepare: &mut prepare,
            spawn_ui: &mut spawn_ui,
            attaches: &mut attaches,
            index_mgr: &mut index_mgr,
            spawn_counter: &mut spawn_counter,
            spawn_config,
            pty_geom: pty_geom_for_drain,
            tui_pid,
            attach_factory: start_attach,
        });

        // Drain attach events. Each pending attach has its own
        // handle; we batch the ready set, apply transitions outside
        // the loop so we can mutate `nav.agents`, `attaches`, and
        // `spawn_ui` without borrow conflicts.
        let mut finished_attaches: Vec<usize> = Vec::new();
        let mut new_agents: Vec<RuntimeAgent> = Vec::new();
        let mut focus_new = false;
        let mut close_modal = false;
        for (idx, attach) in attaches.iter_mut().enumerate() {
            let mut completion: Option<Result<AgentTransport, codemuxd_bootstrap::Error>> = None;
            while let Some(event) = attach.handle.try_recv() {
                match event {
                    AttachEvent::Stage(stage) => {
                        if attach.modal_owner
                            && let Some(ui) = spawn_ui.as_mut()
                        {
                            ui.set_bootstrap_stage(stage);
                        }
                    }
                    AttachEvent::Done(result) => {
                        completion = Some(result);
                        break;
                    }
                }
            }
            if let Some(result) = completion {
                match result {
                    Ok(mut transport) => {
                        // Geometry may have changed during the attach;
                        // the wire `Hello` was sized when the attach
                        // started, so the remote daemon may need an
                        // immediate Resize before any frames flow.
                        let _ = transport.resize(attach.rows, attach.cols);
                        tracing::info!(label = %attach.label, "attach completed; transport ready");
                        new_agents.push(RuntimeAgent::ready(
                            attach.agent_id.clone(),
                            attach.label.clone(),
                            attach.repo.clone(),
                            // SSH agents have a remote cwd; `cwd: PathBuf`
                            // is for local-only operations (git HEAD
                            // reads). Pass None — the meta-worker
                            // skips SSH agents in v1.
                            None,
                            Some(attach.host.clone()),
                            transport,
                            attach.rows,
                            attach.cols,
                            scrollback_len,
                        ));
                    }
                    Err(e) => {
                        tracing::error!(label = %attach.label, "attach failed: {e}");
                        new_agents.push(RuntimeAgent::failed(
                            attach.agent_id.clone(),
                            attach.label.clone(),
                            attach.repo.clone(),
                            None,
                            attach.host.clone(),
                            e,
                            attach.rows,
                            attach.cols,
                        ));
                    }
                }
                if attach.modal_owner {
                    close_modal = true;
                }
                focus_new = true;
                finished_attaches.push(idx);
            }
        }
        if close_modal {
            spawn_ui = None;
            // Drop any prepare slot left over from the prepare→attach
            // transition (its `RemoteFs` was already moved out at
            // attach time, but the slot itself still owns
            // `prepared`).
            prepare = None;
        }
        if !finished_attaches.is_empty() {
            // Remove from highest index down so earlier indices stay
            // valid as we splice.
            for &idx in finished_attaches.iter().rev() {
                attaches.swap_remove(idx);
            }
        }
        let new_count = new_agents.len();
        nav.agents.extend(new_agents);
        if focus_new && new_count > 0 {
            let target = nav.agents.len() - 1;
            nav.change_focus(target);
        }

        for agent in &mut nav.agents {
            match &mut agent.state {
                AgentState::Ready { parser, transport } => {
                    for bytes in transport.try_read() {
                        parser.process(&bytes);
                    }
                }
                // No transport on Failed/Crashed — nothing to drain.
                // The Crashed parser still holds the last frame, so the
                // renderer keeps drawing it; we just don't feed it any
                // new bytes.
                AgentState::Failed { .. } | AgentState::Crashed { .. } => {}
            }
        }

        let transitions = nav.peek_finish_transitions();
        nav.apply_finish_transitions(&transitions);
        if transitions.any() && host_bell_on_finish {
            // BEL on any working → idle transition. The host terminal
            // gates the visual treatment on its own focus state, so
            // this is silent while the user is inside codemux and
            // surfaces only when they're in another window or app.
            // Best-effort: a write failure is logged and dropped, not
            // surfaced — losing one attention cue is preferable to
            // taking down the runtime over a stdout hiccup.
            if let Err(err) = host_title::write_bell(&mut io::stdout()) {
                tracing::debug!(?err, "host terminal bell write failed");
            }
        }

        nav.reap_dead_transports();
        if nav.agents.is_empty() {
            // Reap may have shrunk the Vec via the clean-exit path.
            // The last tab going away (claude `/quit`'d) is the
            // primary clean-exit-out-of-the-TUI path; manual dismiss
            // of the last terminal-state agent goes through this
            // same return.
            return Ok(());
        }
        // No post-reap focus / popup clamping here — `remove_at` (called
        // by both `reap_dead_transports`'s clean-exit path and
        // `dismiss_focused`) owns those invariants.

        let phase = AnimationPhase::from_elapsed(start.elapsed());
        // Sync selection lifecycle: a selection only makes sense for
        // the agent currently focused. Any path that changed focus or
        // reaped agents (key dispatch, attach completion, dismiss,
        // popup pick) lands here, so this single check supersedes
        // per-arm `selection = None` resets.
        if let Some(sel) = selection.as_ref()
            && nav
                .agents
                .get(nav.focused)
                .is_none_or(|a| a.id != sel.agent)
        {
            selection = None;
        }
        // Sync the focused-Failed mouse-capture yield. Off by default
        // because yielding capture also drops tab clicks, scroll-wheel,
        // and the in-app drag-to-select overlay for as long as focus
        // stays on the Failed pane — which most users don't want, since
        // in-app selection (reverse-video highlight + OSC 52 to clipboard)
        // already covers Failed panes via `commit_selection`'s Failed
        // branch. Opt-in via `mouse_yield_on_failed = true` for users
        // who specifically prefer the host terminal's native I-beam
        // cursor and click-drag-copy gesture and accept switching tabs
        // via the keyboard chord while focused on a Failed pane.
        //
        // The setter is always called with the AND of the two conditions
        // rather than wrapped in an `if mouse_yield_on_failed { ... }` so
        // a hypothetical hot-reload of the config from true → false
        // clears the bit instead of leaving it stuck — keeps the state
        // machine the authoritative source of truth.
        let focused_failed = nav
            .agents
            .get(nav.focused)
            .is_some_and(|a| matches!(a.state, AgentState::Failed { .. }));
        mouse_capture_state.set_focused_failed(focused_failed && mouse_yield_on_failed);
        // Same staleness guard for the Ctrl-hover URL highlight: if the
        // focused agent went away (reap, popup pick, dismiss), the cell
        // coordinates point at an agent that no longer renders here.
        if let Some(h) = hover.as_ref()
            && nav.agents.get(nav.focused).is_none_or(|a| a.id != h.agent)
        {
            let transition = update_hover(&mut hover, None);
            if let Err(err) = apply_hover_cursor(&mut io::stdout(), transition) {
                tracing::debug!(?err, "hover cursor update failed");
            }
        }
        // Push the focused agent's title out to the surrounding
        // terminal so Ghostty / iTerm2 / Kitty can label its codemux
        // tab with what's actually on screen. Debounced against the
        // last-emitted value: idle agents emit only when the body
        // changes, while a working agent's title carries the current
        // spinner frame so the dedup naturally lets one OSC through
        // per ~100 ms tick — Ghostty / Kitty / WezTerm spin smoothly
        // in their tab bar; iTerm2 / Terminal.app throttle title
        // writes and look choppier but still readable. Done before
        // `terminal.draw` so the OSC lands in the same byte stream as
        // the next frame and there's no inter-buffer flush race.
        let desired_host_title = host_terminal_title_for_focused(&nav, phase);
        if desired_host_title != last_emitted_host_title {
            if let Some(title) = desired_host_title.as_deref()
                && let Err(err) = host_title::write_set_title(&mut io::stdout(), title)
            {
                tracing::debug!(?err, "host terminal title write failed");
            }
            last_emitted_host_title = desired_host_title;
        }
        // Look up the index for whichever host the modal is
        // targeting. Lifted out of the closure so the borrow ends
        // before `terminal.draw`'s callback re-borrows the world.
        let modal_index_state = spawn_ui
            .as_ref()
            .map(SpawnMinibuffer::active_host_key)
            .and_then(|k| index_mgr.state_for(k));
        terminal
            .draw(|frame| {
                render_frame(
                    frame,
                    &nav.agents,
                    nav.focused,
                    nav_style,
                    nav.popup_state,
                    help_state,
                    spawn_ui.as_ref(),
                    bindings,
                    prefix_state,
                    log_tail,
                    phase,
                    chrome,
                    &mut tab_hitboxes,
                    &mut pane_hitbox,
                    PaneOverlay {
                        selection: selection.as_ref(),
                        hover: hover.as_ref(),
                    },
                    modal_index_state,
                    segments,
                );
            })
            .wrap_err("draw frame")?;

        // OSC 8 hyperlink wrap, post-draw — see `paint_hyperlinks_post_draw`'s
        // doc comment for why this can't live inside the closure. Gated on:
        // - no overlay active (spawn modal / help / popup repaint the agent
        //   pane area in the ratatui buffer; re-emitting URL chars from
        //   the agent's PTY screen would overwrite the modal text).
        // - no in-app `hover` active. The Ctrl+hover overlay paints cyan +
        //   underline directly into the ratatui buffer; re-emitting from
        //   vt100 would clobber that styling because the post-draw walk
        //   reads colors from the PTY screen, not from ratatui's buffer.
        //   When `hover` clears, the next frame re-tags all URL cells so
        //   the host terminal's hyperlink state stays current.
        if no_overlay_active(spawn_ui.as_ref(), nav.popup_state, help_state)
            && hover.is_none()
            && let Some(rect) = pane_hitbox.rect()
            && let Some(agent) = nav.agents.get(nav.focused)
            && let Some(screen) = agent.state.screen()
        {
            let mut stdout = io::stdout().lock();
            if let Err(err) = paint_hyperlinks_post_draw(&mut stdout, rect, screen) {
                tracing::debug!(?err, "OSC 8 hyperlink paint failed");
            } else if let Err(err) = stdout.flush() {
                tracing::debug!(?err, "OSC 8 hyperlink flush failed");
            }
        }

        if !event::poll(FRAME_POLL).wrap_err("poll for input")? {
            continue;
        }

        match event::read().wrap_err("read input")? {
            // Bare-modifier press/release (e.g. Cmd alone) drives the
            // URL-yield state machine: while held, codemux releases mouse
            // capture so the host terminal can run its native URL UX. The
            // KKP `REPORT_ALL_KEYS_AS_ESCAPE_CODES` + `REPORT_EVENT_TYPES`
            // flags push these events; without those flags this arm never
            // fires (terminals that don't support KKP also fall through
            // here and the in-app Ctrl+click handler stays the path).
            // Always swallowed — Claude has no use for bare modifier
            // events and forwarding them would just leak escape bytes.
            Event::Key(key) if matches!(key.code, KeyCode::Modifier(_)) => {
                if let KeyCode::Modifier(mk) = key.code
                    && matches_url_modifier(mk, mouse_url_modifier)
                {
                    tracing::debug!(?mk, kind = ?key.kind, "url-modifier event");
                    match key.kind {
                        KeyEventKind::Press => mouse_capture_state.set_url_modifier_held(true),
                        KeyEventKind::Release => mouse_capture_state.set_url_modifier_held(false),
                        KeyEventKind::Repeat => {}
                    }
                }
            }
            // User alt-tabbed away: clear the URL-modifier yield reason
            // so we can reclaim if it was the only reason in play.
            // Without this, a focus-loss mid-hold would leave codemux in
            // the yielded state until some unrelated event coincidentally
            // sees the modifier released — the user would notice that
            // wheel/tabs stopped responding for the duration. The
            // focused-Failed reason is left intact so re-entering the
            // window with focus still on a Failed pane stays yielded.
            // `Event::FocusGained` needs no explicit handler: capture is
            // already in whatever state we left it and falls through the
            // catch-all arm at the bottom.
            Event::FocusLost => {
                mouse_capture_state.lose_focus();
                let transition = update_hover(&mut hover, None);
                if let Err(err) = apply_hover_cursor(&mut io::stdout(), transition) {
                    tracing::debug!(?err, "hover cursor update failed");
                }
            }
            // Press OR Repeat: a held character key sends Repeat events
            // under KKP `REPORT_EVENT_TYPES`. Treat them the same as
            // Press for forwarding so the user can hold a key to repeat
            // it (e.g. holding Backspace to delete-line). Release events
            // for non-modifier keys are swallowed by the catch-all.
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                // Help screen takes the highest priority: any key closes it
                // (including the prefix key, which is friendly when the user
                // opened help by accident).
                if matches!(help_state, HelpState::Open) {
                    help_state = HelpState::Closed;
                    continue;
                }

                if let Some(ui) = spawn_ui.as_mut() {
                    // Construct the per-keystroke `DirLister`: Remote
                    // when there's a live `RemoteFs` (prepare done,
                    // user is path-picking), Local otherwise. The
                    // runner is zero-sized so allocating it inline
                    // every tick is free.
                    let runner = RealRunner;
                    let mut lister = match prepare.as_ref().and_then(|p| p.remote_fs.as_ref()) {
                        Some(fs) => DirLister::Remote {
                            fs,
                            runner: &runner,
                        },
                        None => DirLister::Local,
                    };
                    match ui.handle(&key, &bindings.on_modal, &mut lister) {
                        ModalOutcome::None => {}
                        ModalOutcome::RefreshIndex => {
                            // Force-rebuild for the *active* host
                            // (local or current SSH host). The
                            // catalog preserves any cached results
                            // as `Refreshing { dirs, .. }` so the
                            // wildmenu doesn't go blank during the
                            // user-triggered rebuild.
                            let key = ui.active_host_key().to_string();
                            if key == HOST_PLACEHOLDER {
                                index_mgr.force_rebuild_local(
                                    &spawn_config.search_roots,
                                    &spawn_config.project_markers,
                                );
                            } else if let Some(fs) =
                                prepare.as_ref().and_then(|p| p.remote_fs.as_ref())
                            {
                                let host_roots = spawn_config.ssh_search_roots(&key);
                                let remote_home = prepare
                                    .as_ref()
                                    .and_then(|p| p.prepared.as_ref())
                                    .map_or_else(
                                        || PathBuf::from("/"),
                                        |ph| ph.remote_home.clone(),
                                    );
                                index_mgr.force_rebuild_remote(
                                    &key,
                                    fs.socket_path(),
                                    &remote_home,
                                    &host_roots,
                                    &spawn_config.project_markers,
                                );
                            } else {
                                tracing::debug!(
                                    host = %key,
                                    "RefreshIndex: no live RemoteFs for host; skipping rebuild",
                                );
                            }
                            tracing::debug!("fuzzy index: rebuild triggered by user");
                        }
                        ModalOutcome::Cancel => {
                            // Esc when not locked: dismiss the modal
                            // and tear down any in-flight prepare /
                            // modal-owned attach. We deliberately
                            // mirror CancelBootstrap's cleanup so a
                            // user who esc'd at the host-zone never
                            // ends up with an orphan worker.
                            spawn_ui = None;
                            prepare = None;
                            cancel_modal_owned_attach(&mut attaches);
                        }
                        ModalOutcome::PrepareHost { host } => {
                            // Replacing an existing prepare slot
                            // cancels the prior worker via Drop —
                            // intentional if the user re-locks for
                            // a different host without going through
                            // CancelBootstrap.
                            prepare = Some(PendingPrepare {
                                host: host.clone(),
                                handle: start_prepare(host.clone()),
                                prepared: None,
                                remote_fs: None,
                                pending_project_path: None,
                            });
                            ui.lock_for_bootstrap(host, Instant::now());
                        }
                        ModalOutcome::PrepareHostThenSpawn { host, path } => {
                            // Same as PrepareHost (replacing a prior
                            // slot cancels its worker via Drop), but
                            // stash `path` so the prepare-Done(Ok)
                            // branch dismisses the modal and spawns
                            // automatically instead of unlocking for
                            // user path entry. Triggered when the
                            // user picks a `[[spawn.projects]]` entry
                            // bound to an SSH `host`.
                            prepare = Some(PendingPrepare {
                                host: host.clone(),
                                handle: start_prepare(host.clone()),
                                prepared: None,
                                remote_fs: None,
                                pending_project_path: Some(path),
                            });
                            ui.lock_for_bootstrap(host, Instant::now());
                        }
                        ModalOutcome::CancelBootstrap => {
                            // User hit Esc / @ during the locked
                            // phase. The modal already updated its
                            // own visual state (back to host zone);
                            // we just need to drop the in-flight
                            // worker so it doesn't surface a Done
                            // event after the modal has moved on.
                            if prepare.is_some() {
                                prepare = None;
                            } else {
                                cancel_modal_owned_attach(&mut attaches);
                            }
                        }
                        ModalOutcome::Spawn { host, path } => {
                            let (term_cols, term_rows) =
                                crossterm::terminal::size().wrap_err("read terminal size")?;
                            let (rows, cols) =
                                pty_size_for(nav_style, term_rows, term_cols, log_tail.is_some());
                            spawn_counter += 1;
                            if host == HOST_PLACEHOLDER {
                                spawn_ui = None;
                                let label = format!("agent-{spawn_counter}");
                                let id = AgentId::new(label.clone());
                                let cwd_path = if path.is_empty() {
                                    None
                                } else {
                                    Some(Path::new(&path))
                                };
                                match spawn_local_agent(
                                    id,
                                    label,
                                    cwd_path,
                                    rows,
                                    cols,
                                    scrollback_len,
                                ) {
                                    Ok(agent) => {
                                        nav.agents.push(agent);
                                        let target = nav.agents.len() - 1;
                                        nav.change_focus(target);
                                    }
                                    Err(e) => {
                                        tracing::error!("spawn failed: {e}");
                                    }
                                }
                            } else {
                                // SSH branch: the prepare slot must
                                // exist (the modal can only emit a
                                // remote `Spawn` after going through
                                // PrepareHost). Move out the
                                // prepared host; the `RemoteFs`
                                // ControlMaster goes with `prepare`'s
                                // Drop since attach has its own
                                // tunnel.
                                let Some(slot) = prepare.take() else {
                                    tracing::error!(
                                        %host,
                                        "remote Spawn without an active prepare slot — \
                                         dropping (modal state machine bug)",
                                    );
                                    spawn_ui = None;
                                    continue;
                                };
                                let Some(prepared) = slot.prepared else {
                                    tracing::error!(
                                        %host,
                                        "remote Spawn before prepare reported Done — \
                                         dropping (modal state machine bug)",
                                    );
                                    spawn_ui = None;
                                    continue;
                                };
                                let attach = build_remote_attach(
                                    prepared,
                                    host.clone(),
                                    &path,
                                    tui_pid,
                                    spawn_counter,
                                    rows,
                                    cols,
                                    /* modal_owner */ true,
                                    start_attach,
                                );
                                // Re-lock the modal so the spinner
                                // continues through the ~1-2 s
                                // attach phase.
                                ui.lock_for_bootstrap(host, Instant::now());
                                attaches.push(attach);
                            }
                        }
                        ModalOutcome::SpawnScratch { host } => {
                            // "Enter without picking" → land in the
                            // configured scratch dir (default
                            // `~/.codemux/scratch`). Resolves the
                            // tilde against the local $HOME for local
                            // spawns and against the remote $HOME
                            // captured during prepare for SSH spawns,
                            // then `mkdir -p`s the path so the daemon's
                            // `cwd.exists()` check passes on the
                            // remote side. On any resolution / mkdir
                            // failure we fall back to today's "use
                            // platform default cwd" behavior so the
                            // user still gets an agent — they just get
                            // a tracing diagnostic instead of a silent
                            // failure.
                            let (term_cols, term_rows) =
                                crossterm::terminal::size().wrap_err("read terminal size")?;
                            let (rows, cols) =
                                pty_size_for(nav_style, term_rows, term_cols, log_tail.is_some());
                            spawn_counter += 1;
                            if host == HOST_PLACEHOLDER {
                                spawn_ui = None;
                                let label = format!("agent-{spawn_counter}");
                                let id = AgentId::new(label.clone());
                                let cwd = resolve_local_scratch_cwd(spawn_config);
                                match spawn_local_agent(
                                    id,
                                    label,
                                    cwd.as_deref(),
                                    rows,
                                    cols,
                                    scrollback_len,
                                ) {
                                    Ok(agent) => {
                                        nav.agents.push(agent);
                                        let target = nav.agents.len() - 1;
                                        nav.change_focus(target);
                                    }
                                    Err(e) => {
                                        tracing::error!("spawn failed: {e}");
                                    }
                                }
                            } else {
                                // SSH branch: same prepare-slot
                                // contract as the Spawn arm. Take the
                                // slot first, then optionally mkdir
                                // via its still-live `RemoteFs`
                                // before the slot drops.
                                let Some(slot) = prepare.take() else {
                                    tracing::error!(
                                        %host,
                                        "remote SpawnScratch without an active \
                                         prepare slot — dropping (modal state \
                                         machine bug)",
                                    );
                                    spawn_ui = None;
                                    continue;
                                };
                                let Some(prepared) = slot.prepared else {
                                    tracing::error!(
                                        %host,
                                        "remote SpawnScratch before prepare \
                                         reported Done — dropping (modal state \
                                         machine bug)",
                                    );
                                    spawn_ui = None;
                                    continue;
                                };
                                // Resolve scratch against the captured
                                // remote $HOME, then mkdir over the
                                // prepare's `RemoteFs`. If the
                                // resolution fails or there's no live
                                // master to mkdir through, fall back
                                // to `cwd_path = None` so the daemon
                                // inherits the remote shell's cwd
                                // ($HOME) — same degradation as today's
                                // empty-path SSH spawn.
                                let cwd_path = resolve_remote_scratch_cwd(
                                    spawn_config,
                                    &prepared.remote_home,
                                    slot.remote_fs.as_ref(),
                                    &runner,
                                );
                                let label = format!("{host}:agent-{spawn_counter}");
                                let runtime_id = AgentId::new(format!("agent-{spawn_counter}"));
                                let daemon_agent_id = daemon_agent_id_for(tui_pid, spawn_counter);
                                let repo = cwd_path
                                    .as_ref()
                                    .and_then(|p| repo_name::resolve_remote(&p.to_string_lossy()));
                                let handle = start_attach(
                                    prepared,
                                    host.clone(),
                                    daemon_agent_id,
                                    cwd_path,
                                    rows,
                                    cols,
                                );
                                tracing::info!(
                                    %host,
                                    label = %label,
                                    "started SSH attach worker (scratch)",
                                );
                                ui.lock_for_bootstrap(host.clone(), Instant::now());
                                attaches.push(PendingAttach {
                                    agent_id: runtime_id,
                                    label,
                                    host,
                                    repo,
                                    rows,
                                    cols,
                                    handle,
                                    modal_owner: true,
                                });
                            }
                        }
                    }
                    // Re-tick fuzzy dispatch so the just-typed
                    // keystroke immediately enqueues a worker
                    // request instead of waiting for the next loop
                    // iteration. The worker scoring runs off-thread
                    // and the result lands a frame or two later;
                    // typing itself is no longer blocked on it.
                    tick_fuzzy_dispatch(
                        spawn_ui.as_mut(),
                        &fuzzy_worker,
                        &index_mgr,
                        &mut last_pushed_index_gen,
                        &mut last_pushed_query,
                    );
                    continue;
                }

                if let PopupState::Open { selection } = nav.popup_state {
                    if let Some(action) = bindings.on_popup.lookup(&key) {
                        match action {
                            PopupAction::Next => {
                                let next = (selection + 1) % nav.agents.len();
                                nav.popup_state = PopupState::Open { selection: next };
                            }
                            PopupAction::Prev => {
                                let prev = if selection == 0 {
                                    nav.agents.len() - 1
                                } else {
                                    selection - 1
                                };
                                nav.popup_state = PopupState::Open { selection: prev };
                            }
                            PopupAction::Confirm => {
                                nav.change_focus(selection);
                                nav.popup_state = PopupState::Closed;
                            }
                            PopupAction::Cancel => {
                                nav.popup_state = PopupState::Closed;
                            }
                        }
                    }
                    continue;
                }

                // Scroll mode: when the nav.focused agent's PTY parser is
                // showing scrollback (offset > 0), arrow keys / PgUp /
                // PgDn / g / G / Esc drive the scroll instead of being
                // forwarded to Claude. We do NOT snap-to-live here for
                // unmatched keys — that would clobber the offset when
                // the user presses the prefix (Cmd+B) or a direct nav
                // chord on their way to switching tabs, losing the
                // scroll position the moment they reach for it. The
                // snap is deferred to the `KeyDispatch::Forward` arm
                // below: only bytes actually being sent to Claude
                // (typing real text, control sequences) collapse the
                // view; navigation, popup, help, spawn, and consume
                // dispatches all leave scroll state untouched. Wheel
                // down to the bottom is the other common exit; once
                // `scrollback() == 0` the whole branch is a no-op and
                // arrow keys flow through normally.
                if let Some(focused_agent) = nav.agents.get_mut(nav.focused)
                    && focused_agent.scrollback_offset() > 0
                    && let Some(action) = bindings.on_scroll.lookup(&key)
                {
                    let page = i32::from(focused_agent.rows.saturating_sub(1).max(1));
                    match action {
                        ScrollAction::LineUp => focused_agent.nudge_scrollback(1),
                        ScrollAction::LineDown => focused_agent.nudge_scrollback(-1),
                        ScrollAction::PageUp => focused_agent.nudge_scrollback(page),
                        ScrollAction::PageDown => focused_agent.nudge_scrollback(-page),
                        ScrollAction::Top => focused_agent.jump_to_top(),
                        ScrollAction::Bottom | ScrollAction::ExitScroll => {
                            focused_agent.snap_to_live();
                        }
                    }
                    continue;
                }

                match dispatch_key(&mut prefix_state, &key, bindings) {
                    KeyDispatch::Forward(bytes) => {
                        // Snap-to-live before forwarding: typing while
                        // scrolled back would otherwise echo into a
                        // view the user can't see. Only Forward (real
                        // bytes to the PTY) triggers the snap; nav /
                        // popup / help / spawn dispatches preserve the
                        // per-agent offset so `Cmd-B 2` to switch tabs
                        // doesn't reset scroll on the agent you just
                        // left.
                        if let Some(a) = nav.agents.get_mut(nav.focused) {
                            a.snap_to_live();
                            match &mut a.state {
                                AgentState::Ready { transport, .. } => {
                                    transport.write(&bytes).wrap_err("write to pty")?;
                                }
                                // Drop the bytes — a Failed or Crashed
                                // pane has no transport. tracing::trace
                                // because this is high-volume during
                                // typing if the user mistakes a dead
                                // pane for a live one. The crash banner
                                // tells them to dismiss with `<prefix>
                                // d` rather than type into the corpse.
                                AgentState::Failed { .. } | AgentState::Crashed { .. } => {
                                    tracing::trace!(
                                        label = %a.label,
                                        n = bytes.len(),
                                        "dropped key bytes (agent has no transport)",
                                    );
                                }
                            }
                        }
                    }
                    KeyDispatch::Consume => {}
                    KeyDispatch::Exit => return Ok(()),
                    KeyDispatch::SpawnAgent => {
                        // SWR trigger: every spawn-modal open kicks
                        // off a fresh local walk so newly-created
                        // directories appear within a few seconds.
                        // The manager preserves cached results as
                        // `Refreshing { dirs, .. }` so the modal
                        // opens instantly with stale-but-usable data.
                        // Run unconditionally (not gated on Fuzzy)
                        // so a mid-session toggle to Fuzzy still
                        // benefits from the most-recent walk.
                        let outcome = index_mgr.request_local_swr(
                            &spawn_config.search_roots,
                            &spawn_config.project_markers,
                        );
                        tracing::debug!(?outcome, "fuzzy index: SWR refresh on modal open");
                        spawn_ui = Some(SpawnMinibuffer::open(
                            initial_cwd,
                            spawn_config.default_mode,
                            spawn_config.projects.clone(),
                        ));
                    }
                    KeyDispatch::FocusNext => {
                        let next = (nav.focused + 1) % nav.agents.len();
                        nav.change_focus(next);
                    }
                    KeyDispatch::FocusPrev => {
                        let prev = if nav.focused == 0 {
                            nav.agents.len() - 1
                        } else {
                            nav.focused - 1
                        };
                        nav.change_focus(prev);
                    }
                    KeyDispatch::FocusLast => {
                        // Bounce. No-op if the previous slot is gone
                        // (already cleared in the per-frame clamp) or
                        // somehow points to current focus.
                        if let Some(prev) = nav.previous_focused
                            && prev < nav.agents.len()
                            && prev != nav.focused
                        {
                            nav.change_focus(prev);
                        }
                    }
                    KeyDispatch::FocusAt(idx) => {
                        if idx < nav.agents.len() {
                            nav.change_focus(idx);
                        }
                    }
                    KeyDispatch::ToggleNav => {
                        nav_style = nav_style.toggle();
                        let (term_cols, term_rows) =
                            crossterm::terminal::size().wrap_err("read terminal size")?;
                        let (rows, cols) =
                            pty_size_for(nav_style, term_rows, term_cols, log_tail.is_some());
                        resize_agents(&mut nav.agents, rows, cols);
                    }
                    KeyDispatch::OpenPopup => {
                        nav.popup_state = PopupState::Open {
                            selection: nav.focused,
                        };
                    }
                    KeyDispatch::OpenHelp => {
                        help_state = HelpState::Open;
                    }
                    KeyDispatch::DismissAgent => {
                        nav.dismiss_focused();
                    }
                    KeyDispatch::KillAgent => {
                        nav.kill_focused();
                    }
                }
            }
            Event::Resize(cols, rows) => {
                let (pty_rows, pty_cols) = pty_size_for(nav_style, rows, cols, log_tail.is_some());
                resize_agents(&mut nav.agents, pty_rows, pty_cols);
                // Selection cells are pane-relative, but a resize
                // reflows vt100 — the cells under the highlight no
                // longer correspond to the same content. Clearing is
                // the cheapest correct answer; the user re-drags if
                // they still want the same text.
                selection = None;
                // Same reflow concern for the hover URL: the row /
                // column the URL was on may now hold different content.
                let transition = update_hover(&mut hover, None);
                if let Err(err) = apply_hover_cursor(&mut io::stdout(), transition) {
                    tracing::debug!(?err, "hover cursor update failed");
                }
            }
            Event::Mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers,
            }) if no_overlay_active(spawn_ui.as_ref(), nav.popup_state, help_state) => {
                // Wheel events are unconditional — anywhere in the
                // window is treated as "scroll the nav.focused agent." In
                // LeftPane mode that means wheel-over-nav scrolls the
                // agent rather than the (currently un-scrollable)
                // agent list, which is mildly weird but harmless and
                // strictly better than the previous behavior of
                // forwarding wheel-as-arrow into Claude's prompt
                // history. Revisit if/when the nav pane becomes
                // independently scrollable.
                match kind {
                    MouseEventKind::ScrollUp => {
                        if let Some(agent) = nav.agents.get_mut(nav.focused) {
                            agent.nudge_scrollback(WHEEL_STEP);
                        }
                    }
                    MouseEventKind::ScrollDown => {
                        if let Some(agent) = nav.agents.get_mut(nav.focused) {
                            agent.nudge_scrollback(-WHEEL_STEP);
                        }
                    }
                    other => {
                        // Ctrl+hover: maintain a URL highlight under the
                        // cursor. Recompute on every Moved event because
                        // we can't see Ctrl-release-without-motion (no
                        // keyup in SGR mouse mode); when the user does
                        // move without Ctrl held, this branch falls
                        // through to the `clear_hover` path below.
                        let ctrl_held = modifiers.contains(KeyModifiers::CONTROL);
                        if ctrl_held && matches!(other, MouseEventKind::Moved) {
                            let new = compute_hover(&pane_hitbox, &nav.agents, column, row);
                            let transition = update_hover(&mut hover, new);
                            if let Err(err) = apply_hover_cursor(&mut io::stdout(), transition) {
                                tracing::debug!(?err, "hover cursor update failed");
                            }
                            continue;
                        }
                        if !ctrl_held && hover.is_some() {
                            let transition = update_hover(&mut hover, None);
                            if let Err(err) = apply_hover_cursor(&mut io::stdout(), transition) {
                                tracing::debug!(?err, "hover cursor update failed");
                            }
                        }
                        // Ctrl+Click on a URL cell hands the URL to the
                        // OS opener and consumes the event so the
                        // selection-arm path below doesn't also fire.
                        // Off-URL Ctrl+Click falls through — the user
                        // probably wants the selection they were going
                        // to start.
                        if ctrl_held
                            && matches!(other, MouseEventKind::Down(MouseButton::Left))
                            && let Some(span) =
                                compute_hover(&pane_hitbox, &nav.agents, column, row)
                        {
                            if let Err(err) = url_opener.open(&span.url) {
                                tracing::debug!(?err, url = %span.url, "URL opener failed");
                            }
                            let transition = update_hover(&mut hover, Some(span));
                            if let Err(err) = apply_hover_cursor(&mut io::stdout(), transition) {
                                tracing::debug!(?err, "hover cursor update failed");
                            }
                            continue;
                        }
                        // Pane-relative selection takes priority over
                        // tab dispatch: a Down inside the live PTY
                        // surface arms / extends / commits a drag-to-
                        // select. Tabs and nav rows live in chrome
                        // (outside the pane rect), so they fall through
                        // to `tab_mouse_dispatch` cleanly.
                        let pane_dispatch = pane_mouse_dispatch(
                            other,
                            column,
                            row,
                            &pane_hitbox,
                            selection.as_ref(),
                        );
                        let handled_by_pane = match pane_dispatch {
                            Some(PaneMouseDispatch::Arm { agent, cell }) => {
                                selection = Some(Selection {
                                    agent,
                                    anchor: cell,
                                    head: cell,
                                });
                                true
                            }
                            Some(PaneMouseDispatch::Extend(cell)) => {
                                if let Some(sel) = selection.as_mut() {
                                    sel.head = cell;
                                }
                                true
                            }
                            Some(PaneMouseDispatch::Commit) => {
                                if let Some(sel) = selection.take() {
                                    commit_selection(&sel, &nav.agents, &pane_hitbox);
                                }
                                true
                            }
                            None => false,
                        };
                        if handled_by_pane {
                            continue;
                        }
                        if let Some(action) = tab_mouse_dispatch(
                            other,
                            column,
                            row,
                            &tab_hitboxes,
                            mouse_press.as_ref(),
                        ) {
                            match action {
                                TabMouseDispatch::PressTab(id) => mouse_press = Some(id),
                                TabMouseDispatch::Click(id) => {
                                    mouse_press = None;
                                    if let Some(idx) = nav.agents.iter().position(|a| a.id == id) {
                                        nav.change_focus(idx);
                                    }
                                }
                                TabMouseDispatch::Reorder { from, to } => {
                                    mouse_press = None;
                                    let from_idx = nav.agents.iter().position(|a| a.id == from);
                                    let to_idx = nav.agents.iter().position(|a| a.id == to);
                                    if let (Some(f), Some(t)) = (from_idx, to_idx) {
                                        reorder_agents(&mut nav.agents, f, t);
                                        nav.focused = shift_index(nav.focused, f, t);
                                        nav.previous_focused =
                                            nav.previous_focused.map(|p| shift_index(p, f, t));
                                    }
                                }
                                TabMouseDispatch::Cancel => mouse_press = None,
                            }
                        }
                    }
                }
            }
            Event::Paste(text)
                if no_overlay_active(spawn_ui.as_ref(), nav.popup_state, help_state) =>
            {
                // Pasting while scrolled-back: snap to live view first
                // so the user sees what landed (otherwise the paste is
                // invisible above the fold), then forward the chunk.
                //
                // Claude advertises `?2004h` (bracketed paste) at start-
                // up. We are the host terminal from its perspective, so
                // we wrap the chunk in `\x1b[200~ ... \x1b[201~` before
                // writing — that's how Claude knows the embedded
                // newlines are pasted text rather than user-typed Enter
                // keys. Without the wrappers Claude would submit the
                // message on every newline in the paste.
                if let Some(agent) = nav.agents.get_mut(nav.focused) {
                    agent.snap_to_live();
                    if let AgentState::Ready { transport, .. } = &mut agent.state {
                        transport
                            .write(&wrap_paste(&text))
                            .wrap_err("write pasted bytes to pty")?;
                    }
                }
            }
            _ => {}
        }
    }
}

/// Drives the prefix-key state machine, consulting the user's bindings.
/// Returns the dispatch the event loop should perform.
///
/// `AwaitingCommand` is **sticky for navigation**: after the user presses
/// the prefix once, repeated nav keystrokes (`h`/`l`/`j`/`k`/`n`/`p`/Tab/digits)
/// keep the state armed so the user can `Ctrl-B h h h` to step back three
/// agents without re-pressing the prefix. Non-nav commands (`q`, `c`, `?`,
/// `v`, `w`) and unbound keys exit the mode after dispatching once.
fn dispatch_key(state: &mut PrefixState, key: &KeyEvent, bindings: &Bindings) -> KeyDispatch {
    match *state {
        PrefixState::Idle => {
            // Direct binds (no-prefix fast path) win first. Their
            // whole point is single-chord access; checking them
            // before the prefix means a user who binds the same
            // chord to both gets the direct behavior — surprising
            // only if they did this on purpose, which they wouldn't.
            if let Some(action) = bindings.on_direct.lookup(key) {
                return match action {
                    DirectAction::SpawnAgent => KeyDispatch::SpawnAgent,
                    DirectAction::FocusNext => KeyDispatch::FocusNext,
                    DirectAction::FocusPrev => KeyDispatch::FocusPrev,
                    DirectAction::FocusLast => KeyDispatch::FocusLast,
                };
            }
            if bindings.prefix.matches(key) {
                *state = PrefixState::AwaitingCommand;
                KeyDispatch::Consume
            } else if let Some(bytes) = key_to_bytes(key.code, key.modifiers) {
                KeyDispatch::Forward(bytes)
            } else {
                KeyDispatch::Consume
            }
        }
        PrefixState::AwaitingCommand => {
            let dispatch = compute_awaiting_dispatch(key, bindings);
            // Sticky semantics: nav dispatches keep us armed so the
            // user can repeat the move without re-pressing the
            // prefix. Anything else (commands, unbound, double-
            // prefix passthrough) drops back to Idle.
            if !is_nav_dispatch(&dispatch) {
                *state = PrefixState::Idle;
            }
            dispatch
        }
    }
}

/// Compute the dispatch for a key pressed while in
/// `AwaitingCommand` state. Pulled out of `dispatch_key` so the
/// state-transition policy (sticky for nav, exit otherwise) lives in
/// one place rather than being interleaved with key-decoding logic.
fn compute_awaiting_dispatch(key: &KeyEvent, bindings: &Bindings) -> KeyDispatch {
    // Double-prefix: forward a literal prefix byte to the focused PTY.
    // Only meaningful when the prefix is a single Ctrl-modified char.
    if bindings.prefix.matches(key) {
        if let Some(byte) = literal_byte_for(&bindings.prefix) {
            return KeyDispatch::Forward(vec![byte]);
        }
        return KeyDispatch::Consume;
    }
    // Hardcoded: digit-keys 1..=9 focus the agent at that index.
    if let KeyCode::Char(c) = key.code
        && c.is_ascii_digit()
        && !key.modifiers.contains(KeyModifiers::CONTROL)
        && let Some(d) = c.to_digit(10)
        && d > 0
    {
        return KeyDispatch::FocusAt((d as usize) - 1);
    }
    match bindings.on_prefix.lookup(key) {
        Some(PrefixAction::Quit) => KeyDispatch::Exit,
        Some(PrefixAction::SpawnAgent) => KeyDispatch::SpawnAgent,
        Some(PrefixAction::FocusNext) => KeyDispatch::FocusNext,
        Some(PrefixAction::FocusPrev) => KeyDispatch::FocusPrev,
        Some(PrefixAction::FocusLast) => KeyDispatch::FocusLast,
        Some(PrefixAction::ToggleNav) => KeyDispatch::ToggleNav,
        Some(PrefixAction::OpenSwitcher) => KeyDispatch::OpenPopup,
        Some(PrefixAction::DismissAgent) => KeyDispatch::DismissAgent,
        Some(PrefixAction::KillAgent) => KeyDispatch::KillAgent,
        Some(PrefixAction::Help) => KeyDispatch::OpenHelp,
        None => KeyDispatch::Consume,
    }
}

/// Is this dispatch a navigation move? Used by the
/// `AwaitingCommand` state machine to decide whether to stay sticky
/// (nav: yes) or fall back to `Idle` (everything else: no).
const fn is_nav_dispatch(dispatch: &KeyDispatch) -> bool {
    matches!(
        dispatch,
        KeyDispatch::FocusNext
            | KeyDispatch::FocusPrev
            | KeyDispatch::FocusLast
            | KeyDispatch::FocusAt(_)
    )
}

/// Compute the byte a "Ctrl-letter" prefix sends on the wire (e.g. Ctrl-B = 0x02).
/// Returns None for non-letter prefixes; the user can configure those but
/// double-prefix passthrough only makes sense for the standard tmux-style
/// Ctrl-letter chord.
fn literal_byte_for(chord: &crate::keymap::KeyChord) -> Option<u8> {
    if !chord.modifiers.contains(KeyModifiers::CONTROL) {
        return None;
    }
    let KeyCode::Char(c) = chord.code else {
        return None;
    };
    let lower = c.to_ascii_lowercase();
    if lower.is_ascii_alphabetic() {
        Some((lower as u8) - b'a' + 1)
    } else {
        None
    }
}

#[allow(clippy::too_many_arguments)]
fn render_frame(
    frame: &mut Frame<'_>,
    agents: &[RuntimeAgent],
    focused: usize,
    nav_style: NavStyle,
    popup: PopupState,
    help: HelpState,
    spawn_ui: Option<&SpawnMinibuffer>,
    bindings: &Bindings,
    prefix_state: PrefixState,
    log_tail: Option<&LogTail>,
    phase: AnimationPhase,
    chrome: &ChromeStyle,
    hitboxes: &mut TabHitboxes,
    pane_hitbox: &mut PaneHitbox,
    overlay: PaneOverlay<'_>,
    index_state: Option<&IndexState>,
    segments: &[Box<dyn StatusSegment>],
) {
    // Cleared at the top of every frame so a stale frame's rects can
    // never bleed into the next event hit-test if the layout changed
    // (terminal resize, nav-style toggle, agent spawn / reap).
    hitboxes.clear();
    pane_hitbox.clear();
    let area = frame.area();
    // Pre-resolve the dismiss-chord label *here*, in the orchestration
    // layer that owns the bindings. The renderer (and its pane helpers)
    // never sees `&Bindings` — they only need the formatted string for
    // the crash banner. Threading the full bindings struct through the
    // render pipeline would couple presentation to input config and was
    // flagged in architecture review as tramp data.
    let dismiss_label = bindings.on_prefix.dismiss_agent.to_string();
    // Carve off the bottom log strip first, if enabled. The remaining
    // area is what the nav-style render sees, so it doesn't need to
    // know the log strip exists.
    let (main_area, log_area) = match log_tail {
        Some(_) => {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(0), Constraint::Length(LOG_STRIP_HEIGHT)])
                .split(area);
            (chunks[0], Some(chunks[1]))
        }
        None => (area, None),
    };
    match nav_style {
        NavStyle::LeftPane => {
            render_left_pane(
                frame,
                main_area,
                agents,
                focused,
                &dismiss_label,
                phase,
                chrome,
                hitboxes,
                pane_hitbox,
                overlay,
            );
        }
        NavStyle::Popup => {
            render_popup_style(
                frame,
                main_area,
                agents,
                focused,
                popup,
                bindings,
                prefix_state,
                &dismiss_label,
                phase,
                chrome,
                hitboxes,
                pane_hitbox,
                overlay,
                segments,
            );
        }
    }
    if let (Some(tail), Some(area)) = (log_tail, log_area) {
        render_log_strip(frame, area, tail, chrome);
    }
    if let Some(ui) = spawn_ui {
        ui.render(frame, area, &bindings.on_modal, index_state);
    }
    if matches!(help, HelpState::Open) {
        render_help(frame, area, bindings);
    }
}

/// Render the bottom log strip — a single row showing the most recent
/// tracing event captured by [`LogTail`]. Uses dim styling so the
/// strip reads as ambient information rather than competing with the
/// agent pane for attention. Empty tail (no events yet) renders as a
/// single dash so the row is visually present and the user can tell
/// the strip is live.
fn render_log_strip(frame: &mut Frame<'_>, area: Rect, tail: &LogTail, chrome: &ChromeStyle) {
    let line = tail.latest().unwrap_or_else(|| "—".to_string());
    let widget = Paragraph::new(Line::raw(line)).style(chrome.secondary);
    // Clear so a previous frame's longer line doesn't leave trailing
    // characters when the latest line is shorter.
    frame.render_widget(Clear, area);
    frame.render_widget(widget, area);
}

/// Render an agent's main pane based on its current [`AgentState`].
///
/// - `Ready`: the live PTY through `tui-term`'s [`PseudoTerminal`].
/// - `Crashed`: the parser's last frame (so the user can see what
///   claude was doing right before it died) plus a one-row banner
///   pinned to the top edge with the exit code and dismiss
///   instruction. `bindings.prefix` is interpolated into the
///   banner so a reconfigured prefix shows the correct chord.
/// - `Failed`: the bootstrap error in red, centered. The failure
///   pane intentionally has no border or title — a bordered
///   placeholder reads as "this is a real UI element" when in fact
///   the slot is dead.
///
/// When the agent's PTY parser is showing scrollback (offset > 0), a
/// floating "↑ scroll N · esc" badge is painted over the bottom-right
/// of the pane via [`render_scroll_indicator`]. We deliberately do
/// NOT shrink the PTY by a row to make space — that would force a
/// `SIGWINCH` on every scroll-mode entry/exit, and Claude redrawing
/// its UI on each transition would be much worse UX than the badge
/// covering ~22 cells of (usually empty) Claude border. See AD-25.
fn render_agent_pane(
    frame: &mut Frame<'_>,
    area: Rect,
    agent: &RuntimeAgent,
    dismiss_label: &str,
    pane_hitbox: &mut PaneHitbox,
    overlay: PaneOverlay<'_>,
) {
    match &agent.state {
        AgentState::Ready { parser, .. } => {
            let widget = PseudoTerminal::new(parser.screen());
            frame.render_widget(widget, area);
            pane_hitbox.record(area, agent.id.clone());
            paint_selection_if_active(frame, area, &agent.id, overlay.selection);
            paint_hover_url_if_active(frame, area, &agent.id, overlay.hover);
            let offset = parser.screen().scrollback();
            if offset > 0 {
                render_scroll_indicator(frame, area, offset);
            }
        }
        AgentState::Crashed { parser, exit_code } => {
            // Draw the last frame underneath — same widget as Ready,
            // just with no live updates landing in the parser. The
            // banner overlay below tells the user the screen is
            // frozen.
            let widget = PseudoTerminal::new(parser.screen());
            frame.render_widget(widget, area);
            pane_hitbox.record(area, agent.id.clone());
            paint_selection_if_active(frame, area, &agent.id, overlay.selection);
            paint_hover_url_if_active(frame, area, &agent.id, overlay.hover);
            let offset = parser.screen().scrollback();
            if offset > 0 {
                render_scroll_indicator(frame, area, offset);
            }
            render_crash_banner(frame, area, *exit_code, dismiss_label);
        }
        AgentState::Failed { error } => {
            // Failed panes record a hitbox and paint the selection
            // overlay so the user can drag-to-copy the error text.
            // There's no live PTY here, so the selection commit path
            // routes to `failure_text_in_range` (the centered-layout
            // mirror of `vt100::Screen::contents_between`) rather
            // than the parser screen.
            let host = agent.host.as_deref().unwrap_or("");
            render_failure_pane(frame, area, host, &error.user_message());
            pane_hitbox.record(area, agent.id.clone());
            paint_selection_if_active(frame, area, &agent.id, overlay.selection);
        }
    }
}

/// Wrap every URL in the visible PTY screen with OSC 8 hyperlink
/// escape sequences so the host terminal (Ghostty / iTerm2 / Kitty /
/// `WezTerm`) can render its native URL-hover indicator and open the
/// link via Cmd-click (macOS) or Ctrl-click (Linux/Win) — whichever
/// modifier the host terminal is configured for.
///
/// Why post-draw and not inside `terminal.draw`: the natural-looking
/// alternative is to mutate the first/last URL cell symbols to embed
/// the OSC 8 setup/reset escape bytes, but that breaks ratatui's diff
/// algorithm. `unicode_width::Width` on a 27-byte symbol returns 27,
/// `Buffer::diff` then thinks the cell is a 27-cell-wide grapheme and
/// suppresses the immediately-following cell from the update list —
/// which leaves whatever was previously at that column on screen as
/// stale content, so `https://example.com` reads back as something like
/// `hetps://example.coms`. Symptoms verified with a focused repro.
///
/// Instead: after `terminal.draw` finishes flushing the diff, we walk
/// each visible URL on the focused pane's PTY screen and write OSC 8
/// directly to stdout — `MoveTo(first_x, y)`, OSC 8 setup, then a
/// per-cell SGR + `Print(contents)` walk so each cell receives the
/// hyperlink attribute (OSC 8 attaches to glyphs printed while the
/// attribute is active; a bare `MoveTo` doesn't tag intervening
/// cells), then OSC 8 reset. The whole batch is bracketed with DECSC
/// (`\x1b7`) and DECRC (`\x1b8`) so cursor position *and* SGR state
/// are restored to whatever ratatui left at end-of-draw — the next
/// frame's diff is unaffected.
///
/// Re-printing the URL chars with their original SGR is a visual
/// no-op (same glyph, same colors) — only the hidden hyperlink
/// attribute changes. Terminals that don't support OSC 8 strip the
/// unknown sequences and the redundant cell prints look identical to
/// what was already on screen.
///
/// Cells past the screen edge are skipped via `screen.cell()` returning
/// `None`. Wide-char continuation cells inside a URL (rare; URL chars
/// are ASCII) are skipped to keep cursor advancement in sync with the
/// glyph stream — the wide cell's primary already printed two columns
/// in one print.
fn paint_hyperlinks_post_draw<W: io::Write>(
    out: &mut W,
    area: Rect,
    screen: &vt100::Screen,
) -> io::Result<()> {
    let urls = crate::url_scan::find_urls_in_screen(screen);
    if urls.is_empty() {
        return Ok(());
    }
    // DECSC saves cursor position + character set + SGR; the matching
    // DECRC at the bottom restores them so ratatui's cursor tracking
    // and the next frame's per-cell SGR emit don't see our intermediate
    // state. CSI s / CSI u (the more portable cursor save/restore that
    // crossterm uses) only saves position, not SGR, which would force
    // an extra explicit reset.
    out.write_all(b"\x1b7")?;
    for url in urls {
        if url.cols.start >= url.cols.end {
            continue;
        }
        let y = area.y.saturating_add(url.row);
        let first_x = area.x.saturating_add(url.cols.start);
        // CUP (cursor position) is 1-based for both row and column.
        write!(
            out,
            "\x1b[{};{}H\x1b]8;;{}\x1b\\",
            u32::from(y).saturating_add(1),
            u32::from(first_x).saturating_add(1),
            url.url,
        )?;
        for col in url.cols.clone() {
            let Some(cell) = screen.cell(url.row, col) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            emit_cell_sgr(out, cell)?;
            let contents = cell.contents();
            if contents.is_empty() {
                out.write_all(b" ")?;
            } else {
                out.write_all(contents.as_bytes())?;
            }
        }
        // Empty OSC 8 closes the hyperlink so any subsequent text on
        // the same row (or in the next cell ratatui re-emits) isn't
        // tagged with this URL.
        out.write_all(b"\x1b]8;;\x1b\\")?;
    }
    out.write_all(b"\x1b8")?;
    Ok(())
}

/// Emit SGR escape bytes for a vt100 cell: full reset first (so the
/// previous cell's attributes don't leak), then bold / italic /
/// underline / inverse if set, then fg + bg colors. We deliberately
/// omit `dim`: most terminals render it by darkening fg, which fights
/// the host terminal's URL-hover tint.
fn emit_cell_sgr<W: io::Write>(out: &mut W, cell: &vt100::Cell) -> io::Result<()> {
    out.write_all(b"\x1b[0m")?;
    if cell.bold() {
        out.write_all(b"\x1b[1m")?;
    }
    if cell.italic() {
        out.write_all(b"\x1b[3m")?;
    }
    if cell.underline() {
        out.write_all(b"\x1b[4m")?;
    }
    if cell.inverse() {
        out.write_all(b"\x1b[7m")?;
    }
    emit_color(out, cell.fgcolor(), true)?;
    emit_color(out, cell.bgcolor(), false)?;
    Ok(())
}

/// Emit one SGR color escape — `Default` → `[39m`/`[49m`, indexed →
/// 256-color form, true-color → 24-bit form. `foreground` switches
/// between the fg (38/39) and bg (48/49) parameter prefixes.
fn emit_color<W: io::Write>(out: &mut W, color: vt100::Color, foreground: bool) -> io::Result<()> {
    let (prefix, default) = if foreground { (38, 39) } else { (48, 49) };
    match color {
        vt100::Color::Default => write!(out, "\x1b[{default}m"),
        vt100::Color::Idx(n) => write!(out, "\x1b[{prefix};5;{n}m"),
        vt100::Color::Rgb(r, g, b) => write!(out, "\x1b[{prefix};2;{r};{g};{b}m"),
    }
}

/// Set the host terminal's *mouse pointer* shape via OSC 22, mirroring
/// the native hover affordance the host gives Cmd-hover-on-OSC-8 cells:
/// switch to a hand pointer over a Ctrl-hovered URL, switch back to the
/// arrow when the user moves off (or releases Ctrl).
///
/// Names follow the CSS cursor convention that modern terminals
/// (Ghostty / iTerm2 / Kitty / `WezTerm`) accept — `pointer` for hand,
/// `default` for arrow. Older xterm-style X11 cursor-font names (e.g.
/// `hand1`, `left_ptr`) would also work but the CSS form is what
/// Ghostty's docs and the iTerm2 escape catalog list. Terminals that
/// don't recognise OSC 22 strip the unknown sequence silently.
///
/// Caller pairs this with [`update_hover`] / [`apply_hover_cursor`] so
/// the emit only fires on the boolean transition
/// `hover.is_some()` ↔ `hover.is_none()` — repeat moves across cells
/// of the same URL don't re-emit.
fn emit_mouse_cursor_shape<W: io::Write>(out: &mut W, pointer: bool) -> io::Result<()> {
    let shape = if pointer { "pointer" } else { "default" };
    write!(out, "\x1b]22;{shape}\x1b\\")
}

/// What changed when [`update_hover`] swapped the hover state.
/// `Activated` and `Deactivated` mark the boolean transitions where
/// the host mouse pointer needs to flip; `Unchanged` covers both
/// `Some → Some` (different cell or different URL — pointer stays a
/// hand) and `None → None` (no-op moves outside any URL — pointer
/// stays an arrow).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HoverTransition {
    Unchanged,
    Activated,
    Deactivated,
}

/// Pure state mutator: swap `hover` for `new` and report whether the
/// boolean activeness transitioned. Pure because the I/O side effect
/// (flipping the host mouse pointer via OSC 22) lives in
/// [`apply_hover_cursor`] — separated so the controller decides when
/// to emit, and the state mutation can be tested without a writer
/// fixture. Centralises the "every hover mutation must keep the
/// cursor shape in sync" invariant: every call site goes
/// `update_hover → apply_hover_cursor`, and a future contributor
/// adding a new mutation site has the pattern visible at every
/// existing one to copy.
fn update_hover(hover: &mut Option<HoverUrl>, new: Option<HoverUrl>) -> HoverTransition {
    let was_active = hover.is_some();
    let now_active = new.is_some();
    *hover = new;
    match (was_active, now_active) {
        (false, true) => HoverTransition::Activated,
        (true, false) => HoverTransition::Deactivated,
        _ => HoverTransition::Unchanged,
    }
}

/// I/O effect for a [`HoverTransition`]: emit OSC 22 to switch the
/// host mouse pointer to a hand on `Activated`, back to arrow on
/// `Deactivated`, no-op on `Unchanged`. Flushes on emit so the
/// pointer change shows up before the next event-poll iteration —
/// otherwise the user would see the old cursor for one frame.
fn apply_hover_cursor<W: io::Write>(out: &mut W, transition: HoverTransition) -> io::Result<()> {
    match transition {
        HoverTransition::Activated => {
            emit_mouse_cursor_shape(out, true)?;
            out.flush()?;
        }
        HoverTransition::Deactivated => {
            emit_mouse_cursor_shape(out, false)?;
            out.flush()?;
        }
        HoverTransition::Unchanged => {}
    }
    Ok(())
}

/// the `REVERSED` modifier on every cell in the normalized selection
/// rectangle. Additive on the cell's existing modifier set so bold /
/// italic / underline / colors pass through unchanged.
///
/// Out-of-bounds cells (clipped by a renderer that drew over part of
/// the area, or coordinates that drifted out during a resize race)
/// are dropped silently — `Buffer::cell_mut` returns `None` and the
/// inner branch becomes a no-op.
fn paint_selection_if_active(
    frame: &mut Frame<'_>,
    area: Rect,
    agent_id: &AgentId,
    selection: Option<&Selection>,
) {
    let Some(sel) = selection else {
        return;
    };
    if &sel.agent != agent_id {
        return;
    }
    let buf = frame.buffer_mut();
    let (start, end) = normalized_range(sel.anchor, sel.head);
    for row in start.row..=end.row {
        let (col_lo, col_hi) = row_bounds(start, end, row, area.width);
        for col in col_lo..col_hi {
            let x = area.x.saturating_add(col);
            let y = area.y.saturating_add(row);
            if let Some(cell) = buf.cell_mut(Position::new(x, y)) {
                cell.modifier.insert(Modifier::REVERSED);
            }
        }
    }
}

/// If `hover` belongs to the agent currently being painted, underline
/// the URL's column range on its row and tint it cyan. Same overlay
/// trick as [`paint_selection_if_active`]: additive on the cell's
/// existing modifier set so styling under the URL passes through.
///
/// Out-of-bounds cells are dropped silently — `Buffer::cell_mut`
/// returns `None` and the inner branch becomes a no-op. This protects
/// against a stale-frame race where the pane shrank between the hover
/// being recorded and the next render.
fn paint_hover_url_if_active(
    frame: &mut Frame<'_>,
    area: Rect,
    agent_id: &AgentId,
    hover: Option<&HoverUrl>,
) {
    let Some(h) = hover else {
        return;
    };
    if &h.agent != agent_id {
        return;
    }
    let buf = frame.buffer_mut();
    let y = area.y.saturating_add(h.row);
    for col in h.cols.clone() {
        let x = area.x.saturating_add(col);
        if let Some(cell) = buf.cell_mut(Position::new(x, y)) {
            cell.modifier.insert(Modifier::UNDERLINED);
            cell.fg = Color::Cyan;
        }
    }
}

/// overlay technique as [`render_scroll_indicator`] (`Clear` + a
/// styled `Paragraph`) so the underlying last-frame doesn't bleed
/// through. Color and copy depend on the exit code:
///
/// - `-1` — `SshDaemonPty` sentinel for socket-level failures
///   (tunnel drop, daemon death, framed reader I/O error). Red bg,
///   "connection lost" copy.
/// - any other non-zero — non-zero process exit. Red bg, `✗ session
///   ended (exit N)` copy.
///
/// Exit code `0` (clean exit) is auto-reaped in
/// [`NavState::reap_dead_transports`] before reaching this renderer,
/// so the `n` arm here will only ever observe non-zero codes in
/// practice. If a synthetic `Crashed { exit_code: 0 }` ever does
/// land here (e.g. through tests), the `n` arm formats it correctly
/// with the red treatment — defensive but unsurprising.
///
/// `dismiss_label` is the pre-formatted dismiss-chord string (e.g.
/// `"d"` or `"ctrl+x"`), resolved by the orchestration layer rather
/// than the renderer. Keeps `&Bindings` out of the render path so
/// the renderer stays decoupled from input config.
fn render_crash_banner(frame: &mut Frame<'_>, area: Rect, exit_code: i32, dismiss_label: &str) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let banner_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: 1,
    };
    let red_bg = Style::default()
        .fg(Color::White)
        .bg(Color::Red)
        .add_modifier(Modifier::BOLD);
    let (text, style) = match exit_code {
        -1 => (
            format!(" ✗ connection lost — {dismiss_label} to dismiss "),
            red_bg,
        ),
        n => (
            format!(" ✗ session ended (exit {n}) — {dismiss_label} to dismiss "),
            red_bg,
        ),
    };
    let widget = Paragraph::new(Line::raw(text))
        .alignment(Alignment::Left)
        .style(style);
    frame.render_widget(Clear, banner_area);
    frame.render_widget(widget, banner_area);
}

/// Floating one-row badge rendered in the bottom-right of the agent
/// pane while scroll mode is active. Width caps at
/// [`SCROLL_INDICATOR_WIDTH`] (24 cells) and clamps to the actual
/// pane width when the user has shrunk the terminal narrower than
/// that. `Clear` is essential — without it the underlying screen
/// content bleeds through the dim text and reads as garbage.
fn render_scroll_indicator(frame: &mut Frame<'_>, area: Rect, offset: usize) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let width = SCROLL_INDICATOR_WIDTH.min(area.width);
    let badge_area = Rect {
        x: area.x + area.width - width,
        y: area.y + area.height - 1,
        width,
        height: 1,
    };
    let text = format!(" ↑ scroll {offset} · esc ");
    let widget = Paragraph::new(Line::raw(text))
        .alignment(Alignment::Right)
        .style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_widget(Clear, badge_area);
    frame.render_widget(widget, badge_area);
}

/// One visible row of the failure pane after centering has been
/// applied. The renderer paints these directly; the selection
/// extractor reads cell ranges off the same data so what the user
/// drag-selects is exactly what lands on the clipboard.
#[derive(Debug, Clone, Eq, PartialEq)]
struct FailureLine {
    /// Pane-relative row this content lands on (0 = top of pane).
    row: u16,
    /// Original (unpadded) content, sourced from
    /// [`failure_source_lines`]. Used by the renderer for styling.
    content: String,
    /// Pane-relative column where `content` starts after horizontal
    /// centering. The renderer paints at `(area.x + col, area.y + row)`;
    /// the extractor uses `col` to map cell offsets back onto chars.
    col: u16,
    /// True for the "✗ bootstrap of {host} failed" header line so
    /// the renderer can paint it bold; body lines render as plain
    /// red. Off-screen lines (clipped by a too-short pane) are
    /// excluded entirely from the returned vec.
    is_header: bool,
}

/// The unstyled, unpadded source lines that the failure pane shows.
/// Source of truth for both rendering and selection extraction so the
/// two never disagree about what text is on screen.
///
/// Layout: header on row 0, blank spacer on row 1, then one row per
/// line of `error.user_message()`. Lifted out of `render_failure_pane`
/// so the cell-to-text path (selection commit) reuses the same content
/// without re-deriving the format.
fn failure_source_lines(host: &str, err: &str) -> Vec<String> {
    let mut lines = Vec::with_capacity(2 + err.lines().count());
    lines.push(format!("✗ bootstrap of {host} failed"));
    lines.push(String::new());
    for line in err.lines() {
        lines.push(line.to_string());
    }
    lines
}

/// Apply horizontal + vertical centering against `area` and return one
/// `FailureLine` per visible row. Off-screen rows (vertical overflow)
/// are dropped so the caller can paint without clipping bookkeeping.
///
/// Centering math mirrors `Paragraph::Center`: `(area_dim - content) / 2`
/// for the leading pad, with `saturating_sub` so overflow stays at 0
/// (content rendered top-left, clipped on the right). Display width
/// uses `UnicodeWidthStr::width` rather than `chars().count()` so wide
/// glyphs (CJK, emoji) center on the same cells the renderer's
/// `Paragraph::Center` lands on — without it, a wide char would shift
/// selection-extraction off the rendered cells by the glyph's width
/// minus one column per row.
fn failure_layout(host: &str, err: &str, area: Rect) -> Vec<FailureLine> {
    let lines = failure_source_lines(host, err);
    let content_height = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let top_pad = area.height.saturating_sub(content_height) / 2;
    let max_visible = area.height.saturating_sub(top_pad);

    let mut out = Vec::with_capacity(lines.len());
    for (i, content) in lines.into_iter().enumerate() {
        let Ok(offset) = u16::try_from(i) else { break };
        if offset >= max_visible {
            break;
        }
        let row = top_pad.saturating_add(offset);
        let width = u16::try_from(UnicodeWidthStr::width(content.as_str())).unwrap_or(u16::MAX);
        let col = area.width.saturating_sub(width) / 2;
        out.push(FailureLine {
            row,
            col,
            content,
            is_header: i == 0,
        });
    }
    out
}

/// Extract the text covered by a pane-relative cell range from a
/// failure pane. Mirrors `vt100::Screen::contents_between` semantics:
/// `start..=end` rows, `row_bounds` gives the column slice per row,
/// blank cells outside any centered line read as spaces.
///
/// Used by `commit_selection` for `Failed` agents — the live-PTY path
/// goes through vt100 instead. Returns an empty string when the
/// selection covers only padding so the caller can skip the OSC 52
/// write (matches the `text.is_empty()` short-circuit on the live
/// path).
fn failure_text_in_range(
    host: &str,
    err: &str,
    area: Rect,
    start: CellPos,
    end: CellPos,
) -> String {
    // Pre-collect each visible row's chars into a Vec so the per-cell
    // lookup below is O(1). The naive `content.chars().nth(c)` walks
    // the string from the start on every cell — O(cells × chars) per
    // row, which is fine for our tiny failure messages but reads as a
    // performance smell against the Rust style guide.
    let layout: Vec<(u16, u16, Vec<char>)> = failure_layout(host, err, area)
        .into_iter()
        .map(|l| (l.row, l.col, l.content.chars().collect()))
        .collect();
    let mut out = String::new();
    for row in start.row..=end.row {
        if row > start.row {
            out.push('\n');
        }
        let (col_lo, col_hi) = row_bounds(start, end, row, area.width);
        let line = layout.iter().find(|(r, _, _)| *r == row);
        for col in col_lo..col_hi {
            let ch = line
                .and_then(|(_, line_col, chars)| {
                    let c = col.checked_sub(*line_col)?;
                    chars.get(usize::from(c)).copied()
                })
                .unwrap_or(' ');
            out.push(ch);
        }
    }
    // Strip trailing whitespace per line so a drag that overshoots the
    // right margin doesn't paste a wall of spaces. Mirrors the practical
    // effect of vt100's `contents_between` on idle cells (which also
    // skips empties). Uses `split('\n')` not `lines()` so a trailing
    // newline (impossible in current code, but defensive) survives the
    // round-trip rather than being silently dropped.
    let trimmed = out
        .split('\n')
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    if trimmed.trim().is_empty() {
        String::new()
    } else {
        trimmed
    }
}

/// Centered failure pane shown when an SSH bootstrap returned an
/// error after the spawn modal closed. Renders without a border so
/// the pane reads as "this slot is dead" rather than "here is a real
/// UI element"; the in-flight phase has its own UX inside the spawn
/// modal and never reaches this renderer.
///
/// Painting goes through `failure_layout` so the cell coordinates
/// the renderer writes match the cells `failure_text_in_range`
/// extracts during a selection commit — the user copying their drag
/// gets exactly what they saw on screen.
fn render_failure_pane(frame: &mut Frame<'_>, area: Rect, host: &str, err: &str) {
    // Wipe the entire pane area first. We only paint individual line
    // cells below; without an explicit clear, whatever the previous
    // frame's renderer left in those cells (e.g. PTY content from a
    // local agent the user just switched away from) bleeds through
    // and reads as garbled characters around the centered text.
    frame.render_widget(Clear, area);

    let header_style = Style::default().fg(Color::Red).add_modifier(Modifier::BOLD);
    let body_style = Style::default().fg(Color::Red);
    for line in failure_layout(host, err, area) {
        let style = if line.is_header {
            header_style
        } else {
            body_style
        };
        let row_area = Rect {
            x: area.x.saturating_add(line.col),
            y: area.y.saturating_add(line.row),
            width: area.width.saturating_sub(line.col),
            height: 1,
        };
        if row_area.width == 0 {
            continue;
        }
        frame.render_widget(Paragraph::new(Line::styled(line.content, style)), row_area);
    }
}

#[allow(clippy::too_many_arguments)]
fn render_left_pane(
    frame: &mut Frame<'_>,
    area: Rect,
    agents: &[RuntimeAgent],
    focused: usize,
    dismiss_label: &str,
    phase: AnimationPhase,
    chrome: &ChromeStyle,
    hitboxes: &mut TabHitboxes,
    pane_hitbox: &mut PaneHitbox,
    overlay: PaneOverlay<'_>,
) {
    let [nav_area, pty_area] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(NAV_PANE_WIDTH), Constraint::Min(1)])
        .areas(area);

    let lines: Vec<Line> = agents
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let prefix = if i == focused { "> " } else { "  " };
            let mut spans: Vec<Span<'static>> = Vec::with_capacity(4);
            spans.push(Span::raw(format!("{prefix}[{}] ", i + 1)));
            spans.extend(agent_label_spans(a, false, phase, chrome));
            Line::from(spans)
        })
        .collect();
    let nav = Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" agents "));
    frame.render_widget(nav, nav_area);

    // Record one hitbox per agent row for click-to-focus and
    // drag-to-reorder. The bordered Block reserves the outer cells, so
    // clickable rows start one cell in from each side and the first
    // visible row is `nav_area.y + 1`.
    let inner_x = nav_area.x.saturating_add(1);
    let inner_w = nav_area.width.saturating_sub(2);
    let last_row_excl = nav_area.y.saturating_add(nav_area.height).saturating_sub(1);
    for (i, agent) in agents.iter().enumerate() {
        let Ok(offset) = u16::try_from(i) else { break };
        let y = nav_area.y.saturating_add(1).saturating_add(offset);
        if y >= last_row_excl {
            // Out of pane: agent list overflowed the nav area. The
            // unfilled rows have no surface to click; better to drop
            // them than record bogus rects past the bottom border.
            break;
        }
        hitboxes.record(
            Rect {
                x: inner_x,
                y,
                width: inner_w,
                height: 1,
            },
            agent.id.clone(),
        );
    }

    if let Some(agent) = agents.get(focused) {
        render_agent_pane(frame, pty_area, agent, dismiss_label, pane_hitbox, overlay);
    }
}

#[allow(clippy::too_many_arguments)]
fn render_popup_style(
    frame: &mut Frame<'_>,
    area: Rect,
    agents: &[RuntimeAgent],
    focused: usize,
    popup: PopupState,
    bindings: &Bindings,
    prefix_state: PrefixState,
    dismiss_label: &str,
    phase: AnimationPhase,
    chrome: &ChromeStyle,
    hitboxes: &mut TabHitboxes,
    pane_hitbox: &mut PaneHitbox,
    overlay: PaneOverlay<'_>,
    segments: &[Box<dyn StatusSegment>],
) {
    let [pty_area, status_area] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(STATUS_BAR_HEIGHT)])
        .areas(area);

    if let Some(agent) = agents.get(focused) {
        render_agent_pane(frame, pty_area, agent, dismiss_label, pane_hitbox, overlay);
    }

    render_status_bar(
        frame,
        status_area,
        agents,
        focused,
        bindings,
        prefix_state,
        phase,
        chrome,
        hitboxes,
        segments,
    );

    if let PopupState::Open { selection } = popup {
        render_switcher_popup(frame, area, agents, selection, phase, chrome);
    }
}

/// Render the bottom status bar in Popup mode: tab strip on the left,
/// status segments on the right (model · repo · branch · prefix-hint
/// by default — see [`crate::status_bar`]). Splitting into discrete
/// areas means each section can be styled and clipped independently;
/// the previous flat-string approach forced uniform style and made it
/// awkward to highlight the focused tab without rendering a custom
/// widget.
///
/// The right-side segments stack is built from the user's configured
/// list and dropped from the LEFT under width pressure, so the
/// rightmost segment (prefix-hint by default) is always visible.
#[allow(clippy::too_many_arguments)]
fn render_status_bar(
    frame: &mut Frame<'_>,
    area: Rect,
    agents: &[RuntimeAgent],
    focused: usize,
    bindings: &Bindings,
    prefix_state: PrefixState,
    phase: AnimationPhase,
    chrome: &ChromeStyle,
    hitboxes: &mut TabHitboxes,
    segments: &[Box<dyn StatusSegment>],
) {
    // Build the segments stack first so we know how much space to
    // reserve on the right. Cap at 3/5 of the area so a long
    // worktree+branch label can never starve the tab strip on a
    // moderately-sized terminal. Drop algorithm inside `render_segments`
    // shrinks the stack from the LEFT until it fits.
    let max_right = area.width.saturating_mul(3) / 5;
    let focused_agent = agents.get(focused);
    let ctx = SegmentCtx {
        repo: focused_agent.and_then(|a| a.repo.as_deref()),
        branch: focused_agent.and_then(|a| a.branch.as_deref()),
        model_effort: focused_agent.and_then(|a| a.model_effort.as_ref()),
        cwd_basename: focused_agent
            .and_then(|a| a.cwd.as_deref())
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str()),
        prefix_state,
        bindings,
        secondary: chrome.secondary,
    };
    let (segments_line, segments_width) = render_segments(segments, &ctx, max_right);

    // Reserve space for the segments on the right when there's anything
    // to draw and we have room for at least one tab cell next to it.
    let (left_area, right_area) = if segments_width > 0 && area.width > segments_width {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(1), Constraint::Length(segments_width)])
            .split(area);
        (chunks[0], Some(chunks[1]))
    } else {
        (area, None)
    };

    // Build the left half as a single Line: tab specs (each carrying its
    // own hitbox geometry) joined by separator spans. Per-tab structs let
    // us record the screen rect of each tab into `hitboxes` while
    // concatenating the visual into a single Line so ratatui clips the
    // whole thing at the area edge as a unit. Without per-tab geometry,
    // we'd have to re-derive widths from the flat span list at hit-test
    // time.
    let separator = " │ ";
    let separator_w = u16::try_from(separator.chars().count()).unwrap_or(3);

    let tabs = build_tab_strip(agents, focused, phase, chrome);
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(tabs.len().saturating_mul(4));
    let area_right = left_area.x.saturating_add(left_area.width);
    let mut x = left_area.x;
    for (i, tab) in tabs.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(separator, chrome.secondary));
            x = x.saturating_add(separator_w);
        }
        let tab_w = tab.width();
        // Clip against the left_area so a tab that bleeds past the
        // segments' reserved space (or the screen edge) records its
        // hitbox only over the cells actually drawn. A zero-width
        // result is dropped by `TabHitboxes::record`.
        let avail = area_right.saturating_sub(x);
        let clipped = tab_w.min(avail);
        hitboxes.record(
            Rect {
                x,
                y: left_area.y,
                width: clipped,
                height: 1,
            },
            tab.agent_id.clone(),
        );
        spans.extend(tab.spans.iter().cloned());
        x = x.saturating_add(tab_w);
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), left_area);

    if let Some(area) = right_area {
        let widget = Paragraph::new(segments_line).alignment(Alignment::Right);
        frame.render_widget(widget, area);
    }
}

/// One tab's worth of spans, decoupled from the separators between
/// tabs. The renderer needs per-tab geometry (to record click hitboxes
/// against [`TabHitboxes`]) but the eye-friendly Paragraph rendering
/// still wants the whole strip as a single `Line`. Returning a struct
/// per tab is the seam: the renderer concatenates with separators, and
/// it knows the natural width of each tab via [`Self::width`].
struct TabSpec {
    agent_id: AgentId,
    spans: Vec<Span<'static>>,
}

impl TabSpec {
    /// Display-cell width of this tab's span sequence (unicode-aware
    /// via `Span::width`). Sum is `usize` upstream; we clamp to `u16`
    /// because `Rect` is `u16` everywhere.
    fn width(&self) -> u16 {
        let total: usize = self.spans.iter().map(Span::width).sum();
        u16::try_from(total).unwrap_or(u16::MAX)
    }
}

/// Build the styled spans for each tab in the status-bar tab strip.
/// Focused tab gets reverse-video + bold so the eye lands on it
/// immediately; others render dim so the focused tab pops without
/// having to look at a marker character. The renderer composes these
/// into a single `Line` with `" │ "` separators between adjacent tabs
/// — close enough to the browser tab convention to read as "tabs"
/// rather than "list of items" — while recording per-tab click
/// hitboxes from the geometry [`TabSpec::width`] exposes.
fn build_tab_strip(
    agents: &[RuntimeAgent],
    focused: usize,
    phase: AnimationPhase,
    chrome: &ChromeStyle,
) -> Vec<TabSpec> {
    agents
        .iter()
        .enumerate()
        .map(|(i, agent)| {
            let focused_tab = i == focused;
            let mut spans: Vec<Span<'static>> = Vec::with_capacity(3);
            spans.push(Span::styled(
                format!(" {} ", i + 1),
                tab_index_style(focused_tab, chrome),
            ));
            spans.extend(agent_label_spans(agent, focused_tab, phase, chrome));
            spans.push(Span::styled(" ", tab_index_style(focused_tab, chrome)));
            TabSpec {
                agent_id: agent.id.clone(),
                spans,
            }
        })
        .collect()
}

fn tab_index_style(focused: bool, chrome: &ChromeStyle) -> Style {
    if focused {
        Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
    } else {
        chrome.secondary
    }
}

/// Wall-clock-derived animation state. Computed once per render in
/// [`render_frame`] and threaded down to every label site so the
/// per-tab spinner and the slow-blink "needs attention" cue stay in
/// lockstep across the navigator. Pure data so tests can construct
/// an arbitrary phase without touching the clock.
#[derive(Clone, Copy, Debug, Default)]
struct AnimationPhase {
    /// Current spinner frame index. Cycled at ~10 Hz (one frame per
    /// 100 ms); modulo'd against [`SPINNER_FRAMES.len()`] in
    /// [`from_elapsed`].
    spinner_frame: usize,
    /// Whether the slow-blink cue is in its *bright* half-cycle this
    /// tick. Toggles every [`BLINK_HALF_CYCLE_MS`] — currently
    /// 1500 ms (3-second full period). Slow heartbeat: noticeable
    /// in peripheral vision but never feels jittery.
    blink_bright: bool,
}

/// Pre-computed styles for the codemux chrome (status bar, tab strip,
/// hints, log strip — everything *around* the agent pane). Built once
/// at startup from [`crate::config::Ui`] and passed by value (it's
/// `Copy`) to every chrome renderer.
///
/// The reason it exists as a struct instead of threading the raw config
/// bool: every renderer needs the *style*, not the flag, and computing
/// the style at every span site would either duplicate the if-else or
/// scatter helper calls. One central conversion keeps the
/// "what-does-subtle-mean" decision in exactly one place.
///
/// Threaded by reference (`&ChromeStyle`) rather than by value because
/// the per-host accent map prevents `Copy`. Cloning a `HashMap` each
/// frame would be silly for what is fundamentally configuration.
#[derive(Clone, Debug)]
struct ChromeStyle {
    /// Used for separators, hints, host prefix on hosts without an
    /// explicit accent, log strip, unfocused tab body — anything that
    /// should read as "ambient context" rather than primary content.
    /// See [`Self::from_ui`] for the two modes.
    secondary: Style,
    /// Pre-computed per-host accent styles, indexed by host name. The
    /// host prefix on an *unfocused* tab uses
    /// [`Self::host_style`], which falls back to `secondary` when the
    /// host isn't configured. Focused tabs ignore this and inherit the
    /// reverse-video tab highlight regardless.
    host_styles: std::collections::HashMap<String, Style>,
}

impl ChromeStyle {
    /// Default chrome (`subtle = false`): a fixed xterm-256 gray
    /// (`Indexed(247)`, the same value [`BLINK_DIM`] uses for the
    /// attention pulse). Deterministic across terminals and visible on
    /// poor monitors — the conservative choice.
    ///
    /// Subtle chrome (`subtle = true`): the original `DarkGray + DIM`
    /// look. `DIM` is ANSI's "decreased intensity" but each terminal
    /// renders it differently (Alacritty blends fg with bg at ~66 %,
    /// iTerm2 uses a slightly darker color, some terminals ignore it)
    /// so it's a poor default but a fine opt-in for users who like the
    /// quieter look on a high-contrast display.
    ///
    /// Per-host accents are pre-computed into `Style` values once here
    /// so lookups in the render hot path are O(1) `HashMap` reads with no
    /// per-frame conversion.
    fn from_ui(ui: &crate::config::Ui) -> Self {
        let secondary = if ui.subtle {
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(Color::Indexed(247))
        };
        let host_styles = ui
            .host_colors
            .iter()
            .map(|(host, color)| (host.clone(), Style::default().fg(color.to_color())))
            .collect();
        Self {
            secondary,
            host_styles,
        }
    }

    /// Style for a host prefix on an unfocused tab. Returns the
    /// configured accent if the user assigned one to this host;
    /// otherwise the secondary chrome style (so unconfigured hosts
    /// quietly blend with the rest of the chrome rather than shouting).
    fn host_style(&self, host: &str) -> Style {
        self.host_styles
            .get(host)
            .copied()
            .unwrap_or(self.secondary)
    }
}

#[cfg(test)]
impl Default for ChromeStyle {
    /// Tests construct chrome by default; the value matches
    /// `from_ui(&Ui::default())`. Production code never hits this —
    /// the runtime always builds chrome from the actual user config.
    fn default() -> Self {
        Self::from_ui(&crate::config::Ui::default())
    }
}

/// Half-period of the "needs attention" blink, in milliseconds. A
/// 1500 ms half-cycle gives a 3-second full pulse — the user
/// reported the previous 500 ms felt too fast and the colors swung
/// too far. Both ends now sit in the light-grey range
/// (see [`body_style`]) so the swing reads as a gentle pulse rather
/// than a strobe.
const BLINK_HALF_CYCLE_MS: u128 = 1500;

/// Bright end of the slow-blink pulse — xterm 256-color index 252
/// is a light grey, just below white. Paired with [`BLINK_DIM`]
/// (~5 steps darker) for a gentle swing that still reads as a
/// distinct cue against the surrounding `DIM` sibling tabs.
const BLINK_BRIGHT: Color = Color::Indexed(252);

/// Dim end of the slow-blink pulse — xterm 256-color index 247
/// is a slightly darker light grey. Sits comfortably above the
/// `DarkGray` baseline used elsewhere so the dim phase is still
/// clearly visible.
const BLINK_DIM: Color = Color::Indexed(247);

impl AnimationPhase {
    fn from_elapsed(elapsed: Duration) -> Self {
        let ms = elapsed.as_millis();
        // Bounded by `% SPINNER_FRAMES.len() as u128` (= % 8), so the
        // result always fits in usize on every platform we target.
        #[allow(clippy::cast_possible_truncation)]
        let spinner_frame = (ms / 100 % SPINNER_FRAMES.len() as u128) as usize;
        Self {
            spinner_frame,
            blink_bright: (ms / BLINK_HALF_CYCLE_MS).is_multiple_of(2),
        }
    }

    fn spinner_glyph(self) -> &'static str {
        // Bounds-safe by construction: `spinner_frame` is computed
        // modulo `SPINNER_FRAMES.len()` in `from_elapsed`. Tests can
        // build phases directly, so guard with `min` rather than an
        // index panic if a future test passes an out-of-range value.
        SPINNER_FRAMES[self.spinner_frame.min(SPINNER_FRAMES.len() - 1)]
    }
}

/// Braille spinner that uses 7-of-8 dots per frame — the rotation
/// reads as a missing dot moving around the cell, with the result
/// that every frame fills most of the character cell vertically.
/// Picked over the upper-half "dots" set Claude itself uses
/// (`⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏`) because those frames live in the top half of
/// the cell and visibly drift up against the navigator's tab strip
/// — the user reported the spinner "touching the top border."
/// Eight frames at 100 ms each is a clean 1.25 Hz rotation.
const SPINNER_FRAMES: [&str; 8] = ["⣾", "⣽", "⣻", "⢿", "⡿", "⣟", "⣯", "⣷"];

/// Smart per-agent label rendered as styled spans. The shape is
/// `[⠋ ][host · ][● ]<repo>: <title>`, where:
///
/// - `⠋` is a Braille spinner frame, rendered when the foreground
///   process is mid-turn (its OSC title carries Claude's spinner /
///   ✱ glyph). Cycles via [`AnimationPhase`] at ~10 Hz so the motion
///   reads as ambient liveness.
/// - `host` (dim/gray) is shown only for SSH-backed agents so the
///   user can tell at a glance which devpod the agent lives on
/// - `●` is a soft pulsing dot rendered only when an unfocused tab
///   needs attention (its agent finished a turn since the user was
///   last there). Pulses on the same 1 Hz cycle as the body itself
///   so the two cues read as a single signal.
/// - `repo` is the git-root basename for local agents (or cwd
///   basename when the cwd isn't inside a git repo) and the cwd
///   basename for remote agents
/// - `title` is the live OSC 0 / OSC 2 window title the foreground
///   process emits — typically Claude Code's "current task" line
///
/// When the agent has no live title yet (fresh spawn, or the
/// foreground process never emits one) the renderer falls back to
/// the static `agent.label` so the tab still has something readable.
/// Focused tabs reuse the surrounding tab-strip styling so the host
/// prefix doesn't fight the reverse-video highlight; unfocused tabs
/// get the dim host prefix to keep the repo+title the visual anchor.
fn agent_label_spans(
    agent: &RuntimeAgent,
    focused: bool,
    phase: AnimationPhase,
    chrome: &ChromeStyle,
) -> Vec<Span<'static>> {
    // Attention only applies to unfocused tabs; the &&!focused guard
    // is belt-and-braces against a stale flag (the per-frame
    // transition detector already skips the focused index).
    let attention = agent.needs_attention && !focused;
    label_spans(
        agent.host.as_deref(),
        &agent_body_text(agent),
        agent.is_working(),
        attention,
        focused,
        phase,
        chrome,
    )
}

/// Pure-data version of [`agent_label_spans`]. The wrapper resolves
/// per-agent state into primitives; this function does the rendering.
/// Split so unit tests can exercise every (host / working / attention /
/// focused) permutation without needing a real `RuntimeAgent` (which
/// requires an `AgentTransport` to reach the working spinner branch).
fn label_spans(
    host: Option<&str>,
    body: &str,
    working: bool,
    attention: bool,
    focused: bool,
    phase: AnimationPhase,
    chrome: &ChromeStyle,
) -> Vec<Span<'static>> {
    let body_style = body_style(focused, attention, phase, chrome);

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(6);

    // Spinner is the leftmost glyph so the working cue lands in the
    // same column on every tab — host-prefixed and not — and so the
    // motion reads independent of the per-host accent next to it.
    // `Color::Gray` *without* DIM on unfocused tabs makes the spinner
    // clearly visible against the surrounding DIM label — the user
    // reported the previous DarkGray+DIM was too faint to read at a
    // glance. On focused tabs we inherit the tab's reverse video so
    // the cue stays visible without breaking the tab-highlight read.
    if working {
        let spinner_style = if focused {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray)
        };
        spans.push(Span::styled(
            format!("{} ", phase.spinner_glyph()),
            spinner_style,
        ));
    }

    if let Some(host) = host {
        // Focused tabs inherit the reverse-video tab highlight so the
        // host doesn't fight it; unfocused tabs use the per-host accent
        // (or fall back to secondary chrome when the user hasn't
        // configured a color for this host).
        let host_style = if focused {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            chrome.host_style(host)
        };
        spans.push(Span::styled(format!("{host} · "), host_style));
    }

    // Attention dot pulses with the body — same style → reads as
    // one signal, not two competing animations.
    if attention {
        spans.push(Span::styled("● ", body_style));
    }

    spans.push(Span::styled(body.to_string(), body_style));
    spans
}

/// Pick a body style based on focus + attention + the current blink
/// phase. The matrix:
///
/// - **focused** → reverse-video bold, ignoring attention (the user
///   is already there; the blink is moot)
/// - **unfocused, attention, bright phase** → [`BLINK_BRIGHT`]
/// - **unfocused, attention, dim phase** → [`BLINK_DIM`]
/// - **unfocused, no attention** → [`ChromeStyle::secondary`] (default
///   chrome: a fixed gray; subtle chrome: `DarkGray + DIM`)
///
/// Both blink ends sit in the light-grey range (256-color indices
/// 252 / 247 — the upper third of the xterm grayscale ramp). The
/// resulting pulse stays well above the surrounding chrome siblings
/// so the cue is unmistakable, but the swing between the two ends
/// is small enough to read as a heartbeat rather than a strobe.
fn body_style(
    focused: bool,
    attention: bool,
    phase: AnimationPhase,
    chrome: &ChromeStyle,
) -> Style {
    if focused {
        Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
    } else if attention {
        let fg = if phase.blink_bright {
            BLINK_BRIGHT
        } else {
            BLINK_DIM
        };
        Style::default().fg(fg)
    } else {
        chrome.secondary
    }
}

/// The non-host portion of the label: `<repo>: <title>` when both
/// pieces are available, just `<title>` when there's no repo, just
/// `<repo>` when there's no title, and the static `label` as the
/// last resort.
fn agent_body_text(agent: &RuntimeAgent) -> String {
    body_text(agent.title(), agent.repo.as_deref(), &agent.label)
}

/// Pure-data version of [`agent_body_text`]. Split out so the four
/// (repo, title) permutations can be tested without needing a Ready
/// agent (which can only carry a live title via a real PTY parser).
fn body_text(title: Option<&str>, repo: Option<&str>, fallback: &str) -> String {
    match (repo, title) {
        (Some(repo), Some(title)) => format!("{repo}: {title}"),
        (Some(repo), None) => repo.to_string(),
        (None, Some(title)) => title.to_string(),
        (None, None) => fallback.to_string(),
    }
}

/// Build the title we want the *outer* terminal emulator (Ghostty,
/// iTerm2, Kitty, …) to display for its codemux tab. Format is
/// `[glyph ]host \u{00b7} body` for SSH agents and `[glyph ]body` for
/// local. Mirrors the shape of [`agent_label_spans`] (same host
/// prefix, same body, optional leading spinner glyph) but drops the
/// styling (attention dot, accent colors) the host terminal can't
/// render in its tab bar anyway.
///
/// `working_glyph` is `Some(frame)` when the focused agent is mid-turn
/// — the runtime passes the current [`AnimationPhase::spinner_glyph`]
/// so the host tab title spins in lockstep with the in-app spinner.
/// `None` produces a steady title; the dedup loop in `event_loop` then
/// keeps the OSC stream silent until the body text actually changes.
///
/// Pure on its inputs so tests can exercise every (working / host /
/// body) permutation without standing up a `RuntimeAgent` carrying a
/// live PTY parser.
fn host_terminal_title(host: Option<&str>, body: &str, working_glyph: Option<&str>) -> String {
    let prefix = working_glyph.map(|g| format!("{g} ")).unwrap_or_default();
    match host {
        Some(h) if !h.is_empty() => format!("{prefix}{h} \u{00b7} {body}"),
        _ => format!("{prefix}{body}"),
    }
}

/// Resolve the focused agent's host + body text and compose the title
/// to ship to the outer terminal. `None` when there are no agents
/// (the runtime is about to exit and the title is irrelevant). When
/// the focused agent is mid-turn, prefixes the title with the current
/// spinner frame from `phase` so the host terminal's tab bar gains
/// the same animated working cue we render inside the navigator.
fn host_terminal_title_for_focused(nav: &NavState, phase: AnimationPhase) -> Option<String> {
    let agent = nav.agents.get(nav.focused)?;
    let working_glyph = agent.is_working().then(|| phase.spinner_glyph());
    Some(host_terminal_title(
        agent.host.as_deref(),
        &agent_body_text(agent),
        working_glyph,
    ))
}

fn render_switcher_popup(
    frame: &mut Frame<'_>,
    area: Rect,
    agents: &[RuntimeAgent],
    selection: usize,
    phase: AnimationPhase,
    chrome: &ChromeStyle,
) {
    let popup_area = centered_rect(50, 60, area);
    frame.render_widget(Clear, popup_area);
    let lines: Vec<Line> = agents
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let prefix = if i == selection { "> " } else { "  " };
            let mut spans: Vec<Span<'static>> = Vec::with_capacity(4);
            spans.push(Span::raw(format!("{prefix}[{}] ", i + 1)));
            spans.extend(agent_label_spans(a, false, phase, chrome));
            Line::from(spans)
        })
        .collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" switch agent ");
    frame.render_widget(Paragraph::new(lines).block(block), popup_area);
}

fn render_help(frame: &mut Frame<'_>, area: Rect, bindings: &Bindings) {
    let popup_area = centered_rect_with_size(64, 50, area);
    frame.render_widget(Clear, popup_area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" codemux help ");
    let inner = block.inner(popup_area);
    frame.render_widget(block, popup_area);

    let mut lines: Vec<Line<'static>> = Vec::new();
    let header_style = Style::default().add_modifier(Modifier::BOLD);

    lines.push(Line::styled(
        format!("prefix:  {}", bindings.prefix),
        header_style,
    ));
    lines.push(Line::raw(""));

    // Direct binds appear first because they're the fast path. The
    // help screen ordering reflects "what the user reaches for most."
    lines.push(Line::styled("direct (no prefix):", header_style));
    for action in DirectAction::ALL {
        lines.push(binding_line(
            bindings.on_direct.binding_for(*action),
            action.description(),
        ));
    }
    lines.push(Line::raw(""));

    lines.push(Line::styled("in prefix mode:", header_style));
    for action in PrefixAction::ALL {
        lines.push(binding_line(
            bindings.on_prefix.binding_for(*action),
            action.description(),
        ));
    }
    lines.push(binding_line_static(
        "1-9",
        "focus agent by one-indexed position",
    ));
    lines.push(Line::raw(""));

    lines.push(Line::styled("in agent switcher popup:", header_style));
    for action in PopupAction::ALL {
        lines.push(binding_line(
            bindings.on_popup.binding_for(*action),
            action.description(),
        ));
    }
    lines.push(Line::raw(""));

    lines.push(Line::styled("in spawn minibuffer:", header_style));
    for action in ModalAction::ALL {
        lines.push(binding_line(
            bindings.on_modal.binding_for(*action),
            action.description(),
        ));
    }
    lines.push(Line::raw(""));

    lines.push(Line::styled("in scroll mode:", header_style));
    lines.push(binding_line_static(
        "wheel",
        "wheel up enters scroll mode; wheel down exits at the bottom",
    ));
    for action in ScrollAction::ALL {
        lines.push(binding_line(
            bindings.on_scroll.binding_for(*action),
            action.description(),
        ));
    }
    lines.push(binding_line_static(
        "type",
        "typing real text snaps to live and forwards (nav chords preserve scroll)",
    ));
    lines.push(Line::raw(""));

    lines.push(Line::styled("mouse:", header_style));
    lines.push(binding_line_static(
        "click",
        "click a tab to focus it (no prefix needed)",
    ));
    lines.push(binding_line_static(
        "drag tab",
        "drag a tab onto another to reorder (browser-tab semantics)",
    ));
    lines.push(binding_line_static(
        "drag pane",
        "select text in the agent pane; release copies via OSC 52",
    ));
    lines.push(binding_line_static(
        "alt+drag",
        "fallback to terminal-native selection (works without OSC 52)",
    ));
    lines.push(Line::raw(""));
    lines.push(Line::raw("press any key to close"));

    frame.render_widget(Paragraph::new(lines), inner);
}

fn binding_line(chord: crate::keymap::KeyChord, description: &str) -> Line<'static> {
    Line::raw(format!("  {:<10}  {}", chord.to_string(), description))
}

fn binding_line_static(chord: &str, description: &str) -> Line<'static> {
    Line::raw(format!("  {chord:<10}  {description}"))
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let [_, vertical_middle, _] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .areas(r);
    let [_, center, _] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .areas(vertical_middle);
    center
}

fn centered_rect_with_size(width: u16, height: u16, r: Rect) -> Rect {
    let x = r.x + (r.width.saturating_sub(width)) / 2;
    let y = r.y + (r.height.saturating_sub(height)) / 2;
    Rect {
        x,
        y,
        width: width.min(r.width),
        height: height.min(r.height),
    }
}

/// Wrap a pasted text chunk in bracketed-paste markers
/// (`\x1b[200~ ... \x1b[201~`) for forwarding to a child that
/// advertised `?2004h`.
///
/// ESC bytes in the payload are stripped first: an embedded
/// `\x1b[201~` would close paste mode early in the child and let the
/// trailing bytes land as live keystrokes — the standard
/// terminal-injection vector. xterm's `disallowedPasteControls`
/// default does the same; legitimate text pastes don't carry control
/// sequences.
fn wrap_paste(text: &str) -> Vec<u8> {
    let sanitized: Vec<u8> = text.bytes().filter(|&b| b != 0x1b).collect();
    [b"\x1b[200~", sanitized.as_slice(), b"\x1b[201~"].concat()
}

/// Translate a crossterm key event into the bytes a terminal-mode child
/// process expects.
///
/// Two-layer pipeline (see AD-28):
///   1. [`translate_readline_shortcut`] — opinionated GUI-chord →
///      readline-byte-sequence adapter. Recognized shortcuts (Cmd+
///      Backspace, Shift+Enter, etc.) short-circuit here so the
///      child sees the byte sequence its readline-style input
///      handler expects.
///   2. [`encode_terminal_key`] — pure VT100/ANSI key encoder, no
///      modifier opinions. Reached only when the chord wasn't a
///      recognized readline shortcut.
fn key_to_bytes(code: KeyCode, modifiers: KeyModifiers) -> Option<Vec<u8>> {
    translate_readline_shortcut(code, modifiers).or_else(|| encode_terminal_key(code, modifiers))
}

/// Pure VT100 / ANSI key encoder. **No modifier opinions.** This
/// function maps a key event to the wire bytes a generic terminal-mode
/// child expects when there is no special chord meaning to apply —
/// `Backspace → DEL`, `Up → ESC[A`, `Char('a') → 'a'`, and so on.
///
/// `Char + CTRL` is the one place modifiers do alter the encoding,
/// because `Ctrl-letter` is itself a primitive VT100 control byte
/// (Ctrl-C = 0x03, etc.) — that's protocol, not opinion.
///
/// All higher-level "Cmd+Backspace means delete-line" or "Shift+Enter
/// means newline-in-input" mappings live in [`translate_readline_shortcut`]
/// instead. Keeping this layer pristine means readers of the encoder
/// aren't surprised by GUI-flavored translations bleeding in, and the
/// shortcut adapter is independently testable / extensible.
fn encode_terminal_key(code: KeyCode, modifiers: KeyModifiers) -> Option<Vec<u8>> {
    match code {
        KeyCode::Char(c) if modifiers.contains(KeyModifiers::CONTROL) => {
            let lower = c.to_ascii_lowercase();
            lower
                .is_ascii_alphabetic()
                .then(|| vec![(lower as u8) - b'a' + 1])
        }
        KeyCode::Char(c) => Some(c.to_string().into_bytes()),
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::BackTab => Some(vec![0x1b, b'[', b'Z']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(vec![0x1b, b'[', b'A']),
        KeyCode::Down => Some(vec![0x1b, b'[', b'B']),
        KeyCode::Right => Some(vec![0x1b, b'[', b'C']),
        KeyCode::Left => Some(vec![0x1b, b'[', b'D']),
        KeyCode::Home => Some(vec![0x1b, b'[', b'H']),
        KeyCode::End => Some(vec![0x1b, b'[', b'F']),
        KeyCode::PageUp => Some(vec![0x1b, b'[', b'5', b'~']),
        KeyCode::PageDown => Some(vec![0x1b, b'[', b'6', b'~']),
        KeyCode::Delete => Some(vec![0x1b, b'[', b'3', b'~']),
        KeyCode::Insert => Some(vec![0x1b, b'[', b'2', b'~']),
        _ => None,
    }
}

/// Readline-shortcut adapter — the **deliberately opinionated** layer
/// that bridges GUI-style keyboard chords (Cmd+Backspace, Shift+Enter,
/// Ctrl+Backspace, …) to the byte sequences a readline-style TUI text
/// input understands.
///
/// Returns `Some(bytes)` only when `(code, modifiers)` matches a
/// recognized shortcut; `None` otherwise, signaling the caller to fall
/// through to plain [`encode_terminal_key`].
///
/// **This function leaks GUI conventions onto the wire on purpose.**
/// Claude (and every other readline-style TUI input we target) speaks
/// the universal readline byte vocabulary — `Ctrl+U` for line-discard,
/// `Meta+DEL` for word-rubout, `Meta+Enter` for newline-in-input — but
/// not the Kitty Keyboard Protocol's CSI-u extended encoding for
/// modified non-character keys. Preserving "fidelity" by passing
/// `Cmd+Backspace` through verbatim would land literal escape garbage
/// in Claude's input field. The job of this layer is precisely to
/// translate user intent into the bytes the child can act on.
///
/// AD-28 captures the rationale and acceptance criteria for adding
/// new shortcuts here.
///
/// Recognized today:
///
/// | Chord                           | Bytes        | Readline name        |
/// |---------------------------------|--------------|----------------------|
/// | `Cmd+Backspace`                 | `\x15`       | `unix-line-discard`  |
/// | `Ctrl+Backspace`/`Alt+Backspace`| `\x1b\x7f`   | `unix-word-rubout`   |
/// | `(Shift\|Alt\|Ctrl\|Cmd)+Enter` | `\x1b\r`     | `meta-enter` newline |
fn translate_readline_shortcut(code: KeyCode, modifiers: KeyModifiers) -> Option<Vec<u8>> {
    // SUPER wins when both Cmd and Ctrl/Alt are held — a Mac user
    // pressing Cmd+Backspace means "kill the line", not "kill the
    // word and also Cmd". Cmd events only reach us at all because
    // the default config triggers Kitty Keyboard Protocol negotiation;
    // on terminals that can't deliver SUPER the user falls back to
    // the Ctrl/Alt variants.
    //
    // Modified Enter — any of Shift/Alt/Ctrl/Super — is the universal
    // "I want a newline, not submit" intent. ESC+CR is what
    // iTerm/Terminal.app emit for Option+Enter when "Use Option as
    // Meta" is on, and what every readline-style input handler treats
    // as in-input newline. Without this branch every modified Enter
    // chord lands as plain `\r` and submits the message.
    const NEWLINE_INTENT_MODS: KeyModifiers = KeyModifiers::SHIFT
        .union(KeyModifiers::ALT)
        .union(KeyModifiers::CONTROL)
        .union(KeyModifiers::SUPER);
    match code {
        KeyCode::Backspace if modifiers.contains(KeyModifiers::SUPER) => Some(vec![0x15]),
        KeyCode::Backspace if modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) => {
            Some(vec![0x1b, 0x7f])
        }
        KeyCode::Enter if modifiers.intersects(NEWLINE_INTENT_MODS) => Some(vec![0x1b, b'\r']),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    // ---- encode_terminal_key (pure VT100 layer, no modifier opinions) ----
    //
    // These tests pin the architectural invariant: the encoder must
    // produce the same bytes regardless of GUI-style modifiers (SUPER,
    // SHIFT, ALT). Any opinion about how Cmd/Shift/Alt should re-mean
    // a key belongs in `translate_readline_shortcut`. If a future
    // change adds modifier-branching here, this layer's contract has
    // been violated and these tests should fail loud.

    #[test]
    fn encode_terminal_key_has_no_modifier_opinion_on_backspace() {
        for modifier in [
            KeyModifiers::NONE,
            KeyModifiers::SUPER,
            KeyModifiers::CONTROL,
            KeyModifiers::ALT,
            KeyModifiers::SUPER | KeyModifiers::CONTROL,
        ] {
            assert_eq!(
                encode_terminal_key(KeyCode::Backspace, modifier),
                Some(vec![0x7f]),
                "encoder must stay opinion-free for Backspace+{modifier:?}",
            );
        }
    }

    #[test]
    fn encode_terminal_key_has_no_modifier_opinion_on_enter() {
        for modifier in [
            KeyModifiers::NONE,
            KeyModifiers::SHIFT,
            KeyModifiers::ALT,
            KeyModifiers::SUPER,
        ] {
            assert_eq!(
                encode_terminal_key(KeyCode::Enter, modifier),
                Some(vec![b'\r']),
                "encoder must stay opinion-free for Enter+{modifier:?}",
            );
        }
    }

    #[test]
    fn encode_terminal_key_keeps_ctrl_letter_as_protocol() {
        // The one place modifiers matter at the encoder level: Ctrl+letter
        // is itself a primitive control byte (Ctrl-C = 0x03). That's
        // protocol, not opinion, and stays in the encoder.
        assert_eq!(
            encode_terminal_key(KeyCode::Char('c'), KeyModifiers::CONTROL),
            Some(vec![0x03]),
        );
    }

    // ---- translate_readline_shortcut (opinionated GUI→readline layer) ----
    //
    // The contract: returns Some(bytes) only for recognized GUI-style
    // chords; None otherwise so the orchestrator falls through to the
    // encoder. New shortcuts (Cmd+Right, Cmd+A, etc.) get added here,
    // never in the encoder. Each test below pins one chord's mapping.

    #[test]
    fn shortcut_returns_none_for_unmodified_keys() {
        // Plain Backspace, plain Enter, plain arrows — none are
        // shortcuts; orchestrator must reach the encoder for them.
        for code in [
            KeyCode::Backspace,
            KeyCode::Enter,
            KeyCode::Up,
            KeyCode::Down,
            KeyCode::Char('a'),
        ] {
            assert_eq!(
                translate_readline_shortcut(code, KeyModifiers::NONE),
                None,
                "{code:?} (no modifiers) must not be a shortcut",
            );
        }
    }

    #[test]
    fn shortcut_returns_none_for_keys_with_no_registered_shortcut() {
        // Cmd+Up has no registered shortcut today — falls through to
        // the encoder, which sends the plain arrow CSI. If a future
        // change adds a shortcut for it, update this test.
        assert_eq!(
            translate_readline_shortcut(KeyCode::Up, KeyModifiers::SUPER),
            None,
        );
    }

    #[test]
    fn shortcut_cmd_backspace_is_unix_line_discard() {
        assert_eq!(
            translate_readline_shortcut(KeyCode::Backspace, KeyModifiers::SUPER),
            Some(vec![0x15]),
        );
    }

    #[test]
    fn shortcut_ctrl_or_alt_backspace_is_unix_word_rubout() {
        for modifier in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
            assert_eq!(
                translate_readline_shortcut(KeyCode::Backspace, modifier),
                Some(vec![0x1b, 0x7f]),
                "wrong bytes for {modifier:?}+Backspace",
            );
        }
    }

    #[test]
    fn shortcut_super_wins_over_ctrl_for_backspace() {
        // Precedence rule: a Mac user pressing Cmd+Backspace means
        // "line delete", not "word delete plus extra modifiers".
        assert_eq!(
            translate_readline_shortcut(
                KeyCode::Backspace,
                KeyModifiers::SUPER | KeyModifiers::CONTROL,
            ),
            Some(vec![0x15]),
        );
    }

    #[test]
    fn shortcut_modified_enter_is_meta_enter_for_newline_in_input() {
        for modifier in [
            KeyModifiers::SHIFT,
            KeyModifiers::ALT,
            KeyModifiers::CONTROL,
            KeyModifiers::SUPER,
        ] {
            assert_eq!(
                translate_readline_shortcut(KeyCode::Enter, modifier),
                Some(vec![0x1b, b'\r']),
                "wrong bytes for {modifier:?}+Enter",
            );
        }
    }

    // ---- key_to_bytes (orchestrator: shortcut first, encoder fallback) ----
    //
    // These tests exercise the composed pipeline. They duplicate a
    // few of the per-layer assertions on purpose — the value is
    // catching wiring regressions (e.g. someone reordering the
    // `or_else` chain) that the per-layer tests can't see.

    #[test]
    fn plain_ascii_char_passes_through_as_one_byte() {
        assert_eq!(
            key_to_bytes(KeyCode::Char('A'), KeyModifiers::NONE),
            Some(vec![b'A'])
        );
    }

    #[test]
    fn ctrl_a_through_ctrl_z_map_to_0x01_through_0x1a() {
        for (i, c) in ('a'..='z').enumerate() {
            let bytes = key_to_bytes(KeyCode::Char(c), KeyModifiers::CONTROL).unwrap();
            let expected = u8::try_from(i + 1).unwrap();
            assert_eq!(bytes, vec![expected], "wrong byte for ctrl-{c}");
        }
    }

    #[test]
    fn enter_is_a_carriage_return() {
        assert_eq!(
            key_to_bytes(KeyCode::Enter, KeyModifiers::NONE),
            Some(vec![b'\r'])
        );
    }

    #[test]
    fn modified_enter_emits_meta_enter_for_in_input_newline() {
        // Pipeline-level pin: a future regression that drops the
        // Enter arm from the shortcut layer would silently submit
        // every Cmd/Shift/Ctrl+Enter as a plain `\r`. Per-layer
        // tests cover the shortcut function directly; this one
        // covers the wiring.
        for modifier in [
            KeyModifiers::SHIFT,
            KeyModifiers::ALT,
            KeyModifiers::CONTROL,
            KeyModifiers::SUPER,
        ] {
            assert_eq!(
                key_to_bytes(KeyCode::Enter, modifier),
                Some(vec![0x1b, b'\r']),
                "wrong bytes for modified Enter ({modifier:?})"
            );
        }
    }

    #[test]
    fn wrap_paste_emits_brackets_around_plain_text() {
        assert_eq!(
            wrap_paste("hello\nworld"),
            b"\x1b[200~hello\nworld\x1b[201~".to_vec(),
        );
    }

    #[test]
    fn wrap_paste_strips_embedded_esc_to_block_end_marker_injection() {
        // Without sanitization, the embedded `\x1b[201~` would close
        // paste mode in the child PTY and the bytes after it would
        // land as live keystrokes — the standard injection vector.
        let wrapped = wrap_paste("ok\x1b[201~rm -rf");
        let s = String::from_utf8(wrapped).unwrap();
        assert_eq!(s, "\x1b[200~ok[201~rm -rf\x1b[201~");
        assert_eq!(s.matches("\x1b[201~").count(), 1);
    }

    #[test]
    fn arrow_keys_emit_csi_letter_sequences() {
        assert_eq!(
            key_to_bytes(KeyCode::Up, KeyModifiers::NONE),
            Some(vec![0x1b, b'[', b'A'])
        );
        assert_eq!(
            key_to_bytes(KeyCode::Down, KeyModifiers::NONE),
            Some(vec![0x1b, b'[', b'B'])
        );
    }

    #[test]
    fn plain_backspace_emits_del() {
        assert_eq!(
            key_to_bytes(KeyCode::Backspace, KeyModifiers::NONE),
            Some(vec![0x7f])
        );
    }

    #[test]
    fn ctrl_or_alt_backspace_emits_meta_del_for_word_delete() {
        // Pipeline-level pin: the shortcut layer's word-rubout
        // mapping must reach the wire untouched.
        for modifier in [KeyModifiers::CONTROL, KeyModifiers::ALT] {
            assert_eq!(
                key_to_bytes(KeyCode::Backspace, modifier),
                Some(vec![0x1b, 0x7f]),
                "wrong bytes for {modifier:?}+Backspace",
            );
        }
        assert_eq!(
            key_to_bytes(
                KeyCode::Backspace,
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ),
            Some(vec![0x1b, 0x7f]),
        );
    }

    #[test]
    fn cmd_backspace_emits_ctrl_u_for_line_delete() {
        assert_eq!(
            key_to_bytes(KeyCode::Backspace, KeyModifiers::SUPER),
            Some(vec![0x15])
        );
    }

    #[test]
    fn cmd_wins_over_ctrl_when_both_modify_backspace() {
        assert_eq!(
            key_to_bytes(
                KeyCode::Backspace,
                KeyModifiers::SUPER | KeyModifiers::CONTROL,
            ),
            Some(vec![0x15]),
        );
    }

    #[test]
    fn unmapped_key_is_dropped() {
        assert_eq!(key_to_bytes(KeyCode::F(12), KeyModifiers::NONE), None);
    }

    // Prefix dispatch with default bindings

    fn defaults() -> Bindings {
        Bindings::default()
    }

    #[test]
    fn idle_forwards_a_normal_char() {
        let mut state = PrefixState::Idle;
        let action = dispatch_key(
            &mut state,
            &key(KeyCode::Char('a'), KeyModifiers::NONE),
            &defaults(),
        );
        assert_eq!(action, KeyDispatch::Forward(vec![b'a']));
        assert_eq!(state, PrefixState::Idle);
    }

    #[test]
    fn idle_forwards_ctrl_c_to_pty() {
        let mut state = PrefixState::Idle;
        let action = dispatch_key(
            &mut state,
            &key(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &defaults(),
        );
        assert_eq!(action, KeyDispatch::Forward(vec![0x03]));
    }

    #[test]
    fn ctrl_b_in_idle_arms_the_state_machine() {
        let mut state = PrefixState::Idle;
        let action = dispatch_key(
            &mut state,
            &key(KeyCode::Char('b'), KeyModifiers::CONTROL),
            &defaults(),
        );
        assert_eq!(action, KeyDispatch::Consume);
        assert_eq!(state, PrefixState::AwaitingCommand);
    }

    #[test]
    fn double_prefix_forwards_a_literal_prefix_byte() {
        let mut state = PrefixState::AwaitingCommand;
        let action = dispatch_key(
            &mut state,
            &key(KeyCode::Char('b'), KeyModifiers::CONTROL),
            &defaults(),
        );
        assert_eq!(action, KeyDispatch::Forward(vec![0x02]));
    }

    #[test]
    fn prefix_q_exits() {
        let mut state = PrefixState::AwaitingCommand;
        let action = dispatch_key(
            &mut state,
            &key(KeyCode::Char('q'), KeyModifiers::NONE),
            &defaults(),
        );
        assert_eq!(action, KeyDispatch::Exit);
    }

    #[test]
    fn prefix_c_opens_spawn_modal() {
        let mut state = PrefixState::AwaitingCommand;
        let action = dispatch_key(
            &mut state,
            &key(KeyCode::Char('c'), KeyModifiers::NONE),
            &defaults(),
        );
        assert_eq!(action, KeyDispatch::SpawnAgent);
    }

    // Direct-bind dispatch (no prefix needed) — the fast path the
    // user pays for via the Cmd modifier.

    #[test]
    fn direct_cmd_apostrophe_focuses_next_without_arming_prefix() {
        let mut state = PrefixState::Idle;
        let action = dispatch_key(
            &mut state,
            &key(KeyCode::Char('\''), KeyModifiers::SUPER),
            &defaults(),
        );
        assert_eq!(action, KeyDispatch::FocusNext);
        // Crucial: state must remain Idle. If the direct bind
        // accidentally armed the prefix state machine, the next
        // keystroke would be consumed as a prefix command.
        assert_eq!(state, PrefixState::Idle);
    }

    #[test]
    fn direct_cmd_semicolon_focuses_prev() {
        let mut state = PrefixState::Idle;
        let action = dispatch_key(
            &mut state,
            &key(KeyCode::Char(';'), KeyModifiers::SUPER),
            &defaults(),
        );
        assert_eq!(action, KeyDispatch::FocusPrev);
        assert_eq!(state, PrefixState::Idle);
    }

    #[test]
    fn direct_cmd_backslash_spawns_agent_without_arming_prefix() {
        let mut state = PrefixState::Idle;
        let action = dispatch_key(
            &mut state,
            &key(KeyCode::Char('\\'), KeyModifiers::SUPER),
            &defaults(),
        );
        assert_eq!(action, KeyDispatch::SpawnAgent);
        // SpawnAgent is not a nav dispatch, so state stays Idle —
        // the user types Cmd+\ once, the modal opens, no sticky
        // mode lingers.
        assert_eq!(state, PrefixState::Idle);
    }

    #[test]
    fn plain_backslash_without_super_still_forwards_as_a_byte() {
        // Without the SUPER modifier, `\` is just a typed character
        // for the focused PTY (paths, escape sequences, shell
        // continuations all use it). The direct-bind layer only
        // fires on the configured chord, not bare keys — otherwise
        // the user couldn't type `\` into Claude Code's prompt
        // without triggering the spawn modal.
        let mut state = PrefixState::Idle;
        let action = dispatch_key(
            &mut state,
            &key(KeyCode::Char('\\'), KeyModifiers::NONE),
            &defaults(),
        );
        assert_eq!(action, KeyDispatch::Forward(vec![b'\\']));
    }

    #[test]
    fn plain_semicolon_without_super_still_forwards_as_a_byte() {
        // Without the SUPER modifier, `;` is just a typed
        // character for the focused PTY. The direct-bind layer
        // only fires on the configured chord, not bare keys.
        let mut state = PrefixState::Idle;
        let action = dispatch_key(
            &mut state,
            &key(KeyCode::Char(';'), KeyModifiers::NONE),
            &defaults(),
        );
        assert_eq!(action, KeyDispatch::Forward(vec![b';']));
    }

    // Sticky prefix mode — after Ctrl-B, repeated nav keys keep the
    // state armed so the user can `Ctrl-B h h h` without re-pressing
    // the prefix. Non-nav commands and unbound keys exit.

    #[test]
    fn prefix_then_nav_key_stays_in_awaiting_command() {
        let mut state = PrefixState::AwaitingCommand;
        let action = dispatch_key(
            &mut state,
            &key(KeyCode::Char('h'), KeyModifiers::NONE),
            &defaults(),
        );
        assert_eq!(action, KeyDispatch::FocusPrev);
        // Sticky: stays armed for repeated nav.
        assert_eq!(state, PrefixState::AwaitingCommand);
    }

    #[test]
    fn prefix_then_repeated_nav_keys_keeps_dispatching() {
        // Simulates `Ctrl-B h h h` — three FocusPrev dispatches
        // without re-pressing the prefix in between.
        let mut state = PrefixState::AwaitingCommand;
        for _ in 0..3 {
            let action = dispatch_key(
                &mut state,
                &key(KeyCode::Char('h'), KeyModifiers::NONE),
                &defaults(),
            );
            assert_eq!(action, KeyDispatch::FocusPrev);
            assert_eq!(state, PrefixState::AwaitingCommand);
        }
    }

    #[test]
    fn prefix_then_digit_stays_sticky() {
        // 1-9 is also a nav move (focus by index), so it should
        // keep us armed for further nav.
        let mut state = PrefixState::AwaitingCommand;
        let action = dispatch_key(
            &mut state,
            &key(KeyCode::Char('2'), KeyModifiers::NONE),
            &defaults(),
        );
        assert_eq!(action, KeyDispatch::FocusAt(1));
        assert_eq!(state, PrefixState::AwaitingCommand);
    }

    #[test]
    fn prefix_then_tab_stays_sticky() {
        let mut state = PrefixState::AwaitingCommand;
        let action = dispatch_key(
            &mut state,
            &key(KeyCode::Tab, KeyModifiers::NONE),
            &defaults(),
        );
        assert_eq!(action, KeyDispatch::FocusLast);
        assert_eq!(state, PrefixState::AwaitingCommand);
    }

    #[test]
    fn prefix_then_non_nav_command_exits_sticky() {
        // Spawn-agent (`c`) is a one-shot command — after dispatch
        // the state should drop back to Idle.
        let mut state = PrefixState::AwaitingCommand;
        let action = dispatch_key(
            &mut state,
            &key(KeyCode::Char('c'), KeyModifiers::NONE),
            &defaults(),
        );
        assert_eq!(action, KeyDispatch::SpawnAgent);
        assert_eq!(state, PrefixState::Idle);
    }

    #[test]
    fn prefix_then_unbound_key_exits_sticky() {
        // `z` isn't bound to anything — exits sticky and consumes.
        let mut state = PrefixState::AwaitingCommand;
        let action = dispatch_key(
            &mut state,
            &key(KeyCode::Char('z'), KeyModifiers::NONE),
            &defaults(),
        );
        assert_eq!(action, KeyDispatch::Consume);
        assert_eq!(state, PrefixState::Idle);
    }

    #[test]
    fn prefix_then_esc_exits_sticky_via_unbound_path() {
        // Esc isn't a bound action — falls through to Consume +
        // exit. This is the user-facing way to leave nav mode.
        let mut state = PrefixState::AwaitingCommand;
        let action = dispatch_key(
            &mut state,
            &key(KeyCode::Esc, KeyModifiers::NONE),
            &defaults(),
        );
        assert_eq!(action, KeyDispatch::Consume);
        assert_eq!(state, PrefixState::Idle);
    }

    #[test]
    fn prefix_h_via_alias_focuses_prev() {
        // After arming with the prefix, vim-style `h` is one of the
        // aliases (alongside tmux `p` and vim `k`) that should map
        // to FocusPrev.
        let mut state = PrefixState::AwaitingCommand;
        let action = dispatch_key(
            &mut state,
            &key(KeyCode::Char('h'), KeyModifiers::NONE),
            &defaults(),
        );
        assert_eq!(action, KeyDispatch::FocusPrev);
    }

    #[test]
    fn prefix_l_via_alias_focuses_next() {
        let mut state = PrefixState::AwaitingCommand;
        let action = dispatch_key(
            &mut state,
            &key(KeyCode::Char('l'), KeyModifiers::NONE),
            &defaults(),
        );
        assert_eq!(action, KeyDispatch::FocusNext);
    }

    #[test]
    fn prefix_tab_dispatches_focus_last() {
        let mut state = PrefixState::AwaitingCommand;
        let action = dispatch_key(
            &mut state,
            &key(KeyCode::Tab, KeyModifiers::NONE),
            &defaults(),
        );
        assert_eq!(action, KeyDispatch::FocusLast);
    }

    #[test]
    fn prefix_x_dispatches_kill_agent() {
        let mut state = PrefixState::AwaitingCommand;
        let action = dispatch_key(
            &mut state,
            &key(KeyCode::Char('x'), KeyModifiers::NONE),
            &defaults(),
        );
        assert_eq!(action, KeyDispatch::KillAgent);
    }

    #[test]
    fn prefix_d_dispatches_dismiss_agent() {
        let mut state = PrefixState::AwaitingCommand;
        let action = dispatch_key(
            &mut state,
            &key(KeyCode::Char('d'), KeyModifiers::NONE),
            &defaults(),
        );
        assert_eq!(action, KeyDispatch::DismissAgent);
    }

    // change_focus semantics — the helper that keeps `previous_focused`
    // in sync with `focused` at every user-initiated switch site.

    #[test]
    fn change_focus_records_previous_when_focus_moves() {
        let mut nav = NavState::new(Vec::new());
        nav.change_focus(2);
        assert_eq!(nav.focused, 2);
        assert_eq!(nav.previous_focused, Some(0));
    }

    #[test]
    fn change_focus_is_a_noop_when_target_is_already_focused() {
        // Critical: a no-op must not clobber `previous`. Otherwise a
        // double-tap of the same direct-bind (or pressing FocusAt(idx)
        // for an already-focused tab) would erase the bounce slot.
        let mut nav = NavState::new(Vec::new());
        nav.focused = 1;
        nav.previous_focused = Some(0);
        nav.change_focus(1);
        assert_eq!(nav.focused, 1);
        assert_eq!(nav.previous_focused, Some(0));
    }

    #[test]
    fn change_focus_lets_alt_tab_bounce_via_two_calls() {
        // Simulates: focused=0, switch to 2 (FocusAt), then FocusLast
        // bounces back to 0 — and `previous` should now point to 2 so a
        // second FocusLast bounces forward again.
        let mut nav = NavState::new(Vec::new());
        nav.change_focus(2);
        assert_eq!((nav.focused, nav.previous_focused), (2, Some(0)));
        // FocusLast handler reads `previous_focused` then calls
        // change_focus(prev).
        let bounce_target = nav.previous_focused.unwrap();
        nav.change_focus(bounce_target);
        assert_eq!((nav.focused, nav.previous_focused), (0, Some(2)));
        // Second bounce.
        let bounce_target = nav.previous_focused.unwrap();
        nav.change_focus(bounce_target);
        assert_eq!((nav.focused, nav.previous_focused), (2, Some(0)));
    }

    #[test]
    fn change_focus_clears_needs_attention_on_target() {
        // Slow-blink dismissal contract: the moment the user lands
        // on a tab that's been screaming for attention, the blink
        // stops. Otherwise the user would have to do something extra
        // ("hit a key", "wait it out") to get the navigator back to
        // a calm state — friction we deliberately want to avoid.
        let mut agents = vec![failed_agent("a"), failed_agent("b")];
        agents[1].needs_attention = true;
        let mut nav = NavState::new(agents);
        nav.change_focus(1);
        assert!(!nav.agents[1].needs_attention);
    }

    #[test]
    fn change_focus_noop_does_not_touch_needs_attention() {
        // If the user is already on tab 1 and a re-focus to 1 fires
        // (e.g. a duplicate direct-bind), needs_attention shouldn't
        // be silently flipped — the no-op semantics apply to the
        // attention bit just like to the bounce slot.
        let mut agents = vec![failed_agent("a"), failed_agent("b")];
        agents[1].needs_attention = true;
        let mut nav = NavState::new(agents);
        nav.focused = 1;
        nav.previous_focused = Some(0);
        nav.change_focus(1);
        // The interesting bit: the re-focus didn't enter the "moved"
        // branch, so the attention flag stays set. The next event-loop
        // tick will see focused==1 and not pulse, but the flag itself
        // is left to the explicit clear path on the *next* real focus
        // change.
        assert!(nav.agents[1].needs_attention);
    }

    // shift_index — the four cases of "where does an existing slot
    // land after a reorder?" Pinned because the focus-follow invariant
    // depends on these arithmetic branches being exactly right.

    #[test]
    fn shift_index_moved_slot_lands_at_destination() {
        // The dragged slot itself: i == from → to.
        assert_eq!(shift_index(2, 2, 5), 5);
        assert_eq!(shift_index(5, 5, 0), 0);
    }

    #[test]
    fn shift_index_drag_right_squeezes_in_between_slots_left() {
        // Reorder: 0 1 2 3 4 → drag(1, 3) → 0 2 3 1 4.
        // Slot 0 unchanged (outside range), 1 → 3 (the moved tab),
        // 2 → 1 (was right of from, now to the left of the destination),
        // 3 → 2 (same reason), 4 → 4 (outside range).
        assert_eq!(shift_index(0, 1, 3), 0);
        assert_eq!(shift_index(2, 1, 3), 1);
        assert_eq!(shift_index(3, 1, 3), 2);
        assert_eq!(shift_index(4, 1, 3), 4);
    }

    #[test]
    fn shift_index_drag_left_pushes_in_between_slots_right() {
        // Reorder: 0 1 2 3 4 → drag(3, 1) → 0 3 1 2 4.
        // Slot 0 unchanged, 1 → 2 (pushed right by the inserted tab),
        // 2 → 3, 3 → 1 (the moved tab), 4 → 4.
        assert_eq!(shift_index(0, 3, 1), 0);
        assert_eq!(shift_index(1, 3, 1), 2);
        assert_eq!(shift_index(2, 3, 1), 3);
        assert_eq!(shift_index(4, 3, 1), 4);
    }

    #[test]
    fn shift_index_outside_the_swap_range_is_untouched() {
        // 0 1 2 3 4 → drag(1, 3): slots 0 and 4 are untouched.
        assert_eq!(shift_index(0, 1, 3), 0);
        assert_eq!(shift_index(4, 1, 3), 4);
        assert_eq!(shift_index(7, 1, 3), 7);
    }

    #[test]
    fn shift_index_no_op_when_from_equals_to() {
        // Degenerate case: drag onto self. Every index unchanged.
        for i in 0..5 {
            assert_eq!(shift_index(i, 2, 2), i);
        }
    }

    // reorder_agents — verifies the underlying Vec mutation. Identity
    // is checked via the `label` field since RuntimeAgent has no id.

    #[test]
    fn reorder_agents_drag_right_inserts_at_destination() {
        let mut agents = vec![
            failed_agent("a"),
            failed_agent("b"),
            failed_agent("c"),
            failed_agent("d"),
        ];
        reorder_agents(&mut agents, 0, 2);
        let labels: Vec<&str> = agents.iter().map(|a| a.label.as_str()).collect();
        assert_eq!(labels, vec!["b", "c", "a", "d"]);
    }

    #[test]
    fn reorder_agents_drag_left_inserts_at_destination() {
        let mut agents = vec![
            failed_agent("a"),
            failed_agent("b"),
            failed_agent("c"),
            failed_agent("d"),
        ];
        reorder_agents(&mut agents, 3, 1);
        let labels: Vec<&str> = agents.iter().map(|a| a.label.as_str()).collect();
        assert_eq!(labels, vec!["a", "d", "b", "c"]);
    }

    #[test]
    fn reorder_agents_noop_on_self_or_out_of_range() {
        let mut agents = vec![failed_agent("a"), failed_agent("b")];
        let snapshot: Vec<String> = agents.iter().map(|a| a.label.clone()).collect();
        reorder_agents(&mut agents, 0, 0); // self
        reorder_agents(&mut agents, 5, 0); // from out of range
        reorder_agents(&mut agents, 0, 5); // to out of range
        let after: Vec<String> = agents.iter().map(|a| a.label.clone()).collect();
        assert_eq!(snapshot, after);
    }

    #[test]
    fn reorder_followed_by_shift_index_keeps_focus_on_the_moved_agent() {
        // The end-to-end invariant the renderer relies on: if I drag the
        // currently-focused tab, focus follows the tab to its new slot.
        let mut agents = vec![
            failed_agent("a"),
            failed_agent("b"),
            failed_agent("c"),
            failed_agent("d"),
        ];
        let mut focused = 1; // user is on "b"
        let mut previous = Some(0); // alt-tab buddy is "a"
        reorder_agents(&mut agents, 1, 3);
        focused = shift_index(focused, 1, 3);
        previous = previous.map(|p| shift_index(p, 1, 3));
        assert_eq!(
            agents[focused].label, "b",
            "focus should follow the moved tab"
        );
        assert_eq!(
            agents[previous.unwrap()].label,
            "a",
            "alt-tab buddy should still point at the same agent",
        );
    }

    #[test]
    fn reorder_a_non_focused_tab_past_the_focused_one_keeps_focus_pinned_to_its_agent() {
        // User is on "c". Drag "a" past "c" to slot 3. The focused
        // agent is still "c"; its index shifted from 2 → 1 because the
        // slot to its left got removed.
        let mut agents = vec![
            failed_agent("a"),
            failed_agent("b"),
            failed_agent("c"),
            failed_agent("d"),
        ];
        let mut focused = 2;
        let mut previous: Option<usize> = None;
        reorder_agents(&mut agents, 0, 3);
        focused = shift_index(focused, 0, 3);
        previous = previous.map(|p| shift_index(p, 0, 3));
        assert_eq!(agents[focused].label, "c");
        assert_eq!(previous, None);
    }

    // TabHitboxes — boundary tests for the click hit-test.

    #[test]
    fn tab_hitboxes_at_finds_recorded_rect() {
        let mut hb = TabHitboxes::default();
        let id_a = AgentId::new("a");
        let id_b = AgentId::new("b");
        hb.record(
            Rect {
                x: 5,
                y: 10,
                width: 4,
                height: 1,
            },
            id_a.clone(),
        );
        hb.record(
            Rect {
                x: 9,
                y: 10,
                width: 6,
                height: 1,
            },
            id_b.clone(),
        );
        assert_eq!(hb.at(5, 10), Some(id_a.clone())); // left edge of tab a
        assert_eq!(hb.at(8, 10), Some(id_a)); // last cell of tab a
        assert_eq!(hb.at(9, 10), Some(id_b.clone())); // first cell of tab b
        assert_eq!(hb.at(14, 10), Some(id_b)); // last cell of tab b
    }

    #[test]
    fn tab_hitboxes_at_misses_outside_recorded_rects() {
        let mut hb = TabHitboxes::default();
        hb.record(
            Rect {
                x: 5,
                y: 10,
                width: 4,
                height: 1,
            },
            AgentId::new("a"),
        );
        assert_eq!(hb.at(4, 10), None); // one cell left of tab a
        assert_eq!(hb.at(9, 10), None); // one past the right edge (exclusive width)
        assert_eq!(hb.at(5, 9), None); // one row above
        assert_eq!(hb.at(5, 11), None); // one row below
    }

    #[test]
    fn tab_hitboxes_clear_drops_all_recorded_rects() {
        let mut hb = TabHitboxes::default();
        hb.record(
            Rect {
                x: 0,
                y: 0,
                width: 10,
                height: 1,
            },
            AgentId::new("a"),
        );
        assert!(hb.at(0, 0).is_some());
        hb.clear();
        assert!(hb.at(0, 0).is_none());
    }

    #[test]
    fn tab_hitboxes_record_rejects_zero_sized_rect() {
        // Zero-width tabs (e.g. a tab that got entirely clipped off
        // the right edge of the status bar) must not produce a phantom
        // hitbox at x with width 0.
        let mut hb = TabHitboxes::default();
        hb.record(
            Rect {
                x: 5,
                y: 10,
                width: 0,
                height: 1,
            },
            AgentId::new("a"),
        );
        hb.record(
            Rect {
                x: 5,
                y: 10,
                width: 4,
                height: 0,
            },
            AgentId::new("b"),
        );
        assert_eq!(hb.at(5, 10), None);
    }

    // build_tab_strip + TabSpec::width — pure helpers feeding the
    // status-bar renderer. These are the functions the renderer relies
    // on for per-tab geometry; pinning their shape catches regressions
    // that would silently mis-place hitboxes.

    #[test]
    fn build_tab_strip_emits_one_spec_per_agent_in_order() {
        let agents = vec![failed_agent("a"), failed_agent("b"), failed_agent("c")];
        let tabs = build_tab_strip(
            &agents,
            1,
            AnimationPhase::default(),
            &ChromeStyle::default(),
        );
        assert_eq!(tabs.len(), 3);
        assert_eq!(tabs[0].agent_id, AgentId::new("a"));
        assert_eq!(tabs[1].agent_id, AgentId::new("b"));
        assert_eq!(tabs[2].agent_id, AgentId::new("c"));
    }

    #[test]
    fn build_tab_strip_each_spec_has_nonzero_width() {
        // Width is the sum of span display cells — for any non-empty
        // tab label it must be > 0, otherwise the renderer's clipped
        // rect would be zero-sized and `TabHitboxes::record` would
        // drop it.
        let agents = vec![failed_agent("agent-1"), failed_agent("agent-2")];
        let tabs = build_tab_strip(
            &agents,
            0,
            AnimationPhase::default(),
            &ChromeStyle::default(),
        );
        for tab in &tabs {
            assert!(tab.width() > 0, "tab {} has zero width", tab.agent_id);
        }
    }

    #[test]
    fn tab_spec_width_sums_span_display_cells() {
        // Construct directly to lock the contract: width is unicode
        // display cells, not byte length, not character count. Pinned
        // because a future refactor that swapped to `.len()` would
        // silently mis-clip multi-cell glyphs in the hitbox math.
        let spec = TabSpec {
            agent_id: AgentId::new("a"),
            spans: vec![
                Span::raw(" "),     // 1
                Span::raw("hello"), // 5
                Span::raw(" "),     // 1
            ],
        };
        assert_eq!(spec.width(), 7);
    }

    // tab_mouse_dispatch — every branch of the click/drag state
    // machine. The wiring inside `event_loop` translates the returned
    // enum into a state mutation; testing the dispatch directly is the
    // cheapest way to lock the gesture semantics without an event-loop
    // harness.

    fn two_tab_hitboxes() -> TabHitboxes {
        // Tabs at columns 0..5 and 5..10, both on row 23 (e.g. the
        // bottom status bar). Adjacent — no gap — to mirror how
        // `render_status_bar` records them with the separator span
        // sitting in between rather than as a third hitbox.
        let mut hb = TabHitboxes::default();
        hb.record(
            Rect {
                x: 0,
                y: 23,
                width: 5,
                height: 1,
            },
            AgentId::new("a"),
        );
        hb.record(
            Rect {
                x: 5,
                y: 23,
                width: 5,
                height: 1,
            },
            AgentId::new("b"),
        );
        hb
    }

    #[test]
    fn tab_mouse_dispatch_down_on_tab_returns_press() {
        let hb = two_tab_hitboxes();
        assert_eq!(
            tab_mouse_dispatch(MouseEventKind::Down(MouseButton::Left), 2, 23, &hb, None),
            Some(TabMouseDispatch::PressTab(AgentId::new("a"))),
        );
        assert_eq!(
            tab_mouse_dispatch(MouseEventKind::Down(MouseButton::Left), 7, 23, &hb, None),
            Some(TabMouseDispatch::PressTab(AgentId::new("b"))),
        );
    }

    #[test]
    fn tab_mouse_dispatch_down_outside_tabs_returns_none() {
        // Pressing on the agent pane (or anywhere not over a tab) must
        // not arm the gesture — otherwise a release back over a tab
        // would teleport focus from a click that started on the PTY.
        let hb = two_tab_hitboxes();
        assert_eq!(
            tab_mouse_dispatch(MouseEventKind::Down(MouseButton::Left), 50, 10, &hb, None),
            None,
        );
    }

    #[test]
    fn tab_mouse_dispatch_up_same_tab_is_a_click() {
        // Same-cell down→up has no Drag in between (crossterm only
        // fires Drag on motion). Up over the same tab the press
        // grabbed → focus that tab.
        let hb = two_tab_hitboxes();
        let pressed = AgentId::new("a");
        assert_eq!(
            tab_mouse_dispatch(
                MouseEventKind::Up(MouseButton::Left),
                2,
                23,
                &hb,
                Some(&pressed),
            ),
            Some(TabMouseDispatch::Click(AgentId::new("a"))),
        );
    }

    #[test]
    fn tab_mouse_dispatch_up_different_tab_is_a_reorder() {
        // Press on tab a, release on tab b → reorder(a, b).
        let hb = two_tab_hitboxes();
        let pressed = AgentId::new("a");
        assert_eq!(
            tab_mouse_dispatch(
                MouseEventKind::Up(MouseButton::Left),
                7,
                23,
                &hb,
                Some(&pressed),
            ),
            Some(TabMouseDispatch::Reorder {
                from: AgentId::new("a"),
                to: AgentId::new("b"),
            }),
        );
    }

    #[test]
    fn tab_mouse_dispatch_up_outside_tabs_cancels() {
        // User dragged off the strip — release over the agent pane
        // (or anywhere with no recorded hitbox) cancels the gesture.
        let hb = two_tab_hitboxes();
        let pressed = AgentId::new("a");
        assert_eq!(
            tab_mouse_dispatch(
                MouseEventKind::Up(MouseButton::Left),
                50,
                10,
                &hb,
                Some(&pressed),
            ),
            Some(TabMouseDispatch::Cancel),
        );
    }

    #[test]
    fn tab_mouse_dispatch_up_with_no_press_is_none() {
        // Stray release with no matching press (e.g. the user pressed
        // outside any tab and we left `mouse_press` empty). Must be a
        // no-op — never let an unprovoked Up trigger focus changes.
        let hb = two_tab_hitboxes();
        assert_eq!(
            tab_mouse_dispatch(MouseEventKind::Up(MouseButton::Left), 2, 23, &hb, None),
            None,
        );
    }

    #[test]
    fn tab_mouse_dispatch_drag_is_none_so_event_loop_keeps_state() {
        // Drag fires only on motion (per crossterm); we deliberately
        // ignore it so the dispatcher is stateless across the full
        // gesture. The decision happens at Up, which has both ends.
        let hb = two_tab_hitboxes();
        let pressed = AgentId::new("a");
        assert_eq!(
            tab_mouse_dispatch(
                MouseEventKind::Drag(MouseButton::Left),
                5,
                23,
                &hb,
                Some(&pressed),
            ),
            None,
        );
    }

    #[test]
    fn tab_mouse_dispatch_non_left_buttons_are_ignored() {
        // Right and middle clicks must not steal tab gestures —
        // they're reserved for whatever the terminal or app wants
        // them for, not for us.
        let hb = two_tab_hitboxes();
        let pressed = AgentId::new("a");
        for button in [MouseButton::Right, MouseButton::Middle] {
            assert_eq!(
                tab_mouse_dispatch(MouseEventKind::Down(button), 2, 23, &hb, None),
                None,
            );
            assert_eq!(
                tab_mouse_dispatch(MouseEventKind::Up(button), 2, 23, &hb, Some(&pressed)),
                None,
            );
        }
    }

    fn failed_agent(label: &str) -> RuntimeAgent {
        let err = codemuxd_bootstrap::Error::Bootstrap {
            stage: codemuxd_bootstrap::Stage::VersionProbe,
            source: Box::new(io::Error::other("test")),
        };
        RuntimeAgent::failed(
            AgentId::new(label),
            label.into(),
            None,
            None,
            "host".into(),
            err,
            24,
            80,
        )
    }

    fn failed_agent_with(label: &str, repo: Option<&str>, host: Option<&str>) -> RuntimeAgent {
        let err = codemuxd_bootstrap::Error::Bootstrap {
            stage: codemuxd_bootstrap::Stage::VersionProbe,
            source: Box::new(io::Error::other("test")),
        };
        let mut agent = RuntimeAgent::failed(
            AgentId::new(label),
            label.into(),
            repo.map(str::to_string),
            None,
            host.unwrap_or("host").into(),
            err,
            24,
            80,
        );
        // The constructor always sets `host: Some(...)` for Failed
        // agents (a Failed agent always has a host — that's what
        // bootstrap was operating on). Tests that want to exercise
        // the "no host prefix in label" code path opt out by passing
        // `None` here; we override to model that case for the renderer
        // tests.
        if host.is_none() {
            agent.host = None;
        }
        agent
    }

    // agent_body_text — covers the four label-source combinations
    // the renderer can hit. Title is `None` here because Failed
    // agents have no parser; the title-present arms are exercised
    // implicitly by the pty_title module tests, which prove vt100
    // surfaces the captured title via `parser.callbacks().title()`.

    #[test]
    fn agent_body_text_uses_repo_when_no_title() {
        let agent = failed_agent_with("agent-1", Some("codemux"), None);
        assert_eq!(agent_body_text(&agent), "codemux");
    }

    #[test]
    fn agent_body_text_falls_back_to_label_when_neither_repo_nor_title() {
        let agent = failed_agent_with("agent-1", None, None);
        assert_eq!(agent_body_text(&agent), "agent-1");
    }

    // agent_label_spans — host prefix is the user-visible signal
    // for "this lives on a remote box," so it must not silently
    // disappear when the agent has a host set.

    #[test]
    fn agent_label_spans_includes_host_prefix_for_ssh() {
        let agent = failed_agent_with("agent-1", Some("codemux"), Some("devpod-01"));
        let spans = agent_label_spans(
            &agent,
            false,
            AnimationPhase::default(),
            &ChromeStyle::default(),
        );
        let rendered: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            rendered.contains("devpod-01"),
            "host prefix missing: {rendered:?}"
        );
        assert!(
            rendered.contains("codemux"),
            "repo body missing: {rendered:?}"
        );
    }

    #[test]
    fn agent_label_spans_omits_host_prefix_for_local() {
        let agent = failed_agent_with("agent-1", Some("codemux"), None);
        let spans = agent_label_spans(
            &agent,
            false,
            AnimationPhase::default(),
            &ChromeStyle::default(),
        );
        let rendered: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(rendered, "codemux");
    }

    // ── animation rendering ──────────────────────────────────────
    //
    // Failed agents are convenient for the wrapper tests because they
    // expose the renderer-visible fields (`needs_attention`) without
    // requiring an `AgentTransport`. The working-spinner branch is
    // exercised separately against [`label_spans`] (the pure-data
    // helper), which takes `working: bool` directly and so doesn't
    // need a real PTY to flip the bit.

    #[test]
    fn agent_label_spans_renders_attention_dot_when_unfocused_and_flagged() {
        let mut agent = failed_agent_with("agent-1", Some("codemux"), None);
        agent.needs_attention = true;
        let spans = agent_label_spans(
            &agent,
            false,
            AnimationPhase::default(),
            &ChromeStyle::default(),
        );
        let rendered: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            rendered.contains('●'),
            "attention dot missing: {rendered:?}"
        );
    }

    #[test]
    fn agent_label_spans_omits_attention_dot_when_focused() {
        // Focused → user is looking → no need to alert. The flag
        // should be cleared by change_focus before rendering, but
        // the renderer also defends against a transient stale flag.
        let mut agent = failed_agent_with("agent-1", Some("codemux"), None);
        agent.needs_attention = true;
        let spans = agent_label_spans(
            &agent,
            true,
            AnimationPhase::default(),
            &ChromeStyle::default(),
        );
        let rendered: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !rendered.contains('●'),
            "focused tab should not show attention dot: {rendered:?}"
        );
    }

    #[test]
    fn agent_label_spans_omits_attention_dot_when_not_flagged() {
        let agent = failed_agent_with("agent-1", Some("codemux"), None);
        let spans = agent_label_spans(
            &agent,
            false,
            AnimationPhase::default(),
            &ChromeStyle::default(),
        );
        let rendered: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!rendered.contains('●'));
    }

    #[test]
    fn body_style_swings_between_grey_tones_when_attention_active() {
        // The blink contract: bright phase → BLINK_BRIGHT (light
        // grey, ANSI 256 index 252), dim phase → BLINK_DIM (slightly
        // darker light grey, index 247). Pin both ends so a renderer
        // change can't accidentally drop one half of the cycle or
        // wander back into the high-contrast palette the user
        // explicitly asked us to leave behind.
        let bright = body_style(
            false,
            true,
            AnimationPhase {
                spinner_frame: 0,
                blink_bright: true,
            },
            &ChromeStyle::default(),
        );
        let dim = body_style(
            false,
            true,
            AnimationPhase {
                spinner_frame: 0,
                blink_bright: false,
            },
            &ChromeStyle::default(),
        );
        assert_eq!(bright.fg, Some(BLINK_BRIGHT));
        assert_eq!(dim.fg, Some(BLINK_DIM));
        assert_ne!(bright, dim);
    }

    #[test]
    fn body_style_focused_overrides_attention() {
        // Focused tabs use the reverse-video tab highlight regardless
        // of attention state. Otherwise the user would see the
        // focused tab pulsing the same as a remote alerting tab and
        // lose the "where am I?" anchor.
        let focused_with_attention = body_style(
            true,
            true,
            AnimationPhase {
                spinner_frame: 0,
                blink_bright: true,
            },
            &ChromeStyle::default(),
        );
        assert!(
            focused_with_attention
                .add_modifier
                .contains(Modifier::REVERSED)
        );
    }

    #[test]
    fn animation_phase_advances_spinner_with_elapsed_time() {
        // Frame index advances every 100 ms. Spot-check a couple of
        // boundaries so a future tweak to the cadence has to update
        // these too — the contract is "10 Hz spinner."
        let f0 = AnimationPhase::from_elapsed(Duration::from_millis(0));
        let f1 = AnimationPhase::from_elapsed(Duration::from_millis(100));
        let f2 = AnimationPhase::from_elapsed(Duration::from_millis(200));
        let wrap = AnimationPhase::from_elapsed(Duration::from_millis(800));
        assert_eq!(f0.spinner_frame, 0);
        assert_eq!(f1.spinner_frame, 1);
        assert_eq!(f2.spinner_frame, 2);
        // Cycle wraps after `SPINNER_FRAMES.len()` frames (800 ms
        // for the current 8-frame dots8 set).
        assert_eq!(wrap.spinner_frame, 0);
    }

    #[test]
    fn animation_phase_blink_toggles_at_half_cycle_boundary() {
        // Pin the slow-heartbeat cadence. The user explicitly asked
        // for a slower pulse than the original 500 ms; if a future
        // edit drops `BLINK_HALF_CYCLE_MS` back down, this test
        // forces the rationale to be revisited.
        let half_ms = u64::try_from(BLINK_HALF_CYCLE_MS).unwrap_or(u64::MAX);
        let bright0 = AnimationPhase::from_elapsed(Duration::from_millis(0));
        let dim = AnimationPhase::from_elapsed(Duration::from_millis(half_ms));
        let bright1 = AnimationPhase::from_elapsed(Duration::from_millis(half_ms * 2));
        assert!(bright0.blink_bright);
        assert!(!dim.blink_bright);
        assert!(bright1.blink_bright);
    }

    #[test]
    fn animation_phase_spinner_glyph_returns_a_known_frame() {
        // Pin both ends of the cycle so a future SPINNER_FRAMES rewrite
        // has to update this test, not silently break the contract.
        let p = AnimationPhase {
            spinner_frame: 0,
            blink_bright: false,
        };
        assert_eq!(p.spinner_glyph(), SPINNER_FRAMES[0]);
        let last = SPINNER_FRAMES.len() - 1;
        let p = AnimationPhase {
            spinner_frame: last,
            blink_bright: false,
        };
        assert_eq!(p.spinner_glyph(), SPINNER_FRAMES[last]);
    }

    #[test]
    fn animation_phase_spinner_glyph_clamps_out_of_range_frame() {
        // `from_elapsed` always produces a modulo'd index, so this only
        // matters for test code that builds an AnimationPhase by hand.
        // The clamp keeps that path bounds-safe rather than panicking.
        let p = AnimationPhase {
            spinner_frame: SPINNER_FRAMES.len() + 100,
            blink_bright: false,
        };
        assert_eq!(p.spinner_glyph(), SPINNER_FRAMES[SPINNER_FRAMES.len() - 1]);
    }

    // ── label_spans (pure renderer) ───────────────────────────────
    //
    // Direct tests against the primitive-taking helper. These cover
    // the working-spinner branch that the agent-wrapper tests can't
    // reach (spinner is gated on `is_working()`, which requires a
    // Ready agent with a real PTY parser).

    fn rendered(spans: &[Span<'_>]) -> String {
        spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn label_spans_renders_spinner_glyph_when_working() {
        let phase = AnimationPhase {
            spinner_frame: 3,
            blink_bright: true,
        };
        let spans = label_spans(
            None,
            "codemux",
            true,
            false,
            false,
            phase,
            &ChromeStyle::default(),
        );
        assert!(
            rendered(&spans).contains(SPINNER_FRAMES[3]),
            "spinner glyph missing: {:?}",
            rendered(&spans)
        );
    }

    #[test]
    fn label_spans_omits_spinner_when_not_working() {
        let phase = AnimationPhase::default();
        let spans = label_spans(
            None,
            "codemux",
            false,
            false,
            false,
            phase,
            &ChromeStyle::default(),
        );
        let out = rendered(&spans);
        for frame in SPINNER_FRAMES {
            assert!(!out.contains(frame), "stray spinner glyph: {out:?}");
        }
    }

    #[test]
    fn label_spans_renders_focused_spinner_with_reverse_style() {
        // Focused spinner inherits the tab's reverse-video highlight
        // rather than the unfocused gray. Otherwise the spinner would
        // visually drop out of the highlighted tab.
        let phase = AnimationPhase::default();
        let spans = label_spans(
            None,
            "codemux",
            true,
            false,
            true,
            phase,
            &ChromeStyle::default(),
        );
        let spinner_span = spans
            .iter()
            .find(|s| SPINNER_FRAMES.iter().any(|g| s.content.contains(g)))
            .expect("spinner span present");
        assert!(spinner_span.style.add_modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn label_spans_renders_host_prefix_when_provided() {
        let spans = label_spans(
            Some("devpod-01"),
            "codemux",
            false,
            false,
            false,
            AnimationPhase::default(),
            &ChromeStyle::default(),
        );
        assert!(rendered(&spans).contains("devpod-01"));
    }

    #[test]
    fn label_spans_omits_host_prefix_when_absent() {
        let spans = label_spans(
            None,
            "codemux",
            false,
            false,
            false,
            AnimationPhase::default(),
            &ChromeStyle::default(),
        );
        assert_eq!(rendered(&spans), "codemux");
    }

    /// Spinner sits at the very front so the working cue lands in the
    /// same column on every tab — host-prefixed and not. A future
    /// refactor that swapped the order back would put the spinner in
    /// a different column on local vs SSH tabs and break the visual
    /// rhythm the user explicitly asked for.
    #[test]
    fn label_spans_renders_spinner_before_host() {
        let phase = AnimationPhase {
            spinner_frame: 3,
            blink_bright: true,
        };
        let spans = label_spans(
            Some("devpod-01"),
            "codemux",
            true,
            false,
            false,
            phase,
            &ChromeStyle::default(),
        );
        let spinner_idx = spans
            .iter()
            .position(|s| SPINNER_FRAMES.iter().any(|g| s.content.contains(g)))
            .expect("spinner span present");
        let host_idx = spans
            .iter()
            .position(|s| s.content.contains("devpod-01"))
            .expect("host span present");
        assert!(
            spinner_idx < host_idx,
            "spinner must render before host: spinner@{spinner_idx} host@{host_idx}",
        );
    }

    // ── body_text (pure formatter) ────────────────────────────────
    //
    // The four (repo, title) permutations. The agent-wrapper tests
    // exercise the no-title arms (Failed agents have no parser); these
    // cover the title-present arms that need a Ready agent's parser
    // output.

    #[test]
    fn body_text_combines_repo_and_title() {
        assert_eq!(
            body_text(Some("Add tab names"), Some("codemux"), "fallback"),
            "codemux: Add tab names"
        );
    }

    #[test]
    fn body_text_uses_title_alone_when_no_repo() {
        assert_eq!(
            body_text(Some("Add tab names"), None, "fallback"),
            "Add tab names"
        );
    }

    #[test]
    fn body_text_uses_repo_alone_when_no_title() {
        assert_eq!(body_text(None, Some("codemux"), "fallback"), "codemux");
    }

    #[test]
    fn body_text_falls_back_to_label_when_neither() {
        assert_eq!(body_text(None, None, "fallback"), "fallback");
    }

    // ── host_terminal_title ──────────────────────────────────────
    //
    // The composer that produces what gets shipped to the outer
    // terminal emulator's tab title. SSH agents get the host
    // prefix; local agents (no host) get just the body. Empty
    // host is treated as no host so a malformed config can't
    // produce a leading " · " orphan. A working glyph (when set)
    // sits at the very front so the spinner lands in the same
    // column whether the agent is local or remote — matches the
    // in-app tab strip layout.

    #[test]
    fn host_terminal_title_prefixes_host_when_remote() {
        assert_eq!(
            host_terminal_title(Some("dev01"), "codemux: Add tab names", None),
            "dev01 \u{00b7} codemux: Add tab names"
        );
    }

    #[test]
    fn host_terminal_title_omits_prefix_when_local() {
        assert_eq!(
            host_terminal_title(None, "codemux: Add tab names", None),
            "codemux: Add tab names"
        );
    }

    #[test]
    fn host_terminal_title_treats_empty_host_as_no_host() {
        // Defensive: a malformed config / empty hostname must not
        // emit a leading " · " orphan that looks like a render bug
        // in the user's terminal tab bar.
        assert_eq!(host_terminal_title(Some(""), "body", None), "body");
    }

    #[test]
    fn host_terminal_title_prepends_working_glyph_for_remote() {
        assert_eq!(
            host_terminal_title(Some("dev01"), "codemux: Working", Some("⣾")),
            "⣾ dev01 \u{00b7} codemux: Working"
        );
    }

    #[test]
    fn host_terminal_title_prepends_working_glyph_for_local() {
        assert_eq!(
            host_terminal_title(None, "codemux: Working", Some("⣾")),
            "⣾ codemux: Working"
        );
    }

    #[test]
    fn host_terminal_title_prepends_working_glyph_when_host_empty() {
        // Defensive companion to the empty-host case: glyph still
        // renders, no orphan separator.
        assert_eq!(host_terminal_title(Some(""), "body", Some("⣾")), "⣾ body");
    }

    #[test]
    fn host_terminal_title_for_focused_returns_none_when_no_agents() {
        // Runtime is about to exit; no focused agent means nothing to
        // ship to the host terminal title bar.
        let nav = NavState::new(vec![]);
        assert!(host_terminal_title_for_focused(&nav, AnimationPhase::default()).is_none());
    }

    #[test]
    fn host_terminal_title_for_focused_uses_focused_agent_with_host() {
        let agents = vec![
            failed_agent_with("a", Some("repo-a"), Some("hostA")),
            failed_agent_with("b", Some("repo-b"), Some("hostB")),
        ];
        let mut nav = NavState::new(agents);
        nav.focused = 1;
        let title = host_terminal_title_for_focused(&nav, AnimationPhase::default());
        // Failed agents are never `is_working()`, so no spinner glyph
        // even though the phase has one available.
        assert_eq!(title.as_deref(), Some("hostB \u{00b7} repo-b"));
    }

    #[test]
    fn host_terminal_title_for_focused_omits_host_for_local_agent() {
        let agents = vec![failed_agent_with("a", Some("repo-a"), None)];
        let nav = NavState::new(agents);
        let title = host_terminal_title_for_focused(&nav, AnimationPhase::default());
        assert_eq!(title.as_deref(), Some("repo-a"));
    }

    /// Build a Ready agent whose parser has consumed `osc_payload` so
    /// downstream callers can exercise `is_working() == true` without
    /// scattering parser-bytes-poking through every test body. Panics
    /// if the test transport ever stops yielding a Ready agent — that
    /// would be a regression in `ready_test_agent` itself, not a
    /// silent skip in the assertion.
    fn ready_agent_with_osc(osc_payload: &str) -> RuntimeAgent {
        let mut agent = ready_test_agent(100);
        let AgentState::Ready { parser, .. } = &mut agent.state else {
            panic!("ready_test_agent must yield AgentState::Ready");
        };
        parser.process(osc_payload.as_bytes());
        agent
    }

    #[test]
    fn host_terminal_title_for_focused_prefixes_spinner_when_agent_is_working() {
        // Integration check on the wiring `is_working().then(spinner_glyph)`.
        // Only Ready agents whose parser has seen a status-glyph OSC return
        // `is_working() == true`, so we feed one through here. Without this
        // test the working branch of `host_terminal_title_for_focused` is
        // dead from the unit tests' point of view — `host_terminal_title`
        // tests cover the prefix shape but not the integration.
        let nav = NavState::new(vec![ready_agent_with_osc("\x1b]0;⠋ Working\x07")]);
        // Phase frame 0 → SPINNER_FRAMES[0] = "⣾". Pinning the index keeps
        // this test resilient to spinner-set tweaks: rebuild the expected
        // string from the same constant the production code reads.
        let title = host_terminal_title_for_focused(&nav, AnimationPhase::default());
        let expected = format!("{} Working", SPINNER_FRAMES[0]);
        assert_eq!(title.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn host_terminal_title_for_focused_threads_phase_into_spinner_frame() {
        // Pins that `phase` actually flows through to the glyph — a future
        // refactor that hard-codes SPINNER_FRAMES[0] would still pass the
        // previous test but break this one.
        let nav = NavState::new(vec![ready_agent_with_osc("\x1b]0;⠋ Working\x07")]);
        let phase = AnimationPhase {
            spinner_frame: 3,
            ..AnimationPhase::default()
        };
        let title = host_terminal_title_for_focused(&nav, phase);
        let expected = format!("{} Working", SPINNER_FRAMES[3]);
        assert_eq!(title.as_deref(), Some(expected.as_str()));
    }

    // ── peek_finish_transitions / apply_finish_transitions ───────
    //
    // The working→idle detector that runs once per tick. Split into
    // a pure query (`peek`) and a pure command (`apply`) so the
    // call site can ask "did anything finish?" without implicitly
    // committing the side effects (`needs_attention` flag, the
    // `last_working` rotation). Pulled out of the event loop so the
    // transition matrix can be tested with Failed agents (whose
    // `is_working()` is always false; we drive the input by setting
    // `last_working` directly).

    #[test]
    fn peek_finish_transitions_does_not_mutate_agent_state() {
        // Pure-query contract: peek alone must leave both
        // `last_working` and `needs_attention` untouched. A future
        // refactor that quietly folded apply back into peek would
        // pass every other test in this section but break this one.
        let mut agents = vec![failed_agent("a"), failed_agent("b")];
        agents[1].last_working = true;
        let nav = NavState::new(agents);
        let _ = nav.peek_finish_transitions();
        assert!(
            nav.agents[1].last_working,
            "peek must not rotate last_working"
        );
        assert!(
            !nav.agents[1].needs_attention,
            "peek must not flag attention"
        );
    }

    #[test]
    fn apply_finish_transitions_marks_unfocused_agent_that_just_finished() {
        let mut agents = vec![failed_agent("a"), failed_agent("b")];
        agents[1].last_working = true;
        let mut nav = NavState::new(agents);
        let transitions = nav.peek_finish_transitions();
        assert!(
            transitions.any(),
            "an unfocused finish must report any()=true"
        );
        nav.apply_finish_transitions(&transitions);
        assert!(nav.agents[1].needs_attention);
        assert!(!nav.agents[1].last_working, "last_working must be rotated");
    }

    #[test]
    fn apply_finish_transitions_skips_attention_on_focused_agent() {
        // Focused → user is already looking → no slow-blink. The
        // command still rotates last_working so the transition is
        // consumed (not re-flagged on the next tick).
        let mut agents = vec![failed_agent("a"), failed_agent("b")];
        agents[0].last_working = true;
        let mut nav = NavState::new(agents);
        let transitions = nav.peek_finish_transitions();
        assert!(
            transitions.any(),
            "focused finish must still report any()=true so the host BEL fires when the user is in another window",
        );
        nav.apply_finish_transitions(&transitions);
        assert!(!nav.agents[0].needs_attention);
        assert!(!nav.agents[0].last_working);
    }

    #[test]
    fn peek_finish_transitions_reports_no_change_when_state_unchanged() {
        let agents = vec![failed_agent("a"), failed_agent("b")];
        let mut nav = NavState::new(agents);
        let transitions = nav.peek_finish_transitions();
        assert!(
            !transitions.any(),
            "no transition this tick must report any()=false so the host BEL stays silent",
        );
        nav.apply_finish_transitions(&transitions);
        assert!(!nav.agents[0].needs_attention);
        assert!(!nav.agents[1].needs_attention);
    }

    #[test]
    fn peek_finish_transitions_reports_any_for_unfocused_finish_even_when_focused_unchanged() {
        // Mixed scenario: focused agent stays idle (no transition),
        // unfocused agent transitions. Pins that "any" really means
        // any — symmetric with the focused-only-finish case above.
        let mut agents = vec![failed_agent("a"), failed_agent("b")];
        agents[1].last_working = true;
        let mut nav = NavState::new(agents);
        let transitions = nav.peek_finish_transitions();
        assert!(transitions.any());
        nav.apply_finish_transitions(&transitions);
        assert!(nav.agents[1].needs_attention);
        assert!(!nav.agents[0].needs_attention);
    }

    // ── reap_dead_transports ─────────────────────────────────────
    //
    // The per-frame Ready→Crashed transition. Pulled out of the
    // event loop so the dead-transport detection can be exercised
    // without driving the full ratatui draw cycle. Tests use
    // [`AgentTransport::for_test`] (a real `cat` PTY) and `kill()`
    // to make `try_wait` return Some.

    /// Spawn a Ready agent backed by a real `cat` PTY, kill the
    /// child, then poll until `try_wait` reports the death. Returns
    /// the agent in a state where the next `reap_dead_transports`
    /// call will transition it to Crashed.
    fn ready_agent_with_dead_transport() -> RuntimeAgent {
        let mut agent = ready_test_agent(100);
        if let AgentState::Ready { transport, .. } = &mut agent.state {
            transport.kill().expect("kill cat");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while transport.try_wait().is_none() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "cat did not die within 2s of kill",
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        agent
    }

    #[test]
    fn reap_transitions_ready_with_dead_transport_to_crashed() {
        let mut nav = NavState::new(vec![ready_agent_with_dead_transport()]);
        nav.reap_dead_transports();
        assert!(
            matches!(nav.agents[0].state, AgentState::Crashed { .. }),
            "expected Crashed after reap, got variant {}",
            state_variant_name(&nav.agents[0].state),
        );
    }

    #[test]
    fn reap_leaves_alive_ready_agent_in_ready_state() {
        let mut nav = NavState::new(vec![ready_test_agent(100)]);
        nav.reap_dead_transports();
        assert!(
            matches!(nav.agents[0].state, AgentState::Ready { .. }),
            "alive Ready agent must stay Ready, got variant {}",
            state_variant_name(&nav.agents[0].state),
        );
    }

    #[test]
    fn reap_leaves_failed_agent_alone() {
        let mut nav = NavState::new(vec![failed_agent("dead")]);
        nav.reap_dead_transports();
        assert!(
            matches!(nav.agents[0].state, AgentState::Failed { .. }),
            "Failed must remain Failed across reap",
        );
    }

    #[test]
    fn reap_leaves_already_crashed_agent_alone() {
        let mut agents = vec![ready_test_agent(100)];
        agents[0].mark_crashed(13);
        let mut nav = NavState::new(agents);
        nav.reap_dead_transports();
        match &nav.agents[0].state {
            AgentState::Crashed { exit_code, .. } => assert_eq!(
                *exit_code, 13,
                "reap must not re-transition an already-Crashed agent",
            ),
            other => panic!(
                "expected Crashed, got variant {}",
                state_variant_name(other),
            ),
        }
    }

    /// Spawn a Ready agent backed by `sh -c 'exit 0'`, then poll
    /// until the child has actually exited so the next
    /// `reap_dead_transports` call is guaranteed to observe `Some(0)`
    /// from `try_wait`. Returns the agent ready to be reaped.
    fn ready_agent_with_clean_exit() -> RuntimeAgent {
        let transport = AgentTransport::for_test_clean_exit("clean-exit-test".into(), 5, 20)
            .expect("for_test_clean_exit transport");
        let mut agent = RuntimeAgent::ready(
            AgentId::new("a"),
            "a".into(),
            None,
            None,
            None,
            transport,
            5,
            20,
            100,
        );
        if let AgentState::Ready { transport, .. } = &mut agent.state {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            while transport.try_wait().is_none() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "sh -c 'exit 0' did not exit within 2s",
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        agent
    }

    #[test]
    fn reap_silently_removes_clean_exit() {
        // The headline of the auto-close work: a Ready agent whose
        // transport returns `Some(0)` from `try_wait` must be
        // *removed* from the Vec, not transitioned to Crashed. The
        // user typed `/quit` (or equivalent) and expects the tab
        // to vanish without further action.
        let mut nav = NavState::new(vec![ready_agent_with_clean_exit()]);
        nav.reap_dead_transports();
        assert!(
            nav.agents.is_empty(),
            "exit 0 must be silently reaped, got {} agent(s) remaining",
            nav.agents.len(),
        );
    }

    #[test]
    fn reap_clean_exit_clamps_focus_to_surviving_agent() {
        // Multiple agents, only the first dies cleanly. Focus must
        // shift to the surviving agent (which slides up to index 0)
        // rather than pointing past the end of the trimmed Vec.
        let mut nav = NavState::new(vec![ready_agent_with_clean_exit(), ready_test_agent(100)]);
        nav.focused = 1;
        nav.reap_dead_transports();
        assert_eq!(nav.agents.len(), 1, "only the dead agent should be reaped");
        assert_eq!(
            nav.focused, 0,
            "focus must follow the surviving agent across the upstream removal",
        );
    }

    // ── dismiss_focused ──────────────────────────────────────────
    //
    // Removes the focused agent if it's in a terminal state and
    // clamps focus / bounce / popup state. Pulled out of the
    // dispatch handler so the clamp logic is testable without
    // driving the event loop.

    #[test]
    fn dismiss_removes_focused_crashed_agent_and_clamps_focus() {
        let mut agent = ready_test_agent(100);
        agent.mark_crashed(0);
        let mut nav = NavState::new(vec![failed_agent("a"), agent]);
        nav.focused = 1;

        let removed = nav.dismiss_focused();

        assert!(removed);
        assert_eq!(nav.agents.len(), 1);
        assert_eq!(
            nav.focused, 0,
            "focus must clamp to the new last index after removing the tail",
        );
    }

    #[test]
    fn dismiss_removes_focused_failed_agent() {
        let mut nav = NavState::new(vec![failed_agent("a"), failed_agent("b")]);

        let removed = nav.dismiss_focused();

        assert!(removed, "Failed must be dismissable");
        assert_eq!(nav.agents.len(), 1);
    }

    #[test]
    fn dismiss_no_op_on_focused_ready_agent() {
        let mut nav = NavState::new(vec![ready_test_agent(100), failed_agent("b")]);
        let before = nav.agents.len();

        let removed = nav.dismiss_focused();

        assert!(
            !removed,
            "Ready must NOT be dismissable (live-session footgun)"
        );
        assert_eq!(nav.agents.len(), before);
        assert_eq!(nav.focused, 0);
    }

    #[test]
    fn dismiss_clears_stale_previous_focused() {
        let mut nav = NavState::new(vec![
            failed_agent("a"),
            failed_agent("b"),
            failed_agent("c"),
        ]);
        nav.focused = 1;
        nav.previous_focused = Some(2);

        nav.dismiss_focused();

        assert_eq!(nav.agents.len(), 2);
        assert!(
            nav.previous_focused.is_none(),
            "previous_focused pointing past the new end must be cleared, got {:?}",
            nav.previous_focused,
        );
    }

    #[test]
    fn dismiss_clears_previous_focused_when_it_collides_with_focused() {
        let mut nav = NavState::new(vec![failed_agent("a"), failed_agent("b")]);
        nav.focused = 1;
        nav.previous_focused = Some(0);

        nav.dismiss_focused();

        assert_eq!(nav.agents.len(), 1);
        assert_eq!(nav.focused, 0);
        assert!(
            nav.previous_focused.is_none(),
            "bouncing onto the same slot as focused is a no-op; clear it",
        );
    }

    #[test]
    fn dismiss_clamps_open_popup_selection() {
        let mut nav = NavState::new(vec![
            failed_agent("a"),
            failed_agent("b"),
            failed_agent("c"),
        ]);
        nav.popup_state = PopupState::Open { selection: 2 };

        nav.dismiss_focused();

        match nav.popup_state {
            PopupState::Open { selection } => {
                assert_eq!(selection, 1, "popup selection must clamp to new last index");
            }
            PopupState::Closed => panic!("popup must remain Open, just clamped"),
        }
    }

    #[test]
    fn dismiss_leaves_empty_vec_when_last_agent_dismissed() {
        let mut nav = NavState::new(vec![failed_agent("only")]);

        let removed = nav.dismiss_focused();

        assert!(removed);
        assert!(
            nav.agents.is_empty(),
            "all-dismissed path leaves the Vec empty",
        );
    }

    // ── kill_focused ─────────────────────────────────────────────
    //
    // The `<prefix> x` chord — force-close. Mirrors `dismiss_focused`
    // but works on Ready agents too, since the user's intent is
    // "remove this tab right now" rather than "clear away the corpse".
    // Drop on the underlying transport handles child / tunnel cleanup.

    #[test]
    fn kill_focused_removes_ready_agent() {
        // The whole point of `kill_focused` over `dismiss_focused`:
        // the Ready guard is gone. Pin it explicitly so a future
        // refactor that re-introduces a state check doesn't slip
        // through silently.
        let mut nav = NavState::new(vec![ready_test_agent(100), failed_agent("b")]);

        let removed = nav.kill_focused();

        assert!(removed, "kill must work on a live Ready agent");
        assert_eq!(nav.agents.len(), 1);
        assert_eq!(nav.focused, 0);
    }

    #[test]
    fn kill_focused_removes_failed_agent() {
        // Symmetric with dismiss for the terminal-state case — the
        // user can use either chord on a dead tab.
        let mut nav = NavState::new(vec![failed_agent("a"), failed_agent("b")]);

        let removed = nav.kill_focused();

        assert!(removed);
        assert_eq!(nav.agents.len(), 1);
    }

    #[test]
    fn kill_focused_no_op_on_empty_vec() {
        let mut nav = NavState::new(Vec::new());
        let removed = nav.kill_focused();
        assert!(!removed, "no agent to kill on an empty list");
    }

    #[test]
    fn kill_focused_clamps_focus_when_killing_last_tab() {
        // Killing the rightmost tab from focused=2 must drop focus
        // to the new last index (1), not leave it pointing past the
        // end. Same invariant `dismiss_focused` already enforced via
        // its open-coded clamp; pinned here so the shared `remove_at`
        // helper keeps honoring it through `kill_focused`.
        let mut nav = NavState::new(vec![
            failed_agent("a"),
            failed_agent("b"),
            ready_test_agent(100),
        ]);
        nav.focused = 2;

        nav.kill_focused();

        assert_eq!(nav.agents.len(), 2);
        assert_eq!(nav.focused, 1);
    }

    // ── remove_at ────────────────────────────────────────────────
    //
    // Two regression scenarios that the prior open-coded clamp in
    // `dismiss_focused` handled by accident or didn't handle at all.
    // `remove_at` is now the single home for these invariants and is
    // called by every Vec-shrinking site.

    #[test]
    fn remove_at_decrements_focused_when_removing_an_earlier_index() {
        // Reap-driven removal of an unfocused agent BEFORE the
        // focused one. Pre-refactor `dismiss_focused` only ever
        // removed the focused index, so this case was uncovered;
        // `reap_dead_transports`'s clean-exit path now drives it.
        let mut nav = NavState::new(vec![
            failed_agent("a"),
            failed_agent("b"),
            failed_agent("c"),
        ]);
        nav.focused = 2;

        nav.remove_at(0);

        assert_eq!(nav.agents.len(), 2);
        assert_eq!(
            nav.focused, 1,
            "focused must follow the same agent across an upstream removal",
        );
    }

    #[test]
    fn remove_at_decrements_previous_focused_when_removing_an_earlier_index() {
        // Latent-bug fix from the refactor: pre-`remove_at`, removing
        // an index *below* `previous_focused` left the bounce slot
        // pointing one past the agent it should have followed.
        let mut nav = NavState::new(vec![
            failed_agent("a"),
            failed_agent("b"),
            failed_agent("c"),
        ]);
        nav.focused = 0;
        nav.previous_focused = Some(2);

        nav.remove_at(1);

        assert_eq!(nav.agents.len(), 2);
        assert_eq!(
            nav.previous_focused,
            Some(1),
            "bounce slot must follow the same agent across an upstream removal",
        );
    }

    #[test]
    fn remove_at_decrements_popup_selection_when_removing_an_earlier_index() {
        // Same shape as the focused / previous_focused decrement —
        // popup selection follows the same agent across an upstream
        // removal rather than silently jumping to the next one.
        let mut nav = NavState::new(vec![
            failed_agent("a"),
            failed_agent("b"),
            failed_agent("c"),
        ]);
        nav.popup_state = PopupState::Open { selection: 2 };

        nav.remove_at(0);

        match nav.popup_state {
            PopupState::Open { selection } => assert_eq!(selection, 1),
            PopupState::Closed => panic!("popup must stay open, just shifted"),
        }
    }

    #[test]
    fn remove_at_clears_previous_focused_when_it_points_at_removed_slot() {
        // The bounce slot pointed exactly at the index we're removing
        // → it can't follow anywhere, so clear it.
        let mut nav = NavState::new(vec![
            failed_agent("a"),
            failed_agent("b"),
            failed_agent("c"),
        ]);
        nav.focused = 0;
        nav.previous_focused = Some(2);

        nav.remove_at(2);

        assert_eq!(nav.agents.len(), 2);
        assert!(
            nav.previous_focused.is_none(),
            "previous_focused pointing at the removed slot must be cleared",
        );
    }

    #[test]
    fn remove_at_no_op_when_index_out_of_bounds() {
        let mut nav = NavState::new(vec![failed_agent("a")]);
        nav.remove_at(5);
        assert_eq!(nav.agents.len(), 1, "out-of-bounds remove_at must no-op");
    }

    #[test]
    fn remove_at_closes_popup_when_last_agent_removed() {
        // The popup overlay can't show a meaningful selection over an
        // empty agent list. Closing it on the way out keeps the
        // PopupState invariant ("Open implies a valid selection")
        // rather than leaving a stale index for whoever next reopens.
        let mut nav = NavState::new(vec![failed_agent("only")]);
        nav.popup_state = PopupState::Open { selection: 0 };

        nav.remove_at(0);

        assert!(nav.agents.is_empty());
        assert!(
            matches!(nav.popup_state, PopupState::Closed),
            "popup must auto-close when the last agent is removed",
        );
    }

    // ── reap clean-exit auto-close ───────────────────────────────
    //
    // Exit code 0 → silent removal (the "I typed /quit" path). The
    // live-PTY exit-0 path is awkward to drive in unit tests
    // (`AgentTransport::for_test` spawns `cat`, which doesn't exit
    // 0 cleanly on its own), so we exercise the synthetic equivalent:
    // a `Crashed { exit_code: 0 }` state set up directly via
    // `mark_crashed`. Note this is a DIFFERENT path from the live
    // reap — `mark_crashed` itself doesn't auto-remove — so the test
    // covers the dispatch shape (`remove_at` correctly removes a
    // crashed-zero slot) rather than the live transport poll.
    //
    // The actual reap-time auto-close is exercised end-to-end via
    // the `RUST_LOG=codemux=debug just run` smoke loop documented
    // in the plan.

    #[test]
    fn dismiss_removes_crashed_zero_slot() {
        // A `Crashed { exit_code: 0 }` slot is reachable today only
        // through tests / synthetic setup (the live reap path
        // auto-removes before `Crashed` is constructed for code 0).
        // But the Crashed variant accepts any code, so dismiss must
        // handle it correctly if it ever appears.
        let mut agent = ready_test_agent(100);
        agent.mark_crashed(0);
        let mut nav = NavState::new(vec![agent]);

        let removed = nav.dismiss_focused();

        assert!(removed);
        assert!(nav.agents.is_empty());
    }

    // ── tab_index_style ──────────────────────────────────────────

    #[test]
    fn tab_index_style_focused_is_reverse_bold() {
        let s = tab_index_style(true, &ChromeStyle::default());
        assert!(s.add_modifier.contains(Modifier::REVERSED));
        assert!(s.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn tab_index_style_unfocused_uses_secondary_chrome() {
        // Default chrome is the readable-on-any-monitor mode: a fixed
        // gray (Indexed 247) with no DIM modifier. Without this pin the
        // unfocused tab index could silently regress back to a
        // terminal-defined dim that disappears on poor monitors.
        let s = tab_index_style(false, &ChromeStyle::default());
        assert!(!s.add_modifier.contains(Modifier::REVERSED));
        assert_eq!(s.fg, Some(Color::Indexed(247)));
        assert!(!s.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn tab_index_style_unfocused_subtle_keeps_dim() {
        // Subtle chrome opt-in restores the original DarkGray + DIM
        // look. Pinning both the color and the modifier guards the
        // contract from a future refactor that flips one but not the
        // other.
        let chrome = ChromeStyle::from_ui(&crate::config::Ui {
            subtle: true,
            ..Default::default()
        });
        let s = tab_index_style(false, &chrome);
        assert_eq!(s.fg, Some(Color::DarkGray));
        assert!(s.add_modifier.contains(Modifier::DIM));
    }

    // ── chrome.host_style — per-host accent ──────────────────────

    #[test]
    fn host_style_falls_back_to_secondary_when_host_is_unconfigured() {
        // Hosts without an entry in `[ui.host_colors]` quietly inherit
        // the secondary chrome style. Without this, an unconfigured
        // host would either crash on lookup or render in a default
        // accent the user didn't pick.
        let chrome = ChromeStyle::from_ui(&crate::config::Ui::default());
        assert_eq!(chrome.host_style("unknown-host"), chrome.secondary);
    }

    #[test]
    fn host_style_uses_configured_color_when_host_is_known() {
        // Configured accent overrides secondary chrome for that host.
        // Pin all three formats so a future refactor of the parser or
        // ChromeStyle::from_ui can't silently drop one branch.
        let mut host_colors = std::collections::HashMap::new();
        host_colors.insert(
            "work".to_string(),
            crate::config::ChromeColor::Named(Color::Blue),
        );
        host_colors.insert(
            "personal".to_string(),
            crate::config::ChromeColor::Indexed(33),
        );
        host_colors.insert(
            "devpod".to_string(),
            crate::config::ChromeColor::Rgb(0xd7, 0x5f, 0x00),
        );
        let chrome = ChromeStyle::from_ui(&crate::config::Ui {
            host_colors,
            ..Default::default()
        });
        assert_eq!(chrome.host_style("work").fg, Some(Color::Blue));
        assert_eq!(chrome.host_style("personal").fg, Some(Color::Indexed(33)));
        assert_eq!(
            chrome.host_style("devpod").fg,
            Some(Color::Rgb(0xd7, 0x5f, 0x00)),
        );
    }

    #[test]
    fn label_spans_unfocused_host_prefix_uses_per_host_accent() {
        // The whole user-facing point of `[ui.host_colors]`: an
        // unfocused tab's host prefix renders in the configured
        // accent. Focused tabs deliberately ignore the accent (they
        // inherit reverse-video tab highlight); that path is covered
        // by `label_spans_renders_focused_spinner_with_reverse_style`.
        let mut host_colors = std::collections::HashMap::new();
        host_colors.insert(
            "work".to_string(),
            crate::config::ChromeColor::Named(Color::Blue),
        );
        let chrome = ChromeStyle::from_ui(&crate::config::Ui {
            host_colors,
            ..Default::default()
        });
        let spans = label_spans(
            Some("work"),
            "claude",
            false,
            false,
            false,
            AnimationPhase::default(),
            &chrome,
        );
        let host_span = spans
            .iter()
            .find(|s| s.content.contains("work"))
            .expect("host prefix span present");
        assert_eq!(
            host_span.style.fg,
            Some(Color::Blue),
            "unfocused host prefix must use configured accent",
        );
    }

    #[test]
    fn label_spans_unconfigured_host_falls_back_to_secondary() {
        // Symmetric guard: when the user hasn't picked an accent for
        // a host, the prefix renders in secondary chrome. Ensures the
        // fallback path (most users on most hosts) actually runs.
        let chrome = ChromeStyle::from_ui(&crate::config::Ui::default());
        let spans = label_spans(
            Some("random-host"),
            "claude",
            false,
            false,
            false,
            AnimationPhase::default(),
            &chrome,
        );
        let host_span = spans
            .iter()
            .find(|s| s.content.contains("random-host"))
            .expect("host prefix span present");
        assert_eq!(host_span.style, chrome.secondary);
    }

    // PrefixHintSegment state-driven branch — the user's visible cue
    // that sticky nav mode is active. The two branches must produce
    // distinguishable output so the layout reserves appropriate room.
    // (The detailed PrefixHintSegment behavior is pinned in the
    // status_bar::segments tests; this is a smoke check here that
    // the runtime-side hint cue is actually different per state.)

    #[test]
    fn prefix_hint_segment_idle_and_awaiting_command_render_different_text() {
        use crate::status_bar::{SegmentCtx, StatusSegment, segments::PrefixHintSegment};
        let bindings = defaults();
        let chrome = ChromeStyle::default();
        let mk = |state| SegmentCtx {
            repo: None,
            branch: None,
            model_effort: None,
            cwd_basename: None,
            prefix_state: state,
            bindings: &bindings,
            secondary: chrome.secondary,
        };
        let idle = PrefixHintSegment.render(&mk(PrefixState::Idle)).unwrap();
        let nav = PrefixHintSegment
            .render(&mk(PrefixState::AwaitingCommand))
            .unwrap();
        let idle_text: String = idle.spans.iter().map(|s| s.content.as_ref()).collect();
        let nav_text: String = nav.spans.iter().map(|s| s.content.as_ref()).collect();
        // If a future edit makes the two states render identical text,
        // the cue gets lost — fail loudly here so the renderer's
        // affordance stays visible.
        assert_ne!(idle_text, nav_text);
        assert!(nav_text.contains("[NAV]"));
        assert!(idle_text.contains("for help"));
    }

    #[test]
    fn prefix_question_mark_opens_help() {
        let mut state = PrefixState::AwaitingCommand;
        // Crossterm sends `?` as Char('?') with SHIFT (varies by platform).
        let action = dispatch_key(
            &mut state,
            &key(KeyCode::Char('?'), KeyModifiers::SHIFT),
            &defaults(),
        );
        assert_eq!(action, KeyDispatch::OpenHelp);
    }

    #[test]
    fn prefix_digit_focuses_by_one_indexed_position() {
        for d in 1..=9_u8 {
            let mut state = PrefixState::AwaitingCommand;
            let c = char::from_digit(u32::from(d), 10).unwrap();
            let action = dispatch_key(
                &mut state,
                &key(KeyCode::Char(c), KeyModifiers::NONE),
                &defaults(),
            );
            assert_eq!(action, KeyDispatch::FocusAt(usize::from(d - 1)));
        }
    }

    #[test]
    fn prefix_zero_is_consumed_no_focus() {
        let mut state = PrefixState::AwaitingCommand;
        let action = dispatch_key(
            &mut state,
            &key(KeyCode::Char('0'), KeyModifiers::NONE),
            &defaults(),
        );
        assert_eq!(action, KeyDispatch::Consume);
    }

    #[test]
    fn unbound_key_after_prefix_is_consumed() {
        let mut state = PrefixState::AwaitingCommand;
        let action = dispatch_key(
            &mut state,
            &key(KeyCode::Char('z'), KeyModifiers::NONE),
            &defaults(),
        );
        assert_eq!(action, KeyDispatch::Consume);
        assert_eq!(state, PrefixState::Idle);
    }

    // User-config-driven dispatch

    #[test]
    fn user_can_remap_quit_to_a_different_key() {
        let toml_text = r#"
            [bindings.on_prefix]
            quit = "x"
        "#;
        let config: crate::config::Config = toml::from_str(toml_text).unwrap();
        let mut state = PrefixState::AwaitingCommand;
        let action = dispatch_key(
            &mut state,
            &key(KeyCode::Char('x'), KeyModifiers::NONE),
            &config.bindings,
        );
        assert_eq!(action, KeyDispatch::Exit);
        // The old key (q) is no longer bound to anything in prefix mode.
        let mut state2 = PrefixState::AwaitingCommand;
        let action2 = dispatch_key(
            &mut state2,
            &key(KeyCode::Char('q'), KeyModifiers::NONE),
            &config.bindings,
        );
        assert_eq!(action2, KeyDispatch::Consume);
    }

    #[test]
    fn user_can_remap_the_prefix_itself() {
        let toml_text = r#"
            [bindings]
            prefix = "ctrl+a"
        "#;
        let config: crate::config::Config = toml::from_str(toml_text).unwrap();
        let mut state = PrefixState::Idle;
        let action = dispatch_key(
            &mut state,
            &key(KeyCode::Char('a'), KeyModifiers::CONTROL),
            &config.bindings,
        );
        assert_eq!(action, KeyDispatch::Consume);
        assert_eq!(state, PrefixState::AwaitingCommand);
        // And the old prefix is now just a normal forwarded byte.
        let mut state2 = PrefixState::Idle;
        let action2 = dispatch_key(
            &mut state2,
            &key(KeyCode::Char('b'), KeyModifiers::CONTROL),
            &config.bindings,
        );
        assert_eq!(action2, KeyDispatch::Forward(vec![0x02]));
    }

    // literal_byte_for

    #[test]
    fn literal_byte_for_ctrl_letters() {
        use crate::keymap::KeyChord;
        assert_eq!(
            literal_byte_for(&KeyChord::ctrl(KeyCode::Char('b'))),
            Some(0x02)
        );
        assert_eq!(
            literal_byte_for(&KeyChord::ctrl(KeyCode::Char('a'))),
            Some(0x01)
        );
    }

    #[test]
    fn literal_byte_for_returns_none_when_prefix_is_not_a_ctrl_letter() {
        use crate::keymap::KeyChord;
        assert_eq!(literal_byte_for(&KeyChord::plain(KeyCode::Char('q'))), None);
        assert_eq!(literal_byte_for(&KeyChord::ctrl(KeyCode::F(1))), None);
    }

    // RuntimeAgent constructors
    //
    // Ready agents are built via `AgentTransport::for_test` (gated on
    // the session crate's `test-util` feature, enabled by this crate's
    // dev-dependencies). The transport is backed by a real local PTY
    // running `cat`; LocalPty's `Drop` reaps the child cleanly when the
    // agent is dropped at end-of-test.

    // The user-facing message formatting (stage hint + source chain)
    // is tested in `codemuxd_bootstrap::error::tests::user_message_*`,
    // co-located with the `Error` type it formats.

    // ── scrollback (vt100 contract guards) ────────────────────────
    //
    // These tests pin the vt100 invariants the runtime's scrollback
    // methods (`RuntimeAgent::nudge_scrollback`, `snap_to_live`,
    // `jump_to_top`) depend on. The method-level behavior is tested
    // directly further below; the contract tests stay because a silent
    // change in vt100's eviction-while-scrolled behavior or zero-len
    // clamp would break user-visible scroll mode in ways the method
    // tests can't pinpoint.

    #[test]
    fn scrollback_zero_len_means_no_history() {
        let mut parser = Parser::new(5, 20, 0);
        for i in 0..50 {
            parser.process(format!("line-{i}\r\n").as_bytes());
        }
        // Even though 45 rows scrolled out of view, scrollback_len = 0
        // discards them — set_scrollback clamps to 0 because the
        // VecDeque is empty.
        parser.screen_mut().set_scrollback(10);
        assert_eq!(
            parser.screen().scrollback(),
            0,
            "set_scrollback must clamp to 0 when no buffer is configured",
        );
    }

    #[test]
    fn scrollback_set_back_round_trips() {
        let mut parser = Parser::new(5, 20, 100);
        for i in 0..50 {
            parser.process(format!("line-{i}\r\n").as_bytes());
        }
        parser.screen_mut().set_scrollback(20);
        assert_eq!(parser.screen().scrollback(), 20);
        // And clamps to the bottom on negative-equivalent reset.
        parser.screen_mut().set_scrollback(0);
        assert_eq!(parser.screen().scrollback(), 0);
    }

    #[test]
    fn scrollback_offset_auto_bumps_when_new_rows_evict() {
        // The vt100 invariant codemux's UX leans on: while the user is
        // looking at scrollback (offset > 0), each evicted row pushes
        // the offset up by one so the same content stays under the
        // user's gaze. If vt100 ever stops doing this, scroll mode
        // would visibly "drift downward" as Claude streams output.
        let mut parser = Parser::new(5, 20, 100);
        for i in 0..30 {
            parser.process(format!("init-{i}\r\n").as_bytes());
        }
        parser.screen_mut().set_scrollback(10);
        assert_eq!(parser.screen().scrollback(), 10);
        for i in 0..7 {
            parser.process(format!("more-{i}\r\n").as_bytes());
        }
        assert_eq!(
            parser.screen().scrollback(),
            17,
            "each newly evicted row must bump the offset to hold the view",
        );
    }

    #[test]
    fn scrollback_clamps_to_buffer_length_at_top() {
        // `jump_to_top` calls set_scrollback(usize::MAX). The vt100
        // contract is "clamp to scrollback.len()" so the offset never
        // exceeds the buffer size.
        let mut parser = Parser::new(5, 20, 100);
        for i in 0..40 {
            parser.process(format!("l{i}\r\n").as_bytes());
        }
        parser.screen_mut().set_scrollback(usize::MAX);
        let offset = parser.screen().scrollback();
        assert!(offset > 0, "expected non-zero offset after jumping to top");
        assert!(
            offset <= 100,
            "offset {offset} must not exceed configured scrollback_len of 100",
        );
    }

    #[test]
    fn scrollback_state_is_per_parser() {
        // Two parsers stand in for two agents; setting scrollback on
        // one must not perturb the other. Trivially true by
        // construction (`Parser`s share no state) but pinned
        // explicitly because the runtime's "scroll persists per agent
        // across `Cmd-B 2` tab switches" contract leans on it. The
        // partner regression — pressing the prefix key while scrolled
        // back inadvertently snapping the source tab — lives at the
        // dispatcher boundary and is guarded by the snap-only-on-
        // Forward policy in the event loop. The method-level
        // independence test below (`nudge_scrollback_only_touches_focused`)
        // pins the multi-agent guarantee at the codemux layer.
        let mut a = Parser::new(5, 20, 100);
        let mut b = Parser::new(5, 20, 100);
        for i in 0..30 {
            a.process(format!("a{i}\r\n").as_bytes());
            b.process(format!("b{i}\r\n").as_bytes());
        }
        a.screen_mut().set_scrollback(15);
        assert_eq!(a.screen().scrollback(), 15);
        assert_eq!(
            b.screen().scrollback(),
            0,
            "agent b's offset must not move when a is scrolled",
        );
        b.screen_mut().set_scrollback(7);
        assert_eq!(
            a.screen().scrollback(),
            15,
            "agent a's offset must not move when b is scrolled",
        );
        assert_eq!(b.screen().scrollback(), 7);
    }

    #[test]
    fn no_overlay_active_returns_true_when_nothing_open() {
        assert!(no_overlay_active(
            None,
            PopupState::Closed,
            HelpState::Closed,
        ));
    }

    #[test]
    fn no_overlay_active_returns_false_when_help_open() {
        assert!(!no_overlay_active(
            None,
            PopupState::Closed,
            HelpState::Open,
        ));
    }

    #[test]
    fn no_overlay_active_returns_false_when_popup_open() {
        assert!(!no_overlay_active(
            None,
            PopupState::Open { selection: 0 },
            HelpState::Closed,
        ));
    }

    // ── scrollback methods (direct) ───────────────────────────────
    //
    // Built on the `AgentTransport::for_test` seam so the methods can
    // be exercised against a real `RuntimeAgent` without `claude` on
    // PATH. The transport just sits there — `cat` waits for input —
    // while the test pokes the parser directly to populate scrollback.

    fn ready_test_agent(scrollback_len: usize) -> RuntimeAgent {
        let transport =
            AgentTransport::for_test("scrollback-test".into(), 5, 20).expect("for_test transport");
        RuntimeAgent::ready(
            AgentId::new("a"),
            "a".into(),
            None,
            None,
            None,
            transport,
            5,
            20,
            scrollback_len,
        )
    }

    fn populate(agent: &mut RuntimeAgent, lines: u32) {
        if let AgentState::Ready { parser, .. } = &mut agent.state {
            for i in 0..lines {
                parser.process(format!("l{i}\r\n").as_bytes());
            }
        }
    }

    /// Stringify the [`AgentState`] discriminant for `panic!` messages
    /// in tests. `AgentState` doesn't derive `Debug` (the `Parser` it
    /// owns drags in heavy machinery and the state's identity here is
    /// the variant tag, not its payload), so we map by hand.
    fn state_variant_name(state: &AgentState) -> &'static str {
        match state {
            AgentState::Ready { .. } => "Ready",
            AgentState::Failed { .. } => "Failed",
            AgentState::Crashed { .. } => "Crashed",
        }
    }

    #[test]
    fn nudge_scrollback_moves_offset_into_history_then_back() {
        let mut agent = ready_test_agent(100);
        populate(&mut agent, 50);
        agent.nudge_scrollback(5);
        assert_eq!(agent.scrollback_offset(), 5);
        agent.nudge_scrollback(-2);
        assert_eq!(agent.scrollback_offset(), 3);
    }

    #[test]
    fn nudge_scrollback_saturates_at_zero_on_negative_overflow() {
        // Wheel-down past the live view must NOT wrap or panic — the
        // method delegates to `usize::saturating_add_signed`. Pinning
        // the explicit policy here means a future refactor can't
        // accidentally regress to wrapping arithmetic without breaking
        // this test.
        let mut agent = ready_test_agent(100);
        populate(&mut agent, 50);
        agent.nudge_scrollback(10);
        agent.nudge_scrollback(i32::MIN);
        assert_eq!(agent.scrollback_offset(), 0);
    }

    #[test]
    fn nudge_scrollback_only_touches_focused_agent() {
        // The runtime's "scroll persists per agent" UX contract: a
        // wheel tick on the focused tab must not perturb other agents'
        // offsets. Pins the multi-agent layer that the per-`Parser`
        // contract test (`scrollback_state_is_per_parser` above)
        // guarantees underneath.
        let mut a = ready_test_agent(100);
        let mut b = ready_test_agent(100);
        populate(&mut a, 30);
        populate(&mut b, 30);
        let mut agents = [a, b];
        agents[0].nudge_scrollback(7);
        assert_eq!(agents[0].scrollback_offset(), 7);
        assert_eq!(agents[1].scrollback_offset(), 0);
    }

    #[test]
    fn nudge_scrollback_no_op_on_failed_agent() {
        let mut agent = failed_agent("dead");
        agent.nudge_scrollback(5);
        assert_eq!(agent.scrollback_offset(), 0);
    }

    #[test]
    fn snap_to_live_resets_offset_to_zero() {
        let mut agent = ready_test_agent(100);
        populate(&mut agent, 50);
        agent.nudge_scrollback(15);
        assert_eq!(agent.scrollback_offset(), 15);
        agent.snap_to_live();
        assert_eq!(agent.scrollback_offset(), 0);
    }

    #[test]
    fn snap_to_live_no_op_on_failed_agent() {
        let mut agent = failed_agent("dead");
        agent.snap_to_live();
        assert_eq!(agent.scrollback_offset(), 0);
    }

    #[test]
    fn jump_to_top_clamps_to_buffer_length() {
        let mut agent = ready_test_agent(100);
        populate(&mut agent, 50);
        agent.jump_to_top();
        let offset = agent.scrollback_offset();
        assert!(offset > 0, "expected non-zero offset after jumping to top");
        assert!(
            offset <= 100,
            "offset {offset} must not exceed configured scrollback_len of 100",
        );
    }

    #[test]
    fn scrollback_offset_returns_zero_for_failed_agent() {
        let agent = failed_agent("dead");
        assert_eq!(agent.scrollback_offset(), 0);
    }

    #[test]
    fn mark_crashed_transitions_ready_to_crashed_preserving_parser_and_exit_code() {
        let mut agent = ready_test_agent(100);
        populate(&mut agent, 5);
        agent.mark_crashed(42);
        match &agent.state {
            AgentState::Crashed { parser, exit_code } => {
                assert_eq!(*exit_code, 42, "exit code must round-trip");
                let last = parser
                    .screen()
                    .rows(0, parser.screen().size().1)
                    .find(|row| !row.trim().is_empty())
                    .expect("crashed parser must retain at least one populated row");
                assert!(
                    last.starts_with('l'),
                    "preserved screen content should still show a populated row, got {last:?}",
                );
            }
            other => panic!(
                "expected Crashed after mark_crashed, got variant {}",
                state_variant_name(other),
            ),
        }
    }

    #[test]
    fn mark_crashed_no_op_on_failed_agent() {
        let mut agent = failed_agent("dead");
        agent.mark_crashed(7);
        assert!(
            matches!(agent.state, AgentState::Failed { .. }),
            "mark_crashed must not promote a Failed agent into Crashed",
        );
    }

    #[test]
    fn mark_crashed_no_op_on_already_crashed_agent() {
        let mut agent = ready_test_agent(100);
        agent.mark_crashed(1);
        agent.mark_crashed(2);
        match &agent.state {
            AgentState::Crashed { exit_code, .. } => assert_eq!(
                *exit_code, 1,
                "second mark_crashed must not overwrite the first exit code",
            ),
            other => panic!(
                "expected Crashed, got variant {}",
                state_variant_name(other)
            ),
        }
    }

    #[test]
    fn nudge_scrollback_moves_offset_on_crashed_agent() {
        let mut agent = ready_test_agent(100);
        populate(&mut agent, 50);
        agent.mark_crashed(0);
        agent.nudge_scrollback(5);
        assert_eq!(
            agent.scrollback_offset(),
            5,
            "scrollback should still respond on Crashed (parser is preserved)",
        );
    }

    #[test]
    fn snap_to_live_resets_offset_on_crashed_agent() {
        let mut agent = ready_test_agent(100);
        populate(&mut agent, 50);
        agent.nudge_scrollback(10);
        agent.mark_crashed(0);
        assert_eq!(agent.scrollback_offset(), 10);
        agent.snap_to_live();
        assert_eq!(agent.scrollback_offset(), 0);
    }

    #[test]
    fn jump_to_top_works_on_crashed_agent() {
        let mut agent = ready_test_agent(100);
        populate(&mut agent, 50);
        agent.mark_crashed(0);
        agent.jump_to_top();
        assert!(
            agent.scrollback_offset() > 0,
            "jump_to_top on a Crashed agent should reach a non-zero offset",
        );
    }

    #[test]
    fn is_working_returns_false_for_crashed_agent() {
        let mut agent = ready_test_agent(100);
        agent.mark_crashed(0);
        assert!(
            !agent.is_working(),
            "a Crashed agent has no foreground process — never working",
        );
    }

    #[test]
    fn title_returns_last_title_on_crashed_agent() {
        // Feed an OSC 0 title sequence, then crash. The last title
        // must remain readable so the renderer can keep showing the
        // tab label the user recognizes.
        let mut agent = ready_test_agent(100);
        if let AgentState::Ready { parser, .. } = &mut agent.state {
            parser.process(b"\x1b]0;hello\x07");
        }
        agent.mark_crashed(0);
        assert_eq!(agent.title(), Some("hello"));
    }

    // Render-fn integration tests via ratatui's TestBackend. These
    // drive the actual renderers against a synthetic terminal and
    // inspect the TabHitboxes the renderer populated, locking the
    // hitbox-recording invariants without resorting to inspecting
    // the rendered cells. This is the first TestBackend-based test
    // in this codebase; if more renderers grow hitbox / interaction
    // surfaces, lift the boilerplate into a small helper.

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn three_failed_agents() -> Vec<RuntimeAgent> {
        vec![failed_agent("a"), failed_agent("b"), failed_agent("c")]
    }

    #[test]
    fn render_status_bar_records_one_hitbox_per_agent_in_order() {
        // 80-cell-wide status bar at row 23, three agents. Expect
        // three hitboxes with the agents' ids in left-to-right
        // order — the tab strip's draw order is agent-vector order.
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let agents = three_failed_agents();
        let bindings = Bindings::default();
        let mut hb = TabHitboxes::default();
        terminal
            .draw(|frame| {
                render_status_bar(
                    frame,
                    Rect {
                        x: 0,
                        y: 23,
                        width: 80,
                        height: 1,
                    },
                    &agents,
                    1,
                    &bindings,
                    PrefixState::Idle,
                    AnimationPhase::default(),
                    &ChromeStyle::default(),
                    &mut hb,
                    &[],
                );
            })
            .unwrap();
        let ids: Vec<AgentId> = hb.rects.iter().map(|h| h.agent_id.clone()).collect();
        assert_eq!(
            ids,
            vec![AgentId::new("a"), AgentId::new("b"), AgentId::new("c")],
        );
    }

    #[test]
    fn render_status_bar_hitboxes_sit_on_the_status_row_and_are_non_overlapping() {
        // Anchor the geometry: every recorded rect must land on the
        // status bar's row and have nonzero width, and adjacent tabs
        // must not overlap (the separator span sits in the gap).
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let agents = three_failed_agents();
        let bindings = Bindings::default();
        let mut hb = TabHitboxes::default();
        terminal
            .draw(|frame| {
                render_status_bar(
                    frame,
                    Rect {
                        x: 0,
                        y: 23,
                        width: 80,
                        height: 1,
                    },
                    &agents,
                    0,
                    &bindings,
                    PrefixState::Idle,
                    AnimationPhase::default(),
                    &ChromeStyle::default(),
                    &mut hb,
                    &[],
                );
            })
            .unwrap();
        for h in &hb.rects {
            assert_eq!(h.rect.y, 23, "hitbox must sit on the status row");
            assert_eq!(h.rect.height, 1, "hitbox must be one row tall");
            assert!(h.rect.width > 0, "hitbox must have nonzero width");
        }
        for window in hb.rects.windows(2) {
            let left = &window[0].rect;
            let right = &window[1].rect;
            assert!(
                left.x.saturating_add(left.width) <= right.x,
                "tabs must not overlap (got {left:?} then {right:?})",
            );
        }
    }

    #[test]
    fn render_status_bar_clears_stale_hitboxes_first() {
        // Even though clearing is render_frame's job, a render that
        // is called twice in a row should leave hb with the latest
        // frame's count, not double-recorded entries. The renderer
        // itself must not append on top of pre-existing rects.
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let agents = three_failed_agents();
        let bindings = Bindings::default();
        // Pre-seed with stale rects from an imaginary previous frame.
        let mut hb = TabHitboxes::default();
        let stale = AgentId::new("stale-agent");
        hb.record(
            Rect {
                x: 0,
                y: 0,
                width: 99,
                height: 99,
            },
            stale.clone(),
        );
        // Caller (render_frame) clears before invoking render_status_bar.
        hb.clear();
        terminal
            .draw(|frame| {
                render_status_bar(
                    frame,
                    Rect {
                        x: 0,
                        y: 23,
                        width: 80,
                        height: 1,
                    },
                    &agents,
                    0,
                    &bindings,
                    PrefixState::Idle,
                    AnimationPhase::default(),
                    &ChromeStyle::default(),
                    &mut hb,
                    &[],
                );
            })
            .unwrap();
        assert_eq!(hb.rects.len(), 3, "stale rect must not leak forward");
        assert!(
            hb.rects.iter().all(|h| h.agent_id != stale),
            "stale agent id must be gone",
        );
    }

    #[test]
    fn render_left_pane_records_one_hitbox_per_agent() {
        // 80x24 terminal. NAV_PANE_WIDTH is 25; the bordered block
        // reserves one cell per side and the title row, so agent rows
        // start at nav_area.y + 1 and span x = 1..NAV_PANE_WIDTH - 1.
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let agents = three_failed_agents();
        let mut hb = TabHitboxes::default();
        terminal
            .draw(|frame| {
                render_left_pane(
                    frame,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 80,
                        height: 24,
                    },
                    &agents,
                    1,
                    "d",
                    AnimationPhase::default(),
                    &ChromeStyle::default(),
                    &mut hb,
                    &mut PaneHitbox::default(),
                    PaneOverlay::default(),
                );
            })
            .unwrap();
        let ids: Vec<AgentId> = hb.rects.iter().map(|h| h.agent_id.clone()).collect();
        assert_eq!(
            ids,
            vec![AgentId::new("a"), AgentId::new("b"), AgentId::new("c")],
        );
    }

    #[test]
    fn render_left_pane_hitboxes_skip_borders_and_advance_one_row_per_agent() {
        // The bordered block consumes one row top/bottom and one
        // cell left/right; clickable rows must sit *inside* the
        // border, advancing one row per agent.
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let agents = three_failed_agents();
        let mut hb = TabHitboxes::default();
        terminal
            .draw(|frame| {
                render_left_pane(
                    frame,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 80,
                        height: 24,
                    },
                    &agents,
                    0,
                    "d",
                    AnimationPhase::default(),
                    &ChromeStyle::default(),
                    &mut hb,
                    &mut PaneHitbox::default(),
                    PaneOverlay::default(),
                );
            })
            .unwrap();
        // x=1: skip the left border. width = NAV_PANE_WIDTH - 2 = 23:
        // skip both borders. y starts at 1 (skip top border) and
        // advances by 1 per agent.
        for (i, h) in hb.rects.iter().enumerate() {
            assert_eq!(h.rect.x, 1, "agent row must skip the left border");
            assert_eq!(h.rect.width, NAV_PANE_WIDTH - 2);
            assert_eq!(h.rect.height, 1);
            assert_eq!(h.rect.y, 1 + u16::try_from(i).unwrap());
        }
    }

    #[test]
    fn render_left_pane_drops_rows_that_overflow_the_pane() {
        // Tiny pane: 24 rows total, top + bottom border, fits 22 rows
        // for agents. Spawning 50 agents must not record bogus
        // hitboxes past the bottom border.
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let agents: Vec<RuntimeAgent> = (0..50)
            .map(|i| failed_agent(&format!("agent-{i}")))
            .collect();
        let mut hb = TabHitboxes::default();
        terminal
            .draw(|frame| {
                render_left_pane(
                    frame,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 80,
                        height: 24,
                    },
                    &agents,
                    0,
                    "d",
                    AnimationPhase::default(),
                    &ChromeStyle::default(),
                    &mut hb,
                    &mut PaneHitbox::default(),
                    PaneOverlay::default(),
                );
            })
            .unwrap();
        assert!(
            hb.rects.len() < agents.len(),
            "overflowing rows must be dropped, got {} hitboxes for {} agents",
            hb.rects.len(),
            agents.len(),
        );
        // No hitbox may sit on or past the bottom border (y=23).
        for h in &hb.rects {
            assert!(
                h.rect.y < 23,
                "hitbox at y={} crosses bottom border",
                h.rect.y,
            );
        }
    }

    /// Helper to scrape the top row of the rendered `TestBackend` buffer
    /// as a single string. Used by the crash-banner tests below to
    /// assert the banner copy without coupling to exact column counts.
    fn top_row_text(terminal: &Terminal<TestBackend>) -> String {
        let buf = terminal.backend().buffer();
        let area = buf.area;
        (area.x..area.x + area.width)
            .map(|x| buf[(x, area.y)].symbol())
            .collect::<String>()
    }

    #[test]
    fn render_agent_pane_paints_red_banner_for_nonzero_exit_code() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut agent = ready_test_agent(100);
        agent.mark_crashed(7);
        terminal
            .draw(|frame| {
                render_agent_pane(
                    frame,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 80,
                        height: 24,
                    },
                    &agent,
                    "d",
                    &mut PaneHitbox::default(),
                    PaneOverlay::default(),
                );
            })
            .unwrap();
        let row = top_row_text(&terminal);
        assert!(
            row.contains("session ended (exit 7)"),
            "non-zero exit banner missing copy, got: {row:?}",
        );
        assert!(
            row.contains("dismiss"),
            "non-zero exit banner missing dismiss hint, got: {row:?}",
        );
    }

    #[test]
    fn render_agent_pane_paints_connection_lost_banner_for_minus_one() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut agent = ready_test_agent(100);
        agent.mark_crashed(-1);
        terminal
            .draw(|frame| {
                render_agent_pane(
                    frame,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 80,
                        height: 24,
                    },
                    &agent,
                    "d",
                    &mut PaneHitbox::default(),
                    PaneOverlay::default(),
                );
            })
            .unwrap();
        let row = top_row_text(&terminal);
        assert!(
            row.contains("connection lost"),
            "minus-one banner must distinguish socket-level failure, got: {row:?}",
        );
    }

    #[test]
    fn render_agent_pane_falls_through_to_red_banner_for_synthetic_zero_exit() {
        // The live reap path auto-removes exit-0 agents before they
        // ever reach `Crashed`, so this scenario is only reachable
        // via direct `mark_crashed(0)` (tests, future internal calls).
        // The renderer no longer has a "clean exit" special case;
        // a synthetic 0 falls through the same red-banner path as any
        // other code so the rendering stays unsurprising rather than
        // silently producing a non-routable visual variant.
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut agent = ready_test_agent(100);
        agent.mark_crashed(0);
        terminal
            .draw(|frame| {
                render_agent_pane(
                    frame,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 80,
                        height: 24,
                    },
                    &agent,
                    "d",
                    &mut PaneHitbox::default(),
                    PaneOverlay::default(),
                );
            })
            .unwrap();
        let row = top_row_text(&terminal);
        assert!(
            row.contains("session ended (exit 0)"),
            "synthetic 0-exit must still render its code in copy, got: {row:?}",
        );
        assert!(
            row.contains('\u{2717}'),
            "synthetic 0-exit now shares the red ✗ treatment, got: {row:?}",
        );
    }

    #[test]
    fn render_agent_pane_banner_uses_configured_dismiss_chord() {
        // The orchestrator (render_frame) is responsible for resolving
        // the bindings to a chord string. Pass a custom label here to
        // verify the renderer interpolates whatever it's given rather
        // than hardcoding "d". Uses a synthetic exit-7 Crashed slot
        // (live exit-0 agents auto-reap before reaching the renderer,
        // so we pick a non-zero exit to stay close to the real path).
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut agent = ready_test_agent(100);
        agent.mark_crashed(7);
        terminal
            .draw(|frame| {
                render_agent_pane(
                    frame,
                    Rect {
                        x: 0,
                        y: 0,
                        width: 80,
                        height: 24,
                    },
                    &agent,
                    "ctrl+x",
                    &mut PaneHitbox::default(),
                    PaneOverlay::default(),
                );
            })
            .unwrap();
        let row = top_row_text(&terminal);
        assert!(
            row.contains("ctrl+x to dismiss"),
            "banner must interpolate the supplied chord label, got: {row:?}",
        );
    }

    // ── selection (cell math + dispatch state machine) ────────────
    //
    // End-to-end clipboard delivery is verified manually — OSC 52
    // lands on the user's host terminal, not on a `TestBackend`.

    fn cell(row: u16, col: u16) -> CellPos {
        CellPos { row, col }
    }

    #[test]
    fn normalized_range_handles_inverted_drag() {
        // Anchor below-and-right of head: must normalize so `start` is
        // top-left and `end` is bottom-right, the order
        // `vt100::Screen::contents_between` requires.
        let (start, end) = normalized_range(cell(5, 12), cell(2, 3));
        assert_eq!(start, cell(2, 3));
        assert_eq!(end, cell(5, 12));
    }

    #[test]
    fn normalized_range_preserves_already_ordered_drag() {
        let (start, end) = normalized_range(cell(2, 3), cell(5, 12));
        assert_eq!(start, cell(2, 3));
        assert_eq!(end, cell(5, 12));
    }

    #[test]
    fn row_bounds_single_row_selection_is_inclusive_at_both_ends() {
        // Single-row drag from col 4 to col 9 must yield iter
        // `4..10` so the cell at col 9 is included.
        let (lo, hi) = row_bounds(cell(2, 4), cell(2, 9), 2, 80);
        assert_eq!((lo, hi), (4, 10));
    }

    #[test]
    fn row_bounds_top_row_of_multirow_runs_to_pane_edge() {
        let (lo, hi) = row_bounds(cell(2, 4), cell(5, 9), 2, 80);
        assert_eq!((lo, hi), (4, 80));
    }

    #[test]
    fn row_bounds_middle_row_spans_full_pane_width() {
        let (lo, hi) = row_bounds(cell(2, 4), cell(5, 9), 3, 80);
        assert_eq!((lo, hi), (0, 80));
    }

    #[test]
    fn row_bounds_bottom_row_runs_from_zero_to_end_col_inclusive() {
        let (lo, hi) = row_bounds(cell(2, 4), cell(5, 9), 5, 80);
        assert_eq!((lo, hi), (0, 10));
    }

    #[test]
    fn row_bounds_single_row_clamps_end_col_to_pane_width() {
        // Defensive: pane shrank between Drag and render, end.col is
        // past the right edge. Must not produce a hi past pane_width.
        let (lo, hi) = row_bounds(cell(2, 4), cell(2, 100), 2, 80);
        assert_eq!((lo, hi), (4, 80));
    }

    #[test]
    fn pane_hitbox_cell_at_translates_to_pane_relative() {
        let mut hb = PaneHitbox::default();
        hb.record(
            Rect {
                x: 25,
                y: 0,
                width: 80,
                height: 24,
            },
            AgentId::new("a"),
        );
        // Click at screen (30, 5) with pane origin (25, 0) → cell (5, 5).
        let (id, c) = hb.cell_at(30, 5).unwrap();
        assert_eq!(id, AgentId::new("a"));
        assert_eq!(c, cell(5, 5));
    }

    #[test]
    fn pane_hitbox_cell_at_returns_none_outside_pane() {
        let mut hb = PaneHitbox::default();
        hb.record(
            Rect {
                x: 25,
                y: 0,
                width: 80,
                height: 24,
            },
            AgentId::new("a"),
        );
        // Click in the nav strip (x < 25): not in the pane.
        assert!(hb.cell_at(10, 5).is_none());
    }

    #[test]
    fn pane_hitbox_clamped_cell_at_clamps_to_nearest_edge() {
        let mut hb = PaneHitbox::default();
        hb.record(
            Rect {
                x: 25,
                y: 0,
                width: 80,
                height: 24,
            },
            AgentId::new("a"),
        );
        // Drag continued past the right edge of the pane: head ends
        // up clamped to pane_width - 1 (col 79 in pane-relative).
        let c = hb.clamped_cell_at(200, 50).unwrap();
        assert_eq!(c, cell(23, 79));
        // Drag dragged off the left + above the pane: clamps to (0, 0).
        let c = hb.clamped_cell_at(0, 0).unwrap();
        assert_eq!(c, cell(0, 0));
    }

    #[test]
    fn pane_hitbox_no_record_means_no_dispatch() {
        let hb = PaneHitbox::default();
        assert!(hb.cell_at(30, 5).is_none());
        assert!(hb.clamped_cell_at(30, 5).is_none());
    }

    #[test]
    fn pane_mouse_dispatch_down_inside_pane_arms_selection() {
        let mut hb = PaneHitbox::default();
        hb.record(
            Rect {
                x: 25,
                y: 0,
                width: 80,
                height: 24,
            },
            AgentId::new("a"),
        );
        let dispatch =
            pane_mouse_dispatch(MouseEventKind::Down(MouseButton::Left), 30, 5, &hb, None);
        assert_eq!(
            dispatch,
            Some(PaneMouseDispatch::Arm {
                agent: AgentId::new("a"),
                cell: cell(5, 5),
            }),
        );
    }

    #[test]
    fn pane_mouse_dispatch_down_outside_pane_returns_none() {
        // Click in chrome (e.g. tab strip): pane dispatcher must
        // return None so the loop falls through to tab_mouse_dispatch.
        let mut hb = PaneHitbox::default();
        hb.record(
            Rect {
                x: 25,
                y: 0,
                width: 80,
                height: 24,
            },
            AgentId::new("a"),
        );
        let dispatch =
            pane_mouse_dispatch(MouseEventKind::Down(MouseButton::Left), 10, 5, &hb, None);
        assert_eq!(dispatch, None);
    }

    #[test]
    fn pane_mouse_dispatch_drag_extends_when_selection_active() {
        let mut hb = PaneHitbox::default();
        hb.record(
            Rect {
                x: 25,
                y: 0,
                width: 80,
                height: 24,
            },
            AgentId::new("a"),
        );
        let sel = Selection {
            agent: AgentId::new("a"),
            anchor: cell(5, 5),
            head: cell(5, 5),
        };
        let dispatch = pane_mouse_dispatch(
            MouseEventKind::Drag(MouseButton::Left),
            40,
            10,
            &hb,
            Some(&sel),
        );
        assert_eq!(dispatch, Some(PaneMouseDispatch::Extend(cell(10, 15))));
    }

    #[test]
    fn pane_mouse_dispatch_drag_outside_pane_clamps() {
        let mut hb = PaneHitbox::default();
        hb.record(
            Rect {
                x: 25,
                y: 0,
                width: 80,
                height: 24,
            },
            AgentId::new("a"),
        );
        let sel = Selection {
            agent: AgentId::new("a"),
            anchor: cell(5, 5),
            head: cell(5, 5),
        };
        // Drag continued off the right edge: head clamps to pane edge
        // so the user can release outside the pane and still get the
        // selection they visibly drew.
        let dispatch = pane_mouse_dispatch(
            MouseEventKind::Drag(MouseButton::Left),
            300,
            5,
            &hb,
            Some(&sel),
        );
        assert_eq!(dispatch, Some(PaneMouseDispatch::Extend(cell(5, 79))));
    }

    #[test]
    fn pane_mouse_dispatch_drag_without_selection_is_none() {
        // Stray drag with nothing armed (e.g. drag started in chrome,
        // then crossed into the pane): pane dispatcher returns None
        // so the loop falls through. tab_mouse_dispatch will also
        // return None for Drag → harmless no-op.
        let mut hb = PaneHitbox::default();
        hb.record(
            Rect {
                x: 25,
                y: 0,
                width: 80,
                height: 24,
            },
            AgentId::new("a"),
        );
        let dispatch =
            pane_mouse_dispatch(MouseEventKind::Drag(MouseButton::Left), 30, 5, &hb, None);
        assert_eq!(dispatch, None);
    }

    #[test]
    fn pane_mouse_dispatch_up_with_selection_commits() {
        let mut hb = PaneHitbox::default();
        hb.record(
            Rect {
                x: 25,
                y: 0,
                width: 80,
                height: 24,
            },
            AgentId::new("a"),
        );
        let sel = Selection {
            agent: AgentId::new("a"),
            anchor: cell(5, 5),
            head: cell(5, 9),
        };
        let dispatch = pane_mouse_dispatch(
            MouseEventKind::Up(MouseButton::Left),
            34,
            5,
            &hb,
            Some(&sel),
        );
        assert_eq!(dispatch, Some(PaneMouseDispatch::Commit));
    }

    #[test]
    fn pane_mouse_dispatch_up_without_selection_is_none() {
        let mut hb = PaneHitbox::default();
        hb.record(
            Rect {
                x: 25,
                y: 0,
                width: 80,
                height: 24,
            },
            AgentId::new("a"),
        );
        let dispatch = pane_mouse_dispatch(MouseEventKind::Up(MouseButton::Left), 30, 5, &hb, None);
        assert_eq!(dispatch, None);
    }

    #[test]
    fn pane_mouse_dispatch_right_button_is_none() {
        // Right-click context menu is a deliberate v1 non-goal.
        let mut hb = PaneHitbox::default();
        hb.record(
            Rect {
                x: 25,
                y: 0,
                width: 80,
                height: 24,
            },
            AgentId::new("a"),
        );
        let dispatch =
            pane_mouse_dispatch(MouseEventKind::Down(MouseButton::Right), 30, 5, &hb, None);
        assert_eq!(dispatch, None);
    }

    #[test]
    fn write_clipboard_to_emits_osc_52_with_base64_payload() {
        // Exact byte shape: ESC ] 5 2 ; c ; <base64> BEL.
        // "hi" → "aGk=" in standard base64.
        let mut buf: Vec<u8> = Vec::new();
        write_clipboard_to(&mut buf, "hi").unwrap();
        assert_eq!(buf, b"\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn write_clipboard_to_handles_empty_string_as_empty_payload() {
        // Empty selection emits OSC 52 with an empty body, which most
        // terminals treat as a clipboard clear. commit_selection guards
        // against this by skipping the call when text is empty, but the
        // helper itself is permissive — same as a Vec<u8> writer.
        let mut buf: Vec<u8> = Vec::new();
        write_clipboard_to(&mut buf, "").unwrap();
        assert_eq!(buf, b"\x1b]52;c;\x07");
    }

    #[test]
    fn write_clipboard_to_round_trips_multibyte_utf8() {
        // Selection text may contain emoji / CJK / accents — base64
        // operates on bytes, so any UTF-8 string round-trips. Just
        // verify the call succeeds and the prefix/suffix are intact.
        let mut buf: Vec<u8> = Vec::new();
        write_clipboard_to(&mut buf, "olá 🦀").unwrap();
        assert!(buf.starts_with(b"\x1b]52;c;"));
        assert!(buf.ends_with(b"\x07"));
    }

    #[test]
    fn vt100_contents_between_extracts_selection_substring() {
        // End-to-end pin on the vt100 boundary: given a Parser with
        // known content and a normalized cell range, contents_between
        // must return the substring the selection covers — including
        // the +1 offset on end_col that commit_selection applies to
        // make the bound inclusive.
        let mut parser = Parser::new(5, 20, 100);
        parser.process(b"hello world\r\n");
        let sel = Selection {
            agent: AgentId::new("a"),
            anchor: cell(0, 0),
            head: cell(0, 4),
        };
        let (start, end) = normalized_range(sel.anchor, sel.head);
        let text = parser.screen().contents_between(
            start.row,
            start.col,
            end.row,
            end.col.saturating_add(1),
        );
        assert_eq!(text, "hello");
    }

    #[test]
    fn vt100_contents_between_handles_multirow_selection() {
        // Top row from start.col to width, full middle rows, bottom
        // row 0 to end.col + 1.
        let mut parser = Parser::new(5, 20, 100);
        parser.process(b"row0 starts here\r\nrow1 fully covered\r\nrow2 ends here");
        let sel = Selection {
            agent: AgentId::new("a"),
            anchor: cell(0, 5),
            head: cell(2, 3),
        };
        let (start, end) = normalized_range(sel.anchor, sel.head);
        let text = parser.screen().contents_between(
            start.row,
            start.col,
            end.row,
            end.col.saturating_add(1),
        );
        assert!(text.starts_with("starts here"));
        assert!(text.contains("row1 fully covered"));
        assert!(text.ends_with("row2"));
    }

    #[test]
    fn paint_selection_if_active_flips_reversed_modifier_on_selected_cells() {
        // TestBackend lets us inspect the rendered buffer cell-by-cell.
        // A 1-row selection from col 2 to col 5 in a 0,0,10,3 area must
        // leave only those four cells with the REVERSED modifier set.
        let backend = TestBackend::new(10, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        let area = Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 3,
        };
        let agent_id = AgentId::new("a");
        let sel = Selection {
            agent: agent_id.clone(),
            anchor: cell(0, 2),
            head: cell(0, 5),
        };
        terminal
            .draw(|frame| {
                paint_selection_if_active(frame, area, &agent_id, Some(&sel));
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        for x in 0..10u16 {
            let modifier = buf[(x, 0)].modifier;
            let in_selection = (2..=5).contains(&x);
            assert_eq!(
                modifier.contains(Modifier::REVERSED),
                in_selection,
                "cell at x={x} expected_in_selection={in_selection}",
            );
        }
    }

    #[test]
    fn paint_selection_if_active_skips_when_agent_id_does_not_match() {
        // Selection bound to agent "a" must not paint over a frame
        // being rendered for agent "b" — common case when the user
        // switches tabs but the per-frame sync hasn't run yet.
        let backend = TestBackend::new(10, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        let area = Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 3,
        };
        let sel = Selection {
            agent: AgentId::new("a"),
            anchor: cell(0, 0),
            head: cell(0, 9),
        };
        terminal
            .draw(|frame| {
                paint_selection_if_active(frame, area, &AgentId::new("b"), Some(&sel));
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        for x in 0..10u16 {
            assert!(
                !buf[(x, 0)].modifier.contains(Modifier::REVERSED),
                "no cell should be flipped when agent id does not match",
            );
        }
    }

    /// `failure_layout` centers each line in the pane area both
    /// vertically (top pad = `(height - lines) / 2`) and horizontally
    /// (left pad = `(width - len) / 2`). The header line gets
    /// `is_header: true` so the renderer can pick the bold style.
    /// Locked down so a future refactor of the layout math doesn't
    /// silently shift cells the user clicks.
    #[test]
    fn failure_layout_centers_lines_and_marks_header() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 10,
        };
        let layout = failure_layout("foo", "boom\nbang", area);
        // header + blank + 2 body lines = 4 visible rows; (10 - 4) / 2 = 3.
        assert_eq!(layout.len(), 4, "header + blank + 2 body lines");
        assert_eq!(layout[0].row, 3, "vertical centering pads top by 3");
        assert!(layout[0].is_header, "row 0 is the header");
        assert_eq!(
            layout[0].content, "✗ bootstrap of foo failed",
            "header copy includes host",
        );
        // (40 - 25) / 2 = 7 for the header (25 visible chars).
        assert_eq!(layout[0].col, 7, "header centered horizontally");
        // Blank line (row 4) — still emitted so the row mapping stays
        // gap-free, useful for the renderer painting a contiguous block.
        assert_eq!(layout[1].row, 4);
        assert!(layout[1].content.is_empty());
        assert!(!layout[1].is_header);
        // Body lines (rows 5, 6).
        assert_eq!(layout[2].row, 5);
        assert_eq!(layout[2].content, "boom");
        assert_eq!(layout[3].row, 6);
        assert_eq!(layout[3].content, "bang");
    }

    /// When the pane is too short for all the content lines, vertical
    /// centering pads the top to 0 and the layout drops the rows that
    /// fall off the bottom. Ensures the failure renderer never tries
    /// to paint past the pane and that the selection extractor agrees
    /// on what's on screen.
    #[test]
    fn failure_layout_clips_when_pane_too_short() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 2,
        };
        let layout = failure_layout("foo", "line1\nline2\nline3", area);
        assert_eq!(layout.len(), 2, "only first 2 lines fit in height=2");
        assert_eq!(layout[0].row, 0);
        assert_eq!(layout[1].row, 1);
    }

    /// `failure_layout` uses `UnicodeWidthStr::width` rather than
    /// `chars().count()` so wide glyphs (CJK, emoji) center on the same
    /// cells the renderer's `Paragraph::Center` lands on. Locked down
    /// because a regression to char count would shift selection-extraction
    /// off the rendered cells by `width - 1` columns per wide glyph and
    /// the user would copy garbage.
    #[test]
    fn failure_layout_centers_using_display_width_not_char_count() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 10,
        };
        // Body line `"橋橋橋"` — three CJK glyphs, each 2 cells wide.
        // Display width = 6, char count = 3. Header centering for host "f"
        // is independent and isolates the body-line math.
        let layout = failure_layout("f", "橋橋橋", area);
        // Body line is the third entry (header, blank, body).
        let body = &layout[2];
        assert_eq!(body.content, "橋橋橋");
        // (40 - 6) / 2 = 17, NOT (40 - 3) / 2 = 18 if we'd used char count.
        assert_eq!(
            body.col, 17,
            "centering must use display width (6 cells), not char count (3)",
        );
    }

    /// `failure_text_in_range` mirrors `vt100::Screen::contents_between`
    /// for the Failed pane: drag-selecting cells over the centered
    /// text returns exactly those characters, with leading-pad cells
    /// outside any line content reading as spaces (preserved at the
    /// front, trimmed at the trailing end so the clipboard doesn't
    /// get a wall of right-margin padding).
    #[test]
    fn failure_text_in_range_extracts_centered_content() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 10,
        };
        // From earlier test: header lands at row 3, col 7, content
        // "✗ bootstrap of foo failed" (25 chars).
        // Drag covering the entire header row (row 3, cols 0..40):
        // 7 leading pad cells + 25 content chars; trailing pad stripped.
        let text = failure_text_in_range(
            "foo",
            "boom",
            area,
            CellPos { row: 3, col: 0 },
            CellPos { row: 3, col: 39 },
        );
        assert_eq!(
            text, "       ✗ bootstrap of foo failed",
            "leading padding preserved (matches vt100), trailing trimmed",
        );

        // Sub-range inside the header — only the inclusive cells.
        // Header content starts at col 7, so col 9 = 'b' of "bootstrap".
        let text = failure_text_in_range(
            "foo",
            "boom",
            area,
            CellPos { row: 3, col: 9 },
            CellPos { row: 3, col: 17 },
        );
        assert_eq!(text, "bootstrap", "sub-range extracts inner chars");
    }

    /// Multi-row drag spans the header, the blank spacer row, and a
    /// body line — the result preserves leading pad on each row and
    /// trims trailing, matching vt100's per-row behavior. Each row is
    /// joined with `\n` so the user copies the visual line structure.
    #[test]
    fn failure_text_in_range_spans_multiple_rows() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 10,
        };
        // Header row 3 (col 7..32), blank row 4, body row 5 ("boom" at col 18..22).
        let text = failure_text_in_range(
            "foo",
            "boom",
            area,
            CellPos { row: 3, col: 0 },
            CellPos { row: 5, col: 39 },
        );
        assert_eq!(
            text,
            "       ✗ bootstrap of foo failed\n\n                  boom",
        );
    }

    /// A drag that lands entirely on padding (above the centered text,
    /// below the centered text, or in the side margins) returns "" so
    /// `commit_selection`'s `text.is_empty()` short-circuit skips the
    /// OSC 52 write and the user's existing clipboard isn't clobbered
    /// by an accidental click on dead space.
    #[test]
    fn failure_text_in_range_returns_empty_for_pure_padding() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 10,
        };
        // Row 0 is above the centered content (which starts at row 3).
        let text = failure_text_in_range(
            "foo",
            "boom",
            area,
            CellPos { row: 0, col: 0 },
            CellPos { row: 0, col: 39 },
        );
        assert!(text.is_empty(), "padding-only drag yields empty string");
    }

    /// End-to-end check that `render_failure_pane` paints text at the
    /// same cells `failure_text_in_range` extracts from. Regression
    /// guard — the renderer and extractor must stay in lockstep, or
    /// drag-to-copy returns text the user didn't actually highlight.
    #[test]
    fn render_failure_pane_paints_at_extractor_cells() {
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 10,
        };
        terminal
            .draw(|frame| {
                render_failure_pane(frame, area, "foo", "boom");
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        // Header lands at row 3, col 7..32 ("✗ bootstrap of foo failed").
        // Verify a known cell — row 3, col 9 — holds the 'b' of
        // "bootstrap" and that cell 0 (a padding cell) is blank.
        assert_eq!(
            buf[(9, 3)].symbol(),
            "b",
            "header 'b' at the cell the extractor picks for col=9",
        );
        assert_eq!(
            buf[(0, 3)].symbol(),
            " ",
            "left margin of the header row is padding",
        );
    }

    #[test]
    fn paint_hover_url_if_active_underlines_url_range_and_tints_cyan() {
        // Mirrors paint_selection_if_active_flips_reversed_modifier_on_selected_cells.
        let backend = TestBackend::new(10, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        let area = Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 3,
        };
        let agent_id = AgentId::new("a");
        let hover = HoverUrl {
            agent: agent_id.clone(),
            row: 1,
            cols: 2..6,
            url: "https://x".to_string(),
        };
        terminal
            .draw(|frame| {
                paint_hover_url_if_active(frame, area, &agent_id, Some(&hover));
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        for x in 0..10u16 {
            let cell = &buf[(x, 1)];
            let in_url = (2..6).contains(&x);
            assert_eq!(
                cell.modifier.contains(Modifier::UNDERLINED),
                in_url,
                "underline at x={x} expected_in_url={in_url}",
            );
            if in_url {
                assert_eq!(cell.fg, Color::Cyan, "cyan fg at x={x}");
            }
        }
        for x in 0..10u16 {
            assert!(!buf[(x, 0)].modifier.contains(Modifier::UNDERLINED));
            assert!(!buf[(x, 2)].modifier.contains(Modifier::UNDERLINED));
        }
    }

    #[test]
    fn paint_hover_url_if_active_skips_when_agent_id_does_not_match() {
        let backend = TestBackend::new(10, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        let area = Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 3,
        };
        let hover = HoverUrl {
            agent: AgentId::new("a"),
            row: 0,
            cols: 0..10,
            url: "https://x".to_string(),
        };
        terminal
            .draw(|frame| {
                paint_hover_url_if_active(frame, area, &AgentId::new("b"), Some(&hover));
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        for x in 0..10u16 {
            assert!(
                !buf[(x, 0)].modifier.contains(Modifier::UNDERLINED),
                "no cell should be underlined when agent id does not match",
            );
        }
    }

    /// `paint_hyperlinks_post_draw` should bracket its writes with
    /// DECSC/DECRC and emit, for each URL, an OSC 8 setup followed by
    /// per-cell SGR plus cell contents and a closing OSC 8 reset.
    /// Verified at the byte level rather than via a fake terminal: the
    /// post-draw approach deliberately bypasses ratatui's diff/emit
    /// pipeline, so `TestBackend` has nothing to observe.
    #[test]
    fn paint_hyperlinks_post_draw_emits_osc_8_wrap_around_cell_walk() {
        let mut parser = vt100::Parser::new(3, 40, 0);
        parser.process(b"plain row\r\nsee https://example.com here\r\nbottom");

        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 3,
        };
        let mut out = Vec::new();
        paint_hyperlinks_post_draw(&mut out, area, parser.screen()).unwrap();
        let s = String::from_utf8(out).expect("ASCII-only output");

        assert!(s.starts_with("\x1b7"), "leads with DECSC, got {s:?}");
        assert!(s.ends_with("\x1b8"), "trails with DECRC, got {s:?}");
        // CUP positions cursor 1-based at row 2 (URL is on screen row 1),
        // col 5 ('h' is at pane-relative col 4).
        assert!(
            s.contains("\x1b[2;5H"),
            "CUP for first URL cell missing, got {s:?}",
        );
        assert!(
            s.contains("\x1b]8;;https://example.com\x1b\\"),
            "OSC 8 setup with URL missing, got {s:?}",
        );
        // Each URL char gets re-printed under the active hyperlink so
        // every cell receives the OSC 8 attribute.
        for ch in "https://example.com".chars() {
            assert!(
                s.contains(ch),
                "URL char {ch:?} missing from re-print, got {s:?}",
            );
        }
        assert!(
            s.contains("\x1b]8;;\x1b\\"),
            "OSC 8 reset missing, got {s:?}",
        );
    }

    #[test]
    fn paint_hyperlinks_post_draw_translates_pane_offset_into_terminal_cup() {
        let mut parser = vt100::Parser::new(1, 40, 0);
        parser.process(b"https://example.com");
        // Pane offset by 10 cols, 5 rows in the terminal — CUP must
        // reflect the absolute position, not the pane-local one.
        let area = Rect {
            x: 10,
            y: 5,
            width: 40,
            height: 1,
        };
        let mut out = Vec::new();
        paint_hyperlinks_post_draw(&mut out, area, parser.screen()).unwrap();
        let s = String::from_utf8(out).expect("ASCII-only output");
        // 1-based CUP, so screen row 0 + pane row 5 → "6", and pane-local
        // col 0 + pane offset 10 → "11".
        assert!(
            s.contains("\x1b[6;11H"),
            "CUP must translate pane offset into absolute coords, got {s:?}",
        );
    }

    #[test]
    fn paint_hyperlinks_post_draw_is_a_no_op_when_no_urls_present() {
        let mut parser = vt100::Parser::new(2, 20, 0);
        parser.process(b"plain text\r\nno url here");
        let area = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 2,
        };
        let mut out = Vec::new();
        paint_hyperlinks_post_draw(&mut out, area, parser.screen()).unwrap();
        assert!(
            out.is_empty(),
            "no bytes should be written when there are no URLs, got {out:?}",
        );
    }

    /// The bug this whole post-draw approach exists to fix: with the
    /// previous `paint_hyperlinks` (cell-symbol baking) the rendered URL
    /// came out as `hetps://example.coms` because ratatui's diff treated
    /// the OSC-8-bearing cell as a 27-cell-wide grapheme and suppressed
    /// the immediately-following cell from the update list, leaving
    /// stale content visible. The post-draw painter has no diff, so the
    /// emitted byte stream contains every URL char in order.
    #[test]
    fn paint_hyperlinks_post_draw_does_not_drop_chars_adjacent_to_url_boundaries() {
        let mut parser = vt100::Parser::new(1, 40, 0);
        parser.process(b"see https://example.com here");
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 1,
        };
        let mut out = Vec::new();
        paint_hyperlinks_post_draw(&mut out, area, parser.screen()).unwrap();
        let s = String::from_utf8(out).expect("ASCII-only output");

        // The URL must appear, in order, inside the OSC 8 wrap. Per-cell
        // SGR escapes are interleaved between the URL chars (one full
        // reset + fg/bg per cell), so we strip CSI sequences before
        // looking for the URL substring rather than asserting it appears
        // verbatim. The setup/reset OSC 8 markers anchor the search
        // window to the URL region.
        let setup = "\x1b]8;;https://example.com\x1b\\";
        let reset = "\x1b]8;;\x1b\\";
        let setup_at = s.find(setup).expect("OSC 8 setup present");
        let reset_at = s[setup_at..]
            .find(reset)
            .expect("OSC 8 reset present after setup");
        let between = &s[setup_at + setup.len()..setup_at + reset_at];
        let stripped = strip_csi_sequences(between);
        assert_eq!(
            stripped, "https://example.com",
            "URL must be re-printed contiguously between OSC 8 setup and reset",
        );
    }

    /// Test helper: drop all `\x1b[…<final>` CSI escape sequences from a
    /// string so we can compare the visible glyph stream against the
    /// expected URL text without per-cell SGR noise getting in the way.
    /// CSI is `ESC [`, parameter bytes `0x30-0x3F`, intermediate bytes
    /// `0x20-0x2F`, terminated by a final byte `0x40-0x7E`.
    fn strip_csi_sequences(input: &str) -> String {
        let mut out = String::with_capacity(input.len());
        let bytes = input.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                i += 2;
                while i < bytes.len() && (0x30..=0x3F).contains(&bytes[i]) {
                    i += 1;
                }
                while i < bytes.len() && (0x20..=0x2F).contains(&bytes[i]) {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1; // skip the final byte
                }
            } else {
                out.push(bytes[i] as char);
                i += 1;
            }
        }
        out
    }

    #[test]
    fn emit_cell_sgr_resets_then_emits_attrs_and_colors_for_a_styled_cell() {
        // Bold + underline, red fg on yellow bg, then "X". After the
        // process, the cell at col 0 should report all those attrs.
        let mut parser = vt100::Parser::new(1, 5, 0);
        parser.process(b"\x1b[1;4;31;43mX\x1b[0m");
        let cell = parser.screen().cell(0, 0).expect("cell exists");

        let mut out = Vec::new();
        emit_cell_sgr(&mut out, cell).unwrap();
        let s = String::from_utf8(out).expect("ASCII-only");
        assert!(s.starts_with("\x1b[0m"), "leads with full reset, got {s:?}");
        assert!(s.contains("\x1b[1m"), "bold missing, got {s:?}");
        assert!(s.contains("\x1b[4m"), "underline missing, got {s:?}");
        // vt100 normalizes the standard 16 colors to indexed-256 entries,
        // so "red" and "yellow" surface as Idx(1) / Idx(3) and we emit
        // them via the 256-color SGR form.
        assert!(s.contains("\x1b[38;5;1m"), "red fg missing, got {s:?}");
        assert!(s.contains("\x1b[48;5;3m"), "yellow bg missing, got {s:?}");
    }

    #[test]
    fn emit_color_emits_default_indexed_and_rgb_forms() {
        let mut out = Vec::new();
        emit_color(&mut out, vt100::Color::Default, true).unwrap();
        emit_color(&mut out, vt100::Color::Default, false).unwrap();
        emit_color(&mut out, vt100::Color::Idx(42), true).unwrap();
        emit_color(&mut out, vt100::Color::Rgb(10, 20, 30), false).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s, "\x1b[39m\x1b[49m\x1b[38;5;42m\x1b[48;2;10;20;30m");
    }

    #[test]
    fn emit_mouse_cursor_shape_writes_osc_22_with_pointer_or_default() {
        let mut out = Vec::new();
        emit_mouse_cursor_shape(&mut out, true).unwrap();
        emit_mouse_cursor_shape(&mut out, false).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s, "\x1b]22;pointer\x1b\\\x1b]22;default\x1b\\");
    }

    #[test]
    fn update_hover_returns_activated_on_none_to_some() {
        let mut hover: Option<HoverUrl> = None;
        let new = HoverUrl {
            agent: AgentId::new("a"),
            row: 0,
            cols: 0..5,
            url: "https://x".into(),
        };
        let transition = update_hover(&mut hover, Some(new.clone()));
        assert_eq!(transition, HoverTransition::Activated);
        assert_eq!(hover, Some(new));
    }

    #[test]
    fn update_hover_returns_deactivated_on_some_to_none() {
        let mut hover: Option<HoverUrl> = Some(HoverUrl {
            agent: AgentId::new("a"),
            row: 0,
            cols: 0..5,
            url: "https://x".into(),
        });
        let transition = update_hover(&mut hover, None);
        assert_eq!(transition, HoverTransition::Deactivated);
        assert_eq!(hover, None);
    }

    #[test]
    fn update_hover_returns_unchanged_when_active_state_unchanged() {
        // Some → Some (different cell or different URL): pointer was
        // already a hand, no transition.
        let mut hover: Option<HoverUrl> = Some(HoverUrl {
            agent: AgentId::new("a"),
            row: 0,
            cols: 0..5,
            url: "https://x".into(),
        });
        let next = HoverUrl {
            agent: AgentId::new("a"),
            row: 0,
            cols: 6..11,
            url: "https://y".into(),
        };
        assert_eq!(
            update_hover(&mut hover, Some(next.clone())),
            HoverTransition::Unchanged,
        );
        assert_eq!(hover, Some(next));

        // None → None: also no transition.
        let mut hover: Option<HoverUrl> = None;
        assert_eq!(update_hover(&mut hover, None), HoverTransition::Unchanged,);
    }

    #[test]
    fn apply_hover_cursor_emits_pointer_on_activated_default_on_deactivated_nothing_on_unchanged() {
        let mut out = Vec::new();
        apply_hover_cursor(&mut out, HoverTransition::Activated).unwrap();
        apply_hover_cursor(&mut out, HoverTransition::Deactivated).unwrap();
        apply_hover_cursor(&mut out, HoverTransition::Unchanged).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert_eq!(s, "\x1b]22;pointer\x1b\\\x1b]22;default\x1b\\");
    }

    fn ready_agent_with(id: &str, cols: u16, body: &[u8]) -> RuntimeAgent {
        let transport =
            AgentTransport::for_test(format!("{id}-test"), 5, cols).expect("for_test transport");
        let mut agent = RuntimeAgent::ready(
            AgentId::new(id),
            id.into(),
            None,
            None,
            None,
            transport,
            5,
            cols,
            100,
        );
        if let AgentState::Ready { parser, .. } = &mut agent.state {
            parser.process(body);
        }
        agent
    }

    #[test]
    fn compute_hover_returns_url_under_pane_cell() {
        let agents = vec![ready_agent_with("h", 40, b"see https://example.com here")];
        let mut hitbox = PaneHitbox::default();
        hitbox.record(
            Rect {
                x: 10,
                y: 5,
                width: 40,
                height: 5,
            },
            AgentId::new("h"),
        );
        let span =
            compute_hover(&hitbox, &agents, 14, 5).expect("col 14 maps to pane col 4 (the 'h')");
        assert_eq!(span.url, "https://example.com");
        assert_eq!(span.agent, AgentId::new("h"));
        assert_eq!(span.row, 0);
        assert_eq!(span.cols, 4..23);
    }

    #[test]
    fn compute_hover_returns_none_when_screen_cell_outside_pane() {
        let agents = vec![ready_agent_with("h", 40, b"https://example.com")];
        let mut hitbox = PaneHitbox::default();
        hitbox.record(
            Rect {
                x: 10,
                y: 5,
                width: 40,
                height: 5,
            },
            AgentId::new("h"),
        );
        // (0, 0) is well outside the pane (x=10..50, y=5..10).
        assert!(compute_hover(&hitbox, &agents, 0, 0).is_none());
    }

    #[test]
    fn compute_hover_returns_none_when_no_url_under_cell() {
        let agents = vec![ready_agent_with("h", 40, b"plain text, no url here")];
        let mut hitbox = PaneHitbox::default();
        hitbox.record(
            Rect {
                x: 0,
                y: 0,
                width: 40,
                height: 5,
            },
            AgentId::new("h"),
        );
        assert!(compute_hover(&hitbox, &agents, 5, 0).is_none());
    }

    /// Locks in the [`UrlOpener`] seam: the trait must be implementable
    /// outside the production [`OsUrlOpener`] so future event-loop
    /// tests can record URLs without spawning a browser. If this stops
    /// compiling because the trait gained a method or sealed itself,
    /// that's a regression in the abstraction.
    #[test]
    fn url_opener_trait_supports_recording_mock_implementations() {
        use std::cell::RefCell;
        struct Recorder {
            calls: RefCell<Vec<String>>,
        }
        impl UrlOpener for Recorder {
            fn open(&self, url: &str) -> io::Result<()> {
                self.calls.borrow_mut().push(url.to_string());
                Ok(())
            }
        }
        let r = Recorder {
            calls: RefCell::new(Vec::new()),
        };
        let opener: &dyn UrlOpener = &r;
        opener.open("https://example.com").unwrap();
        opener.open("https://second.example").unwrap();
        assert_eq!(
            r.calls.borrow().as_slice(),
            &["https://example.com", "https://second.example"],
        );
    }

    /// `matches_url_modifier` is the gate between a bare-modifier KKP
    /// event and the yield state machine. Locking the matrix down so a
    /// future refactor that adds a new `MouseUrlModifier` variant
    /// can't silently miss a left-vs-right physical-key case.
    #[test]
    fn matches_url_modifier_covers_both_sides_for_each_variant() {
        use ModifierKeyCode::*;
        // Cmd: Super + Meta, both sides; everything else rejected.
        for mk in [LeftSuper, RightSuper, LeftMeta, RightMeta] {
            assert!(matches_url_modifier(mk, MouseUrlModifier::Cmd), "{mk:?}");
        }
        for mk in [
            LeftControl,
            RightControl,
            LeftAlt,
            RightAlt,
            LeftShift,
            RightShift,
        ] {
            assert!(!matches_url_modifier(mk, MouseUrlModifier::Cmd), "{mk:?}");
        }
        // Ctrl
        assert!(matches_url_modifier(LeftControl, MouseUrlModifier::Ctrl));
        assert!(matches_url_modifier(RightControl, MouseUrlModifier::Ctrl));
        assert!(!matches_url_modifier(LeftSuper, MouseUrlModifier::Ctrl));
        // Alt
        assert!(matches_url_modifier(LeftAlt, MouseUrlModifier::Alt));
        assert!(matches_url_modifier(RightAlt, MouseUrlModifier::Alt));
        assert!(!matches_url_modifier(LeftShift, MouseUrlModifier::Alt));
        // Shift
        assert!(matches_url_modifier(LeftShift, MouseUrlModifier::Shift));
        assert!(matches_url_modifier(RightShift, MouseUrlModifier::Shift));
        assert!(!matches_url_modifier(LeftAlt, MouseUrlModifier::Shift));
        // None: rejects everything.
        for mk in [LeftSuper, LeftControl, LeftAlt, LeftShift, LeftMeta] {
            assert!(!matches_url_modifier(mk, MouseUrlModifier::None), "{mk:?}");
        }
    }

    /// `CaptureMode::Disabled` short-circuits every yield-reason
    /// setter — yielding from a state that never had capture would
    /// emit a `?1006l` for a terminal that never opted into `?1006h`,
    /// which is at best wasted bytes and at worst a state-machine
    /// confusion in some terminal emulators. The state assertions
    /// below don't write to stdout because `sync` short-circuits on
    /// the `Disabled` arm before any `execute!` call.
    #[test]
    fn mouse_capture_state_no_op_when_capture_inactive() {
        let mut s = MouseCaptureState::new(false);
        assert_eq!(s.mode, CaptureMode::Disabled);
        s.set_url_modifier_held(true);
        assert_eq!(
            s.mode,
            CaptureMode::Disabled,
            "url-modifier yield must not transition Disabled mode",
        );
        assert!(s.reasons.url_modifier_held, "reason bit still tracks");
        s.set_url_modifier_held(false);
        assert_eq!(
            s.mode,
            CaptureMode::Disabled,
            "url-modifier release must not transition Disabled mode",
        );
        s.set_focused_failed(true);
        assert_eq!(
            s.mode,
            CaptureMode::Disabled,
            "focused-Failed yield must not transition Disabled mode",
        );
        s.lose_focus();
        assert_eq!(
            s.mode,
            CaptureMode::Disabled,
            "focus loss must not transition Disabled mode",
        );
    }

    /// Two yield reasons (URL modifier, focused-Failed) are independent:
    /// either one alone keeps capture yielded, and capture only reclaims
    /// when both clear. Locked down so a future refactor that consolidates
    /// reasons into a single bool can't silently regress the "alt-tab to
    /// a Ready agent while still holding Cmd" interaction.
    #[test]
    fn mouse_capture_state_yield_reasons_compose_independently() {
        // Construct in `Captured` mode without going through `new` so
        // the test doesn't write to stdout. `sync` is what would emit;
        // we mutate `mode` directly to simulate "terminal accepted the
        // initial enable" without actually touching the OS.
        let mut s = MouseCaptureState {
            mode: CaptureMode::Captured,
            reasons: YieldReasons::default(),
        };
        assert!(!s.reasons.any(), "no reasons → captured");

        s.reasons.url_modifier_held = true;
        s.reasons.focused_failed = true;
        assert!(s.reasons.any(), "both reasons → would yield");

        s.reasons.url_modifier_held = false;
        assert!(
            s.reasons.any(),
            "focused-Failed alone keeps capture yielded",
        );

        s.reasons.focused_failed = false;
        assert!(!s.reasons.any(), "no reasons → would reclaim");
    }

    /// `lose_focus` clears the URL-modifier bit (the OS swallows the
    /// release on alt-tab) but leaves `focused_failed` intact — re-
    /// entering the codemux window with focus still on a Failed pane
    /// must stay yielded without the user clicking back into the pane.
    #[test]
    fn mouse_capture_state_lose_focus_preserves_focused_failed() {
        let mut s = MouseCaptureState {
            mode: CaptureMode::Disabled,
            reasons: YieldReasons {
                url_modifier_held: true,
                focused_failed: true,
            },
        };
        s.lose_focus();
        assert!(!s.reasons.url_modifier_held, "url-modifier bit cleared");
        assert!(
            s.reasons.focused_failed,
            "focused-Failed bit preserved across alt-tab",
        );
    }

    /// The PID prefix is the whole point of `daemon_agent_id_for` — it
    /// is what stops a relaunched codemux from re-attaching to the
    /// previous run's surviving remote daemon. Lock the shape down so
    /// a future refactor cannot quietly drop it and reintroduce the
    /// "kinda resumes my last session" bug.
    #[test]
    fn daemon_agent_id_includes_tui_pid_and_counter() {
        assert_eq!(daemon_agent_id_for(48321, 2), "agent-48321-2");
        assert_eq!(daemon_agent_id_for(1, 1), "agent-1-1");
    }

    // ── resolve_remote_scratch_cwd ───────────────────────────────
    //
    // The SSH branch of the SpawnScratch arm calls this helper before
    // consuming the prepare slot's `RemoteFs`. Each branch matters:
    //
    //   - Unresolvable scratch_dir → None (caller falls back to
    //     remote $HOME on the daemon side).
    //   - No live RemoteFs → still return Some(dir) so the daemon
    //     can validate; we can't mkdir without ssh, but the dir
    //     might already exist (it's the user's habitual scratch).
    //   - mkdir success → Some(dir).
    //   - mkdir failure → None so the caller falls back rather than
    //     sending an unwritable cwd to the daemon (which would
    //     trip its `cwd.exists()` check and surface as "EOF before
    //     HelloAck").

    /// Minimal `CommandRunner` for testing `resolve_remote_scratch_cwd`
    /// without spawning real ssh. Records the args and returns a
    /// scripted `CommandOutput`. Mirrors `remote_fs::tests::ScriptedRunner`
    /// but lives here because runtime tests can't reach into the
    /// `remote_fs` module's `#[cfg(test)]` items.
    struct ScratchRunner {
        response: std::sync::Mutex<Option<std::io::Result<codemuxd_bootstrap::CommandOutput>>>,
    }

    impl ScratchRunner {
        fn ok() -> Self {
            Self {
                response: std::sync::Mutex::new(Some(Ok(codemuxd_bootstrap::CommandOutput {
                    status: 0,
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                }))),
            }
        }

        fn fail(status: i32, stderr: &[u8]) -> Self {
            Self {
                response: std::sync::Mutex::new(Some(Ok(codemuxd_bootstrap::CommandOutput {
                    status,
                    stdout: Vec::new(),
                    stderr: stderr.to_vec(),
                }))),
            }
        }
    }

    impl codemuxd_bootstrap::CommandRunner for ScratchRunner {
        fn run(
            &self,
            _program: &str,
            _args: &[&str],
        ) -> std::io::Result<codemuxd_bootstrap::CommandOutput> {
            self.response
                .lock()
                .unwrap()
                .take()
                .expect("ScratchRunner.run called twice but only one response was scripted")
        }

        fn spawn_detached(&self, _: &str, _: &[&str]) -> std::io::Result<std::process::Child> {
            unreachable!("resolve_remote_scratch_cwd does not spawn detached subprocesses")
        }
    }

    fn config_with_scratch(scratch_dir: &str) -> SpawnConfig {
        SpawnConfig {
            scratch_dir: scratch_dir.to_string(),
            ..SpawnConfig::default()
        }
    }

    #[test]
    fn resolve_remote_scratch_cwd_returns_none_when_path_unresolvable() {
        // Relative scratch_dir → expand_scratch returns None →
        // helper returns None without ever touching the runner.
        let cfg = config_with_scratch("relative-not-allowed");
        let fs = RemoteFs::for_test("host.example".into(), PathBuf::from("/tmp/sock"));
        let runner = ScratchRunner::ok();
        assert_eq!(
            resolve_remote_scratch_cwd(&cfg, Path::new("/root"), Some(&fs), &runner),
            None,
        );
    }

    #[test]
    fn resolve_remote_scratch_cwd_returns_dir_without_mkdir_when_no_remote_fs() {
        // No live ControlMaster → can't mkdir, but still surface
        // the resolved dir so the daemon validates. The runner is
        // never consulted (asserted by ScratchRunner panicking on
        // double-take if the helper accidentally called .run()).
        let cfg = config_with_scratch("~/.codemux/scratch");
        let runner = ScratchRunner::ok();
        let resolved = resolve_remote_scratch_cwd(&cfg, Path::new("/root"), None, &runner).unwrap();
        assert_eq!(resolved, PathBuf::from("/root/.codemux/scratch"));
    }

    #[test]
    fn resolve_remote_scratch_cwd_returns_dir_on_mkdir_success() {
        let cfg = config_with_scratch("~/.codemux/scratch");
        let fs = RemoteFs::for_test("host.example".into(), PathBuf::from("/tmp/sock"));
        let runner = ScratchRunner::ok();
        let resolved =
            resolve_remote_scratch_cwd(&cfg, Path::new("/root"), Some(&fs), &runner).unwrap();
        assert_eq!(resolved, PathBuf::from("/root/.codemux/scratch"));
    }

    #[test]
    fn resolve_remote_scratch_cwd_returns_none_on_mkdir_failure() {
        // Permission-denied is the realistic failure mode; the
        // helper logs and returns None so the SpawnScratch arm
        // falls back to "let the daemon pick remote $HOME" rather
        // than sending an unwritable cwd that trips the daemon's
        // cwd.exists() check.
        let cfg = config_with_scratch("~/.codemux/scratch");
        let fs = RemoteFs::for_test("host.example".into(), PathBuf::from("/tmp/sock"));
        let runner = ScratchRunner::fail(1, b"mkdir: Permission denied\n");
        assert_eq!(
            resolve_remote_scratch_cwd(&cfg, Path::new("/root"), Some(&fs), &runner),
            None,
        );
    }

    // ── SSH state-machine harness ────────────────────────────────
    //
    // These tests exercise the runtime's prepare-event drain (the
    // extracted `drain_prepare_events` free fn) without driving the
    // full event loop. Two seams make this work:
    //
    //   1. `PrepareHandle::from_events` / `AttachHandle::from_events`
    //      (in `bootstrap_worker.rs`) — synthetic handles backed by
    //      pre-loaded crossbeam channels. No worker thread.
    //   2. `attach_factory` parameter on `build_remote_attach` and
    //      `drain_prepare_events` — production passes `start_attach`;
    //      tests pass a closure returning a synthetic AttachHandle.
    //
    // Together they let us drive the prepare → success-or-failure →
    // (auto-spawn | unlock | error) state machine deterministically.

    use codemuxd_bootstrap::{Error as BootstrapError, Stage as BootstrapStage};

    fn fake_attach_factory(
        _prepared: PreparedHost,
        _host: String,
        _agent_id: String,
        _cwd: Option<PathBuf>,
        _rows: u16,
        _cols: u16,
    ) -> AttachHandle {
        AttachHandle::from_events(Vec::new())
    }

    fn prepared_host_for_test() -> PreparedHost {
        PreparedHost {
            remote_home: PathBuf::from("/home/u"),
            binary_was_updated: false,
        }
    }

    #[test]
    fn build_remote_attach_constructs_pending_attach_with_correct_fields() {
        let prepared = prepared_host_for_test();
        let attach = build_remote_attach(
            prepared,
            "devpod".to_string(),
            "/work/p",
            12345,
            7,
            24,
            80,
            true,
            fake_attach_factory,
        );
        assert_eq!(attach.label, "devpod:agent-7");
        assert_eq!(attach.agent_id, AgentId::new("agent-7"));
        assert_eq!(attach.host, "devpod");
        assert_eq!(attach.repo.as_deref(), Some("p"));
        assert_eq!(attach.rows, 24);
        assert_eq!(attach.cols, 80);
        assert!(attach.modal_owner);
    }

    #[test]
    fn build_remote_attach_empty_path_omits_repo() {
        // An empty `path` is meaningful: it tells the daemon to
        // inherit `$HOME` as cwd. The navigator falls back to the
        // static label since there's no repo basename to display.
        let prepared = prepared_host_for_test();
        let attach = build_remote_attach(
            prepared,
            "devpod".to_string(),
            "",
            12345,
            7,
            24,
            80,
            false,
            fake_attach_factory,
        );
        assert!(
            attach.repo.is_none(),
            "empty path must produce repo: None, got {:?}",
            attach.repo,
        );
        assert!(!attach.modal_owner);
    }

    fn make_pending_prepare(
        host: &str,
        events: Vec<PrepareEvent>,
        pending_project_path: Option<String>,
    ) -> PendingPrepare {
        PendingPrepare {
            host: host.to_string(),
            handle: PrepareHandle::from_events(events),
            prepared: None,
            remote_fs: None,
            pending_project_path,
        }
    }

    fn drain_test_geometry() -> (u16, u16) {
        (24, 80)
    }

    #[test]
    fn drain_prepare_events_with_pending_path_dismisses_modal_and_launches_attach() {
        // Auto-spawn-after-prepare: prepare reports Done(Ok) and the
        // slot has a stashed path. Modal is dismissed, attach is
        // queued. Slot is consumed.
        let mut prepare = Some(make_pending_prepare(
            "devpod",
            vec![PrepareEvent::Done(Ok(PrepareSuccess {
                prepared: prepared_host_for_test(),
                fs: None,
            }))],
            Some("/work/p".to_string()),
        ));
        let mut spawn_ui = Some(SpawnMinibuffer::open(
            Path::new("/tmp"),
            crate::config::SearchMode::Precise,
            Vec::new(),
        ));
        let mut attaches: Vec<PendingAttach> = Vec::new();
        let mut index_mgr = IndexManager::new();
        let cfg = SpawnConfig::default();
        let mut spawn_counter: usize = 0;

        drain_prepare_events(PrepareDrainCtx {
            prepare: &mut prepare,
            spawn_ui: &mut spawn_ui,
            attaches: &mut attaches,
            index_mgr: &mut index_mgr,
            spawn_counter: &mut spawn_counter,
            spawn_config: &cfg,
            pty_geom: drain_test_geometry(),
            tui_pid: 999,
            attach_factory: fake_attach_factory,
        });

        assert!(prepare.is_none(), "auto-spawn consumes the prepare slot");
        assert!(spawn_ui.is_none(), "auto-spawn dismisses the modal");
        assert_eq!(attaches.len(), 1, "auto-spawn queues exactly one attach");
        assert_eq!(attaches[0].host, "devpod");
        assert_eq!(attaches[0].label, "devpod:agent-1");
        assert_eq!(attaches[0].repo.as_deref(), Some("p"));
        assert!(
            !attaches[0].modal_owner,
            "auto-spawn flow must not own the modal — it dismissed it",
        );
        assert_eq!(spawn_counter, 1, "spawn_counter incremented for the attach");
    }

    #[test]
    fn drain_prepare_events_without_pending_path_unlocks_modal() {
        // Regular PrepareHost flow: prepare succeeds, modal is
        // unlocked into PathMode::Remote so the user can pick a
        // remote folder. Slot is re-stashed with `prepared` set so
        // the subsequent user-driven Spawn arm can consume it.
        let mut prepare = Some(make_pending_prepare(
            "devpod",
            vec![PrepareEvent::Done(Ok(PrepareSuccess {
                prepared: prepared_host_for_test(),
                fs: None,
            }))],
            None,
        ));
        let mut spawn_ui = Some(SpawnMinibuffer::open(
            Path::new("/tmp"),
            crate::config::SearchMode::Precise,
            Vec::new(),
        ));
        let mut attaches: Vec<PendingAttach> = Vec::new();
        let mut index_mgr = IndexManager::new();
        let cfg = SpawnConfig::default();
        let mut spawn_counter: usize = 0;

        drain_prepare_events(PrepareDrainCtx {
            prepare: &mut prepare,
            spawn_ui: &mut spawn_ui,
            attaches: &mut attaches,
            index_mgr: &mut index_mgr,
            spawn_counter: &mut spawn_counter,
            spawn_config: &cfg,
            pty_geom: drain_test_geometry(),
            tui_pid: 999,
            attach_factory: fake_attach_factory,
        });

        assert!(
            spawn_ui.is_some(),
            "PrepareHost flow keeps the modal open for path entry",
        );
        assert!(
            attaches.is_empty(),
            "no attach launched without a stashed path"
        );
        let slot = prepare
            .as_ref()
            .expect("slot is re-stashed for the subsequent user-driven Spawn");
        assert!(
            slot.prepared.is_some(),
            "prepared host is recorded on the slot",
        );
        assert_eq!(
            spawn_counter, 0,
            "spawn_counter unchanged in PrepareHost flow"
        );
    }

    #[test]
    fn drain_prepare_events_with_failure_unlocks_back_to_host_zone_even_with_pending_path() {
        // Failure path takes precedence over auto-spawn: the user
        // sees the error in the modal even when the slot had a
        // stashed `pending_project_path`. Slot is dropped.
        let err = BootstrapError::Bootstrap {
            stage: BootstrapStage::VersionProbe,
            source: Box::new(io::Error::other("simulated probe failure")),
        };
        let mut prepare = Some(make_pending_prepare(
            "devpod",
            vec![PrepareEvent::Done(Err(err))],
            Some("/work/p".to_string()),
        ));
        let mut spawn_ui = Some(SpawnMinibuffer::open(
            Path::new("/tmp"),
            crate::config::SearchMode::Precise,
            Vec::new(),
        ));
        let mut attaches: Vec<PendingAttach> = Vec::new();
        let mut index_mgr = IndexManager::new();
        let cfg = SpawnConfig::default();
        let mut spawn_counter: usize = 0;

        drain_prepare_events(PrepareDrainCtx {
            prepare: &mut prepare,
            spawn_ui: &mut spawn_ui,
            attaches: &mut attaches,
            index_mgr: &mut index_mgr,
            spawn_counter: &mut spawn_counter,
            spawn_config: &cfg,
            pty_geom: drain_test_geometry(),
            tui_pid: 999,
            attach_factory: fake_attach_factory,
        });

        assert!(prepare.is_none(), "failure drops the slot");
        assert!(
            spawn_ui.is_some(),
            "modal stays open so the user sees the error",
        );
        assert!(
            attaches.is_empty(),
            "no attach launched on failure, even with pending path",
        );
        assert_eq!(spawn_counter, 0);
    }

    #[test]
    fn drain_prepare_events_with_no_done_event_re_stashes_slot() {
        // In-flight: handle has a Stage event but no Done. Slot is
        // re-stashed; modal sees the stage tick.
        let mut prepare = Some(make_pending_prepare(
            "devpod",
            vec![PrepareEvent::Stage(BootstrapStage::VersionProbe)],
            Some("/work/p".to_string()),
        ));
        let mut spawn_ui = Some(SpawnMinibuffer::open(
            Path::new("/tmp"),
            crate::config::SearchMode::Precise,
            Vec::new(),
        ));
        let mut attaches: Vec<PendingAttach> = Vec::new();
        let mut index_mgr = IndexManager::new();
        let cfg = SpawnConfig::default();
        let mut spawn_counter: usize = 0;

        drain_prepare_events(PrepareDrainCtx {
            prepare: &mut prepare,
            spawn_ui: &mut spawn_ui,
            attaches: &mut attaches,
            index_mgr: &mut index_mgr,
            spawn_counter: &mut spawn_counter,
            spawn_config: &cfg,
            pty_geom: drain_test_geometry(),
            tui_pid: 999,
            attach_factory: fake_attach_factory,
        });

        assert!(prepare.is_some(), "in-flight slot is re-stashed");
        assert!(spawn_ui.is_some(), "modal stays open while in-flight");
        assert!(
            attaches.is_empty(),
            "no attach yet — prepare hasn't completed"
        );
        let slot = prepare.as_ref().expect("slot re-stashed");
        assert_eq!(
            slot.pending_project_path.as_deref(),
            Some("/work/p"),
            "pending path preserved across in-flight drains",
        );
    }

    #[test]
    fn drain_prepare_events_no_slot_is_a_noop() {
        // The drain must tolerate being called with no pending
        // prepare — the event_loop calls it every frame regardless.
        let mut prepare: Option<PendingPrepare> = None;
        let mut spawn_ui: Option<SpawnMinibuffer> = None;
        let mut attaches: Vec<PendingAttach> = Vec::new();
        let mut index_mgr = IndexManager::new();
        let cfg = SpawnConfig::default();
        let mut spawn_counter: usize = 0;

        drain_prepare_events(PrepareDrainCtx {
            prepare: &mut prepare,
            spawn_ui: &mut spawn_ui,
            attaches: &mut attaches,
            index_mgr: &mut index_mgr,
            spawn_counter: &mut spawn_counter,
            spawn_config: &cfg,
            pty_geom: drain_test_geometry(),
            tui_pid: 999,
            attach_factory: fake_attach_factory,
        });

        assert!(prepare.is_none());
        assert!(spawn_ui.is_none());
        assert!(attaches.is_empty());
        assert_eq!(spawn_counter, 0);
    }

    #[test]
    fn tick_fuzzy_dispatch_records_pushed_index_and_query_for_active_modal() {
        // The runtime memoizes both the per-host index generation
        // and the modal's query string after dispatching to the
        // fuzzy worker. This is the line that turns per-frame
        // score_fuzzy work into "once per index transition" — the
        // table entries are the observable proof the dispatch fired.
        let fuzzy_worker = FuzzyWorker::start();
        let mut spawn_ui = Some(SpawnMinibuffer::open(
            Path::new("/tmp"),
            crate::config::SearchMode::Fuzzy,
            Vec::new(),
        ));
        if let Some(ui) = spawn_ui.as_mut() {
            ui.set_fuzzy_query_for_test("code");
        }
        let mut index_mgr = IndexManager::new();
        index_mgr.hydrate_for_test(
            HOST_PLACEHOLDER.into(),
            crate::index_worker::IndexSaveCtx::Local { roots: Vec::new() },
            vec![crate::index_worker::IndexedDir {
                path: PathBuf::from("/code"),
                kind: crate::index_worker::ProjectKind::Plain,
            }],
        );
        let mut last_pushed_index_gen: HashMap<String, u64> = HashMap::new();
        let mut last_pushed_query: HashMap<String, String> = HashMap::new();

        tick_fuzzy_dispatch(
            spawn_ui.as_mut(),
            &fuzzy_worker,
            &index_mgr,
            &mut last_pushed_index_gen,
            &mut last_pushed_query,
        );

        assert_eq!(
            last_pushed_index_gen.get(HOST_PLACEHOLDER).copied(),
            index_mgr.state_generation_for(HOST_PLACEHOLDER),
            "SetIndex memoization must record the dispatched generation",
        );
        assert_eq!(
            last_pushed_query.get(HOST_PLACEHOLDER).map(String::as_str),
            Some("code"),
            "Query memoization must record the dispatched query",
        );

        // Second call with no state change must not re-dispatch (the
        // memoization table values stay identical).
        let gen_before = last_pushed_index_gen.clone();
        let query_before = last_pushed_query.clone();
        tick_fuzzy_dispatch(
            spawn_ui.as_mut(),
            &fuzzy_worker,
            &index_mgr,
            &mut last_pushed_index_gen,
            &mut last_pushed_query,
        );
        assert_eq!(last_pushed_index_gen, gen_before);
        assert_eq!(last_pushed_query, query_before);
    }

    #[test]
    fn tick_fuzzy_dispatch_skips_when_modal_closed() {
        // Modal closed: no dispatch, no memoization. Drained results
        // (none here) are dropped silently.
        let fuzzy_worker = FuzzyWorker::start();
        let index_mgr = IndexManager::new();
        let mut last_pushed_index_gen: HashMap<String, u64> = HashMap::new();
        let mut last_pushed_query: HashMap<String, String> = HashMap::new();

        tick_fuzzy_dispatch(
            None,
            &fuzzy_worker,
            &index_mgr,
            &mut last_pushed_index_gen,
            &mut last_pushed_query,
        );

        assert!(last_pushed_index_gen.is_empty());
        assert!(last_pushed_query.is_empty());
    }

    #[test]
    fn tick_fuzzy_dispatch_skips_when_query_empty() {
        // Empty query short-circuits inside the modal's
        // `fuzzy_dispatch_request` — the runtime must not push a
        // SetIndex or Query for it.
        let fuzzy_worker = FuzzyWorker::start();
        let mut spawn_ui = Some(SpawnMinibuffer::open(
            Path::new("/tmp"),
            crate::config::SearchMode::Fuzzy,
            Vec::new(),
        ));
        let mut index_mgr = IndexManager::new();
        index_mgr.hydrate_for_test(
            HOST_PLACEHOLDER.into(),
            crate::index_worker::IndexSaveCtx::Local { roots: Vec::new() },
            Vec::new(),
        );
        let mut last_pushed_index_gen: HashMap<String, u64> = HashMap::new();
        let mut last_pushed_query: HashMap<String, String> = HashMap::new();

        tick_fuzzy_dispatch(
            spawn_ui.as_mut(),
            &fuzzy_worker,
            &index_mgr,
            &mut last_pushed_index_gen,
            &mut last_pushed_query,
        );

        assert!(last_pushed_index_gen.is_empty());
        assert!(last_pushed_query.is_empty());
    }

    #[test]
    fn drain_prepare_events_with_remote_fs_kicks_off_swr_index() {
        // PrepareSuccess with `fs: Some(_)` exercises the SWR-on-success
        // branch: the remote fuzzy index for this host gets a hydrate
        // request even if the user is going to dismiss the modal
        // without browsing. Pre-warms the next session.
        let fs = RemoteFs::for_test("devpod".into(), PathBuf::from("/tmp/sock"));
        let mut prepare = Some(make_pending_prepare(
            "devpod",
            vec![PrepareEvent::Done(Ok(PrepareSuccess {
                prepared: prepared_host_for_test(),
                fs: Some(fs),
            }))],
            None,
        ));
        let mut spawn_ui = Some(SpawnMinibuffer::open(
            Path::new("/tmp"),
            crate::config::SearchMode::Precise,
            Vec::new(),
        ));
        let mut attaches: Vec<PendingAttach> = Vec::new();
        let mut index_mgr = IndexManager::new();
        let cfg = SpawnConfig::default();
        let mut spawn_counter: usize = 0;

        drain_prepare_events(PrepareDrainCtx {
            prepare: &mut prepare,
            spawn_ui: &mut spawn_ui,
            attaches: &mut attaches,
            index_mgr: &mut index_mgr,
            spawn_counter: &mut spawn_counter,
            spawn_config: &cfg,
            pty_geom: drain_test_geometry(),
            tui_pid: 999,
            attach_factory: fake_attach_factory,
        });

        // Slot is re-stashed, modal unlocked into the remote-path zone
        // with a live RemoteFs lister (not the literal-path fallback).
        let slot = prepare
            .as_ref()
            .expect("slot re-stashed for user-driven Spawn");
        assert!(
            slot.remote_fs.is_some(),
            "RemoteFs handed off to the slot for the modal's per-keystroke listing",
        );
        assert!(slot.prepared.is_some());
        assert!(spawn_ui.is_some());
    }

    // ---- apply_meta_events ----

    #[test]
    fn apply_meta_events_routes_branch_to_matching_agent() {
        // Branch event addressed to agent "a" lands on "a", not "b".
        let mut a = ready_test_agent(100);
        a.id = AgentId::new("a");
        let mut b = ready_test_agent(100);
        b.id = AgentId::new("b");
        let mut agents = vec![a, b];
        apply_meta_events(
            &mut agents,
            vec![MetaEvent::Branch {
                agent_id: AgentId::new("a"),
                value: Some("feature/x".into()),
            }],
        );
        assert_eq!(agents[0].branch.as_deref(), Some("feature/x"));
        assert!(agents[1].branch.is_none());
    }

    #[test]
    fn apply_meta_events_writes_model_and_effort_together() {
        // Some(ModelEffort) populates both fields on the agent.
        // Pairing them in one event is the contract that keeps the
        // segment from rendering a stale model with a fresh effort.
        let mut agent = ready_test_agent(100);
        agent.id = AgentId::new("a");
        let mut agents = vec![agent];
        apply_meta_events(
            &mut agents,
            vec![MetaEvent::Model {
                agent_id: AgentId::new("a"),
                value: Some(crate::agent_meta_worker::ModelEffort {
                    model: "opus[1m]".into(),
                    effort: Some("xhigh".into()),
                }),
            }],
        );
        let me = agents[0].model_effort.as_ref().unwrap();
        assert_eq!(me.model, "opus[1m]");
        assert_eq!(me.effort.as_deref(), Some("xhigh"));
    }

    #[test]
    fn apply_meta_events_clears_model_and_effort_on_none() {
        // None means settings.json couldn't be read this poll. Both
        // fields must clear together so the segment hides cleanly
        // rather than displaying a stale pair.
        let mut agent = ready_test_agent(100);
        agent.id = AgentId::new("a");
        agent.model_effort = Some(crate::agent_meta_worker::ModelEffort {
            model: "opus[1m]".into(),
            effort: Some("xhigh".into()),
        });
        let mut agents = vec![agent];
        apply_meta_events(
            &mut agents,
            vec![MetaEvent::Model {
                agent_id: AgentId::new("a"),
                value: None,
            }],
        );
        assert!(agents[0].model_effort.is_none());
    }

    #[test]
    fn apply_meta_events_drops_events_for_unknown_agents() {
        // A focus change or reorder mid-poll can leave events
        // addressed to agents that no longer exist in the slice.
        // The function must drop them silently rather than panic.
        let mut agent = ready_test_agent(100);
        agent.id = AgentId::new("a");
        let mut agents = vec![agent];
        apply_meta_events(
            &mut agents,
            vec![MetaEvent::Branch {
                agent_id: AgentId::new("ghost"),
                value: Some("ghost-branch".into()),
            }],
        );
        assert!(agents[0].branch.is_none());
    }
}
