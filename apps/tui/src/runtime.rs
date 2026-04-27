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
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use tui_term::widget::PseudoTerminal;
use vt100::Parser;

use crate::bootstrap_worker::{
    AttachEvent, AttachHandle, PrepareEvent, PrepareHandle, PrepareSuccess, start_attach,
    start_prepare,
};
use crate::config::Config;
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

struct TerminalGuard {
    enhanced_keyboard: bool,
    mouse_captured: bool,
    bracketed_paste: bool,
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

/// Per-agent state. `Ready` is the steady state for both local and
/// SSH transports once they have an [`AgentTransport`] and a
/// renderable [`Parser`]. `Failed` captures an SSH bootstrap that
/// completed with an error after the spawn modal closed; the dead
/// worker handle has already been dropped so the variant only carries
/// the data the renderer needs.
///
/// In-flight SSH bootstraps live in the spawn modal (see
/// [`crate::spawn::SpawnMinibuffer::lock_for_bootstrap`]) rather than
/// in this enum: the user picks a remote folder *between* prepare and
/// attach, so the in-flight phase has UX that doesn't fit a per-agent
/// pane. A `Failed` agent stays `Failed` until the user exits the TUI
/// (no per-agent dismiss key yet — future P2 lifecycle work).
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
    /// mode" without matching on the state.
    fn scrollback_offset(&self) -> usize {
        match &self.state {
            AgentState::Ready { parser, .. } => parser.screen().scrollback(),
            AgentState::Failed { .. } => 0,
        }
    }

    /// Adjust the scrollback offset by `delta` (positive scrolls back
    /// into history, negative toward the live view). Saturates at zero
    /// on the bottom; `vt100::Screen::set_scrollback` clamps the top to
    /// the buffer length, so we don't need to know the cap. No-op for
    /// `Failed` agents — they have no parser.
    fn nudge_scrollback(&mut self, delta: i32) {
        if let AgentState::Ready { parser, .. } = &mut self.state {
            let screen = parser.screen_mut();
            let next = screen.scrollback().saturating_add_signed(delta as isize);
            screen.set_scrollback(next);
        }
    }

    /// Snap back to the live view (offset = 0). Used by the
    /// `ScrollAction::ExitScroll` path and by the non-sticky "any
    /// forwarded keystroke snaps" rule.
    fn snap_to_live(&mut self) {
        if let AgentState::Ready { parser, .. } = &mut self.state {
            parser.screen_mut().set_scrollback(0);
        }
    }

    /// Jump to the top of the buffer. `vt100::Screen::set_scrollback`
    /// clamps to the buffer length, so passing `usize::MAX` reaches the
    /// top regardless of the configured `scrollback_len` — no need to
    /// thread the cap through.
    fn jump_to_top(&mut self) {
        if let AgentState::Ready { parser, .. } = &mut self.state {
            parser.screen_mut().set_scrollback(usize::MAX);
        }
    }

    /// Live OSC title from the agent's parser, if any. `None` for
    /// `Failed` agents and for `Ready` agents whose foreground
    /// process hasn't emitted a title yet.
    fn title(&self) -> Option<&str> {
        match &self.state {
            AgentState::Ready { parser, .. } => parser.callbacks().title(),
            AgentState::Failed { .. } => None,
        }
    }

    /// Whether the foreground process is currently in a working
    /// state per its OSC title. `false` for `Failed` agents and for
    /// `Ready` agents whose title doesn't carry a status glyph.
    fn is_working(&self) -> bool {
        match &self.state {
            AgentState::Ready { parser, .. } => parser.callbacks().is_working(),
            AgentState::Failed { .. } => false,
        }
    }
}

/// In-flight prepare worker plus the data it produces. Owned by the
/// runtime; the spawn modal only sees the prepare's progress through
/// the [`Stage`] events the runtime forwards via
/// [`SpawnMinibuffer::set_bootstrap_stage`]. On success the worker
/// hands us a [`RemoteFs`] alongside the [`PreparedHost`] (opened on
/// the worker thread so the main render loop isn't blocked on a
/// synchronous `ssh -M -N` poll); the modal's path-zone autocomplete
/// then queries through the live `ssh -S` `ControlMaster`. On
/// `RemoteFs::open` failure the worker hands us `None` and the modal
/// degrades to literal-path mode rather than blocking the user from
/// typing a path.
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

    let _guard = TerminalGuard {
        enhanced_keyboard,
        mouse_captured,
        bracketed_paste,
    };

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).wrap_err("construct ratatui terminal")?;

    event_loop(
        &mut terminal,
        agents,
        nav_style,
        &config.bindings,
        log_tail,
        initial_cwd,
        config.scrollback_len,
        ChromeStyle::from_ui(&config.ui),
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
        if let AgentState::Ready { parser, transport } = &mut a.state {
            // PTY resize is best-effort: failure here means the child
            // sees a stale size until next resize, which is a harmless
            // cosmetic glitch (claude re-lays-out on the next paint
            // cycle). Surfacing as an error would force callers to
            // handle a non-actionable failure.
            let _ = transport.resize(rows, cols);
            parser.screen_mut().set_size(rows, cols);
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

/// Move focus to `new`, remembering the previous index for `FocusLast`
/// Move focus to `new`, recording the prior focus index for `prefix + Tab`
/// (alt-tab) bouncing. No-op if the focus is already on `new` — that
/// keeps a double-tap of the same direct-bind from clobbering the
/// bounce slot. Centralized helper because the event loop has six
/// focus-mutation sites and open-coding the `previous` update at each
/// would be the obvious bug source.
///
/// Also clears [`RuntimeAgent::needs_attention`] on the newly-focused
/// agent so the slow-blink dismisses the moment the user actually
/// looks at it. Done here rather than at each call site for the same
/// reason as the bounce-slot bookkeeping: one place to reason about.
fn change_focus(
    agents: &mut [RuntimeAgent],
    focused: &mut usize,
    previous: &mut Option<usize>,
    new: usize,
) {
    if new != *focused {
        *previous = Some(*focused);
        *focused = new;
        if let Some(a) = agents.get_mut(new) {
            a.needs_attention = false;
        }
    }
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

/// Detect working→idle transitions on unfocused agents and flag them for
/// the slow-blink attention cue. Called once per tick after the PTY drain
/// so the title parser sees the freshest state. Focused agents are skipped
/// — the user is already looking; nothing to alert about — and any agent
/// caught in this window has its blink cleared on the next focus change
/// via [`change_focus`].
fn flag_finished_unfocused(agents: &mut [RuntimeAgent], focused: usize) {
    for (i, agent) in agents.iter_mut().enumerate() {
        let cur = agent.is_working();
        if agent.last_working && !cur && i != focused {
            agent.needs_attention = true;
        }
        agent.last_working = cur;
    }
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
    mut agents: Vec<RuntimeAgent>,
    mut nav_style: NavStyle,
    bindings: &Bindings,
    log_tail: Option<&LogTail>,
    initial_cwd: &Path,
    scrollback_len: usize,
    chrome: ChromeStyle,
) -> Result<()> {
    // Long, but it is the central event loop and breaks naturally into
    // sequential phases (drain / reap / render / dispatch). Pulling each
    // arm into its own helper would require threading >5 mutable references
    // through the helper and gain little.
    #![allow(clippy::too_many_lines, clippy::too_many_arguments)]
    let mut prefix_state = PrefixState::default();
    let mut popup_state = PopupState::default();
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
    let mut focused: usize = 0;
    // Last agent the user was focused on, before the most recent
    // switch. Lets `prefix + Tab` (`FocusLast`) bounce between two
    // agents — the canonical alt-tab move when juggling a couple of
    // workspaces. `None` until the first switch happens; cleared if
    // the agent it points to is reaped (the user explicitly quit it
    // or the transport died) so the bounce never lands on a stale slot.
    let mut previous_focused: Option<usize> = None;
    // Per-frame click hitboxes for the tab strip / nav rows. Populated
    // by the leaf renderers, consumed by the mouse handler. Cleared at
    // the top of every `render_frame` so a stale frame's geometry can
    // never bleed into a fresh event hit-test.
    let mut tab_hitboxes = TabHitboxes::default();
    // Tab grabbed on `MouseEventKind::Down(Left)` — by stable
    // `AgentId`, not by index, so a reap or background reorder between
    // Down and Up still resolves to the same agent (or returns `None`
    // and the gesture cancels gracefully).
    let mut mouse_press: Option<AgentId> = None;
    let mut spawn_counter: usize = agents.len();
    // Captured once at loop entry so per-tab spinner / blink phases
    // are derived from a stable monotonic origin. The 50 ms event
    // poll below already redraws on each tick when nothing else
    // happens, so the wall-clock derivation is enough — no extra
    // wakeup machinery needed.
    let start = Instant::now();

    loop {
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
        // the loop so we can mutate `agents`, `attaches`, and
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
        agents.extend(new_agents);
        if focus_new && new_count > 0 {
            let target = agents.len() - 1;
            change_focus(&mut agents, &mut focused, &mut previous_focused, target);
        }

        for agent in &mut agents {
            match &mut agent.state {
                AgentState::Ready { parser, transport } => {
                    for bytes in transport.try_read() {
                        parser.process(&bytes);
                    }
                }
                AgentState::Failed { .. } => {}
            }
        }

        flag_finished_unfocused(&mut agents, focused);

        agents.retain_mut(|agent| match &mut agent.state {
            AgentState::Ready { transport, .. } => transport.try_wait().is_none(),
            // Failed agents are kept until the user exits — there's no
            // per-agent dismiss key yet, so auto-reaping a Failed agent
            // would erase the only place the user sees the error
            // message. Future P2 work (agent lifecycle keys) can
            // revisit.
            AgentState::Failed { .. } => true,
        });
        if agents.is_empty() {
            return Ok(());
        }
        focused = focused.min(agents.len() - 1);
        // Clear the bounce slot if the agent it pointed to was just
        // reaped — landing alt-tab on a stale index would silently
        // jump to whatever filled that slot, which is worse than no-op.
        if let Some(prev) = previous_focused
            && (prev >= agents.len() || prev == focused)
        {
            previous_focused = None;
        }
        if let PopupState::Open { selection } = popup_state
            && selection >= agents.len()
        {
            popup_state = PopupState::Open {
                selection: agents.len() - 1,
            };
        }

        let phase = AnimationPhase::from_elapsed(start.elapsed());
        terminal
            .draw(|frame| {
                render_frame(
                    frame,
                    &agents,
                    focused,
                    previous_focused,
                    nav_style,
                    popup_state,
                    help_state,
                    spawn_ui.as_ref(),
                    bindings,
                    prefix_state,
                    log_tail,
                    phase,
                    chrome,
                    &mut tab_hitboxes,
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
                                        agents.push(agent);
                                        let target = agents.len() - 1;
                                        change_focus(
                                            &mut agents,
                                            &mut focused,
                                            &mut previous_focused,
                                            target,
                                        );
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
                                let agent_id = format!("agent-{spawn_counter}");
                                let runtime_id = AgentId::new(agent_id.clone());
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
                                    agent_id,
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
                    continue;
                }

                if let PopupState::Open { selection } = popup_state {
                    if let Some(action) = bindings.on_popup.lookup(&key) {
                        match action {
                            PopupAction::Next => {
                                let next = (selection + 1) % agents.len();
                                popup_state = PopupState::Open { selection: next };
                            }
                            PopupAction::Prev => {
                                let prev = if selection == 0 {
                                    agents.len() - 1
                                } else {
                                    selection - 1
                                };
                                popup_state = PopupState::Open { selection: prev };
                            }
                            PopupAction::Confirm => {
                                change_focus(
                                    &mut agents,
                                    &mut focused,
                                    &mut previous_focused,
                                    selection,
                                );
                                popup_state = PopupState::Closed;
                            }
                            PopupAction::Cancel => {
                                popup_state = PopupState::Closed;
                            }
                        }
                    }
                    continue;
                }

                // Scroll mode: when the focused agent's PTY parser is
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
                if let Some(focused_agent) = agents.get_mut(focused)
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
                        if let Some(a) = agents.get_mut(focused) {
                            a.snap_to_live();
                            match &mut a.state {
                                AgentState::Ready { transport, .. } => {
                                    transport.write(&bytes).wrap_err("write to pty")?;
                                }
                                // Drop the bytes — a Failed pane can't
                                // accept input. tracing::trace because
                                // this is high-volume during typing if
                                // the user mistakes a placeholder pane
                                // for a live one.
                                AgentState::Failed { .. } => {
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
                        spawn_ui = Some(SpawnMinibuffer::open(initial_cwd));
                    }
                    KeyDispatch::FocusNext => {
                        let next = (focused + 1) % agents.len();
                        change_focus(&mut agents, &mut focused, &mut previous_focused, next);
                    }
                    KeyDispatch::FocusPrev => {
                        let prev = if focused == 0 {
                            agents.len() - 1
                        } else {
                            focused - 1
                        };
                        change_focus(&mut agents, &mut focused, &mut previous_focused, prev);
                    }
                    KeyDispatch::FocusLast => {
                        // Bounce. No-op if the previous slot is gone
                        // (already cleared in the per-frame clamp) or
                        // somehow points to current focus.
                        if let Some(prev) = previous_focused
                            && prev < agents.len()
                            && prev != focused
                        {
                            change_focus(&mut agents, &mut focused, &mut previous_focused, prev);
                        }
                    }
                    KeyDispatch::FocusAt(idx) => {
                        if idx < agents.len() {
                            change_focus(&mut agents, &mut focused, &mut previous_focused, idx);
                        }
                    }
                    KeyDispatch::ToggleNav => {
                        nav_style = nav_style.toggle();
                        let (term_cols, term_rows) =
                            crossterm::terminal::size().wrap_err("read terminal size")?;
                        let (rows, cols) =
                            pty_size_for(nav_style, term_rows, term_cols, log_tail.is_some());
                        resize_agents(&mut agents, rows, cols);
                    }
                    KeyDispatch::OpenPopup => {
                        popup_state = PopupState::Open { selection: focused };
                    }
                    KeyDispatch::OpenHelp => {
                        help_state = HelpState::Open;
                    }
                }
            }
            Event::Resize(cols, rows) => {
                let (pty_rows, pty_cols) = pty_size_for(nav_style, rows, cols, log_tail.is_some());
                resize_agents(&mut agents, pty_rows, pty_cols);
            }
            Event::Mouse(MouseEvent {
                kind, column, row, ..
            }) if no_overlay_active(spawn_ui.as_ref(), popup_state, help_state) => {
                // Wheel events are unconditional — anywhere in the
                // window is treated as "scroll the focused agent." In
                // LeftPane mode that means wheel-over-nav scrolls the
                // agent rather than the (currently un-scrollable)
                // agent list, which is mildly weird but harmless and
                // strictly better than the previous behavior of
                // forwarding wheel-as-arrow into Claude's prompt
                // history. Revisit if/when the nav pane becomes
                // independently scrollable.
                match kind {
                    MouseEventKind::ScrollUp => {
                        if let Some(agent) = agents.get_mut(focused) {
                            agent.nudge_scrollback(WHEEL_STEP);
                        }
                    }
                    MouseEventKind::ScrollDown => {
                        if let Some(agent) = agents.get_mut(focused) {
                            agent.nudge_scrollback(-WHEEL_STEP);
                        }
                    }
                    other => {
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
                                    if let Some(idx) = agents.iter().position(|a| a.id == id) {
                                        change_focus(
                                            &mut agents,
                                            &mut focused,
                                            &mut previous_focused,
                                            idx,
                                        );
                                    }
                                }
                                TabMouseDispatch::Reorder { from, to } => {
                                    mouse_press = None;
                                    let from_idx = agents.iter().position(|a| a.id == from);
                                    let to_idx = agents.iter().position(|a| a.id == to);
                                    if let (Some(f), Some(t)) = (from_idx, to_idx) {
                                        reorder_agents(&mut agents, f, t);
                                        focused = shift_index(focused, f, t);
                                        previous_focused =
                                            previous_focused.map(|p| shift_index(p, f, t));
                                    }
                                }
                                TabMouseDispatch::Cancel => mouse_press = None,
                            }
                        }
                    }
                }
            }
            Event::Paste(text) if no_overlay_active(spawn_ui.as_ref(), popup_state, help_state) => {
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
                if let Some(agent) = agents.get_mut(focused) {
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
    previous_focused: Option<usize>,
    nav_style: NavStyle,
    popup: PopupState,
    help: HelpState,
    spawn_ui: Option<&SpawnMinibuffer>,
    bindings: &Bindings,
    prefix_state: PrefixState,
    log_tail: Option<&LogTail>,
    phase: AnimationPhase,
    chrome: ChromeStyle,
    hitboxes: &mut TabHitboxes,
) {
    // Cleared at the top of every frame so a stale frame's rects can
    // never bleed into the next event hit-test if the layout changed
    // (terminal resize, nav-style toggle, agent spawn / reap).
    hitboxes.clear();
    let area = frame.area();
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
            render_left_pane(frame, main_area, agents, focused, phase, chrome, hitboxes);
        }
        NavStyle::Popup => {
            render_popup_style(
                frame,
                main_area,
                agents,
                focused,
                previous_focused,
                popup,
                bindings,
                prefix_state,
                phase,
                chrome,
                hitboxes,
            );
        }
    }
    if let (Some(tail), Some(area)) = (log_tail, log_area) {
        render_log_strip(frame, area, tail, chrome);
    }
    if let Some(ui) = spawn_ui {
        ui.render(frame, area, &bindings.on_modal);
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
fn render_log_strip(frame: &mut Frame<'_>, area: Rect, tail: &LogTail, chrome: ChromeStyle) {
    let line = tail.latest().unwrap_or_else(|| "—".to_string());
    let widget = Paragraph::new(Line::raw(line)).style(chrome.secondary);
    // Clear so a previous frame's longer line doesn't leave trailing
    // characters when the latest line is shorter.
    frame.render_widget(Clear, area);
    frame.render_widget(widget, area);
}

/// Render an agent's main pane based on its current [`AgentState`]. A
/// `Ready` agent shows the live PTY through `tui-term`'s
/// [`PseudoTerminal`]; a `Failed` agent shows the bootstrap error in
/// red, centered. The failure pane intentionally has no border or
/// title — a bordered placeholder reads as "this is a real UI
/// element" when in fact the slot is dead.
///
/// When the agent's PTY parser is showing scrollback (offset > 0), a
/// floating "↑ scroll N · esc" badge is painted over the bottom-right
/// of the pane via [`render_scroll_indicator`]. We deliberately do
/// NOT shrink the PTY by a row to make space — that would force a
/// `SIGWINCH` on every scroll-mode entry/exit, and Claude redrawing
/// its UI on each transition would be much worse UX than the badge
/// covering ~22 cells of (usually empty) Claude border. See AD-25.
fn render_agent_pane(frame: &mut Frame<'_>, area: Rect, agent: &RuntimeAgent) {
    match &agent.state {
        AgentState::Ready { parser, .. } => {
            let widget = PseudoTerminal::new(parser.screen());
            frame.render_widget(widget, area);
            let offset = parser.screen().scrollback();
            if offset > 0 {
                render_scroll_indicator(frame, area, offset);
            }
        }
        AgentState::Failed { error } => {
            let host = agent.host.as_deref().unwrap_or("");
            render_failure_pane(frame, area, host, &error.user_message());
        }
    }
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

fn render_left_pane(
    frame: &mut Frame<'_>,
    area: Rect,
    agents: &[RuntimeAgent],
    focused: usize,
    phase: AnimationPhase,
    chrome: ChromeStyle,
    hitboxes: &mut TabHitboxes,
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
        render_agent_pane(frame, pty_area, agent);
    }
}

#[allow(clippy::too_many_arguments)]
fn render_popup_style(
    frame: &mut Frame<'_>,
    area: Rect,
    agents: &[RuntimeAgent],
    focused: usize,
    previous_focused: Option<usize>,
    popup: PopupState,
    bindings: &Bindings,
    prefix_state: PrefixState,
    phase: AnimationPhase,
    chrome: ChromeStyle,
    hitboxes: &mut TabHitboxes,
) {
    let [pty_area, status_area] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(STATUS_BAR_HEIGHT)])
        .areas(area);

    if let Some(agent) = agents.get(focused) {
        render_agent_pane(frame, pty_area, agent);
    }

    render_status_bar(
        frame,
        status_area,
        agents,
        focused,
        previous_focused,
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

/// Render the bottom status bar in Popup mode: tab strip (left,
/// styled spans), buddy tail (middle, dim) showing the last non-blank
/// line of the previously-focused agent's screen, and the prefix hint
/// right-aligned. Splitting into discrete areas means each section
/// can be styled and clipped independently; the previous flat-string
/// approach forced uniform style and made it awkward to highlight the
/// focused tab without rendering a custom widget.
///
/// The buddy tail is the codemux-specific affordance: tmux can't show
/// what an unfocused window is doing without a custom integration,
/// because it doesn't parse the child's output. We already do (for
/// rendering), so the last visible line is free.
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
    previous_focused: Option<usize>,
    bindings: &Bindings,
    prefix_state: PrefixState,
    phase: AnimationPhase,
    chrome: ChromeStyle,
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

    // Build the left half as a single Line: tabs first (with separator
    // spans between), then a separator and the buddy tail if there's a
    // sensible buddy. Per-tab structs let us record the screen rect of
    // each tab into `hitboxes` while concatenating the visual into a
    // single Line so ratatui clips the whole thing at the area edge as
    // a unit. Without per-tab geometry, we'd have to re-derive widths
    // from the flat span list at hit-test time.
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
    if let Some(tail) = buddy_tail_spans(agents, focused, previous_focused, chrome) {
        spans.push(Span::styled("    ← ", chrome.secondary));
        spans.extend(tail);
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
    chrome: ChromeStyle,
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
    chrome: ChromeStyle,
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

fn tab_index_style(focused: bool, chrome: ChromeStyle) -> Style {
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
#[derive(Clone, Copy, Debug)]
struct ChromeStyle {
    /// Used for separators, hints, host prefix, log strip, unfocused
    /// tab body — anything that should read as "ambient context" rather
    /// than primary content. See [`Self::from_ui`] for the two modes.
    secondary: Style,
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
    fn from_ui(ui: &crate::config::Ui) -> Self {
        let secondary = if ui.subtle {
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM)
        } else {
            Style::default().fg(Color::Indexed(247))
        };
        Self { secondary }
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
/// `[host · ][⠋ ][● ]<repo>: <title>`, where:
///
/// - `host` (dim/gray) is shown only for SSH-backed agents so the
///   user can tell at a glance which devpod the agent lives on
/// - `⠋` is a Braille spinner frame, rendered when the foreground
///   process is mid-turn (its OSC title carries Claude's spinner /
///   ✱ glyph). Cycles via [`AnimationPhase`] at ~10 Hz so the motion
///   reads as ambient liveness.
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
    chrome: ChromeStyle,
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
    chrome: ChromeStyle,
) -> Vec<Span<'static>> {
    let body_style = body_style(focused, attention, phase, chrome);
    let host_style = if focused {
        // Reversed already; keep it readable by inheriting the same
        // reverse style (no extra fg/bg) so the host doesn't blend
        // into the highlight.
        Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
    } else {
        chrome.secondary
    };

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(6);

    if let Some(host) = host {
        spans.push(Span::styled(format!("{host} · "), host_style));
    }

    // Spinner before the body so its position stays put as titles
    // grow and shrink. `Color::Gray` *without* DIM on unfocused tabs
    // makes the spinner clearly visible against the surrounding DIM
    // label — the user reported the previous DarkGray+DIM was too
    // faint to read at a glance. On focused tabs we inherit the
    // tab's reverse video so the cue stays visible without breaking
    // the tab-highlight read.
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
fn body_style(focused: bool, attention: bool, phase: AnimationPhase, chrome: ChromeStyle) -> Style {
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

/// Build the spans for the buddy tail — the last non-blank line of
/// the previously-focused agent's screen. Returns `None` when there's
/// nothing useful to show: no previous, previous index out of range,
/// previous agent isn't a `Ready` (no parser), or all rows are blank.
/// The "prev != focused" guard is belt-and-braces; `change_focus`
/// already filters that out, but the renderer shouldn't trust it.
fn buddy_tail_spans(
    agents: &[RuntimeAgent],
    focused: usize,
    previous_focused: Option<usize>,
    chrome: ChromeStyle,
) -> Option<Vec<Span<'static>>> {
    let prev = previous_focused?;
    if prev == focused {
        return None;
    }
    let agent = agents.get(prev)?;
    let parser = match &agent.state {
        AgentState::Ready { parser, .. } => parser,
        AgentState::Failed { .. } => return None,
    };
    let (rows, cols) = parser.screen().size();
    let line = last_non_blank_row(parser, rows, cols)?;
    Some(vec![
        Span::styled(format!("[{}] ", prev + 1), chrome.secondary),
        Span::styled(line, chrome.secondary.add_modifier(Modifier::ITALIC)),
    ])
}

/// Walk the visible screen and return the last row whose trimmed
/// contents aren't empty. Used by [`buddy_tail_spans`] to find the
/// most recent meaningful output of an unfocused agent. Returns `None`
/// if every row is blank (fresh PTY before any output).
///
/// Generic over the callbacks type so the existing `Parser::new(...)`
/// tests (which use `Parser<()>`) keep compiling alongside the
/// `Parser<TitleCapture>` the runtime uses in production.
fn last_non_blank_row<CB: vt100::Callbacks>(
    parser: &Parser<CB>,
    rows: u16,
    cols: u16,
) -> Option<String> {
    if rows == 0 || cols == 0 {
        return None;
    }
    parser
        .screen()
        .rows(0, cols)
        .filter(|line| !line.trim().is_empty())
        .last()
        .map(|line| line.trim_end().to_string())
}

fn render_switcher_popup(
    frame: &mut Frame<'_>,
    area: Rect,
    agents: &[RuntimeAgent],
    selection: usize,
    phase: AnimationPhase,
    chrome: ChromeStyle,
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
        "drag",
        "drag a tab onto another to reorder (browser-tab semantics)",
    ));
    lines.push(binding_line_static(
        "alt+drag",
        "select text for native copy/paste (mouse capture is on)",
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
fn key_to_bytes(code: KeyCode, modifiers: KeyModifiers) -> Option<Vec<u8>> {
    let bytes = match code {
        KeyCode::Char(c) => {
            if modifiers.contains(KeyModifiers::CONTROL) {
                let lower = c.to_ascii_lowercase();
                if lower.is_ascii_alphabetic() {
                    return Some(vec![(lower as u8) - b'a' + 1]);
                }
                return None;
            }
            return Some(c.to_string().into_bytes());
        }
        KeyCode::Enter => {
            // Plain Enter submits; any modifier (Shift, Alt, Ctrl, Super)
            // is the "I want a newline, not submit" intent. Claude reads
            // `\x1b\r` (the Meta+Enter / Alt+Enter convention) as an
            // in-input newline — same byte sequence iTerm/Terminal.app
            // emit for Option+Enter when "Use Option as Meta" is on.
            //
            // Without this branch every Cmd/Ctrl/Shift+Enter chord lands
            // as plain `\r` and submits the message — the same failure
            // mode users hit when bracketed paste is off.
            if modifiers.intersects(
                KeyModifiers::SHIFT
                    | KeyModifiers::ALT
                    | KeyModifiers::CONTROL
                    | KeyModifiers::SUPER,
            ) {
                vec![0x1b, b'\r']
            } else {
                vec![b'\r']
            }
        }
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => vec![0x1b, b'[', b'Z'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => vec![0x1b, b'[', b'A'],
        KeyCode::Down => vec![0x1b, b'[', b'B'],
        KeyCode::Right => vec![0x1b, b'[', b'C'],
        KeyCode::Left => vec![0x1b, b'[', b'D'],
        KeyCode::Home => vec![0x1b, b'[', b'H'],
        KeyCode::End => vec![0x1b, b'[', b'F'],
        KeyCode::PageUp => vec![0x1b, b'[', b'5', b'~'],
        KeyCode::PageDown => vec![0x1b, b'[', b'6', b'~'],
        KeyCode::Delete => vec![0x1b, b'[', b'3', b'~'],
        KeyCode::Insert => vec![0x1b, b'[', b'2', b'~'],
        _ => return None,
    };
    Some(bytes)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    // key_to_bytes (unchanged from before)

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
        // Plain `\r` would submit; `ESC + \r` is the universal
        // Meta/Alt+Enter sequence Claude treats as a newline within
        // input. Each modifier the user might pair with Enter must take
        // this branch — Shift, Alt, Ctrl, Super (Cmd/Win).
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

    // change_focus semantics — the helper that keeps `previous_focused`
    // in sync with `focused` at every user-initiated switch site.

    #[test]
    fn change_focus_records_previous_when_focus_moves() {
        let mut agents: Vec<RuntimeAgent> = Vec::new();
        let mut focused = 0;
        let mut previous = None;
        change_focus(&mut agents, &mut focused, &mut previous, 2);
        assert_eq!(focused, 2);
        assert_eq!(previous, Some(0));
    }

    #[test]
    fn change_focus_is_a_noop_when_target_is_already_focused() {
        // Critical: a no-op must not clobber `previous`. Otherwise a
        // double-tap of the same direct-bind (or pressing FocusAt(idx)
        // for an already-focused tab) would erase the bounce slot.
        let mut agents: Vec<RuntimeAgent> = Vec::new();
        let mut focused = 1;
        let mut previous = Some(0);
        change_focus(&mut agents, &mut focused, &mut previous, 1);
        assert_eq!(focused, 1);
        assert_eq!(previous, Some(0));
    }

    #[test]
    fn change_focus_lets_alt_tab_bounce_via_two_calls() {
        // Simulates: focused=0, switch to 2 (FocusAt), then FocusLast
        // bounces back to 0 — and `previous` should now point to 2 so a
        // second FocusLast bounces forward again.
        let mut agents: Vec<RuntimeAgent> = Vec::new();
        let mut focused = 0;
        let mut previous = None;
        change_focus(&mut agents, &mut focused, &mut previous, 2);
        assert_eq!((focused, previous), (2, Some(0)));
        // FocusLast handler reads `previous` then calls change_focus(prev).
        let bounce_target = previous.unwrap();
        change_focus(&mut agents, &mut focused, &mut previous, bounce_target);
        assert_eq!((focused, previous), (0, Some(2)));
        // Second bounce.
        let bounce_target = previous.unwrap();
        change_focus(&mut agents, &mut focused, &mut previous, bounce_target);
        assert_eq!((focused, previous), (2, Some(0)));
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
        let mut focused = 0;
        let mut previous = None;
        change_focus(&mut agents, &mut focused, &mut previous, 1);
        assert!(!agents[1].needs_attention);
    }

    #[test]
    fn change_focus_noop_does_not_touch_needs_attention() {
        // If the user is already on tab 1 and a re-focus to 1 fires
        // (e.g. a duplicate direct-bind), needs_attention shouldn't
        // be silently flipped — the no-op semantics apply to the
        // attention bit just like to the bounce slot.
        let mut agents = vec![failed_agent("a"), failed_agent("b")];
        agents[1].needs_attention = true;
        let mut focused = 1;
        let mut previous = Some(0);
        change_focus(&mut agents, &mut focused, &mut previous, 1);
        // The interesting bit: the re-focus didn't enter the "moved"
        // branch, so the attention flag stays set. The next event-loop
        // tick will see focused==1 and not pulse, but the flag itself
        // is left to the explicit clear path on the *next* real focus
        // change.
        assert!(agents[1].needs_attention);
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
            ChromeStyle::default(),
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
            ChromeStyle::default(),
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

    // last_non_blank_row — feeds the buddy tail.

    #[test]
    fn last_non_blank_row_returns_none_for_a_fresh_parser() {
        // No bytes processed: every row is blank.
        let parser = Parser::new(10, 40, 0);
        assert_eq!(last_non_blank_row(&parser, 10, 40), None);
    }

    #[test]
    fn last_non_blank_row_finds_the_most_recent_meaningful_line() {
        let mut parser = Parser::new(5, 20, 0);
        parser.process(b"first\r\n");
        parser.process(b"second\r\n");
        parser.process(b"third\r\n");
        // VT terminals don't naturally append blank lines after the
        // cursor — we just expect the last *written* line to come back.
        assert_eq!(last_non_blank_row(&parser, 5, 20).as_deref(), Some("third"),);
    }

    #[test]
    fn last_non_blank_row_skips_trailing_whitespace_rows() {
        let mut parser = Parser::new(5, 20, 0);
        parser.process(b"useful output\r\n\r\n\r\n");
        // Last meaningful line is still `useful output`, not the
        // empty rows pushed by the trailing newlines.
        assert_eq!(
            last_non_blank_row(&parser, 5, 20).as_deref(),
            Some("useful output"),
        );
    }

    // buddy_tail_spans gating — protect the renderer from stale or
    // missing previous slots. The "renders the right styled spans for
    // a Ready agent" path requires constructing an AgentTransport,
    // which the TUI crate can't build directly (see RuntimeAgent
    // constructors note); we cover the None gates here and rely on
    // last_non_blank_row tests for the data-extraction half.

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
            ChromeStyle::default(),
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
            ChromeStyle::default(),
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
            ChromeStyle::default(),
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
            ChromeStyle::default(),
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
            ChromeStyle::default(),
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
            ChromeStyle::default(),
        );
        let dim = body_style(
            false,
            true,
            AnimationPhase {
                spinner_frame: 0,
                blink_bright: false,
            },
            ChromeStyle::default(),
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
            ChromeStyle::default(),
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
            ChromeStyle::default(),
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
            ChromeStyle::default(),
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
            ChromeStyle::default(),
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
            ChromeStyle::default(),
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
            ChromeStyle::default(),
        );
        assert_eq!(rendered(&spans), "codemux");
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
        flag_finished_unfocused(&mut agents, 0);
        assert!(agents[1].needs_attention);
        assert!(!agents[1].last_working, "last_working must be reset");
    }

    #[test]
    fn flag_finished_unfocused_skips_focused_agent() {
        // Focused → user is already looking → no slow-blink. The
        // detector still resets last_working so the transition is
        // consumed (not re-flagged on the next tick).
        let mut agents = vec![failed_agent("a"), failed_agent("b")];
        agents[0].last_working = true;
        flag_finished_unfocused(&mut agents, 0);
        assert!(!agents[0].needs_attention);
        assert!(!agents[0].last_working);
    }

    #[test]
    fn flag_finished_unfocused_no_op_when_state_unchanged() {
        let mut agents = vec![failed_agent("a"), failed_agent("b")];
        // Both already idle; nothing transitioned.
        flag_finished_unfocused(&mut agents, 0);
        assert!(!agents[0].needs_attention);
        assert!(!agents[1].needs_attention);
    }

    // ── tab_index_style ──────────────────────────────────────────

    #[test]
    fn tab_index_style_focused_is_reverse_bold() {
        let s = tab_index_style(true, ChromeStyle::default());
        assert!(s.add_modifier.contains(Modifier::REVERSED));
        assert!(s.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn tab_index_style_unfocused_uses_secondary_chrome() {
        // Default chrome is the readable-on-any-monitor mode: a fixed
        // gray (Indexed 247) with no DIM modifier. Without this pin the
        // unfocused tab index could silently regress back to a
        // terminal-defined dim that disappears on poor monitors.
        let s = tab_index_style(false, ChromeStyle::default());
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
        let chrome = ChromeStyle::from_ui(&crate::config::Ui { subtle: true });
        let s = tab_index_style(false, chrome);
        assert_eq!(s.fg, Some(Color::DarkGray));
        assert!(s.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn buddy_tail_spans_returns_none_when_no_previous() {
        let agents = vec![failed_agent("a"), failed_agent("b")];
        assert!(buddy_tail_spans(&agents, 0, None, ChromeStyle::default()).is_none());
    }

    #[test]
    fn buddy_tail_spans_returns_none_when_previous_equals_focused() {
        // Belt-and-braces: change_focus already filters this, but the
        // renderer mustn't crash if the invariant ever slips.
        let agents = vec![failed_agent("a"), failed_agent("b")];
        assert!(buddy_tail_spans(&agents, 1, Some(1), ChromeStyle::default()).is_none());
    }

    #[test]
    fn buddy_tail_spans_returns_none_when_previous_index_is_out_of_range() {
        // The per-frame clamp clears stale slots, but a transient
        // out-of-range value (mid-reap) must still produce no output.
        let agents = vec![failed_agent("a")];
        assert!(buddy_tail_spans(&agents, 0, Some(7), ChromeStyle::default()).is_none());
    }

    #[test]
    fn buddy_tail_spans_returns_none_when_previous_agent_is_failed() {
        // A Failed agent has no Parser, so there's no last-line to
        // surface. The buddy tail just stays hidden in this case.
        let agents = vec![failed_agent("a"), failed_agent("b")];
        assert!(buddy_tail_spans(&agents, 0, Some(1), ChromeStyle::default()).is_none());
    }

    // build_hint state-driven branch — the user's visible cue that
    // sticky nav mode is active. The two branches must produce
    // distinguishable output (different widths) so the layout reserves
    // appropriate room.

    #[test]
    fn build_hint_idle_and_awaiting_command_produce_different_widths() {
        let bindings = defaults();
        let (_, idle_width) = build_hint(&bindings, PrefixState::Idle, ChromeStyle::default());
        let (_, nav_width) = build_hint(
            &bindings,
            PrefixState::AwaitingCommand,
            ChromeStyle::default(),
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
                    Some(0),
                    &bindings,
                    PrefixState::Idle,
                    AnimationPhase::default(),
                    ChromeStyle::default(),
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
                    None,
                    &bindings,
                    PrefixState::Idle,
                    AnimationPhase::default(),
                    ChromeStyle::default(),
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
                    None,
                    &bindings,
                    PrefixState::Idle,
                    AnimationPhase::default(),
                    ChromeStyle::default(),
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
                    AnimationPhase::default(),
                    ChromeStyle::default(),
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
                    AnimationPhase::default(),
                    ChromeStyle::default(),
                    &mut hb,
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
                    AnimationPhase::default(),
                    ChromeStyle::default(),
                    &mut hb,
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
}
