//! P0 walking skeleton: spawn `claude` in a single PTY, render its output into
//! a single ratatui-managed window, forward keystrokes to it, exit on Ctrl-C.
//!
//! No chrome, no navigator, no persistence, no SSH, no diff panel. The whole
//! window is the PTY. P1 onwards adds everything else.

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

/// Drop guard: leaves the alternate screen and restores cooked mode no matter
/// how the event loop returns (early `?`, panic, normal exit).
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
    }
}

pub fn run() -> Result<()> {
    tracing::info!("codemux starting");

    let (cols, rows) = crossterm::terminal::size().wrap_err("read terminal size")?;

    // Spawn the PTY in cooked mode: a failure here prints a real error to the
    // user's terminal instead of vanishing behind the alt screen.
    // portable-pty returns `anyhow::Result`, which does not implement
    // `std::error::Error` and so cannot use `wrap_err` directly. Lift each
    // call into an `eyre::Report` via `map_err`.
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
                    if is_exit(&key) {
                        return Ok(());
                    }
                    if let Some(bytes) = key_to_bytes(key.code, key.modifiers) {
                        writer.write_all(&bytes).wrap_err("write to pty")?;
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

fn is_exit(key: &KeyEvent) -> bool {
    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL)
}

/// Translate a crossterm key event into the bytes a terminal-mode child
/// process expects. Minimal P0 mapping; does not yet handle alt/meta, function
/// keys beyond F1-F4 hidden in `_`, or non-CSI tilde sequences.
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

    #[test]
    fn ctrl_c_is_recognized_as_exit_intent() {
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(is_exit(&key));
    }

    #[test]
    fn plain_c_is_not_exit() {
        let key = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE);
        assert!(!is_exit(&key));
    }
}
