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
//! Stage 5 added [`AgentState`] so an SSH agent can sit in a
//! `Bootstrapping` placeholder while [`crate::bootstrap_worker`] drives
//! the install/scp/build/spawn pipeline on a worker thread. The event
//! loop polls the worker each tick and flips the placeholder into a
//! `Ready` state with a real [`AgentTransport`] once the bootstrap
//! returns.

use std::error::Error as StdError;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::ValueEnum;
use codemux_session::AgentTransport;
use codemuxd_bootstrap::Stage;
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
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use tui_term::widget::PseudoTerminal;
use vt100::Parser;

use crate::bootstrap_worker::{BootstrapEvent, BootstrapHandle};
use crate::config::Config;
use crate::keymap::{Bindings, ModalAction, PopupAction, PrefixAction};
use crate::log_tail::LogTail;
use crate::spawn::{ModalOutcome, SpawnMinibuffer};

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
    FocusAt(usize),
    ToggleNav,
    OpenPopup,
    OpenHelp,
}

/// Result of inspecting a `RuntimeAgent` during the event loop's
/// drain phase. Computed while we hold a borrow on `agent.state`,
/// then applied in a second statement that has exclusive access.
/// Carries the transition payload (the new transport, or the
/// structured bootstrap error + host) so the apply-step doesn't
/// need to re-match on the prior state.
enum AgentTransition {
    Stay,
    PromoteToReady(AgentTransport),
    PromoteToFailed {
        host: String,
        error: codemuxd_bootstrap::Error,
    },
}

struct RuntimeAgent {
    label: String,
    /// Cell width/height the agent's pane is currently allocated.
    /// Tracked separately because a `Bootstrapping` agent has no
    /// transport/parser to ask, and on transition to `Ready` the new
    /// transport must be created (and the new parser sized) at the
    /// current geometry — not whatever was passed when the bootstrap
    /// started.
    rows: u16,
    cols: u16,
    state: AgentState,
}

/// Per-agent state. `Bootstrapping` is the placeholder a SSH spawn
/// occupies while [`crate::bootstrap_worker`] drives the install on a
/// worker thread. `Failed` captures a bootstrap that completed with
/// an error; the dead [`BootstrapHandle`] is dropped at that point so
/// the variant only carries the data the renderer actually needs.
/// `Ready` is the steady state for both local and SSH transports
/// once they have an [`AgentTransport`] and a renderable [`Parser`].
///
/// Transitions: a `Bootstrapping` agent either becomes `Ready` (on
/// `Ok(transport)`) or `Failed` (on `Err(_)`); a `Failed` agent stays
/// `Failed` until the user exits the TUI (no per-agent dismiss key
/// yet, that lands with future agent lifecycle work). The split
/// between `Bootstrapping` and `Failed` makes the bad combination
/// "live handle and rendered error" structurally unrepresentable.
enum AgentState {
    /// SSH agent waiting for [`crate::bootstrap_worker`] to finish.
    Bootstrapping {
        /// Hostname rendered in the placeholder pane. Stored
        /// separately from `label` so the placeholder text stays
        /// readable even if `label` ends up encoding more than the
        /// host (e.g. `host:agent-3`).
        host: String,
        /// Most-recent [`Stage`] reported by the worker through the
        /// [`BootstrapEvent::Stage`] stream. `None` until the very
        /// first event arrives (typically within a few ms — the
        /// worker emits `VersionProbe` before its first subprocess
        /// call). The placeholder renderer formats this as a
        /// human-readable label appended to the spinner line so the
        /// user can tell whether they're waiting on a 30-60s
        /// `RemoteBuild` or a sub-second `SocketConnect`.
        current_stage: Option<Stage>,
        /// When the bootstrap was started, used to compute the
        /// spinner phase. Per-agent rather than process-wide so
        /// concurrent bootstraps each have their own spinner cycle
        /// (and so a UI restart logically resets the animation).
        started_at: Instant,
        handle: BootstrapHandle,
    },
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
        /// grid (~720 bytes), which dwarfs the `Bootstrapping`
        /// variant. Without the box, every `RuntimeAgent` pays the
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

    fn bootstrapping(
        label: String,
        host: String,
        handle: BootstrapHandle,
        rows: u16,
        cols: u16,
    ) -> Self {
        Self {
            label,
            rows,
            cols,
            state: AgentState::Bootstrapping {
                host,
                current_stage: None,
                started_at: Instant::now(),
                handle,
            },
        }
    }
}

pub fn run(nav_style: NavStyle, config: &Config, log_tail: Option<&LogTail>) -> Result<()> {
    tracing::info!("codemux starting (nav={nav_style:?})");

    let (term_cols, term_rows) = crossterm::terminal::size().wrap_err("read terminal size")?;
    let (pty_rows, pty_cols) = pty_size_for(nav_style, term_rows, term_cols, log_tail.is_some());

    let initial = spawn_local_agent("agent-1".into(), None, pty_rows, pty_cols)?;
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

    event_loop(&mut terminal, agents, nav_style, &config.bindings, log_tail)
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
        // Keep the geometry on the agent itself so a Bootstrapping
        // agent that flips to Ready later still gets sized correctly
        // (its parser/transport are constructed at transition time).
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

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    mut agents: Vec<RuntimeAgent>,
    mut nav_style: NavStyle,
    bindings: &Bindings,
    log_tail: Option<&LogTail>,
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
    let mut focused: usize = 0;
    let mut spawn_counter: usize = agents.len();
    // Status-bar hint is bindings-derived but bindings cannot change at
    // runtime. Cache the formatted suffix so the render loop does not
    // re-allocate it 20 times per second (per the FRAME_POLL cadence).
    let status_hint = format!("{} {} for help", bindings.prefix, bindings.on_prefix.help,);

    loop {
        for agent in &mut agents {
            // Two-phase to satisfy the borrow checker: the
            // Bootstrapping arm needs a borrow of `agent.state` to
            // poll the worker, while a successful or failed poll
            // wants to *replace* `agent.state`. Compute the
            // transition first, then apply it in a separate
            // statement that has exclusive access.
            let transition = match &mut agent.state {
                AgentState::Ready { parser, transport } => {
                    for bytes in transport.try_read() {
                        parser.process(&bytes);
                    }
                    AgentTransition::Stay
                }
                AgentState::Bootstrapping {
                    host,
                    current_stage,
                    started_at: _,
                    handle,
                } => {
                    // Drain in a tight loop: the worker can emit
                    // several `Stage` events per frame on the fast
                    // path (~225ms total). Reading one at a time
                    // would render an indicator that's perpetually
                    // one stage behind.
                    let mut transition = AgentTransition::Stay;
                    while let Some(event) = handle.try_recv() {
                        match event {
                            BootstrapEvent::Stage(stage) => {
                                *current_stage = Some(stage);
                            }
                            BootstrapEvent::Done(Ok(transport)) => {
                                transition = AgentTransition::PromoteToReady(transport);
                                break;
                            }
                            BootstrapEvent::Done(Err(e)) => {
                                tracing::error!(label = %agent.label, "bootstrap failed: {e}");
                                transition = AgentTransition::PromoteToFailed {
                                    host: host.clone(),
                                    error: e,
                                };
                                break;
                            }
                        }
                    }
                    transition
                }
                AgentState::Failed { .. } => AgentTransition::Stay,
            };
            match transition {
                AgentTransition::Stay => {}
                AgentTransition::PromoteToReady(mut transport) => {
                    // Geometry may have changed during the bootstrap;
                    // the wire `Hello` was sized at
                    // start-of-bootstrap, so the remote daemon may
                    // need an immediate Resize before any frames
                    // flow.
                    let _ = transport.resize(agent.rows, agent.cols);
                    tracing::info!(label = %agent.label, "bootstrap completed; transport ready");
                    agent.state = AgentState::Ready {
                        parser: Box::new(Parser::new(agent.rows, agent.cols, 0)),
                        transport,
                    };
                }
                AgentTransition::PromoteToFailed { host, error } => {
                    agent.state = AgentState::Failed { host, error };
                }
            }
        }

        agents.retain_mut(|agent| match &mut agent.state {
            AgentState::Ready { transport, .. } => transport.try_wait().is_none(),
            // Bootstrapping and Failed agents are kept until the user
            // exits — there's no per-agent dismiss key yet, so
            // auto-reaping a Failed agent would erase the only place
            // the user sees the error message. Future P2 work (agent
            // lifecycle keys) can revisit.
            AgentState::Bootstrapping { .. } | AgentState::Failed { .. } => true,
        });
        if agents.is_empty() {
            return Ok(());
        }
        focused = focused.min(agents.len() - 1);
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
                    nav_style,
                    popup_state,
                    help_state,
                    spawn_ui.as_ref(),
                    bindings,
                    &status_hint,
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
                    match ui.handle(&key, &bindings.on_modal) {
                        ModalOutcome::None => {}
                        ModalOutcome::Cancel => {
                            spawn_ui = None;
                        }
                        ModalOutcome::Spawn { host, path } => {
                            spawn_ui = None;
                            let (term_cols, term_rows) =
                                crossterm::terminal::size().wrap_err("read terminal size")?;
                            let (rows, cols) =
                                pty_size_for(nav_style, term_rows, term_cols, log_tail.is_some());
                            spawn_counter += 1;
                            if host == "local" {
                                let label = format!("agent-{spawn_counter}");
                                let cwd_path = if path.is_empty() {
                                    None
                                } else {
                                    Some(Path::new(&path))
                                };
                                match spawn_local_agent(label, cwd_path, rows, cols) {
                                    Ok(agent) => {
                                        agents.push(agent);
                                        focused = agents.len() - 1;
                                    }
                                    Err(e) => {
                                        tracing::error!("spawn failed: {e}");
                                    }
                                }
                            } else {
                                // SSH branch: kick off the bootstrap on
                                // a worker thread and drop a placeholder
                                // agent into the navigator. The event
                                // loop's drain phase polls the worker
                                // each tick and flips the placeholder to
                                // Ready when the transport arrives.
                                let label = format!("{host}:agent-{spawn_counter}");
                                let agent_id = format!("agent-{spawn_counter}");
                                // Empty path → None: omit `--cwd` on the
                                // remote daemon and let it inherit the
                                // remote shell's login cwd ($HOME). A
                                // local path here would otherwise be
                                // sent verbatim to the remote, fail
                                // `cwd.exists()`, and exit the daemon
                                // before it ever bound the socket — the
                                // user-visible "EOF before HelloAck"
                                // failure mode.
                                let cwd_path = if path.is_empty() {
                                    None
                                } else {
                                    Some(PathBuf::from(&path))
                                };
                                let handle = crate::bootstrap_worker::start(
                                    host.clone(),
                                    agent_id,
                                    cwd_path,
                                    rows,
                                    cols,
                                );
                                tracing::info!(
                                    %host,
                                    label = %label,
                                    "started SSH bootstrap worker",
                                );
                                agents.push(RuntimeAgent::bootstrapping(
                                    label, host, handle, rows, cols,
                                ));
                                focused = agents.len() - 1;
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
                                focused = selection;
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
                                // Drop the bytes — the placeholder pane
                                // (whether still bootstrapping or stuck
                                // on a failure) can't accept input.
                                // tracing::trace because this is
                                // high-volume during typing if the user
                                // mistakes a placeholder pane for a
                                // live one.
                                AgentState::Bootstrapping { .. } | AgentState::Failed { .. } => {
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
                        spawn_ui = Some(SpawnMinibuffer::open());
                    }
                    KeyDispatch::FocusNext => {
                        focused = (focused + 1) % agents.len();
                    }
                    KeyDispatch::FocusPrev => {
                        focused = if focused == 0 {
                            agents.len() - 1
                        } else {
                            focused - 1
                        };
                    }
                    KeyDispatch::FocusAt(idx) => {
                        if idx < agents.len() {
                            focused = idx;
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
fn dispatch_key(state: &mut PrefixState, key: &KeyEvent, bindings: &Bindings) -> KeyDispatch {
    match *state {
        PrefixState::Idle => {
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
            *state = PrefixState::Idle;
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
            // Bound prefix-mode actions.
            match bindings.on_prefix.lookup(key) {
                Some(PrefixAction::Quit) => KeyDispatch::Exit,
                Some(PrefixAction::SpawnAgent) => KeyDispatch::SpawnAgent,
                Some(PrefixAction::FocusNext) => KeyDispatch::FocusNext,
                Some(PrefixAction::FocusPrev) => KeyDispatch::FocusPrev,
                Some(PrefixAction::ToggleNav) => KeyDispatch::ToggleNav,
                Some(PrefixAction::OpenSwitcher) => KeyDispatch::OpenPopup,
                Some(PrefixAction::Help) => KeyDispatch::OpenHelp,
                None => KeyDispatch::Consume,
            }
        }
    }
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
    status_hint: &str,
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
            render_popup_style(frame, main_area, agents, focused, popup, status_hint);
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
/// [`PseudoTerminal`]; a `Bootstrapping` agent shows a centered
/// spinner + status line; a `Failed` agent shows the bootstrap error
/// in red, also centered. Neither placeholder draws a border so the
/// pane reads as "this slot is being prepared" rather than "here is a
/// dead UI element."
fn render_agent_pane(frame: &mut Frame<'_>, area: Rect, agent: &RuntimeAgent) {
    match &agent.state {
        AgentState::Ready { parser, .. } => {
            let widget = PseudoTerminal::new(parser.screen());
            frame.render_widget(widget, area);
        }
        AgentState::Bootstrapping {
            host,
            current_stage,
            started_at,
            ..
        } => {
            render_bootstrap_placeholder(frame, area, host, *current_stage, *started_at, None);
        }
        AgentState::Failed { host, error } => {
            let formatted = format_bootstrap_error(error);
            render_bootstrap_placeholder(frame, area, host, None, Instant::now(), Some(&formatted));
        }
    }
}

/// Centered placeholder shown in the agent pane while a SSH bootstrap
/// is in flight, or to surface the bootstrap error after a failure.
///
/// Renders **without a border or title** — the user explicitly
/// rejected both during the Stage 5 UX pass: a bordered placeholder
/// reads as "this is a real UI element" when in fact the pane is
/// transient. The spinner (animated braille) + single status line
/// communicate "we're working" without taking visual ownership of
/// the slot.
///
/// The stage label appended in parens is the most recent
/// [`Stage`] reported by [`crate::bootstrap_worker::BootstrapEvent`];
/// without it, every bootstrap looks like a 30-60s black box (the
/// `RemoteBuild` step dominates wall time on first contact).
fn render_bootstrap_placeholder(
    frame: &mut Frame<'_>,
    area: Rect,
    host: &str,
    stage: Option<Stage>,
    started_at: Instant,
    error: Option<&str>,
) {
    // Build the status lines first so we can size the vertical
    // centering layout to exactly the content height.
    let mut lines: Vec<Line<'static>> = Vec::new();
    if let Some(err) = error {
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
    } else {
        let spinner = spinner_frame(started_at);
        let head = match stage {
            Some(s) => format!(
                "{spinner} bootstrapping codemuxd on {host}…  ({})",
                stage_label(s)
            ),
            None => format!("{spinner} bootstrapping codemuxd on {host}…"),
        };
        lines.push(Line::raw(head));
        if matches!(stage, Some(Stage::RemoteBuild)) {
            // The build is the only stage that takes long enough on
            // a fresh host to make the user wonder if the TUI hung.
            // The other 6 stages each finish in under a second.
            lines.push(Line::raw(""));
            lines.push(Line::styled(
                "  this can take 30-60s on first contact while codemuxd builds",
                Style::default().fg(Color::DarkGray),
            ));
        }
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

/// Map a bootstrap [`Stage`] to a short, user-readable label rendered
/// next to the spinner. Kept terse so the whole status line fits on
/// the typical 80-100 col terminal even when the host name is long.
/// Updated alongside the `Stage` enum — if a new stage lands without
/// a label here, the label silently becomes "running" (the catch-all
/// `_` arm keeps the renderer from panicking on a future enum
/// variant).
fn stage_label(stage: Stage) -> &'static str {
    match stage {
        Stage::VersionProbe => "probing host",
        Stage::TarballStage => "preparing source",
        Stage::Scp => "uploading source",
        Stage::RemoteBuild => "building remote daemon",
        Stage::DaemonSpawn => "spawning daemon",
        Stage::SocketTunnel => "opening tunnel",
        Stage::SocketConnect => "connecting",
        // `Stage` is `#[non_exhaustive]` upstream — give a sensible
        // default rather than failing to compile here when a future
        // stage is added.
        _ => "running",
    }
}

/// Single-character braille spinner frame keyed off the elapsed time
/// since `started_at`. Rotates every [`SPINNER_PERIOD_MS`]; the
/// runtime polls render at 20 Hz (`FRAME_POLL` = 50 ms) which means
/// every other frame steps the spinner. The start instant is owned by
/// the caller (typically `AgentState::Bootstrapping::started_at`) so
/// concurrent bootstraps each animate independently.
fn spinner_frame(started_at: Instant) -> char {
    const FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
    const SPINNER_PERIOD_MS: u128 = 80;
    let frames_len = u128::try_from(FRAMES.len()).unwrap_or(1);
    let idx = usize::try_from(started_at.elapsed().as_millis() / SPINNER_PERIOD_MS % frames_len)
        .unwrap_or(0);
    FRAMES[idx]
}

/// Render the bootstrap error envelope for the placeholder pane. The
/// stage hint is the first line; subsequent lines walk the source
/// chain so the user sees the underlying ssh/scp/cargo message
/// without having to dig in tracing logs.
fn format_bootstrap_error(e: &codemuxd_bootstrap::Error) -> String {
    use codemuxd_bootstrap::{Error, Stage};
    let head = match e {
        Error::Bootstrap {
            stage: Stage::VersionProbe,
            ..
        } => "ssh probe failed (host unreachable or auth refused)",
        Error::Bootstrap {
            stage: Stage::TarballStage,
            ..
        } => "couldn't stage local tarball (disk full?)",
        Error::Bootstrap {
            stage: Stage::Scp, ..
        } => "scp failed (network or remote disk)",
        Error::Bootstrap {
            stage: Stage::RemoteBuild,
            ..
        } => "remote build failed (cargo missing or compile error)",
        Error::Bootstrap {
            stage: Stage::DaemonSpawn,
            ..
        } => "remote daemon failed to spawn",
        Error::Bootstrap {
            stage: Stage::SocketTunnel,
            ..
        } => "ssh -L tunnel failed (OpenSSH < 6.7?)",
        Error::Bootstrap {
            stage: Stage::SocketConnect,
            ..
        } => "could not connect to remote daemon socket",
        Error::Bootstrap { .. } => "bootstrap failed",
        Error::Session { .. } => "wire handshake failed after bootstrap",
        // Bootstrap's `Error` is `#[non_exhaustive]`; keep an
        // explicit fallback so the renderer never panics on a
        // future variant added downstream.
        _ => "bootstrap failed (unknown error variant)",
    };
    let mut msg = head.to_string();
    let mut source: Option<&dyn StdError> = e.source();
    while let Some(s) = source {
        msg.push('\n');
        msg.push_str(&s.to_string());
        source = s.source();
    }
    msg
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

fn render_popup_style(
    frame: &mut Frame<'_>,
    area: Rect,
    agents: &[RuntimeAgent],
    focused: usize,
    popup: PopupState,
    status_hint: &str,
) {
    let [pty_area, status_area] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(STATUS_BAR_HEIGHT)])
        .areas(area);

    if let Some(agent) = agents.get(focused) {
        render_agent_pane(frame, pty_area, agent);
    }

    let labels: Vec<String> = agents
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let marker = if i == focused { "*" } else { " " };
            format!("[{}{}] {}", i + 1, marker, a.label)
        })
        .collect();
    let status = format!("{}    {status_hint}", labels.join("  "));
    // Status bar is a single row; truncate with an ellipsis if the labels
    // plus the hint overflow the available width. Without this, long user
    // chords (e.g. `ctrl+alt+pageup`) or many agent labels would clip
    // silently at the right edge.
    let display = clip_to_width(&status, status_area.width as usize);
    frame.render_widget(Paragraph::new(display), status_area);

    if let PopupState::Open { selection } = popup {
        render_switcher_popup(frame, area, agents, selection);
    }
}

/// Truncate `s` to at most `max` terminal cells, appending an ellipsis when
/// truncation actually happened. Counts Unicode code points (good enough for
/// the ASCII-heavy status bar); CJK / emoji widths would need the
/// `unicode-width` crate, which is not pulled in for one helper.
fn clip_to_width(s: &str, max: usize) -> String {
    let len = s.chars().count();
    if len <= max {
        return s.to_string();
    }
    if max == 0 {
        return String::new();
    }
    if max == 1 {
        return "…".into();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
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
    let popup_area = centered_rect_with_size(64, 26, area);
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

    #[test]
    fn clip_to_width_returns_input_unchanged_when_short_enough() {
        assert_eq!(clip_to_width("hello", 10), "hello");
        assert_eq!(clip_to_width("hello", 5), "hello");
    }

    #[test]
    fn clip_to_width_truncates_with_ellipsis_when_overflowing() {
        assert_eq!(clip_to_width("hello world", 8), "hello w…");
        assert_eq!(clip_to_width("hello", 4), "hel…");
    }

    #[test]
    fn clip_to_width_handles_max_zero_and_one() {
        assert_eq!(clip_to_width("hello", 0), "");
        assert_eq!(clip_to_width("hello", 1), "…");
    }

    #[test]
    fn clip_to_width_handles_empty_input() {
        assert_eq!(clip_to_width("", 10), "");
        assert_eq!(clip_to_width("", 0), "");
    }

    #[test]
    fn clip_to_width_counts_codepoints_not_bytes() {
        // Multi-byte chars: "café" is 4 chars, 5 bytes.
        assert_eq!(clip_to_width("café", 4), "café");
        assert_eq!(clip_to_width("café bar", 5), "café…");
    }

    // RuntimeAgent constructors
    //
    // We test only the `Bootstrapping` constructor here. The `Ready`
    // constructor needs an `AgentTransport`, and the TUI crate can't
    // construct `AgentTransport::Local(_)` directly because the enum
    // is `#[non_exhaustive]`. `AgentTransport::spawn_local` hardcodes
    // the `claude` binary, so a constructor test would either depend
    // on a real claude install or pull in a test-only entry point on
    // the session crate (out of scope here). The Bootstrapping test
    // exercises the same field-placement shape, and the Ready
    // constructor is also covered indirectly by the
    // bootstrap-completed transition in the event loop's drain
    // phase (see end-to-end smoke in docs/codemuxd-stages.md).

    /// Tiny runner that errors on every call. Used to spin up a real
    /// `BootstrapHandle` for tests that only care about the
    /// constructor's field placement (not the worker's behavior).
    /// The worker exits within microseconds with a Bootstrap error.
    struct NoopRunner;

    impl codemuxd_bootstrap::CommandRunner for NoopRunner {
        fn run(
            &self,
            _program: &str,
            _args: &[&str],
        ) -> std::io::Result<codemuxd_bootstrap::CommandOutput> {
            Err(std::io::Error::other("noop runner"))
        }

        fn spawn_detached(
            &self,
            _program: &str,
            _args: &[&str],
        ) -> std::io::Result<std::process::Child> {
            Err(std::io::Error::other("noop runner"))
        }
    }

    fn dummy_handle() -> BootstrapHandle {
        crate::bootstrap_worker::start_with_runner(
            Box::new(NoopRunner),
            "host".into(),
            "agent-x".into(),
            Some(PathBuf::from("/tmp")),
            24,
            80,
        )
    }

    #[test]
    fn bootstrapping_constructor_stores_label_host_geometry_and_state() {
        let agent = RuntimeAgent::bootstrapping(
            "host:agent-2".into(),
            "host".into(),
            dummy_handle(),
            30,
            120,
        );
        assert_eq!(agent.label, "host:agent-2");
        assert_eq!(agent.rows, 30);
        assert_eq!(agent.cols, 120);
        match agent.state {
            AgentState::Bootstrapping { host, .. } => {
                assert_eq!(host, "host");
            }
            AgentState::Ready { .. } | AgentState::Failed { .. } => {
                unreachable!("bootstrapping constructor must yield Bootstrapping state")
            }
        }
    }

    // format_bootstrap_error

    fn boot_err(
        stage: codemuxd_bootstrap::Stage,
        source: &'static str,
    ) -> codemuxd_bootstrap::Error {
        codemuxd_bootstrap::Error::Bootstrap {
            stage,
            source: Box::new(io::Error::other(source)),
        }
    }

    #[test]
    fn format_bootstrap_error_includes_stage_hint_and_source_chain() {
        let err = boot_err(codemuxd_bootstrap::Stage::Scp, "permission denied");
        let msg = format_bootstrap_error(&err);
        assert!(msg.starts_with("scp failed"), "got {msg:?}");
        assert!(msg.contains("permission denied"), "got {msg:?}");
    }

    #[test]
    fn format_bootstrap_error_keys_each_stage_to_a_distinct_hint() {
        use codemuxd_bootstrap::Stage;
        let stages = [
            (Stage::VersionProbe, "ssh probe"),
            (Stage::TarballStage, "tarball"),
            (Stage::Scp, "scp"),
            (Stage::RemoteBuild, "remote build"),
            (Stage::DaemonSpawn, "daemon"),
            (Stage::SocketTunnel, "tunnel"),
            (Stage::SocketConnect, "remote daemon socket"),
        ];
        for (stage, expected_substr) in stages {
            let msg = format_bootstrap_error(&boot_err(stage, "x"));
            assert!(
                msg.to_lowercase().contains(expected_substr),
                "stage {stage:?}: expected message to contain {expected_substr:?}, got {msg:?}",
            );
        }
    }

    #[test]
    fn format_bootstrap_error_handles_session_variant() {
        let err = codemuxd_bootstrap::Error::Session {
            source: Box::new(io::Error::other("handshake EOF")),
        };
        let msg = format_bootstrap_error(&err);
        assert!(msg.contains("handshake"), "got {msg:?}");
        assert!(msg.contains("handshake EOF"), "got {msg:?}");
    }
}
