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
use codemuxd_bootstrap::{PreparedHost, RealRunner, RemoteFs};
use color_eyre::Result;
use color_eyre::eyre::WrapErr;
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
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
use crate::keymap::{Bindings, DirectAction, ModalAction, PopupAction, PrefixAction};
use crate::log_tail::LogTail;
use crate::spawn::{DirLister, HOST_PLACEHOLDER, ModalOutcome, SpawnMinibuffer};

const FRAME_POLL: Duration = Duration::from_millis(50);
const NAV_PANE_WIDTH: u16 = 25;
const STATUS_BAR_HEIGHT: u16 = 1;
/// Height of the bottom log strip rendered when `--log` is passed.
/// Currently 1 row (the user's chosen UX is "show only the latest
/// line"); a future scrollable overlay could be N rows behind a
/// keybinding without changing this constant.
const LOG_STRIP_HEIGHT: u16 = 1;

struct TerminalGuard {
    enhanced_keyboard: bool,
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        // Best-effort cleanup. Failures here are unrecoverable (we are mid-drop
        // and may be on a panic path); the user's terminal may already be in
        // a degraded state, and surfacing an error would clobber whatever the
        // panic backtrace was about to say.
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
    label: String,
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
    /// the runtime baking in a stringification policy here.
    Failed {
        host: String,
        error: codemuxd_bootstrap::Error,
    },
    /// Live agent with an attached PTY (local or SSH-tunneled).
    Ready {
        /// Boxed because `vt100::Parser` carries a screen-sized cell
        /// grid (~720 bytes), which dwarfs the `Failed` variant.
        /// Without the box, every `RuntimeAgent` pays the
        /// `Ready`-sized footprint regardless of state, and clippy
        /// fires `large_enum_variant`. The pointer indirection is
        /// invisible against the per-frame parser/render work.
        parser: Box<Parser>,
        transport: AgentTransport,
    },
}

impl RuntimeAgent {
    fn ready(label: String, transport: AgentTransport, rows: u16, cols: u16) -> Self {
        Self {
            label,
            rows,
            cols,
            state: AgentState::Ready {
                parser: Box::new(Parser::new(rows, cols, 0)),
                transport,
            },
        }
    }

    fn failed(
        label: String,
        host: String,
        error: codemuxd_bootstrap::Error,
        rows: u16,
        cols: u16,
    ) -> Self {
        Self {
            label,
            rows,
            cols,
            state: AgentState::Failed { host, error },
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
    label: String,
    host: String,
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

    let initial = spawn_local_agent("agent-1".into(), Some(initial_cwd), pty_rows, pty_cols)?;
    let agents = vec![initial];

    enable_raw_mode().wrap_err("enable raw mode")?;
    execute!(io::stdout(), EnterAlternateScreen).wrap_err("enter alt screen")?;

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

    let _guard = TerminalGuard { enhanced_keyboard };

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).wrap_err("construct ratatui terminal")?;

    event_loop(
        &mut terminal,
        agents,
        nav_style,
        &config.bindings,
        log_tail,
        initial_cwd,
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
    label: String,
    cwd: Option<&Path>,
    rows: u16,
    cols: u16,
) -> Result<RuntimeAgent> {
    let transport = AgentTransport::spawn_local(label.clone(), cwd, rows, cols)
        .wrap_err("spawn local agent")?;
    Ok(RuntimeAgent::ready(label, transport, rows, cols))
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
/// (alt-tab) bouncing. No-op if the focus is already on `new` — that
/// keeps a double-tap of the same direct-bind from clobbering the
/// bounce slot. Centralized helper because the event loop has six
/// focus-mutation sites and open-coding the `previous` update at each
/// would be the obvious bug source.
fn change_focus(focused: &mut usize, previous: &mut Option<usize>, new: usize) {
    if new != *focused {
        *previous = Some(*focused);
        *focused = new;
    }
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    mut agents: Vec<RuntimeAgent>,
    mut nav_style: NavStyle,
    bindings: &Bindings,
    log_tail: Option<&LogTail>,
    initial_cwd: &Path,
) -> Result<()> {
    // Long, but it is the central event loop and breaks naturally into
    // sequential phases (drain / reap / render / dispatch). Pulling each
    // arm into its own helper would require threading >5 mutable references
    // through the helper and gain little.
    #![allow(clippy::too_many_lines)]
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
    let mut spawn_counter: usize = agents.len();

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
                            attach.label.clone(),
                            transport,
                            attach.rows,
                            attach.cols,
                        ));
                    }
                    Err(e) => {
                        tracing::error!(label = %attach.label, "attach failed: {e}");
                        new_agents.push(RuntimeAgent::failed(
                            attach.label.clone(),
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
            change_focus(&mut focused, &mut previous_focused, agents.len() - 1);
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
                                let cwd_path = if path.is_empty() {
                                    None
                                } else {
                                    Some(Path::new(&path))
                                };
                                match spawn_local_agent(label, cwd_path, rows, cols) {
                                    Ok(agent) => {
                                        agents.push(agent);
                                        change_focus(
                                            &mut focused,
                                            &mut previous_focused,
                                            agents.len() - 1,
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
                                    label,
                                    host,
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
                                change_focus(&mut focused, &mut previous_focused, selection);
                                popup_state = PopupState::Closed;
                            }
                            PopupAction::Cancel => {
                                popup_state = PopupState::Closed;
                            }
                        }
                    }
                    continue;
                }

                match dispatch_key(&mut prefix_state, &key, bindings) {
                    KeyDispatch::Forward(bytes) => {
                        if let Some(a) = agents.get_mut(focused) {
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
                        change_focus(&mut focused, &mut previous_focused, next);
                    }
                    KeyDispatch::FocusPrev => {
                        let prev = if focused == 0 {
                            agents.len() - 1
                        } else {
                            focused - 1
                        };
                        change_focus(&mut focused, &mut previous_focused, prev);
                    }
                    KeyDispatch::FocusLast => {
                        // Bounce. No-op if the previous slot is gone
                        // (already cleared in the per-frame clamp) or
                        // somehow points to current focus.
                        if let Some(prev) = previous_focused
                            && prev < agents.len()
                            && prev != focused
                        {
                            change_focus(&mut focused, &mut previous_focused, prev);
                        }
                    }
                    KeyDispatch::FocusAt(idx) => {
                        if idx < agents.len() {
                            change_focus(&mut focused, &mut previous_focused, idx);
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
) {
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
        NavStyle::LeftPane => render_left_pane(frame, main_area, agents, focused),
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
            );
        }
    }
    if let (Some(tail), Some(area)) = (log_tail, log_area) {
        render_log_strip(frame, area, tail);
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
fn render_log_strip(frame: &mut Frame<'_>, area: Rect, tail: &LogTail) {
    let line = tail.latest().unwrap_or_else(|| "—".to_string());
    let widget = Paragraph::new(Line::raw(line)).style(
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM),
    );
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
fn render_agent_pane(frame: &mut Frame<'_>, area: Rect, agent: &RuntimeAgent) {
    match &agent.state {
        AgentState::Ready { parser, .. } => {
            let widget = PseudoTerminal::new(parser.screen());
            frame.render_widget(widget, area);
        }
        AgentState::Failed { host, error } => {
            render_failure_pane(frame, area, host, &error.user_message());
        }
    }
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

fn render_left_pane(frame: &mut Frame<'_>, area: Rect, agents: &[RuntimeAgent], focused: usize) {
    let [nav_area, pty_area] = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(NAV_PANE_WIDTH), Constraint::Min(1)])
        .areas(area);

    let lines: Vec<Line> = agents
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let prefix = if i == focused { "> " } else { "  " };
            Line::from(format!("{prefix}[{}] {}", i + 1, a.label))
        })
        .collect();
    let nav = Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" agents "));
    frame.render_widget(nav, nav_area);

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
    );

    if let PopupState::Open { selection } = popup {
        render_switcher_popup(frame, area, agents, selection);
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
fn render_status_bar(
    frame: &mut Frame<'_>,
    area: Rect,
    agents: &[RuntimeAgent],
    focused: usize,
    previous_focused: Option<usize>,
    bindings: &Bindings,
    prefix_state: PrefixState,
) {
    // Compute the hint as both rendered Line (with styling) and a
    // plain text-width measurement (for the layout split). The two
    // need to stay in sync — the alternative was to render twice.
    let (hint_line, hint_width) = build_hint(bindings, prefix_state);

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

    // Build the left half as a single Line: tabs first, then a
    // separator and the buddy tail if there's a sensible buddy. Using
    // one Line lets ratatui clip the whole thing at the area edge as
    // a unit, instead of splitting tabs vs tail into competing layout
    // children that would each get half the width regardless of
    // content length.
    let mut spans = build_tab_strip_spans(agents, focused);
    if let Some(tail) = buddy_tail_spans(agents, focused, previous_focused) {
        spans.push(Span::styled(
            "    ← ",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ));
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
fn build_hint(bindings: &Bindings, prefix_state: PrefixState) -> (Line<'static>, u16) {
    match prefix_state {
        PrefixState::Idle => {
            let text = format!("{} {} for help", bindings.prefix, bindings.on_prefix.help);
            let width = u16::try_from(text.chars().count()).unwrap_or(u16::MAX);
            let line = Line::styled(
                text,
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM),
            );
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
                Span::styled(
                    body,
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::DIM),
                ),
            ]);
            (line, width)
        }
    }
}

/// Build the styled spans for the tab strip portion of the status
/// bar. Focused tab gets reverse-video + bold so the eye lands on it
/// immediately; others render dim so the focused tab pops without
/// having to look at a marker character. Tabs are separated by a thin
/// vertical bar — close enough to the browser tab convention to read
/// as "tabs" rather than "list of items."
fn build_tab_strip_spans(agents: &[RuntimeAgent], focused: usize) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(agents.len().saturating_mul(3));
    let separator_style = Style::default()
        .fg(Color::DarkGray)
        .add_modifier(Modifier::DIM);
    for (i, agent) in agents.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" │ ", separator_style));
        }
        let label = format!(" {} {} ", i + 1, agent.label);
        let style = if i == focused {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        };
        spans.push(Span::styled(label, style));
    }
    spans
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
        Span::styled(
            format!("[{}] ", prev + 1),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        ),
        Span::styled(
            line,
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM | Modifier::ITALIC),
        ),
    ])
}

/// Walk the visible screen and return the last row whose trimmed
/// contents aren't empty. Used by [`buddy_tail_spans`] to find the
/// most recent meaningful output of an unfocused agent. Returns `None`
/// if every row is blank (fresh PTY before any output).
fn last_non_blank_row(parser: &Parser, rows: u16, cols: u16) -> Option<String> {
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
) {
    let popup_area = centered_rect(50, 60, area);
    frame.render_widget(Clear, popup_area);
    let lines: Vec<Line> = agents
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let prefix = if i == selection { "> " } else { "  " };
            Line::from(format!("{prefix}[{}] {}", i + 1, a.label))
        })
        .collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" switch agent ");
    frame.render_widget(Paragraph::new(lines).block(block), popup_area);
}

fn render_help(frame: &mut Frame<'_>, area: Rect, bindings: &Bindings) {
    let popup_area = centered_rect_with_size(64, 30, area);
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
        KeyCode::Enter => vec![b'\r'],
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
#[allow(clippy::unwrap_used)]
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
        let mut focused = 0;
        let mut previous = None;
        change_focus(&mut focused, &mut previous, 2);
        assert_eq!(focused, 2);
        assert_eq!(previous, Some(0));
    }

    #[test]
    fn change_focus_is_a_noop_when_target_is_already_focused() {
        // Critical: a no-op must not clobber `previous`. Otherwise a
        // double-tap of the same direct-bind (or pressing FocusAt(idx)
        // for an already-focused tab) would erase the bounce slot.
        let mut focused = 1;
        let mut previous = Some(0);
        change_focus(&mut focused, &mut previous, 1);
        assert_eq!(focused, 1);
        assert_eq!(previous, Some(0));
    }

    #[test]
    fn change_focus_lets_alt_tab_bounce_via_two_calls() {
        // Simulates: focused=0, switch to 2 (FocusAt), then FocusLast
        // bounces back to 0 — and `previous` should now point to 2 so a
        // second FocusLast bounces forward again.
        let mut focused = 0;
        let mut previous = None;
        change_focus(&mut focused, &mut previous, 2);
        assert_eq!((focused, previous), (2, Some(0)));
        // FocusLast handler reads `previous` then calls change_focus(prev).
        let bounce_target = previous.unwrap();
        change_focus(&mut focused, &mut previous, bounce_target);
        assert_eq!((focused, previous), (0, Some(2)));
        // Second bounce.
        let bounce_target = previous.unwrap();
        change_focus(&mut focused, &mut previous, bounce_target);
        assert_eq!((focused, previous), (2, Some(0)));
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
        RuntimeAgent::failed(label.into(), "host".into(), err, 24, 80)
    }

    #[test]
    fn buddy_tail_spans_returns_none_when_no_previous() {
        let agents = vec![failed_agent("a"), failed_agent("b")];
        assert!(buddy_tail_spans(&agents, 0, None).is_none());
    }

    #[test]
    fn buddy_tail_spans_returns_none_when_previous_equals_focused() {
        // Belt-and-braces: change_focus already filters this, but the
        // renderer mustn't crash if the invariant ever slips.
        let agents = vec![failed_agent("a"), failed_agent("b")];
        assert!(buddy_tail_spans(&agents, 1, Some(1)).is_none());
    }

    #[test]
    fn buddy_tail_spans_returns_none_when_previous_index_is_out_of_range() {
        // The per-frame clamp clears stale slots, but a transient
        // out-of-range value (mid-reap) must still produce no output.
        let agents = vec![failed_agent("a")];
        assert!(buddy_tail_spans(&agents, 0, Some(7)).is_none());
    }

    #[test]
    fn buddy_tail_spans_returns_none_when_previous_agent_is_failed() {
        // A Failed agent has no Parser, so there's no last-line to
        // surface. The buddy tail just stays hidden in this case.
        let agents = vec![failed_agent("a"), failed_agent("b")];
        assert!(buddy_tail_spans(&agents, 0, Some(1)).is_none());
    }

    // build_hint state-driven branch — the user's visible cue that
    // sticky nav mode is active. The two branches must produce
    // distinguishable output (different widths) so the layout reserves
    // appropriate room.

    #[test]
    fn build_hint_idle_and_awaiting_command_produce_different_widths() {
        let bindings = defaults();
        let (_, idle_width) = build_hint(&bindings, PrefixState::Idle);
        let (_, nav_width) = build_hint(&bindings, PrefixState::AwaitingCommand);
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
    // The `Ready` constructor needs an `AgentTransport`, and the TUI
    // crate can't construct `AgentTransport::Local(_)` directly because
    // the enum is `#[non_exhaustive]`. `AgentTransport::spawn_local`
    // hardcodes the `claude` binary, so a constructor test would either
    // depend on a real claude install or pull in a test-only entry
    // point on the session crate (out of scope here). It's covered
    // indirectly by the spawn-local path tests in `spawn::tests`.

    // The user-facing message formatting (stage hint + source chain)
    // is tested in `codemuxd_bootstrap::error::tests::user_message_*`,
    // co-located with the `Error` type it formats.
}
