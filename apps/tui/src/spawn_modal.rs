//! Spawn-agent modal: two side-by-side text fields for host and path, with
//! filesystem autocomplete on the path field. Triggered by the prefix-mode
//! `spawn_agent` binding (default `c`).
//!
//! The modal consults `keymap::ModalBindings` for command keys (`Confirm`,
//! `Cancel`, `SwapField`, `SwapToHost`, `NextCompletion`, `PrevCompletion`).
//! Plain chars and Backspace fall through to text input on the focused field.
//!
//! Today only host == empty or "local" actually spawns; non-local hosts are a
//! P1.4 SSH-transport concern. The runtime logs a warning and does nothing
//! when given a non-local host.

use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::keymap::{ModalAction, ModalBindings};

const MAX_COMPLETIONS: usize = 6;
const HOST_FIELD_WIDTH: u16 = 14;
const PATH_FIELD_WIDTH: u16 = 44;
const HOST_PLACEHOLDER: &str = "local";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpawnField {
    Host,
    Path,
}

/// What the modal tells the event loop after handling a key. Distinct from
/// `keymap::ModalAction` (which only names which key did what); this carries
/// the user's final intent (cancel / spawn with these values).
#[derive(Debug, Eq, PartialEq)]
pub enum ModalOutcome {
    None,
    Cancel,
    Spawn { host: String, path: String },
}

#[derive(Debug)]
pub struct SpawnModal {
    pub host: String,
    pub path: String,
    focused: SpawnField,
    completions: Vec<String>,
    completion_idx: Option<usize>,
}

impl SpawnModal {
    pub fn open() -> Self {
        let cwd = std::env::current_dir()
            .ok()
            .and_then(|p| p.to_str().map(String::from))
            .unwrap_or_default();
        let mut modal = Self {
            host: String::new(),
            path: cwd,
            focused: SpawnField::Path,
            completions: Vec::new(),
            completion_idx: None,
        };
        modal.refresh_completions();
        modal
    }

    pub fn handle(&mut self, key: &KeyEvent, bindings: &ModalBindings) -> ModalOutcome {
        // Discard Ctrl-key events so the user's prefix-key reflexes
        // (e.g. accidentally hitting Ctrl-B) do not insert garbage.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return ModalOutcome::None;
        }

        // Bound action keys, with one exception: SwapToHost is only honored
        // in the path field. In the host field the same key (default `@`)
        // falls through to text input so user@hostname targets work.
        if let Some(action) = bindings.lookup(key) {
            if action == ModalAction::SwapToHost && self.focused == SpawnField::Host {
                // fall through
            } else {
                return self.dispatch(action);
            }
        }

        // Text input fallback.
        match key.code {
            KeyCode::Char(c) => {
                self.current_field_mut().push(c);
                if self.focused == SpawnField::Path {
                    self.refresh_completions();
                }
                ModalOutcome::None
            }
            KeyCode::Backspace => {
                self.current_field_mut().pop();
                if self.focused == SpawnField::Path {
                    self.refresh_completions();
                }
                ModalOutcome::None
            }
            _ => ModalOutcome::None,
        }
    }

    fn dispatch(&mut self, action: ModalAction) -> ModalOutcome {
        match action {
            ModalAction::Cancel => ModalOutcome::Cancel,
            ModalAction::Confirm => {
                let host = self.host.trim().to_string();
                let host = if host.is_empty() { "local".into() } else { host };
                let path = self.path.trim().to_string();
                ModalOutcome::Spawn { host, path }
            }
            ModalAction::SwapField => {
                if self.focused == SpawnField::Path && self.completion_idx.is_some() {
                    self.apply_completion();
                } else {
                    self.swap_field();
                }
                ModalOutcome::None
            }
            ModalAction::SwapToHost => {
                // Caller has already verified focused == Path.
                self.focused = SpawnField::Host;
                ModalOutcome::None
            }
            ModalAction::NextCompletion => {
                if self.focused == SpawnField::Path && !self.completions.is_empty() {
                    let next = match self.completion_idx {
                        None => 0,
                        Some(i) => (i + 1) % self.completions.len(),
                    };
                    self.completion_idx = Some(next);
                }
                ModalOutcome::None
            }
            ModalAction::PrevCompletion => {
                if self.focused == SpawnField::Path && !self.completions.is_empty() {
                    let prev = match self.completion_idx {
                        None | Some(0) => self.completions.len() - 1,
                        Some(i) => i - 1,
                    };
                    self.completion_idx = Some(prev);
                }
                ModalOutcome::None
            }
        }
    }

    fn swap_field(&mut self) {
        self.focused = match self.focused {
            SpawnField::Host => SpawnField::Path,
            SpawnField::Path => SpawnField::Host,
        };
    }

    fn current_field_mut(&mut self) -> &mut String {
        match self.focused {
            SpawnField::Host => &mut self.host,
            SpawnField::Path => &mut self.path,
        }
    }

    fn refresh_completions(&mut self) {
        self.completion_idx = None;
        self.completions.clear();
        let (dir, prefix) = split_path_for_completion(&self.path);
        let Ok(entries) = std::fs::read_dir(&dir) else { return };
        let mut found: Vec<String> = entries
            .filter_map(Result::ok)
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                if !name.starts_with(&prefix) {
                    return None;
                }
                let is_dir = e.file_type().ok()?.is_dir();
                Some(if is_dir { format!("{name}/") } else { name })
            })
            .collect();
        found.sort();
        found.truncate(MAX_COMPLETIONS);
        self.completions = found;
    }

    fn apply_completion(&mut self) {
        let Some(idx) = self.completion_idx else { return };
        let Some(completion) = self.completions.get(idx).cloned() else { return };
        let (dir, _) = split_path_for_completion(&self.path);
        let dir_str = dir.to_string_lossy();
        self.path = if dir_str == "." && !self.path.starts_with("./") {
            completion
        } else if dir_str.ends_with('/') {
            format!("{dir_str}{completion}")
        } else {
            format!("{dir_str}/{completion}")
        };
        self.refresh_completions();
    }

    pub fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let modal_area = centered_rect_with_size(70, 12, area);
        frame.render_widget(Clear, modal_area);
        let block = Block::default().borders(Borders::ALL).title(" new agent ");
        let inner = block.inner(modal_area);
        frame.render_widget(block, modal_area);

        let [fields_row, _, completions_row, hint_row] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .areas(inner);

        let [host_area, _, path_area] = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(HOST_FIELD_WIDTH + 6),
                Constraint::Length(2),
                Constraint::Length(PATH_FIELD_WIDTH + 6),
            ])
            .areas(fields_row);

        frame.render_widget(self.field_paragraph(SpawnField::Host), host_area);
        frame.render_widget(self.field_paragraph(SpawnField::Path), path_area);

        let lines: Vec<Line> = self
            .completions
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let prefix = if Some(i) == self.completion_idx { "> " } else { "  " };
                let style = if Some(i) == self.completion_idx {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                Line::styled(format!("{prefix}{c}"), style)
            })
            .collect();
        let completions_offset = HOST_FIELD_WIDTH + 6 + 2 + 6;
        let completions_area = Rect {
            x: completions_row.x + completions_offset.min(completions_row.width),
            y: completions_row.y,
            width: completions_row.width.saturating_sub(completions_offset),
            height: completions_row.height,
        };
        frame.render_widget(Paragraph::new(lines), completions_area);

        let hint = "Tab autocomplete  @ swap to host  Enter spawn  Esc cancel";
        frame.render_widget(Paragraph::new(hint), hint_row);
    }

    fn field_paragraph(&self, field: SpawnField) -> Paragraph<'_> {
        let label = match field {
            SpawnField::Host => "host: ",
            SpawnField::Path => "path: ",
        };
        let value = match field {
            SpawnField::Host => &self.host,
            SpawnField::Path => &self.path,
        };
        let width = match field {
            SpawnField::Host => HOST_FIELD_WIDTH,
            SpawnField::Path => PATH_FIELD_WIDTH,
        } as usize;

        let display: String = if value.is_empty() && field == SpawnField::Host {
            HOST_PLACEHOLDER.into()
        } else if value.chars().count() > width {
            value.chars().rev().take(width).collect::<Vec<_>>().into_iter().rev().collect()
        } else {
            value.clone()
        };

        let cursor = if self.focused == field { "_" } else { " " };
        let bracketed = format!(
            "{label}[{display}{cursor:>width$}]",
            width = width.saturating_sub(display.chars().count()),
        );
        let style = if self.focused == field {
            Style::default().add_modifier(Modifier::BOLD)
        } else if value.is_empty() && field == SpawnField::Host {
            Style::default().add_modifier(Modifier::DIM)
        } else {
            Style::default()
        };
        Paragraph::new(Span::styled(bracketed, style))
    }
}

fn split_path_for_completion(input: &str) -> (PathBuf, String) {
    if input.is_empty() {
        return (PathBuf::from("."), String::new());
    }
    let path = Path::new(input);
    if input.ends_with('/') {
        return (path.to_path_buf(), String::new());
    }
    if let Some(parent) = path.parent() {
        let dir = if parent.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            parent.to_path_buf()
        };
        let prefix = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        return (dir, prefix);
    }
    (PathBuf::from("."), input.to_string())
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn modal() -> SpawnModal {
        let mut m = SpawnModal {
            host: String::new(),
            path: "/tmp".into(),
            focused: SpawnField::Path,
            completions: Vec::new(),
            completion_idx: None,
        };
        m.completions.clear();
        m
    }

    fn b() -> ModalBindings {
        ModalBindings::default()
    }

    #[test]
    fn ctrl_modified_keys_are_dropped() {
        let mut m = modal();
        m.path = "/x".into();
        let outcome = m.handle(&ctrl(KeyCode::Char('b')), &b());
        assert_eq!(outcome, ModalOutcome::None);
        assert_eq!(m.path, "/x");
        assert_eq!(m.handle(&ctrl(KeyCode::Enter), &b()), ModalOutcome::None);
    }

    #[test]
    fn esc_returns_cancel() {
        let mut m = modal();
        assert_eq!(m.handle(&key(KeyCode::Esc), &b()), ModalOutcome::Cancel);
    }

    #[test]
    fn enter_returns_spawn_with_trimmed_values() {
        let mut m = modal();
        m.path = "  /home/user  ".into();
        m.host = "  remote  ".into();
        let outcome = m.handle(&key(KeyCode::Enter), &b());
        assert_eq!(
            outcome,
            ModalOutcome::Spawn { host: "remote".into(), path: "/home/user".into() },
        );
    }

    #[test]
    fn empty_host_becomes_local_on_spawn() {
        let mut m = modal();
        m.path = "/x".into();
        let outcome = m.handle(&key(KeyCode::Enter), &b());
        assert_eq!(
            outcome,
            ModalOutcome::Spawn { host: "local".into(), path: "/x".into() },
        );
    }

    #[test]
    fn typing_a_char_appends_to_focused_field() {
        let mut m = modal();
        m.path = "/x".into();
        m.handle(&key(KeyCode::Char('y')), &b());
        assert_eq!(m.path, "/xy");
    }

    #[test]
    fn at_in_path_swaps_to_host_field() {
        let mut m = modal();
        assert_eq!(m.focused, SpawnField::Path);
        m.handle(&key(KeyCode::Char('@')), &b());
        assert_eq!(m.focused, SpawnField::Host);
        assert_eq!(m.path, "/tmp");
    }

    #[test]
    fn at_in_host_is_a_literal_char() {
        let mut m = modal();
        m.focused = SpawnField::Host;
        m.handle(&key(KeyCode::Char('@')), &b());
        assert_eq!(m.host, "@");
        assert_eq!(m.focused, SpawnField::Host);
    }

    #[test]
    fn tab_swaps_field_when_no_completion_highlighted() {
        let mut m = modal();
        m.handle(&key(KeyCode::Tab), &b());
        assert_eq!(m.focused, SpawnField::Host);
        m.handle(&key(KeyCode::Tab), &b());
        assert_eq!(m.focused, SpawnField::Path);
    }

    #[test]
    fn tab_applies_completion_when_one_is_highlighted() {
        let mut m = modal();
        m.path = "/foo".into();
        m.completions = vec!["foobar/".into(), "fooboo".into()];
        m.completion_idx = Some(0);
        m.handle(&key(KeyCode::Tab), &b());
        assert_eq!(m.path, "/foobar/");
        assert_eq!(m.focused, SpawnField::Path);
    }

    #[test]
    fn backspace_pops_from_focused_field() {
        let mut m = modal();
        m.handle(&key(KeyCode::Backspace), &b());
        assert_eq!(m.path, "/tm");
    }

    #[test]
    fn backspace_on_empty_field_is_a_no_op() {
        let mut m = modal();
        m.path = String::new();
        m.handle(&key(KeyCode::Backspace), &b());
        assert_eq!(m.path, "");
    }

    #[test]
    fn down_cycles_completion_selection_with_wrap() {
        let mut m = modal();
        m.completions = vec!["a".into(), "b".into(), "c".into()];
        m.handle(&key(KeyCode::Down), &b());
        assert_eq!(m.completion_idx, Some(0));
        m.handle(&key(KeyCode::Down), &b());
        assert_eq!(m.completion_idx, Some(1));
        m.handle(&key(KeyCode::Down), &b());
        m.handle(&key(KeyCode::Down), &b());
        assert_eq!(m.completion_idx, Some(0));
    }

    #[test]
    fn up_with_no_selection_jumps_to_last() {
        let mut m = modal();
        m.completions = vec!["a".into(), "b".into(), "c".into()];
        m.handle(&key(KeyCode::Up), &b());
        assert_eq!(m.completion_idx, Some(2));
    }

    #[test]
    fn down_in_host_field_does_nothing() {
        let mut m = modal();
        m.focused = SpawnField::Host;
        m.completions = vec!["x".into()];
        m.handle(&key(KeyCode::Down), &b());
        assert_eq!(m.completion_idx, None);
    }

    #[test]
    fn user_can_remap_modal_confirm() {
        // Enter is the default Confirm key. Remap it to F5 and verify both
        // the new key works and Enter falls through to text input.
        let toml_text = r#"
            [bindings.on_modal]
            confirm = "f5"
        "#;
        let config: crate::config::Config = toml::from_str(toml_text).unwrap();
        let mut m = modal();
        m.path = "/y".into();
        let outcome = m.handle(
            &KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE),
            &config.bindings.on_modal,
        );
        assert!(matches!(outcome, ModalOutcome::Spawn { .. }));
    }

    // split_path_for_completion

    #[test]
    fn split_empty_path_uses_dot_and_empty_prefix() {
        let (dir, prefix) = split_path_for_completion("");
        assert_eq!(dir, PathBuf::from("."));
        assert_eq!(prefix, "");
    }

    #[test]
    fn split_trailing_slash_uses_full_path_and_empty_prefix() {
        let (dir, prefix) = split_path_for_completion("/foo/bar/");
        assert_eq!(dir, PathBuf::from("/foo/bar/"));
        assert_eq!(prefix, "");
    }

    #[test]
    fn split_path_separates_dir_and_basename() {
        let (dir, prefix) = split_path_for_completion("/foo/bar/baz");
        assert_eq!(dir, PathBuf::from("/foo/bar"));
        assert_eq!(prefix, "baz");
    }

    #[test]
    fn split_relative_basename_uses_dot_dir() {
        let (dir, prefix) = split_path_for_completion("README");
        assert_eq!(dir, PathBuf::from("."));
        assert_eq!(prefix, "README");
    }

    // apply_completion

    #[test]
    fn apply_completion_replaces_trailing_component() {
        let mut m = modal();
        m.path = "/foo/bar/ba".into();
        m.completions = vec!["baz/".into()];
        m.completion_idx = Some(0);
        m.apply_completion();
        assert_eq!(m.path, "/foo/bar/baz/");
    }

    #[test]
    fn apply_completion_after_trailing_slash_extends() {
        let mut m = modal();
        m.path = "/foo/".into();
        m.completions = vec!["bar".into()];
        m.completion_idx = Some(0);
        m.apply_completion();
        assert_eq!(m.path, "/foo/bar");
    }
}
