//! P0 + P1.1: spawn `claude` in a single PTY, render its output through a
//! ratatui-managed window, forward keystrokes to it. P1.1 adds a tmux-style
//! prefix key (Ctrl-B by default): Ctrl-B then `q` quits codemux, Ctrl-B
//! twice forwards a literal Ctrl-B byte, anything else after the prefix is
//! consumed and returns the state machine to idle.
//!
//! No multi-agent yet, no chrome, no persistence, no SSH. Those are later P1.

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
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tui_term::widget::PseudoTerminal;
use vt100::Parser;

const FRAME_POLL: Duration = Duration::from_millis(50);
const READ_BUFFER_SIZE: usize = 8 * 1024;
const PREFIX_BYTE: u8 = 0x02; // ASCII STX, what Ctrl-B sends on the wire

/// Drop guard: leaves the alternate screen and restores cooked mode no matter
/// how the event loop returns (early `?`, panic, normal exit).
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

/// Tmux-style prefix-key state. `Idle` forwards every key to the PTY (modulo
/// the prefix itself). `AwaitingCommand` interprets the next key as a codemux
/// command, then returns to `Idle` regardless of whether the key matched.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum PrefixState {
    #[default]
    Idle,
    AwaitingCommand,
}

/// What the event loop should do after `handle_key` decides on a key event.
#[derive(Debug, Eq, PartialEq)]
enum KeyAction {
    Forward(Vec<u8>),
    Consume,
    Exit,
}

pub fn run() -> Result<()> {
    tracing::info!("codemux starting");

    let (cols, rows) = crossterm::terminal::size().wrap_err("read terminal size")?;

    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
        .map_err(|e| eyre!("open pty: {e}"))?;
    let cmd = CommandBuilder::new("claude");
    let mut child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| eyre!("spawn `claude` (is it on PATH?): {e}"))?;
    drop(pair.slave);

    let mut writer = pair.master.take_writer().map_err(|e| eyre!("take pty writer: {e}"))?;
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| eyre!("clone pty reader: {e}"))?;
    drop(pair.master);

    let rx = spawn_reader_thread(reader);

    enable_raw_mode().wrap_err("enable raw mode")?;
    execute!(io::stdout(), EnterAlternateScreen).wrap_err("enter alt screen")?;
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend).wrap_err("construct ratatui terminal")?;

    let mut parser = Parser::new(rows, cols, 0);
    let result = event_loop(&mut terminal, &mut parser, &rx, writer.as_mut(), &mut *child);

    let _ = child.kill();
    let _ = child.wait();
    result
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

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    parser: &mut Parser,
    rx: &Receiver<Vec<u8>>,
    writer: &mut (dyn Write + Send),
    child: &mut (dyn portable_pty::Child + Send + Sync),
) -> Result<()> {
    let mut prefix_state = PrefixState::default();

    loop {
        while let Ok(bytes) = rx.try_recv() {
            parser.process(&bytes);
        }

        terminal
            .draw(|frame| {
                let widget = PseudoTerminal::new(parser.screen());
                frame.render_widget(widget, frame.area());
            })
            .wrap_err("draw frame")?;

        if matches!(child.try_wait(), Ok(Some(_))) {
            return Ok(());
        }

        if event::poll(FRAME_POLL).wrap_err("poll for input")? {
            match event::read().wrap_err("read input")? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    match handle_key(&mut prefix_state, &key) {
                        KeyAction::Forward(bytes) => {
                            writer.write_all(&bytes).wrap_err("write to pty")?;
                        }
                        KeyAction::Consume => {}
                        KeyAction::Exit => return Ok(()),
                    }
                }
                Event::Resize(cols, rows) => {
                    parser.screen_mut().set_size(rows, cols);
                    // TODO(P1): also resize the PTY itself via master.resize()
                }
                _ => {}
            }
        }
    }
}

/// Drives the prefix-key state machine. Returns the action the event loop
/// should perform: forward bytes to the PTY, consume (do nothing), or exit.
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
                KeyAction::Forward(vec![PREFIX_BYTE])
            } else {
                match key.code {
                    KeyCode::Char('q') => KeyAction::Exit,
                    _ => KeyAction::Consume,
                }
            }
        }
    }
}

fn is_prefix(key: &KeyEvent) -> bool {
    key.code == KeyCode::Char('b') && key.modifiers.contains(KeyModifiers::CONTROL)
}

/// Translate a crossterm key event into the bytes a terminal-mode child
/// process expects. Minimal P0/P1 mapping; does not yet handle alt/meta or
/// function keys beyond what the `_` arm drops.
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
        assert_eq!(
            key_to_bytes(KeyCode::Char('A'), KeyModifiers::CONTROL),
            Some(vec![0x01]),
        );
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
        assert_eq!(
            key_to_bytes(KeyCode::Up, KeyModifiers::NONE),
            Some(vec![0x1b, b'[', b'A']),
        );
        assert_eq!(
            key_to_bytes(KeyCode::Down, KeyModifiers::NONE),
            Some(vec![0x1b, b'[', b'B']),
        );
        assert_eq!(
            key_to_bytes(KeyCode::Right, KeyModifiers::NONE),
            Some(vec![0x1b, b'[', b'C']),
        );
        assert_eq!(
            key_to_bytes(KeyCode::Left, KeyModifiers::NONE),
            Some(vec![0x1b, b'[', b'D']),
        );
    }

    #[test]
    fn unmapped_key_is_dropped() {
        assert_eq!(key_to_bytes(KeyCode::F(12), KeyModifiers::NONE), None);
    }

    // Prefix-key state machine.

    #[test]
    fn idle_forwards_a_normal_char() {
        let mut state = PrefixState::Idle;
        let action = handle_key(&mut state, &key(KeyCode::Char('a'), KeyModifiers::NONE));
        assert_eq!(action, KeyAction::Forward(vec![b'a']));
        assert_eq!(state, PrefixState::Idle);
    }

    #[test]
    fn idle_forwards_ctrl_c_to_pty_after_p11() {
        // Ctrl-C is no longer codemux's exit key; it goes to claude.
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
    fn prefix_then_q_exits_codemux() {
        let mut state = PrefixState::AwaitingCommand;
        let action = handle_key(&mut state, &key(KeyCode::Char('q'), KeyModifiers::NONE));
        assert_eq!(action, KeyAction::Exit);
        assert_eq!(state, PrefixState::Idle);
    }

    #[test]
    fn double_prefix_forwards_a_literal_prefix_byte() {
        let mut state = PrefixState::AwaitingCommand;
        let action = handle_key(&mut state, &key(KeyCode::Char('b'), KeyModifiers::CONTROL));
        assert_eq!(action, KeyAction::Forward(vec![PREFIX_BYTE]));
        assert_eq!(state, PrefixState::Idle);
    }

    #[test]
    fn unknown_key_after_prefix_consumes_and_returns_to_idle() {
        let mut state = PrefixState::AwaitingCommand;
        let action = handle_key(&mut state, &key(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(action, KeyAction::Consume);
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
}
