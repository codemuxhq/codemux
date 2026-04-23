//! P0 + P1.1 + P1.2 + P1.3: multi-agent runtime with two togglable navigator
//! styles and a config-driven keymap.
//!
//! The keymap (`crate::keymap`) is the single source of truth for which key
//! triggers which action. The runtime consults the appropriate `Bindings`
//! struct per scope and translates the matched action into a state mutation.
//! `Ctrl-B ?` (default) opens a help popup that lists every binding for every
//! scope, generated from the same Bindings POD.

use std::io::{self, Read, Write};
use std::path::Path;
use std::thread;
use std::time::Duration;

use clap::ValueEnum;
use color_eyre::Result;
use color_eyre::eyre::{WrapErr, eyre};
use crossbeam_channel::{Receiver, unbounded};
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use tui_term::widget::PseudoTerminal;
use vt100::Parser;

use crate::config::Config;
use crate::keymap::{Bindings, ModalAction, PopupAction, PrefixAction};
use crate::spawn_modal::{ModalOutcome, SpawnModal};

const FRAME_POLL: Duration = Duration::from_millis(50);
const READ_BUFFER_SIZE: usize = 8 * 1024;
const NAV_PANE_WIDTH: u16 = 25;
const STATUS_BAR_HEIGHT: u16 = 1;

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
    Open { selection: usize },
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

struct RuntimeAgent {
    label: String,
    parser: Parser,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    rx: Receiver<Vec<u8>>,
}

pub fn run(nav_style: NavStyle, config: &Config) -> Result<()> {
    tracing::info!("codemux starting (nav={nav_style:?})");

    let (term_cols, term_rows) = crossterm::terminal::size().wrap_err("read terminal size")?;
    let (pty_rows, pty_cols) = pty_size_for(nav_style, term_rows, term_cols);

    let initial = spawn_agent("agent-1".into(), None, pty_rows, pty_cols)?;
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

    event_loop(&mut terminal, agents, nav_style, &config.bindings)
}

fn pty_size_for(style: NavStyle, term_rows: u16, term_cols: u16) -> (u16, u16) {
    match style {
        NavStyle::LeftPane => (term_rows, term_cols.saturating_sub(NAV_PANE_WIDTH)),
        NavStyle::Popup => (term_rows.saturating_sub(STATUS_BAR_HEIGHT), term_cols),
    }
}

fn spawn_agent(
    label: String,
    cwd: Option<&Path>,
    rows: u16,
    cols: u16,
) -> Result<RuntimeAgent> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
        .map_err(|e| eyre!("open pty: {e}"))?;
    let mut cmd = CommandBuilder::new("claude");
    if let Some(cwd) = cwd {
        cmd.cwd(cwd);
    }
    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| eyre!("spawn `claude` (is it on PATH?): {e}"))?;
    drop(pair.slave);

    let writer = pair.master.take_writer().map_err(|e| eyre!("take pty writer: {e}"))?;
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| eyre!("clone pty reader: {e}"))?;
    let master = pair.master;

    let rx = spawn_reader_thread(reader);
    let parser = Parser::new(rows, cols, 0);
    Ok(RuntimeAgent { label, parser, master, writer, child, rx })
}

fn spawn_reader_thread(mut reader: Box<dyn Read + Send>) -> Receiver<Vec<u8>> {
    let (tx, rx) = unbounded::<Vec<u8>>();
    thread::spawn(move || {
        let mut buf = vec![0u8; READ_BUFFER_SIZE];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    rx
}

fn resize_agents(agents: &mut [RuntimeAgent], rows: u16, cols: u16) {
    for a in agents {
        // PTY resize is best-effort: failure here means the child sees a
        // stale size until next resize, which is a harmless cosmetic glitch
        // (claude re-lays-out on the next paint cycle). Surfacing as an
        // error would force callers to handle a non-actionable failure.
        let _ = a.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
        a.parser.screen_mut().set_size(rows, cols);
    }
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    mut agents: Vec<RuntimeAgent>,
    mut nav_style: NavStyle,
    bindings: &Bindings,
) -> Result<()> {
    // Long, but it is the central event loop and breaks naturally into
    // sequential phases (drain / reap / render / dispatch). Pulling each
    // arm into its own helper would require threading >5 mutable references
    // through the helper and gain little.
    #![allow(clippy::too_many_lines)]
    let mut prefix_state = PrefixState::default();
    let mut popup_state = PopupState::default();
    let mut help_state = HelpState::default();
    let mut spawn_modal: Option<SpawnModal> = None;
    let mut focused: usize = 0;
    let mut spawn_counter: usize = agents.len();

    loop {
        for agent in &mut agents {
            while let Ok(bytes) = agent.rx.try_recv() {
                agent.parser.process(&bytes);
            }
        }

        agents.retain_mut(|agent| !matches!(agent.child.try_wait(), Ok(Some(_))));
        if agents.is_empty() {
            return Ok(());
        }
        focused = focused.min(agents.len() - 1);
        if let PopupState::Open { selection } = popup_state
            && selection >= agents.len()
        {
            popup_state = PopupState::Open { selection: agents.len() - 1 };
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
                    spawn_modal.as_ref(),
                    bindings,
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

                if let Some(modal) = spawn_modal.as_mut() {
                    match modal.handle(&key, &bindings.on_modal) {
                        ModalOutcome::None => {}
                        ModalOutcome::Cancel => {
                            spawn_modal = None;
                        }
                        ModalOutcome::Spawn { host, path } => {
                            spawn_modal = None;
                            if host == "local" {
                                let (term_cols, term_rows) = crossterm::terminal::size()
                                    .wrap_err("read terminal size")?;
                                let (rows, cols) =
                                    pty_size_for(nav_style, term_rows, term_cols);
                                spawn_counter += 1;
                                let label = format!("agent-{spawn_counter}");
                                let cwd_path = if path.is_empty() {
                                    None
                                } else {
                                    Some(Path::new(&path))
                                };
                                match spawn_agent(label, cwd_path, rows, cols) {
                                    Ok(agent) => {
                                        agents.push(agent);
                                        focused = agents.len() - 1;
                                    }
                                    Err(e) => {
                                        tracing::error!("spawn failed: {e}");
                                    }
                                }
                            } else {
                                tracing::warn!(
                                    "ssh transport not yet implemented; \
                                     skipping spawn on host {host}",
                                );
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
                            a.writer.write_all(&bytes).wrap_err("write to pty")?;
                        }
                    }
                    KeyDispatch::Consume => {}
                    KeyDispatch::Exit => return Ok(()),
                    KeyDispatch::SpawnAgent => {
                        spawn_modal = Some(SpawnModal::open());
                    }
                    KeyDispatch::FocusNext => {
                        focused = (focused + 1) % agents.len();
                    }
                    KeyDispatch::FocusPrev => {
                        focused = if focused == 0 { agents.len() - 1 } else { focused - 1 };
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
                        let (rows, cols) = pty_size_for(nav_style, term_rows, term_cols);
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
                let (pty_rows, pty_cols) = pty_size_for(nav_style, rows, cols);
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
    let KeyCode::Char(c) = chord.code else { return None };
    let lower = c.to_ascii_lowercase();
    if lower.is_ascii_alphabetic() {
        Some((lower as u8) - b'a' + 1)
    } else {
        None
    }
}

// ---------- Rendering ----------

#[allow(clippy::too_many_arguments)]
fn render_frame(
    frame: &mut Frame<'_>,
    agents: &[RuntimeAgent],
    focused: usize,
    nav_style: NavStyle,
    popup: PopupState,
    help: HelpState,
    spawn_modal: Option<&SpawnModal>,
    bindings: &Bindings,
) {
    let area = frame.area();
    match nav_style {
        NavStyle::LeftPane => render_left_pane(frame, area, agents, focused),
        NavStyle::Popup => render_popup_style(frame, area, agents, focused, popup, bindings),
    }
    if let Some(modal) = spawn_modal {
        modal.render(frame, area);
    }
    if matches!(help, HelpState::Open) {
        render_help(frame, area, bindings);
    }
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
    let nav = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" agents "));
    frame.render_widget(nav, nav_area);

    if let Some(agent) = agents.get(focused) {
        let widget = PseudoTerminal::new(agent.parser.screen());
        frame.render_widget(widget, pty_area);
    }
}

fn render_popup_style(
    frame: &mut Frame<'_>,
    area: Rect,
    agents: &[RuntimeAgent],
    focused: usize,
    popup: PopupState,
    bindings: &Bindings,
) {
    let [pty_area, status_area] = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(STATUS_BAR_HEIGHT)])
        .areas(area);

    if let Some(agent) = agents.get(focused) {
        let widget = PseudoTerminal::new(agent.parser.screen());
        frame.render_widget(widget, pty_area);
    }

    let labels: Vec<String> = agents
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let marker = if i == focused { "*" } else { " " };
            format!("[{}{}] {}", i + 1, marker, a.label)
        })
        .collect();
    // Render the actual prefix + help binding so the hint stays accurate
    // regardless of what the user configured.
    let status = format!(
        "{}    {} {} for help",
        labels.join("  "),
        bindings.prefix,
        bindings.on_prefix.help,
    );
    frame.render_widget(Paragraph::new(status), status_area);

    if let PopupState::Open { selection } = popup {
        render_switcher_popup(frame, area, agents, selection);
    }
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
    let block = Block::default().borders(Borders::ALL).title(" switch agent ");
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

    lines.push(Line::styled(format!("prefix:  {}", bindings.prefix), header_style));
    lines.push(Line::raw(""));

    lines.push(Line::styled("in prefix mode:", header_style));
    for action in PrefixAction::ALL {
        lines.push(binding_line(bindings.on_prefix.binding_for(*action), action.description()));
    }
    lines.push(binding_line_static("1-9", "focus agent by one-indexed position"));
    lines.push(Line::raw(""));

    lines.push(Line::styled("in agent switcher popup:", header_style));
    for action in PopupAction::ALL {
        lines.push(binding_line(bindings.on_popup.binding_for(*action), action.description()));
    }
    lines.push(Line::raw(""));

    lines.push(Line::styled("in spawn modal:", header_style));
    for action in ModalAction::ALL {
        lines.push(binding_line(bindings.on_modal.binding_for(*action), action.description()));
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
        assert_eq!(key_to_bytes(KeyCode::Char('A'), KeyModifiers::NONE), Some(vec![b'A']));
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
        assert_eq!(key_to_bytes(KeyCode::Enter, KeyModifiers::NONE), Some(vec![b'\r']));
    }

    #[test]
    fn arrow_keys_emit_csi_letter_sequences() {
        assert_eq!(key_to_bytes(KeyCode::Up, KeyModifiers::NONE), Some(vec![0x1b, b'[', b'A']));
        assert_eq!(key_to_bytes(KeyCode::Down, KeyModifiers::NONE), Some(vec![0x1b, b'[', b'B']));
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
        let action = dispatch_key(&mut state, &key(KeyCode::Char('a'), KeyModifiers::NONE), &defaults());
        assert_eq!(action, KeyDispatch::Forward(vec![b'a']));
        assert_eq!(state, PrefixState::Idle);
    }

    #[test]
    fn idle_forwards_ctrl_c_to_pty() {
        let mut state = PrefixState::Idle;
        let action = dispatch_key(&mut state, &key(KeyCode::Char('c'), KeyModifiers::CONTROL), &defaults());
        assert_eq!(action, KeyDispatch::Forward(vec![0x03]));
    }

    #[test]
    fn ctrl_b_in_idle_arms_the_state_machine() {
        let mut state = PrefixState::Idle;
        let action = dispatch_key(&mut state, &key(KeyCode::Char('b'), KeyModifiers::CONTROL), &defaults());
        assert_eq!(action, KeyDispatch::Consume);
        assert_eq!(state, PrefixState::AwaitingCommand);
    }

    #[test]
    fn double_prefix_forwards_a_literal_prefix_byte() {
        let mut state = PrefixState::AwaitingCommand;
        let action = dispatch_key(&mut state, &key(KeyCode::Char('b'), KeyModifiers::CONTROL), &defaults());
        assert_eq!(action, KeyDispatch::Forward(vec![0x02]));
    }

    #[test]
    fn prefix_q_exits() {
        let mut state = PrefixState::AwaitingCommand;
        let action = dispatch_key(&mut state, &key(KeyCode::Char('q'), KeyModifiers::NONE), &defaults());
        assert_eq!(action, KeyDispatch::Exit);
    }

    #[test]
    fn prefix_c_opens_spawn_modal() {
        let mut state = PrefixState::AwaitingCommand;
        let action = dispatch_key(&mut state, &key(KeyCode::Char('c'), KeyModifiers::NONE), &defaults());
        assert_eq!(action, KeyDispatch::SpawnAgent);
    }

    #[test]
    fn prefix_question_mark_opens_help() {
        let mut state = PrefixState::AwaitingCommand;
        // Crossterm sends `?` as Char('?') with SHIFT (varies by platform).
        let action = dispatch_key(&mut state, &key(KeyCode::Char('?'), KeyModifiers::SHIFT), &defaults());
        assert_eq!(action, KeyDispatch::OpenHelp);
    }

    #[test]
    fn prefix_digit_focuses_by_one_indexed_position() {
        for d in 1..=9_u8 {
            let mut state = PrefixState::AwaitingCommand;
            let c = char::from_digit(u32::from(d), 10).unwrap();
            let action = dispatch_key(&mut state, &key(KeyCode::Char(c), KeyModifiers::NONE), &defaults());
            assert_eq!(action, KeyDispatch::FocusAt(usize::from(d - 1)));
        }
    }

    #[test]
    fn prefix_zero_is_consumed_no_focus() {
        let mut state = PrefixState::AwaitingCommand;
        let action = dispatch_key(&mut state, &key(KeyCode::Char('0'), KeyModifiers::NONE), &defaults());
        assert_eq!(action, KeyDispatch::Consume);
    }

    #[test]
    fn unbound_key_after_prefix_is_consumed() {
        let mut state = PrefixState::AwaitingCommand;
        let action = dispatch_key(&mut state, &key(KeyCode::Char('z'), KeyModifiers::NONE), &defaults());
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
        let action = dispatch_key(&mut state, &key(KeyCode::Char('x'), KeyModifiers::NONE), &config.bindings);
        assert_eq!(action, KeyDispatch::Exit);
        // The old key (q) is no longer bound to anything in prefix mode.
        let mut state2 = PrefixState::AwaitingCommand;
        let action2 = dispatch_key(&mut state2, &key(KeyCode::Char('q'), KeyModifiers::NONE), &config.bindings);
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
        let action = dispatch_key(&mut state, &key(KeyCode::Char('a'), KeyModifiers::CONTROL), &config.bindings);
        assert_eq!(action, KeyDispatch::Consume);
        assert_eq!(state, PrefixState::AwaitingCommand);
        // And the old prefix is now just a normal forwarded byte.
        let mut state2 = PrefixState::Idle;
        let action2 = dispatch_key(&mut state2, &key(KeyCode::Char('b'), KeyModifiers::CONTROL), &config.bindings);
        assert_eq!(action2, KeyDispatch::Forward(vec![0x02]));
    }

    // literal_byte_for

    #[test]
    fn literal_byte_for_ctrl_letters() {
        use crate::keymap::KeyChord;
        assert_eq!(literal_byte_for(&KeyChord::ctrl(KeyCode::Char('b'))), Some(0x02));
        assert_eq!(literal_byte_for(&KeyChord::ctrl(KeyCode::Char('a'))), Some(0x01));
    }

    #[test]
    fn literal_byte_for_returns_none_when_prefix_is_not_a_ctrl_letter() {
        use crate::keymap::KeyChord;
        assert_eq!(literal_byte_for(&KeyChord::plain(KeyCode::Char('q'))), None);
        assert_eq!(literal_byte_for(&KeyChord::ctrl(KeyCode::F(1))), None);
    }
}
