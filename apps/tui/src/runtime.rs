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

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::ValueEnum;
use codemux_session::AgentTransport;
use codemux_shared_kernel::AgentId;
use codemuxd_bootstrap::{PreparedHost, RealRunner, RemoteFs};
use color_eyre::Result;
use color_eyre::eyre::WrapErr;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseButton,
    MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
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
use vt100::Parser;

use crate::bootstrap_worker::{
    AttachEvent, AttachHandle, PrepareEvent, PrepareHandle, PrepareSuccess, start_attach,
    start_prepare,
};
use crate::config::{Config, SearchMode, SpawnConfig};
use crate::host_title;
use crate::index_manager::IndexManager;
use crate::index_worker::IndexState;
use crate::keymap::{Bindings, DirectAction, ModalAction, PopupAction, PrefixAction, ScrollAction};
use crate::log_tail::LogTail;
use crate::pty_title::TitleCapture;
use crate::repo_name;
use crate::spawn::{DirLister, HOST_PLACEHOLDER, ModalOutcome, SpawnMinibuffer};

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
// would just be ceremony — they're four orthogonal flags.
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
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Best-effort cleanup. Failures here are unrecoverable (we are mid-drop
        // and may be on a panic path); the user's terminal may already be in
        // a degraded state, and surfacing an error would clobber whatever the
        // panic backtrace was about to say.
        //
        // Drop order is the reverse of acquisition: bracketed paste, mouse
        // capture, then keyboard enhancement, then leave-alt-screen +
        // raw-mode. Each is an independent escape sequence (`?2004l`,
        // `?1006l`, `<u`); mirroring acquisition order is the safe
        // discipline — and skipping the matching disable when the matching
        // enable failed avoids generating spurious sequences the terminal
        // never opted into.
        if self.bracketed_paste {
            let _ = execute!(io::stdout(), DisableBracketedPaste);
        }
        if self.mouse_captured {
            let _ = execute!(io::stdout(), DisableMouseCapture);
        }
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
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum PrefixState {
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

    /// Detect working→idle transitions on unfocused agents and flag
    /// them for the slow-blink attention cue. Called once per tick
    /// after the PTY drain so the title parser sees the freshest
    /// state. The currently-focused agent is skipped — the user is
    /// already looking; nothing to alert about — and any agent
    /// caught in this window has its blink cleared on the next focus
    /// change via [`Self::change_focus`].
    fn flag_finished_unfocused(&mut self) {
        for (i, agent) in self.agents.iter_mut().enumerate() {
            let cur = agent.is_working();
            if agent.last_working && !cur && i != self.focused {
                agent.needs_attention = true;
            }
            agent.last_working = cur;
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
    /// Hostname for SSH-backed agents (`Some` for both Ready and
    /// Failed SSH agents, `None` for local). The single source of
    /// truth — the renderer derives the dim/gray prefix from this,
    /// and the failure pane reads it for the "✗ bootstrap of {host}
    /// failed" line.
    host: Option<String>,
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

impl RuntimeAgent {
    // 8 args after AD-27 added the stable id; the identity, label,
    // working dir, host, transport, geometry, and scrollback budget
    // are all distinct concerns that the single call site sets at
    // once. A builder would add code without making any of these
    // optional or composable.
    #[allow(clippy::too_many_arguments)]
    fn ready(
        id: AgentId,
        label: String,
        repo: Option<String>,
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
            host,
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

    fn failed(
        id: AgentId,
        label: String,
        repo: Option<String>,
        host: String,
        error: codemuxd_bootstrap::Error,
        rows: u16,
        cols: u16,
    ) -> Self {
        Self {
            id,
            label,
            repo,
            host: Some(host),
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
        match &self.state {
            AgentState::Ready { parser, .. } | AgentState::Crashed { parser, .. } => {
                parser.screen().scrollback()
            }
            AgentState::Failed { .. } => 0,
        }
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

    // Auto-detect: enable the Kitty Keyboard Protocol only when the user has
    // bound something to a SUPER (Cmd / Win) chord. Without this, terminals
    // that support the protocol (Ghostty, Kitty, WezTerm, recent Alacritty,
    // Foot) cannot deliver Cmd events to the application. Terminals that do
    // not understand the negotiation simply ignore it; the help screen
    // remains the escape hatch ("if my chord does not register, the
    // terminal is the limit, not codemux").
    let enhanced_keyboard = config.bindings.uses_super_modifier()
        && execute!(
            io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
        )
        .is_ok();
    if enhanced_keyboard {
        tracing::debug!("Kitty Keyboard Protocol enabled (binding uses SUPER)");
    }

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
    };

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).wrap_err("construct ratatui terminal")?;

    let chrome = ChromeStyle::from_ui(&config.ui);
    event_loop(
        &mut terminal,
        agents,
        nav_style,
        &config.bindings,
        log_tail,
        initial_cwd,
        config.scrollback_len,
        &chrome,
        &config.spawn,
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
        None,
        transport,
        rows,
        cols,
        scrollback_len,
    ))
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

/// Pull text from the focused agent's vt100 parser for the given
/// selection range and write it to the system clipboard via OSC 52.
///
/// Lookup is by stable [`AgentId`], not by index, so a tab switch /
/// agent reap between Down and Up cancels gracefully — `position`
/// returns `None` and we silently bail without writing.
///
/// `vt100::Screen::contents_between` walks `visible_rows()`, so the
/// selection respects the parser's current scrollback offset (selection
/// while scrolled-back yields scrollback text, not live text). Empty
/// selections (zero-width or all-whitespace cells in trimmed regions)
/// produce empty strings; we skip the OSC 52 write in that case to
/// avoid clobbering whatever was on the clipboard before.
fn commit_selection(sel: &Selection, agents: &[RuntimeAgent]) {
    let Some(agent) = agents.iter().find(|a| a.id == sel.agent) else {
        return;
    };
    let parser = match &agent.state {
        AgentState::Ready { parser, .. } | AgentState::Crashed { parser, .. } => parser,
        AgentState::Failed { .. } => return,
    };
    let (start, end) = normalized_range(sel.anchor, sel.head);
    let text =
        parser
            .screen()
            .contents_between(start.row, start.col, end.row, end.col.saturating_add(1));
    if text.is_empty() {
        return;
    }
    if let Err(err) = write_clipboard_to(&mut io::stdout(), &text) {
        tracing::debug!(?err, "OSC 52 clipboard write failed");
    }
}

/// Emit an OSC 52 sequence carrying the selection payload. The host
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

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    agents: Vec<RuntimeAgent>,
    mut nav_style: NavStyle,
    bindings: &Bindings,
    log_tail: Option<&LogTail>,
    initial_cwd: &Path,
    scrollback_len: usize,
    chrome: &ChromeStyle,
    spawn_config: &SpawnConfig,
) -> Result<()> {
    // Long, but it is the central event loop and breaks naturally into
    // sequential phases (drain / reap / render / dispatch). Pulling each
    // arm into its own helper would require threading >5 mutable references
    // through the helper and gain little.
    #![allow(clippy::too_many_lines, clippy::too_many_arguments)]
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
    if spawn_config.default_mode == SearchMode::Fuzzy {
        let outcome =
            index_mgr.request_local_swr(&spawn_config.search_roots, &spawn_config.project_markers);
        tracing::debug!(?outcome, "fuzzy index: build started at session start");
    }
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

    loop {
        // Drain in-flight index events for every host (local + each
        // SSH host the user has spawned to), update states, and
        // dispatch any completed walks to a detached disk-save
        // thread. The manager owns this whole pipeline; the runtime
        // just yields control once per frame.
        index_mgr.tick();
        // Refresh the modal's wildmenu from the index for whichever
        // host the modal is currently targeting. The modal
        // short-circuits when not in Fuzzy + Path, so this is cheap
        // on every other frame.
        if let Some(ui) = spawn_ui.as_mut() {
            let key = ui.active_host_key().to_string();
            ui.notify_index_state(index_mgr.state_for(&key));
        }

        // Drain prepare events first: the modal should reflect the
        // worker's progress on the same frame the events arrive,
        // before any keystroke handling. On `Done` we either unlock
        // the modal for a remote-folder pick (success) or unlock back
        // to the host zone with the error visible (failure).
        if let Some(p) = prepare.as_mut() {
            let mut completion = None;
            while let Some(event) = p.handle.try_recv() {
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
            match completion {
                Some(Ok(PrepareSuccess { prepared, fs })) => {
                    // The worker opened the ssh `ControlMaster` for us
                    // (see `start_prepare_with_runner`) so the main
                    // thread doesn't block on a synchronous
                    // `RemoteFs::open` poll while the spinner is
                    // locked. `fs == None` means the open failed; the
                    // modal degrades to literal-path mode (logged in
                    // the worker).
                    if let Some(ui) = spawn_ui.as_mut() {
                        // Once unlocked, the modal sits in
                        // PathMode::Remote and immediately refreshes
                        // the wildmenu against the remote `$HOME` —
                        // pass the live ControlMaster (or fall back to
                        // Local if RemoteFs::open failed in the
                        // worker) so the first listing is real, not
                        // empty.
                        let runner = RealRunner;
                        let mut lister = match fs.as_ref() {
                            Some(rfs) => DirLister::Remote {
                                fs: rfs,
                                runner: &runner,
                            },
                            None => DirLister::Local,
                        };
                        ui.unlock_for_remote_path(
                            p.host.clone(),
                            prepared.remote_home.clone(),
                            &mut lister,
                        );
                    }
                    // SWR for the SSH host: hydrate from the remote
                    // disk cache if present, then start a fresh walk
                    // in the background. Skip cleanly when `fs` is
                    // `None` (the modal will be in literal-path mode
                    // anyway).
                    if let Some(rfs) = fs.as_ref() {
                        let host_roots = spawn_config.ssh_search_roots(&p.host);
                        let outcome = index_mgr.request_remote_swr(
                            &p.host,
                            rfs.socket_path(),
                            &prepared.remote_home,
                            &host_roots,
                            &spawn_config.project_markers,
                        );
                        tracing::debug!(?outcome, host = %p.host, "remote fuzzy index: SWR start");
                    }
                    p.remote_fs = fs;
                    p.prepared = Some(prepared);
                }
                Some(Err(e)) => {
                    tracing::error!(host = %p.host, "prepare failed: {e}");
                    if let Some(ui) = spawn_ui.as_mut() {
                        // Pass the structured error; the modal formats
                        // via `user_message()` at render time.
                        ui.unlock_back_to_host(&mut DirLister::Local, Some(e));
                    }
                    prepare = None;
                }
                None => {}
            }
        }

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

        nav.flag_finished_unfocused();

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
                    selection.as_ref(),
                    modal_index_state,
                );
            })
            .wrap_err("draw frame")?;

        if !event::poll(FRAME_POLL).wrap_err("poll for input")? {
            continue;
        }

        match event::read().wrap_err("read input")? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
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
                                let label = format!("{host}:agent-{spawn_counter}");
                                let runtime_id = AgentId::new(format!("agent-{spawn_counter}"));
                                // Daemon-facing id is namespaced by the
                                // TUI's pid so a relaunch never
                                // collides with a still-live remote
                                // daemon from a previous codemux
                                // invocation. Without the prefix the
                                // bootstrap silently re-attaches to the
                                // old socket (the new daemon's bind
                                // fails on the held pid file, but the
                                // surviving socket is what the poll
                                // loop sees). The user-visible label
                                // and in-process AgentId stay short
                                // intentionally — the prefix is for the
                                // remote filesystem, not for humans.
                                let daemon_agent_id = daemon_agent_id_for(tui_pid, spawn_counter);
                                // Empty path → None: omit `--cwd` on
                                // the remote daemon and let it
                                // inherit the remote shell's login
                                // cwd ($HOME). A local path here
                                // would otherwise be sent verbatim
                                // to the remote, fail
                                // `cwd.exists()`, and exit the
                                // daemon before it ever bound the
                                // socket — the user-visible "EOF
                                // before HelloAck" failure mode.
                                let cwd_path = if path.is_empty() {
                                    None
                                } else {
                                    Some(PathBuf::from(&path))
                                };
                                // Repo name shown in the navigator
                                // for this agent. We can't probe
                                // the remote filesystem from here
                                // without a second ssh round-trip
                                // (the prepare's `RemoteFs` was
                                // already dropped), so we settle
                                // for the basename of whatever the
                                // user typed. `None` for empty
                                // paths — the renderer falls back
                                // to the static label.
                                let repo = if path.is_empty() {
                                    None
                                } else {
                                    repo_name::resolve_remote(&path)
                                };
                                let handle = start_attach(
                                    prepared,
                                    host.clone(),
                                    daemon_agent_id,
                                    cwd_path,
                                    rows,
                                    cols,
                                );
                                tracing::info!(%host, label = %label, "started SSH attach worker");
                                // Re-lock the modal so the spinner
                                // continues through the ~1-2 s
                                // attach phase.
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
                    // Refresh the fuzzy wildmenu against the current
                    // index state so a keystroke updates `filtered` on
                    // the same frame instead of waiting for the next
                    // loop iteration's drain. Cheap when not in fuzzy
                    // mode (the modal short-circuits inside).
                    if let Some(ui) = spawn_ui.as_mut() {
                        let key = ui.active_host_key().to_string();
                        ui.notify_index_state(index_mgr.state_for(&key));
                    }
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
                        if spawn_config.default_mode == SearchMode::Fuzzy {
                            let outcome = index_mgr.request_local_swr(
                                &spawn_config.search_roots,
                                &spawn_config.project_markers,
                            );
                            tracing::debug!(?outcome, "fuzzy index: SWR refresh on modal open");
                        }
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
            }
            Event::Mouse(MouseEvent {
                kind, column, row, ..
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
                                    commit_selection(&sel, &nav.agents);
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
    selection: Option<&Selection>,
    index_state: Option<&IndexState>,
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
                selection,
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
                selection,
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
    selection: Option<&Selection>,
) {
    match &agent.state {
        AgentState::Ready { parser, .. } => {
            let widget = PseudoTerminal::new(parser.screen());
            frame.render_widget(widget, area);
            pane_hitbox.record(area, agent.id.clone());
            paint_selection_if_active(frame, area, &agent.id, selection);
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
            paint_selection_if_active(frame, area, &agent.id, selection);
            let offset = parser.screen().scrollback();
            if offset > 0 {
                render_scroll_indicator(frame, area, offset);
            }
            render_crash_banner(frame, area, *exit_code, dismiss_label);
        }
        AgentState::Failed { error } => {
            // No pane hitbox for Failed agents — there is no live PTY
            // surface to select text from, just an error message.
            let host = agent.host.as_deref().unwrap_or("");
            render_failure_pane(frame, area, host, &error.user_message());
        }
    }
}

/// If `selection` belongs to the agent currently being painted, flip
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

/// One-row banner pinned to the top of a Crashed agent's pane. Same
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

/// Centered failure pane shown when an SSH bootstrap returned an
/// error after the spawn modal closed. Renders without a border so
/// the pane reads as "this slot is dead" rather than "here is a real
/// UI element"; the in-flight phase has its own UX inside the spawn
/// modal and never reaches this renderer.
fn render_failure_pane(frame: &mut Frame<'_>, area: Rect, host: &str, err: &str) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::styled(
        format!("✗ bootstrap of {host} failed"),
        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
    ));
    lines.push(Line::raw(""));
    for line in err.lines() {
        lines.push(Line::styled(
            line.to_string(),
            Style::default().fg(Color::Red),
        ));
    }

    // Vertical centering: top filler, content, bottom filler. The
    // content gets exactly its line count so it sits on a single
    // row in the visual middle.
    let content_height = u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(content_height),
            Constraint::Min(0),
        ])
        .split(area);
    // Wipe the entire pane area first. The vertical centering layout
    // only paints `chunks[1]`; the top/bottom `Min(0)` regions are
    // never written. Without an explicit clear, whatever the previous
    // frame's renderer left in those cells (e.g. PTY content from a
    // local agent the user just switched away from) bleeds through
    // and reads as garbled characters around the centered text.
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).alignment(Alignment::Center),
        chunks[1],
    );
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
    selection: Option<&Selection>,
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
        render_agent_pane(
            frame,
            pty_area,
            agent,
            dismiss_label,
            pane_hitbox,
            selection,
        );
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
    selection: Option<&Selection>,
) {
    let [pty_area, status_area] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(STATUS_BAR_HEIGHT)])
        .areas(area);

    if let Some(agent) = agents.get(focused) {
        render_agent_pane(
            frame,
            pty_area,
            agent,
            dismiss_label,
            pane_hitbox,
            selection,
        );
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
    );

    if let PopupState::Open { selection } = popup {
        render_switcher_popup(frame, area, agents, selection, phase, chrome);
    }
}

/// Render the bottom status bar in Popup mode: tab strip on the left
/// and the prefix hint right-aligned. Splitting into discrete areas
/// means each section can be styled and clipped independently; the
/// previous flat-string approach forced uniform style and made it
/// awkward to highlight the focused tab without rendering a custom
/// widget.
///
/// The hint at the right swaps based on `prefix_state`: idle shows
/// the help reminder; `AwaitingCommand` shows a `[NAV]` badge plus a
/// short reminder of the sticky moves available, so the user has a
/// visible cue that they're "in nav mode" and what they can do.
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
) {
    // Compute the hint as both rendered Line (with styling) and a
    // plain text-width measurement (for the layout split). The two
    // need to stay in sync — the alternative was to render twice.
    let (hint_line, hint_width) = build_hint(bindings, prefix_state, chrome);

    // Reserve space for the hint on the right when there's room.
    // Below a small threshold (just enough for one tab plus the hint),
    // drop the hint entirely so the user can still see at least one
    // tab label. Truncation is handled implicitly by ratatui's
    // Paragraph clipping at the area edge.
    let (left_area, hint_area) = if area.width > hint_width.saturating_add(8) {
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(1), Constraint::Length(hint_width)])
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
        // hint's reserved space (or the screen edge) records its
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

    if let Some(area) = hint_area {
        let widget = Paragraph::new(hint_line).alignment(Alignment::Right);
        frame.render_widget(widget, area);
    }
}

/// Build the right-aligned hint shown in the Popup-mode status bar.
/// The text changes based on `prefix_state` so the user has a
/// visible cue when sticky nav mode is active. Returns the styled
/// `Line` plus the plain-text width — the caller needs both because
/// ratatui's layout splits need a numeric width while rendering
/// uses the styled `Line`.
fn build_hint(
    bindings: &Bindings,
    prefix_state: PrefixState,
    chrome: &ChromeStyle,
) -> (Line<'static>, u16) {
    match prefix_state {
        PrefixState::Idle => {
            let text = format!("{} {} for help", bindings.prefix, bindings.on_prefix.help);
            let width = u16::try_from(text.chars().count()).unwrap_or(u16::MAX);
            let line = Line::styled(text, chrome.secondary);
            (line, width)
        }
        PrefixState::AwaitingCommand => {
            // [NAV] in yellow + bold to draw the eye; the rest dim
            // to read as ambient guidance, matching the idle hint
            // style. Width is the full plain-text length so the
            // layout reserves enough room for both spans.
            let badge = "[NAV] ";
            let body = "h/l prev/next  esc exit";
            let width =
                u16::try_from(badge.chars().count() + body.chars().count()).unwrap_or(u16::MAX);
            let line = Line::from(vec![
                Span::styled(
                    badge,
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(body, chrome.secondary),
            ]);
            (line, width)
        }
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

    // ── flag_finished_unfocused ──────────────────────────────────
    //
    // The working→idle detector that runs once per tick. Pulled out
    // of the event loop so the transition matrix can be tested with
    // Failed agents (whose `is_working()` is always false; we drive
    // the input by setting `last_working` directly).

    #[test]
    fn flag_finished_unfocused_marks_unfocused_agent_that_just_finished() {
        let mut agents = vec![failed_agent("a"), failed_agent("b")];
        agents[1].last_working = true;
        let mut nav = NavState::new(agents);
        nav.flag_finished_unfocused();
        assert!(nav.agents[1].needs_attention);
        assert!(!nav.agents[1].last_working, "last_working must be reset",);
    }

    #[test]
    fn flag_finished_unfocused_skips_focused_agent() {
        // Focused → user is already looking → no slow-blink. The
        // detector still resets last_working so the transition is
        // consumed (not re-flagged on the next tick).
        let mut agents = vec![failed_agent("a"), failed_agent("b")];
        agents[0].last_working = true;
        let mut nav = NavState::new(agents);
        nav.flag_finished_unfocused();
        assert!(!nav.agents[0].needs_attention);
        assert!(!nav.agents[0].last_working);
    }

    #[test]
    fn flag_finished_unfocused_no_op_when_state_unchanged() {
        let agents = vec![failed_agent("a"), failed_agent("b")];
        let mut nav = NavState::new(agents);
        // Both already idle; nothing transitioned.
        nav.flag_finished_unfocused();
        assert!(!nav.agents[0].needs_attention);
        assert!(!nav.agents[1].needs_attention);
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
            "uber".to_string(),
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
        assert_eq!(chrome.host_style("uber").fg, Some(Color::Blue));
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
            "uber".to_string(),
            crate::config::ChromeColor::Named(Color::Blue),
        );
        let chrome = ChromeStyle::from_ui(&crate::config::Ui {
            host_colors,
            ..Default::default()
        });
        let spans = label_spans(
            Some("uber"),
            "claude",
            false,
            false,
            false,
            AnimationPhase::default(),
            &chrome,
        );
        let host_span = spans
            .iter()
            .find(|s| s.content.contains("uber"))
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

    // build_hint state-driven branch — the user's visible cue that
    // sticky nav mode is active. The two branches must produce
    // distinguishable output (different widths) so the layout reserves
    // appropriate room.

    #[test]
    fn build_hint_idle_and_awaiting_command_produce_different_widths() {
        let bindings = defaults();
        let (_, idle_width) = build_hint(&bindings, PrefixState::Idle, &ChromeStyle::default());
        let (_, nav_width) = build_hint(
            &bindings,
            PrefixState::AwaitingCommand,
            &ChromeStyle::default(),
        );
        // The NAV-mode hint includes a `[NAV]` badge plus a sticky-mode
        // reminder, so it's strictly wider than the idle help reminder.
        // If a future edit makes them equal, the cue gets lost — fail
        // loudly here so the renderer's affordance stays visible.
        assert!(nav_width > 0);
        assert!(idle_width > 0);
        assert_ne!(idle_width, nav_width);
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
                    None,
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
                    None,
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
                    None,
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
                    None,
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
                    None,
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
                    None,
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
                    None,
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
}
