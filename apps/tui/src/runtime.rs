//! P0 + P1.1 + P1.2 (prototype): multi-agent with two togglable navigator
//! styles for A/B comparison.
//!
//! Two navigator chrome options are available simultaneously, switchable at
//! runtime via Ctrl-B v:
//!
//! - `LeftPane` — always-visible 25-column navigator on the left, focused
//!   PTY on the right. Constant glanceability.
//! - `Popup` — full-screen focused PTY with a 1-row status bar at the
//!   bottom; Ctrl-B w opens a centered switcher popup.
//!
//! Once the user picks one, the loser and the toggle plumbing come out in a
//! follow-up commit.
//!
//! Prefix-key commands available in either navigator:
//! - `c` spawn a new agent in the current cwd
//! - `n` / `p` next / previous agent
//! - `1`-`9` focus the agent at that index
//! - `v` toggle navigator style
//! - `w` open the switcher popup (Popup style only)
//! - `q` exit codemux
//! - prefix again forwards a literal Ctrl-B byte to the focused agent

use std::io::{self, Read, Write};
use std::thread;
use std::time::Duration;

use color_eyre::Result;
use color_eyre::eyre::{WrapErr, eyre};
use crossbeam_channel::{Receiver, unbounded};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use tui_term::widget::PseudoTerminal;
use vt100::Parser;

const FRAME_POLL: Duration = Duration::from_millis(50);
const READ_BUFFER_SIZE: usize = 8 * 1024;
const PREFIX_BYTE: u8 = 0x02;
const NAV_PANE_WIDTH: u16 = 25;
const STATUS_BAR_HEIGHT: u16 = 1;

/// Drop guard: leaves the alternate screen and restores cooked mode no matter
/// how the event loop returns.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NavStyle {
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

#[derive(Debug, Eq, PartialEq)]
enum KeyAction {
    Forward(Vec<u8>),
    Consume,
    Exit,
    SpawnAgent,
    FocusNext,
    FocusPrev,
    FocusAt(usize),
    ToggleNav,
    OpenPopup,
}

#[derive(Debug, Eq, PartialEq)]
enum PopupAction {
    None,
    ChangeSelection(usize),
    Confirm,
    Cancel,
}

struct RuntimeAgent {
    label: String,
    parser: Parser,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    rx: Receiver<Vec<u8>>,
}

pub fn run() -> Result<()> {
    tracing::info!("codemux starting");

    let nav_style = NavStyle::LeftPane;
    let (term_cols, term_rows) = crossterm::terminal::size().wrap_err("read terminal size")?;
    let (pty_rows, pty_cols) = pty_size_for(nav_style, term_rows, term_cols);

    let initial = spawn_agent("agent-1".into(), pty_rows, pty_cols)?;
    let agents = vec![initial];

    enable_raw_mode().wrap_err("enable raw mode")?;
    execute!(io::stdout(), EnterAlternateScreen).wrap_err("enter alt screen")?;
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).wrap_err("construct ratatui terminal")?;

    event_loop(&mut terminal, agents, nav_style)
}

fn pty_size_for(style: NavStyle, term_rows: u16, term_cols: u16) -> (u16, u16) {
    match style {
        NavStyle::LeftPane => (term_rows, term_cols.saturating_sub(NAV_PANE_WIDTH)),
        NavStyle::Popup => (term_rows.saturating_sub(STATUS_BAR_HEIGHT), term_cols),
    }
}

fn spawn_agent(label: String, rows: u16, cols: u16) -> Result<RuntimeAgent> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
        .map_err(|e| eyre!("open pty: {e}"))?;
    let cmd = CommandBuilder::new("claude");
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
        let _ = a.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
        a.parser.screen_mut().set_size(rows, cols);
    }
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    mut agents: Vec<RuntimeAgent>,
    mut nav_style: NavStyle,
) -> Result<()> {
    let mut prefix_state = PrefixState::default();
    let mut popup_state = PopupState::default();
    let mut focused: usize = 0;
    let mut spawn_counter: usize = agents.len();

    loop {
        // Drain bytes from every agent's channel into its parser.
        for agent in &mut agents {
            while let Ok(bytes) = agent.rx.try_recv() {
                agent.parser.process(&bytes);
            }
        }

        // Reap any dead agents. If they all died, exit.
        agents.retain_mut(|agent| !matches!(agent.child.try_wait(), Ok(Some(_))));
        if agents.is_empty() {
            return Ok(());
        }
        focused = focused.min(agents.len() - 1);
        // Also clamp the popup selection if an agent disappeared.
        if let PopupState::Open { selection } = popup_state
            && selection >= agents.len()
        {
            popup_state = PopupState::Open { selection: agents.len() - 1 };
        }

        terminal
            .draw(|frame| {
                render_frame(frame, &agents, focused, nav_style, popup_state);
            })
            .wrap_err("draw frame")?;

        if !event::poll(FRAME_POLL).wrap_err("poll for input")? {
            continue;
        }

        match event::read().wrap_err("read input")? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if let PopupState::Open { selection } = popup_state {
                    match handle_popup_key(&key, selection, agents.len()) {
                        PopupAction::ChangeSelection(new) => {
                            popup_state = PopupState::Open { selection: new };
                        }
                        PopupAction::Confirm => {
                            focused = selection;
                            popup_state = PopupState::Closed;
                        }
                        PopupAction::Cancel => {
                            popup_state = PopupState::Closed;
                        }
                        PopupAction::None => {}
                    }
                    continue;
                }

                match handle_key(&mut prefix_state, &key) {
                    KeyAction::Forward(bytes) => {
                        if let Some(a) = agents.get_mut(focused) {
                            a.writer.write_all(&bytes).wrap_err("write to pty")?;
                        }
                    }
                    KeyAction::Consume => {}
                    KeyAction::Exit => return Ok(()),
                    KeyAction::SpawnAgent => {
                        let (term_cols, term_rows) =
                            crossterm::terminal::size().wrap_err("read terminal size")?;
                        let (rows, cols) = pty_size_for(nav_style, term_rows, term_cols);
                        spawn_counter += 1;
                        let label = format!("agent-{spawn_counter}");
                        match spawn_agent(label, rows, cols) {
                            Ok(agent) => {
                                agents.push(agent);
                                focused = agents.len() - 1;
                            }
                            Err(e) => {
                                tracing::error!("spawn failed: {e}");
                            }
                        }
                    }
                    KeyAction::FocusNext => {
                        focused = (focused + 1) % agents.len();
                    }
                    KeyAction::FocusPrev => {
                        focused = if focused == 0 { agents.len() - 1 } else { focused - 1 };
                    }
                    KeyAction::FocusAt(idx) => {
                        if idx < agents.len() {
                            focused = idx;
                        }
                    }
                    KeyAction::ToggleNav => {
                        nav_style = nav_style.toggle();
                        let (term_cols, term_rows) =
                            crossterm::terminal::size().wrap_err("read terminal size")?;
                        let (rows, cols) = pty_size_for(nav_style, term_rows, term_cols);
                        resize_agents(&mut agents, rows, cols);
                    }
                    KeyAction::OpenPopup => {
                        popup_state = PopupState::Open { selection: focused };
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

fn render_frame(
    frame: &mut Frame<'_>,
    agents: &[RuntimeAgent],
    focused: usize,
    nav_style: NavStyle,
    popup: PopupState,
) {
    let area = frame.area();
    match nav_style {
        NavStyle::LeftPane => render_left_pane(frame, area, agents, focused),
        NavStyle::Popup => render_popup_style(frame, area, agents, focused, popup),
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
    let status = format!("{}    Ctrl-B for menu", labels.join("  "));
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

/// Drives the prefix-key state machine. Returns the action the event loop
/// should perform.
fn handle_key(state: &mut PrefixState, key: &KeyEvent) -> KeyAction {
    match *state {
        PrefixState::Idle => {
            if is_prefix(key) {
                *state = PrefixState::AwaitingCommand;
                KeyAction::Consume
            } else if let Some(bytes) = key_to_bytes(key.code, key.modifiers) {
                KeyAction::Forward(bytes)
            } else {
                KeyAction::Consume
            }
        }
        PrefixState::AwaitingCommand => {
            *state = PrefixState::Idle;
            if is_prefix(key) {
                return KeyAction::Forward(vec![PREFIX_BYTE]);
            }
            match key.code {
                KeyCode::Char('q') => KeyAction::Exit,
                KeyCode::Char('c') => KeyAction::SpawnAgent,
                KeyCode::Char('n') => KeyAction::FocusNext,
                KeyCode::Char('p') => KeyAction::FocusPrev,
                KeyCode::Char('v') => KeyAction::ToggleNav,
                KeyCode::Char('w') => KeyAction::OpenPopup,
                KeyCode::Char(c) if c.is_ascii_digit() => {
                    if let Some(d) = c.to_digit(10)
                        && d > 0
                    {
                        return KeyAction::FocusAt((d as usize) - 1);
                    }
                    KeyAction::Consume
                }
                _ => KeyAction::Consume,
            }
        }
    }
}

fn handle_popup_key(key: &KeyEvent, selection: usize, count: usize) -> PopupAction {
    if count == 0 {
        return PopupAction::Cancel;
    }
    match key.code {
        KeyCode::Up => {
            let new = if selection == 0 { count - 1 } else { selection - 1 };
            PopupAction::ChangeSelection(new)
        }
        KeyCode::Down => PopupAction::ChangeSelection((selection + 1) % count),
        KeyCode::Enter => PopupAction::Confirm,
        KeyCode::Esc => PopupAction::Cancel,
        _ => PopupAction::None,
    }
}

fn is_prefix(key: &KeyEvent) -> bool {
    key.code == KeyCode::Char('b') && key.modifiers.contains(KeyModifiers::CONTROL)
}

/// Translate a crossterm key event into the bytes a terminal-mode child
/// process expects. Minimal mapping; alt/meta and most function keys land when
/// needed.
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

    // key_to_bytes

    #[test]
    fn plain_ascii_char_passes_through_as_one_byte() {
        assert_eq!(key_to_bytes(KeyCode::Char('A'), KeyModifiers::NONE), Some(vec![b'A']));
    }

    #[test]
    fn shift_modifier_does_not_alter_the_char_byte() {
        assert_eq!(key_to_bytes(KeyCode::Char('Z'), KeyModifiers::SHIFT), Some(vec![b'Z']));
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
    fn ctrl_uppercase_collapses_to_the_lowercase_control_byte() {
        assert_eq!(key_to_bytes(KeyCode::Char('A'), KeyModifiers::CONTROL), Some(vec![0x01]));
    }

    #[test]
    fn ctrl_with_non_alpha_char_is_dropped() {
        assert_eq!(key_to_bytes(KeyCode::Char('1'), KeyModifiers::CONTROL), None);
    }

    #[test]
    fn enter_is_a_carriage_return() {
        assert_eq!(key_to_bytes(KeyCode::Enter, KeyModifiers::NONE), Some(vec![b'\r']));
    }

    #[test]
    fn backspace_is_del_not_bs() {
        assert_eq!(key_to_bytes(KeyCode::Backspace, KeyModifiers::NONE), Some(vec![0x7f]));
    }

    #[test]
    fn arrow_keys_emit_csi_letter_sequences() {
        assert_eq!(key_to_bytes(KeyCode::Up, KeyModifiers::NONE), Some(vec![0x1b, b'[', b'A']));
        assert_eq!(key_to_bytes(KeyCode::Down, KeyModifiers::NONE), Some(vec![0x1b, b'[', b'B']));
        assert_eq!(key_to_bytes(KeyCode::Right, KeyModifiers::NONE), Some(vec![0x1b, b'[', b'C']));
        assert_eq!(key_to_bytes(KeyCode::Left, KeyModifiers::NONE), Some(vec![0x1b, b'[', b'D']));
    }

    #[test]
    fn unmapped_key_is_dropped() {
        assert_eq!(key_to_bytes(KeyCode::F(12), KeyModifiers::NONE), None);
    }

    // Prefix state machine — basics

    #[test]
    fn idle_forwards_a_normal_char() {
        let mut state = PrefixState::Idle;
        let action = handle_key(&mut state, &key(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(action, KeyAction::Forward(vec![b'a']));
        assert_eq!(state, PrefixState::Idle);
    }

    #[test]
    fn idle_forwards_ctrl_c_to_pty() {
        let mut state = PrefixState::Idle;
        let action = handle_key(&mut state, &key(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(action, KeyAction::Forward(vec![0x03]));
        assert_eq!(state, PrefixState::Idle);
    }

    #[test]
    fn ctrl_b_in_idle_arms_the_state_machine_without_forwarding() {
        let mut state = PrefixState::Idle;
        let action = handle_key(&mut state, &key(KeyCode::Char('b'), KeyModifiers::CONTROL));
        assert_eq!(action, KeyAction::Consume);
        assert_eq!(state, PrefixState::AwaitingCommand);
    }

    #[test]
    fn double_prefix_forwards_a_literal_prefix_byte() {
        let mut state = PrefixState::AwaitingCommand;
        let action = handle_key(&mut state, &key(KeyCode::Char('b'), KeyModifiers::CONTROL));
        assert_eq!(action, KeyAction::Forward(vec![PREFIX_BYTE]));
        assert_eq!(state, PrefixState::Idle);
    }

    #[test]
    fn esc_after_prefix_cancels() {
        let mut state = PrefixState::AwaitingCommand;
        let action = handle_key(&mut state, &key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(action, KeyAction::Consume);
        assert_eq!(state, PrefixState::Idle);
    }

    #[test]
    fn typing_q_without_prefix_is_just_q() {
        let mut state = PrefixState::Idle;
        let action = handle_key(&mut state, &key(KeyCode::Char('q'), KeyModifiers::NONE));
        assert_eq!(action, KeyAction::Forward(vec![b'q']));
        assert_eq!(state, PrefixState::Idle);
    }

    #[test]
    fn unmapped_key_in_idle_state_is_consumed() {
        let mut state = PrefixState::Idle;
        let action = handle_key(&mut state, &key(KeyCode::F(12), KeyModifiers::NONE));
        assert_eq!(action, KeyAction::Consume);
        assert_eq!(state, PrefixState::Idle);
    }

    // Prefix state machine — multi-agent commands

    #[test]
    fn prefix_q_exits() {
        let mut state = PrefixState::AwaitingCommand;
        let action = handle_key(&mut state, &key(KeyCode::Char('q'), KeyModifiers::NONE));
        assert_eq!(action, KeyAction::Exit);
        assert_eq!(state, PrefixState::Idle);
    }

    #[test]
    fn prefix_c_spawns_an_agent() {
        let mut state = PrefixState::AwaitingCommand;
        let action = handle_key(&mut state, &key(KeyCode::Char('c'), KeyModifiers::NONE));
        assert_eq!(action, KeyAction::SpawnAgent);
        assert_eq!(state, PrefixState::Idle);
    }

    #[test]
    fn prefix_n_focuses_the_next_agent() {
        let mut state = PrefixState::AwaitingCommand;
        let action = handle_key(&mut state, &key(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(action, KeyAction::FocusNext);
    }

    #[test]
    fn prefix_p_focuses_the_previous_agent() {
        let mut state = PrefixState::AwaitingCommand;
        let action = handle_key(&mut state, &key(KeyCode::Char('p'), KeyModifiers::NONE));
        assert_eq!(action, KeyAction::FocusPrev);
    }

    #[test]
    fn prefix_digit_focuses_by_one_indexed_position() {
        for d in 1..=9 {
            let mut state = PrefixState::AwaitingCommand;
            let c = char::from_digit(d, 10).unwrap();
            let action = handle_key(&mut state, &key(KeyCode::Char(c), KeyModifiers::NONE));
            assert_eq!(action, KeyAction::FocusAt(usize::try_from(d - 1).unwrap()));
        }
    }

    #[test]
    fn prefix_zero_is_consumed_no_focus() {
        let mut state = PrefixState::AwaitingCommand;
        let action = handle_key(&mut state, &key(KeyCode::Char('0'), KeyModifiers::NONE));
        assert_eq!(action, KeyAction::Consume);
    }

    #[test]
    fn prefix_v_toggles_the_navigator_style() {
        let mut state = PrefixState::AwaitingCommand;
        let action = handle_key(&mut state, &key(KeyCode::Char('v'), KeyModifiers::NONE));
        assert_eq!(action, KeyAction::ToggleNav);
    }

    #[test]
    fn prefix_w_opens_the_switcher_popup() {
        let mut state = PrefixState::AwaitingCommand;
        let action = handle_key(&mut state, &key(KeyCode::Char('w'), KeyModifiers::NONE));
        assert_eq!(action, KeyAction::OpenPopup);
    }

    // NavStyle toggle

    #[test]
    fn nav_style_toggle_is_a_two_state_cycle() {
        assert_eq!(NavStyle::LeftPane.toggle(), NavStyle::Popup);
        assert_eq!(NavStyle::Popup.toggle(), NavStyle::LeftPane);
    }

    // Popup key handler

    #[test]
    fn popup_down_advances_selection_with_wrap() {
        assert_eq!(
            handle_popup_key(&key(KeyCode::Down, KeyModifiers::NONE), 0, 3),
            PopupAction::ChangeSelection(1),
        );
        assert_eq!(
            handle_popup_key(&key(KeyCode::Down, KeyModifiers::NONE), 2, 3),
            PopupAction::ChangeSelection(0),
        );
    }

    #[test]
    fn popup_up_retreats_selection_with_wrap() {
        assert_eq!(
            handle_popup_key(&key(KeyCode::Up, KeyModifiers::NONE), 1, 3),
            PopupAction::ChangeSelection(0),
        );
        assert_eq!(
            handle_popup_key(&key(KeyCode::Up, KeyModifiers::NONE), 0, 3),
            PopupAction::ChangeSelection(2),
        );
    }

    #[test]
    fn popup_enter_confirms_selection() {
        assert_eq!(
            handle_popup_key(&key(KeyCode::Enter, KeyModifiers::NONE), 1, 3),
            PopupAction::Confirm,
        );
    }

    #[test]
    fn popup_esc_cancels() {
        assert_eq!(
            handle_popup_key(&key(KeyCode::Esc, KeyModifiers::NONE), 0, 3),
            PopupAction::Cancel,
        );
    }

    #[test]
    fn popup_with_zero_agents_cancels_immediately() {
        assert_eq!(
            handle_popup_key(&key(KeyCode::Down, KeyModifiers::NONE), 0, 0),
            PopupAction::Cancel,
        );
    }
}
