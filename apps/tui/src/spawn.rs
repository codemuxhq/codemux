//! Spawn-agent UI: minibuffer-style bottom prompt with structured host/path
//! zones.
//!
//! Layout when active (full-width, bottom of screen):
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────┐
//! │ ▸ /home/user/workbench/repositories/codemux              │  ← wildmenu
//! │   /home/user/workbench/repositories/codemux/apps/        │     for the
//! │   /home/user/workbench/repositories/codemux/crates/      │     focused
//! │ ──────────────────────────────────────────────────────── │     zone
//! │ spawn: @local : /home/user/workbench/repositories/co█    │  ← prompt
//! └──────────────────────────────────────────────────────────┘
//! ```
//!
//! Two zones in the prompt:
//! - **path** (default focus) — live `read_dir` scan as you type, like shell
//!   tab completion. Wildmenu shows full paths.
//! - **host** — autocompletes against `~/.ssh/config` `Host` entries
//!   (wildcards skipped). Empty host → spawns locally.
//!
//! Zone navigation:
//! - `@` typed in the path zone jumps the cursor to the host zone.
//! - `Tab` toggles zones in either direction.
//! - `@` typed in the host zone is a literal char (`user@hostname` works).
//! - `Down` / `Up` move within the wildmenu. `Enter` spawns using the
//!   highlighted candidate's value if any, otherwise the literal text.
//! - `Esc` cancels.
//!
//! ## Future variants
//!
//! When a second spawn-UI variant earns its keep, convert `SpawnMinibuffer`
//! into an enum dispatcher; the compiler will guide every call site. Do NOT
//! introduce a `SpawnUi` trait until there is a real second consumer of the
//! abstraction (e.g. the future phone-view per AD-23, or third-party plugins).
//!
//! Per the architecture-guide review (NLM 2026-04-24):
//! *"the last acceptable moment to introduce the abstraction is the day you
//! actually decide to build the second variant."* Premature traits are the
//! Needless Complexity smell; premature `Box<dyn Trait>` is on the Rust Cheat
//! Sheet's "avoid" list. Stick to the concrete struct until evidence forces
//! otherwise. If the day comes, watch out for `large_enum_variant` clippy
//! lint and `Box` the heavier variant if needed.

use std::path::{Path, PathBuf};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use crate::keymap::{ModalAction, ModalBindings};

const WILDMENU_ROWS: u16 = 4;
const STRIP_ROWS: u16 = WILDMENU_ROWS + 1;
const MAX_COMPLETIONS: usize = 8;
/// Cap for the synchronous `read_dir` scan that runs on every keystroke in
/// the path zone. Without this guard, landing the prompt in a huge directory
/// (`/usr/lib`, `node_modules`, mailbox) would block the render loop.
const MAX_SCAN_ENTRIES: usize = 1024;
const HOST_PLACEHOLDER: &str = "local";

/// What the spawn UI tells the event loop after handling a key.
#[derive(Debug, Eq, PartialEq)]
pub enum ModalOutcome {
    None,
    Cancel,
    Spawn { host: String, path: String },
}

/// Which zone of the structured prompt is currently accepting input.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Zone {
    #[default]
    Path,
    Host,
}

#[derive(Debug)]
pub struct SpawnMinibuffer {
    host: String,
    path: String,
    focused: Zone,
    /// SSH `Host` entries from `~/.ssh/config`. Cached at `open()` since the
    /// config file does not change mid-session. Empty if the file is missing.
    ssh_hosts: Vec<String>,
    /// Filtered candidates for whichever zone is focused. Refreshed on every
    /// keystroke and on every zone toggle.
    filtered: Vec<String>,
    selected: Option<usize>,
}

impl SpawnMinibuffer {
    pub fn open() -> Self {
        let cwd = std::env::current_dir()
            .ok()
            .and_then(|p| p.to_str().map(String::from))
            .unwrap_or_default();
        let mut m = Self {
            host: String::new(),
            path: cwd,
            focused: Zone::Path,
            ssh_hosts: load_ssh_hosts(),
            filtered: Vec::new(),
            selected: None,
        };
        m.refresh();
        m
    }

    pub fn handle(&mut self, key: &KeyEvent, bindings: &ModalBindings) -> ModalOutcome {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return ModalOutcome::None;
        }

        // SwapToHost (`@`) is dual-purpose: an action in the path zone
        // (jump to host) but a literal char in the host zone (so
        // `user@hostname` works). Resolve that up front so the action
        // match below never has to "fall through" — the original implicit
        // fall-through pattern was flagged in the architecture-guide
        // review (NLM 2026-04-24) as not idiomatic Rust.
        let action = bindings.lookup(key).and_then(|a| match a {
            ModalAction::SwapToHost if self.focused == Zone::Host => None,
            other => Some(other),
        });

        if let Some(action) = action {
            return match action {
                ModalAction::Cancel => ModalOutcome::Cancel,
                ModalAction::Confirm => self.confirm(),
                ModalAction::SwapField => {
                    self.toggle_zone();
                    ModalOutcome::None
                }
                ModalAction::SwapToHost => {
                    self.enter_host_zone();
                    ModalOutcome::None
                }
                ModalAction::NextCompletion => {
                    self.move_selection_forward();
                    ModalOutcome::None
                }
                ModalAction::PrevCompletion => {
                    self.move_selection_backward();
                    ModalOutcome::None
                }
            };
        }

        match key.code {
            KeyCode::Char(c) => {
                self.current_field_mut().push(c);
                self.refresh();
                ModalOutcome::None
            }
            KeyCode::Backspace => {
                self.current_field_mut().pop();
                self.refresh();
                ModalOutcome::None
            }
            _ => ModalOutcome::None,
        }
    }

    fn confirm(&self) -> ModalOutcome {
        // Apply highlighted wildmenu candidate to the focused field if any —
        // this lets the user arrow-down + Enter without an extra Tab step.
        let (host, path) = self.resolved_values();
        let host = if host.trim().is_empty() {
            "local".into()
        } else {
            host.trim().to_string()
        };
        let path = if path.trim().is_empty() {
            std::env::current_dir()
                .ok()
                .and_then(|p| p.to_str().map(String::from))
                .unwrap_or_default()
        } else {
            path.trim().to_string()
        };
        ModalOutcome::Spawn { host, path }
    }

    /// Resolve `(host, path)` honoring the highlighted wildmenu item for the
    /// focused zone. Used both by `confirm` and by tests.
    fn resolved_values(&self) -> (String, String) {
        let chosen = self.selected.and_then(|i| self.filtered.get(i)).cloned();
        match (self.focused, chosen) {
            (Zone::Host, Some(h)) => (h, self.path.clone()),
            (Zone::Path, Some(p)) => (self.host.clone(), p),
            (_, None) => (self.host.clone(), self.path.clone()),
        }
    }

    fn toggle_zone(&mut self) {
        match self.focused {
            Zone::Path => self.enter_host_zone(),
            Zone::Host => {
                self.focused = Zone::Path;
                self.refresh();
            }
        }
    }

    /// Enter the host zone, pre-filling `host` with the first SSH config
    /// entry if it is currently empty. Mirrors how the path zone is
    /// pre-filled with cwd at modal open — without this, `@` would land
    /// the cursor on a placeholder string ("local") that can't be edited
    /// with backspace, and the user would just see typing replace the
    /// placeholder wholesale on the first keystroke.
    ///
    /// Architecture note: the architecture-guide review (NLM 2026-04-24)
    /// flagged this mutation as a Fragility smell because focus toggling
    /// silently changes business state. The smell is real in general, but
    /// this is the *exact* behavior the user requested for autocomplete
    /// parity with the path zone — the pre-fill IS the UX. If you ever
    /// want to revisit, the alternative is a "ghost suggestion" that's
    /// rendered inline but lives outside `self.host` (e.g. fish-shell's
    /// autosuggest) — accepted via a dedicated key. Significantly more
    /// code; not worth it for a single-user tool.
    ///
    /// User who wants to spawn local: backspace it out (host falls back
    /// to "local" on confirm) or hit Esc and skip the `@` jump entirely.
    fn enter_host_zone(&mut self) {
        if self.host.is_empty() && !self.ssh_hosts.is_empty() {
            self.host.clone_from(&self.ssh_hosts[0]);
        }
        self.focused = Zone::Host;
        self.refresh();
    }

    fn current_field_mut(&mut self) -> &mut String {
        match self.focused {
            Zone::Host => &mut self.host,
            Zone::Path => &mut self.path,
        }
    }

    fn move_selection_forward(&mut self) {
        if self.filtered.is_empty() {
            self.selected = None;
            return;
        }
        let len = self.filtered.len();
        self.selected = Some(match self.selected {
            None => 0,
            Some(i) => (i + 1) % len,
        });
    }

    fn move_selection_backward(&mut self) {
        if self.filtered.is_empty() {
            self.selected = None;
            return;
        }
        let len = self.filtered.len();
        self.selected = Some(match self.selected {
            None | Some(0) => len - 1,
            Some(i) => i - 1,
        });
    }

    fn refresh(&mut self) {
        self.filtered = match self.focused {
            Zone::Path => path_completions(&self.path),
            Zone::Host => host_completions(&self.host, &self.ssh_hosts),
        };
        self.selected = if self.filtered.is_empty() {
            None
        } else {
            Some(0)
        };
    }

    /// Render the minibuffer at the bottom of `area`. The widget
    /// self-positions: it carves `STRIP_ROWS` off the bottom and renders
    /// over whatever the parent has already drawn there (typically the
    /// running claude PTY).
    ///
    /// Note: the architecture-guide review (NLM 2026-04-24) flagged this
    /// self-positioning as an Encapsulation smell ("the widget is aware of
    /// its layout context"). The general advice — caller passes a `Rect`
    /// — is right for compositional layouts but wrong for overlays like
    /// this one: the spawn UI is *defined* by where it appears relative
    /// to the active screen, the same way the help and switcher popups in
    /// `runtime.rs` are. Inverting the control would force the runtime to
    /// know each widget's layout grammar. Trade-off accepted.
    pub fn render(&self, frame: &mut Frame<'_>, area: Rect, bindings: &ModalBindings) {
        if area.height < STRIP_ROWS + 4 {
            self.render_fallback_popup(frame, area, bindings);
            return;
        }

        let strip = Rect {
            x: area.x,
            y: area.y + area.height - STRIP_ROWS,
            width: area.width,
            height: STRIP_ROWS,
        };
        frame.render_widget(Clear, strip);

        let [wildmenu_area, prompt_area] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(WILDMENU_ROWS), Constraint::Length(1)])
            .areas(strip);

        frame.render_widget(
            self.wildmenu_view(wildmenu_area.width as usize),
            wildmenu_area,
        );
        frame.render_widget(self.prompt_view(bindings), prompt_area);
    }

    fn wildmenu_view(&self, width: usize) -> Paragraph<'_> {
        let block = Block::default().borders(Borders::TOP);
        if self.filtered.is_empty() {
            let msg = match self.focused {
                Zone::Path => " (no matches — Enter spawns at literal path)",
                Zone::Host => {
                    if self.ssh_hosts.is_empty() {
                        " (no hosts in ~/.ssh/config — type a name; SSH lands in P1.4)"
                    } else {
                        " (no matching SSH host — Enter spawns on this literal name)"
                    }
                }
            };
            return Paragraph::new(Line::styled(
                msg,
                Style::default().add_modifier(Modifier::DIM),
            ))
            .block(block);
        }

        let usable = WILDMENU_ROWS as usize - 1; // top border eats one row
        let lines: Vec<Line> = self
            .filtered
            .iter()
            .take(usable)
            .enumerate()
            .map(|(i, c)| {
                let is_selected = Some(i) == self.selected;
                let marker = if is_selected { " ▸ " } else { "   " };
                let style = if is_selected {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let display = clip_middle(c, width.saturating_sub(3));
                Line::styled(format!("{marker}{display}"), style)
            })
            .collect();
        Paragraph::new(lines).block(block)
    }

    fn prompt_view(&self, bindings: &ModalBindings) -> Paragraph<'_> {
        let label_style = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let dim = Style::default().add_modifier(Modifier::DIM);
        let separator_style = dim;

        let host_text = if self.host.is_empty() {
            HOST_PLACEHOLDER.to_string()
        } else {
            self.host.clone()
        };
        let path_text = if self.path.is_empty() {
            "<cwd>".to_string()
        } else {
            self.path.clone()
        };

        let host_zone_style = zone_style(self.focused == Zone::Host, self.host.is_empty());
        let path_zone_style = zone_style(self.focused == Zone::Path, self.path.is_empty());

        let mut spans = vec![
            Span::styled("spawn: ", label_style),
            Span::styled("@", host_marker_style(self.focused == Zone::Host)),
            Span::styled(host_text, host_zone_style),
        ];
        if self.focused == Zone::Host {
            spans.push(Span::styled(
                "█",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::SLOW_BLINK),
            ));
        }
        spans.push(Span::styled(" : ", separator_style));
        spans.push(Span::styled(path_text, path_zone_style));
        if self.focused == Zone::Path {
            spans.push(Span::styled(
                "█",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::SLOW_BLINK),
            ));
        }

        let hint = format!(
            "  [{} toggle · {} pick · {} spawn · {} cancel]",
            bindings.binding_for(ModalAction::SwapField),
            bindings.binding_for(ModalAction::NextCompletion),
            bindings.binding_for(ModalAction::Confirm),
            bindings.binding_for(ModalAction::Cancel),
        );
        spans.push(Span::styled(hint, dim));
        Paragraph::new(Line::from(spans))
    }

    /// Tiny terminal escape hatch: when the screen is too short for the
    /// minibuffer + wildmenu, fall back to a centered popup so the variant
    /// remains usable.
    fn render_fallback_popup(&self, frame: &mut Frame<'_>, area: Rect, bindings: &ModalBindings) {
        let popup = centered_rect_with_size(60, 8, area);
        frame.render_widget(Clear, popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" spawn (fallback) ");
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        let [wm, p] = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .areas(inner);
        frame.render_widget(self.wildmenu_view(wm.width as usize), wm);
        frame.render_widget(self.prompt_view(bindings), p);
    }
}

fn zone_style(focused: bool, empty: bool) -> Style {
    match (focused, empty) {
        (true, _) => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        (false, true) => Style::default().add_modifier(Modifier::DIM),
        (false, false) => Style::default(),
    }
}

fn host_marker_style(focused: bool) -> Style {
    let s = Style::default().fg(Color::Cyan);
    if focused {
        s.add_modifier(Modifier::BOLD)
    } else {
        s.add_modifier(Modifier::DIM)
    }
}

/// Path-zone completions: scan `read_dir(parent)` for entries matching the
/// trailing component of the typed path. Returns *full* paths (parent +
/// entry) so the wildmenu shows the user where the candidate actually
/// lands and so applying a candidate produces a complete path string.
///
/// Note: this is intentionally a separate function from
/// `host_completions`. Both produce a `Vec<String>` for the wildmenu, but
/// the underlying logic differs in three ways: the data source (live
/// filesystem vs in-memory list), the matching rule (prefix-on-basename
/// vs substring-fuzzy-on-full-name), and the post-processing (path
/// joining vs identity). The architecture-guide review (NLM 2026-04-24)
/// flagged this as Needless Repetition; on closer reading the only thing
/// they share is the wrapper signature, and a generic "filter" abstraction
/// would either swallow these differences or grow a configuration enum
/// that re-introduces the same branching one level up. Kept separate.
fn path_completions(input: &str) -> Vec<String> {
    let (dir, prefix) = split_path_for_completion(input);
    let entries = scan_dir(&dir, &prefix, MAX_COMPLETIONS);
    let dir_str = dir.to_string_lossy();
    entries
        .into_iter()
        .map(|e| {
            if dir_str == "." && !input.starts_with("./") {
                e
            } else if dir_str.ends_with('/') {
                format!("{dir_str}{e}")
            } else {
                format!("{dir_str}/{e}")
            }
        })
        .collect()
}

/// Split a partial path into (parent dir, basename prefix) for completion.
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

/// Scan `dir` for entries matching `prefix`, returning at most `cap` names
/// (directories with a trailing slash). Hidden entries are kept only if the
/// prefix itself starts with a dot.
///
/// I/O failures (missing directory, permission denied, non-utf8) degrade
/// silently to an empty Vec — autocomplete should never crash the TUI. A
/// `tracing::debug!` event is emitted so the operator can investigate via
/// `RUST_LOG=codemux=debug`.
fn scan_dir(dir: &Path, prefix: &str, cap: usize) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        tracing::debug!("read_dir({}) failed", dir.display());
        return Vec::new();
    };
    let mut out: Vec<String> = entries
        .filter_map(Result::ok)
        .take(MAX_SCAN_ENTRIES)
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.starts_with(prefix) {
                return None;
            }
            if name.starts_with('.') && !prefix.starts_with('.') {
                return None;
            }
            let is_dir = e.file_type().ok()?.is_dir();
            Some(if is_dir { format!("{name}/") } else { name })
        })
        .collect();
    out.sort();
    out.truncate(cap);
    out
}

/// Host-zone completions: filter the cached SSH `Host` list against the
/// typed prefix. Returns the full pool when the input is empty (so the user
/// can browse).
fn host_completions(input: &str, hosts: &[String]) -> Vec<String> {
    let needle = input.trim().to_lowercase();
    if needle.is_empty() {
        return hosts.to_vec();
    }
    let mut scored: Vec<(usize, &String)> = hosts
        .iter()
        .filter_map(|c| score(&c.to_lowercase(), &needle).map(|s| (s, c)))
        .collect();
    scored.sort_by_key(|(s, _)| *s);
    scored.into_iter().map(|(_, c)| c.clone()).collect()
}

fn score(haystack: &str, needle: &str) -> Option<usize> {
    haystack.find(needle).map(|pos| {
        let prefix_bonus = if pos == 0 { 0 } else { 100 };
        prefix_bonus + pos + haystack.len() / 8
    })
}

/// Read `~/.ssh/config` and return the list of `Host` entries with wildcards
/// (`*`, `?`, `!`) skipped. Returns an empty Vec if the file is missing or
/// unreadable; the host zone falls back to free-text input in that case.
///
/// Missing config is normal (a fresh user account has no `~/.ssh/config`),
/// so failures degrade quietly to "empty list" but emit a `tracing::debug!`
/// event for `RUST_LOG=codemux=debug` debugging.
fn load_ssh_hosts() -> Vec<String> {
    let Ok(home) = std::env::var("HOME") else {
        tracing::debug!("HOME unset; SSH host autocomplete disabled");
        return Vec::new();
    };
    let path = PathBuf::from(home).join(".ssh/config");
    let Ok(content) = std::fs::read_to_string(&path) else {
        tracing::debug!(
            "read {} failed; SSH host autocomplete disabled",
            path.display(),
        );
        return Vec::new();
    };
    parse_ssh_hosts(&content)
}

fn parse_ssh_hosts(content: &str) -> Vec<String> {
    let mut hosts: Vec<String> = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let mut parts = trimmed.splitn(2, char::is_whitespace);
        let Some(keyword) = parts.next() else {
            continue;
        };
        if !keyword.eq_ignore_ascii_case("host") {
            continue;
        }
        let Some(rest) = parts.next() else {
            continue;
        };
        for entry in rest.split_whitespace() {
            // Wildcards (`Host *`, `Host *.foo`, `Host !bar`) are too generic
            // for autocomplete — skip them rather than offering them as
            // candidates the user cannot actually SSH to.
            if entry.contains('*') || entry.contains('?') || entry.contains('!') {
                continue;
            }
            hosts.push(entry.to_string());
        }
    }
    hosts.sort();
    hosts.dedup();
    hosts
}

fn clip_middle(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let len = s.chars().count();
    if len <= width {
        return s.to_string();
    }
    let take = width.saturating_sub(1);
    let mut out = String::from("…");
    out.extend(
        s.chars()
            .rev()
            .take(take)
            .collect::<Vec<_>>()
            .into_iter()
            .rev(),
    );
    out
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

    fn b() -> ModalBindings {
        ModalBindings::default()
    }

    /// Construct a minibuffer with controlled state, bypassing `open()` so
    /// tests are deterministic regardless of the real cwd / `~/.ssh/config`.
    fn mb(host: &str, path: &str, focused: Zone, ssh_hosts: &[&str]) -> SpawnMinibuffer {
        let mut m = SpawnMinibuffer {
            host: host.into(),
            path: path.into(),
            focused,
            ssh_hosts: ssh_hosts.iter().map(|s| (*s).to_string()).collect(),
            filtered: Vec::new(),
            selected: None,
        };
        m.refresh();
        m
    }

    #[test]
    fn ctrl_modified_keys_are_dropped() {
        let mut m = mb("", "/tmp", Zone::Path, &[]);
        let outcome = m.handle(&ctrl(KeyCode::Char('b')), &b());
        assert_eq!(outcome, ModalOutcome::None);
        assert_eq!(m.path, "/tmp");
    }

    #[test]
    fn esc_returns_cancel() {
        let mut m = mb("", "/tmp", Zone::Path, &[]);
        assert_eq!(m.handle(&key(KeyCode::Esc), &b()), ModalOutcome::Cancel);
    }

    #[test]
    fn typing_a_char_appends_to_focused_zone() {
        let mut m = mb("", "/tm", Zone::Path, &[]);
        m.handle(&key(KeyCode::Char('p')), &b());
        assert_eq!(m.path, "/tmp");
        assert_eq!(m.host, "");
    }

    #[test]
    fn backspace_pops_from_focused_zone() {
        let mut m = mb("", "/tmp", Zone::Path, &[]);
        m.handle(&key(KeyCode::Backspace), &b());
        assert_eq!(m.path, "/tm");
    }

    #[test]
    fn at_in_path_zone_jumps_to_host() {
        // No SSH hosts → no prefill; host stays empty.
        let mut m = mb("", "/tmp", Zone::Path, &[]);
        m.handle(&key(KeyCode::Char('@')), &b());
        assert_eq!(m.focused, Zone::Host);
        // The `@` was consumed, not appended to either field.
        assert_eq!(m.path, "/tmp");
        assert_eq!(m.host, "");
    }

    #[test]
    fn at_prefills_host_with_first_ssh_entry() {
        let mut m = mb("", "/tmp", Zone::Path, &["alpha", "bravo"]);
        m.handle(&key(KeyCode::Char('@')), &b());
        assert_eq!(m.focused, Zone::Host);
        assert_eq!(m.host, "alpha");
        // After the prefill the wildmenu narrows to entries matching
        // "alpha" — i.e. just "alpha" itself — and the user can backspace
        // to widen it again.
        assert_eq!(m.filtered, vec!["alpha".to_string()]);
    }

    #[test]
    fn at_does_not_overwrite_existing_host() {
        let mut m = mb("custom", "/tmp", Zone::Path, &["alpha"]);
        m.handle(&key(KeyCode::Char('@')), &b());
        assert_eq!(m.focused, Zone::Host);
        assert_eq!(m.host, "custom");
    }

    #[test]
    fn tab_to_host_also_prefills() {
        // The `@`-jump and Tab-toggle share the same entry semantics; if
        // they diverged, Tab users would not get the autocomplete UX.
        let mut m = mb("", "/tmp", Zone::Path, &["devpod-1"]);
        m.handle(&key(KeyCode::Tab), &b());
        assert_eq!(m.focused, Zone::Host);
        assert_eq!(m.host, "devpod-1");
    }

    #[test]
    fn backspace_after_prefill_shrinks_host() {
        let mut m = mb("", "/tmp", Zone::Path, &["devpod-1"]);
        m.handle(&key(KeyCode::Char('@')), &b());
        assert_eq!(m.host, "devpod-1");
        m.handle(&key(KeyCode::Backspace), &b());
        assert_eq!(m.host, "devpod-");
        m.handle(&key(KeyCode::Backspace), &b());
        assert_eq!(m.host, "devpod");
    }

    #[test]
    fn tab_back_to_path_does_not_clear_host() {
        let mut m = mb("custom", "/tmp", Zone::Host, &[]);
        m.handle(&key(KeyCode::Tab), &b());
        assert_eq!(m.focused, Zone::Path);
        assert_eq!(m.host, "custom");
    }

    #[test]
    fn at_in_host_zone_is_a_literal_char() {
        // Important for `user@hostname` SSH targets.
        let mut m = mb("", "", Zone::Host, &[]);
        m.handle(&key(KeyCode::Char('@')), &b());
        assert_eq!(m.focused, Zone::Host);
        assert_eq!(m.host, "@");
    }

    #[test]
    fn tab_toggles_zones_in_both_directions() {
        let mut m = mb("", "/tmp", Zone::Path, &[]);
        m.handle(&key(KeyCode::Tab), &b());
        assert_eq!(m.focused, Zone::Host);
        m.handle(&key(KeyCode::Tab), &b());
        assert_eq!(m.focused, Zone::Path);
    }

    #[test]
    fn empty_host_becomes_local_on_spawn() {
        let mut m = mb("", "/x", Zone::Path, &[]);
        // No wildmenu match → resolved values are the literal fields.
        m.filtered = vec![];
        m.selected = None;
        let outcome = m.handle(&key(KeyCode::Enter), &b());
        assert_eq!(
            outcome,
            ModalOutcome::Spawn {
                host: "local".into(),
                path: "/x".into()
            },
        );
    }

    #[test]
    fn enter_uses_highlighted_path_candidate() {
        let mut m = mb("", "/tmp", Zone::Path, &[]);
        m.filtered = vec!["/tmp/alpha".into(), "/tmp/beta".into()];
        m.selected = Some(1);
        let outcome = m.handle(&key(KeyCode::Enter), &b());
        assert_eq!(
            outcome,
            ModalOutcome::Spawn {
                host: "local".into(),
                path: "/tmp/beta".into()
            },
        );
    }

    #[test]
    fn enter_uses_highlighted_host_candidate() {
        let mut m = mb("dev", "/work", Zone::Host, &["devpod-1", "devpod-2"]);
        // refresh() filtered "dev" against the seed.
        m.selected = Some(1);
        let outcome = m.handle(&key(KeyCode::Enter), &b());
        assert_eq!(
            outcome,
            ModalOutcome::Spawn {
                host: "devpod-2".into(),
                path: "/work".into()
            },
        );
    }

    #[test]
    fn host_field_with_text_overrides_local_default() {
        let mut m = mb("custom-host", "/work", Zone::Path, &[]);
        m.filtered = vec![];
        m.selected = None;
        let outcome = m.handle(&key(KeyCode::Enter), &b());
        assert_eq!(
            outcome,
            ModalOutcome::Spawn {
                host: "custom-host".into(),
                path: "/work".into()
            },
        );
    }

    #[test]
    fn down_cycles_with_wrap() {
        let mut m = mb("", "", Zone::Host, &["a", "b", "c"]);
        // refresh() seeds selected to Some(0).
        m.handle(&key(KeyCode::Down), &b());
        assert_eq!(m.selected, Some(1));
        m.handle(&key(KeyCode::Down), &b());
        m.handle(&key(KeyCode::Down), &b());
        assert_eq!(m.selected, Some(0));
    }

    #[test]
    fn toggling_zone_refreshes_wildmenu_to_new_pool() {
        let mut m = mb("dev", "/tmp", Zone::Path, &["devpod-1", "devpod-2"]);
        // Path zone: filtered is path-derived (probably non-empty if /tmp exists,
        // empty otherwise — ignore here).
        m.handle(&key(KeyCode::Tab), &b());
        assert_eq!(m.focused, Zone::Host);
        // Filtered now reflects the host pool, narrowed by "dev".
        assert_eq!(
            m.filtered,
            vec!["devpod-1".to_string(), "devpod-2".to_string()]
        );
    }

    #[test]
    fn parse_ssh_hosts_returns_empty_for_empty_file() {
        assert!(parse_ssh_hosts("").is_empty());
    }

    #[test]
    fn parse_ssh_hosts_returns_a_single_named_host() {
        assert_eq!(parse_ssh_hosts("Host foo"), vec!["foo".to_string()]);
    }

    #[test]
    fn parse_ssh_hosts_returns_multiple_hosts_per_line() {
        let mut got = parse_ssh_hosts("Host alpha bravo charlie");
        got.sort();
        assert_eq!(got, vec!["alpha", "bravo", "charlie"]);
    }

    #[test]
    fn parse_ssh_hosts_skips_wildcards() {
        let got = parse_ssh_hosts(
            "Host *\nHost *.uber.com\nHost real-host\nHost !excluded\nHost q?stion",
        );
        assert_eq!(got, vec!["real-host".to_string()]);
    }

    #[test]
    fn parse_ssh_hosts_skips_comments_and_blank_lines() {
        let got = parse_ssh_hosts("# comment\n\n  Host  foo  \n");
        assert_eq!(got, vec!["foo".to_string()]);
    }

    #[test]
    fn parse_ssh_hosts_is_case_insensitive_on_keyword() {
        // `host`, `Host`, `HOST` all valid per the SSH config grammar.
        assert_eq!(parse_ssh_hosts("host foo"), vec!["foo".to_string()]);
        assert_eq!(parse_ssh_hosts("HOST bar"), vec!["bar".to_string()]);
    }

    #[test]
    fn parse_ssh_hosts_ignores_other_directives() {
        let got = parse_ssh_hosts(
            "User daniel\nHostName example.com\nHost actual\nIdentityFile ~/.ssh/id_rsa",
        );
        assert_eq!(got, vec!["actual".to_string()]);
    }

    #[test]
    fn parse_ssh_hosts_dedups() {
        let got = parse_ssh_hosts("Host foo\nHost foo\nHost bar");
        assert_eq!(got, vec!["bar".to_string(), "foo".to_string()]);
    }

    #[test]
    fn host_completions_returns_full_pool_for_empty_input() {
        let pool = vec!["a".into(), "b".into()];
        assert_eq!(host_completions("", &pool), pool);
    }

    #[test]
    fn host_completions_filters_by_substring() {
        let pool = vec!["devpod-1".into(), "devpod-2".into(), "laptop".into()];
        assert_eq!(
            host_completions("dev", &pool),
            vec!["devpod-1".to_string(), "devpod-2".to_string()],
        );
    }

    #[test]
    fn host_completions_prefers_prefix_matches() {
        let pool = vec!["xxprodxx".into(), "prod-east".into()];
        let got = host_completions("prod", &pool);
        assert_eq!(got[0], "prod-east");
    }

    #[test]
    fn split_empty_path_uses_dot_and_empty_prefix() {
        let (dir, prefix) = split_path_for_completion("");
        assert_eq!(dir, PathBuf::from("."));
        assert_eq!(prefix, "");
    }

    #[test]
    fn split_trailing_slash_keeps_full_path() {
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
    fn clip_middle_passes_short_strings_through() {
        assert_eq!(clip_middle("hello", 10), "hello");
    }

    #[test]
    fn clip_middle_keeps_tail_with_ellipsis_when_too_long() {
        let out = clip_middle("/very/long/path/foo", 10);
        assert!(out.starts_with('…'));
        assert_eq!(out.chars().count(), 10);
        assert!(out.ends_with("foo"));
    }

    #[test]
    fn clip_middle_returns_empty_when_width_is_zero() {
        assert_eq!(clip_middle("anything", 0), "");
    }

    #[test]
    fn move_selection_clears_selection_when_filtered_is_empty() {
        let mut m = mb("", "/tmp", Zone::Host, &[]);
        m.filtered.clear();
        m.selected = Some(5);
        m.move_selection_forward();
        assert_eq!(m.selected, None);
    }

    #[test]
    fn move_selection_from_none_with_backward_jumps_to_last() {
        let mut m = mb("", "", Zone::Host, &["a", "b", "c"]);
        m.selected = None;
        m.move_selection_backward();
        assert_eq!(m.selected, Some(2));
    }

    #[test]
    fn centered_rect_with_size_centers_within_parent() {
        let parent = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 50,
        };
        let r = centered_rect_with_size(40, 20, parent);
        assert_eq!(r.x, 30);
        assert_eq!(r.y, 15);
        assert_eq!(r.width, 40);
        assert_eq!(r.height, 20);
    }

    #[test]
    fn centered_rect_with_size_clamps_to_parent_when_oversized() {
        let parent = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 10,
        };
        let r = centered_rect_with_size(50, 30, parent);
        assert_eq!(r.width, 20);
        assert_eq!(r.height, 10);
    }

    #[test]
    fn zone_style_focused_overrides_empty() {
        let s = zone_style(true, true);
        assert_eq!(s.fg, Some(Color::Cyan));
    }

    #[test]
    fn zone_style_unfocused_empty_is_dim() {
        let s = zone_style(false, true);
        assert!(s.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn zone_style_unfocused_non_empty_is_default() {
        let s = zone_style(false, false);
        assert_eq!(s, Style::default());
    }
}
