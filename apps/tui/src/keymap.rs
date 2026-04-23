//! Key bindings: typed Action enums per scope, a parsed `KeyChord` type, and
//! `Bindings` POD structs that the user can override via TOML config.
//!
//! Per the architecture-guide review (P1.3 NLM session): mapping a key to an
//! action is strictly a presentation/delivery concern, so this lives in
//! `apps/tui/`. The Action enums name what each scope can do; the runtime is
//! responsible for turning a returned action into the right state mutation.
//!
//! TEA-style dispatch (per the Ratatui docs): each scope exposes a
//! `lookup(KeyEvent) -> Option<Action>` method; the handler's job is just to
//! consult the table.

use std::fmt;
use std::str::FromStr;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::de::{self, Deserializer, Visitor};
use serde::Deserialize;

// ---------- KeyChord ----------

/// A keystroke as the user writes it in config: code + modifiers, no kind/state.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct KeyChord {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl KeyChord {
    pub const fn plain(code: KeyCode) -> Self {
        Self { code, modifiers: KeyModifiers::NONE }
    }

    pub const fn ctrl(code: KeyCode) -> Self {
        Self { code, modifiers: KeyModifiers::CONTROL }
    }

    /// Compare against a `KeyEvent`. Crossterm reports SHIFT alongside an
    /// already-uppercase `Char(_)` for shifted symbols (e.g. `?` arrives as
    /// `Char('?')` + SHIFT), so we strip SHIFT from char-key comparisons to
    /// keep configs intuitive (`help = "?"` works without the user spelling
    /// out `shift+?`).
    pub fn matches(&self, event: &KeyEvent) -> bool {
        normalize(self.code, self.modifiers) == normalize(event.code, event.modifiers)
    }
}

fn normalize(code: KeyCode, modifiers: KeyModifiers) -> (KeyCode, KeyModifiers) {
    let mods = if matches!(code, KeyCode::Char(_)) {
        modifiers - KeyModifiers::SHIFT
    } else {
        modifiers
    };
    (code, mods)
}

impl fmt::Display for KeyChord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut parts: Vec<&'static str> = Vec::new();
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            parts.push("ctrl");
        }
        if self.modifiers.contains(KeyModifiers::ALT) {
            parts.push("alt");
        }
        if self.modifiers.contains(KeyModifiers::SHIFT) && !matches!(self.code, KeyCode::Char(_)) {
            parts.push("shift");
        }
        if self.modifiers.contains(KeyModifiers::SUPER) {
            parts.push("super");
        }
        let key = key_code_name(self.code);
        if parts.is_empty() {
            f.write_str(&key)
        } else {
            write!(f, "{}+{}", parts.join("+"), key)
        }
    }
}

fn key_code_name(code: KeyCode) -> String {
    match code {
        KeyCode::Enter => "enter".into(),
        KeyCode::Esc => "esc".into(),
        KeyCode::Tab => "tab".into(),
        KeyCode::BackTab => "backtab".into(),
        KeyCode::Backspace => "backspace".into(),
        KeyCode::Delete => "delete".into(),
        KeyCode::Insert => "insert".into(),
        KeyCode::Up => "up".into(),
        KeyCode::Down => "down".into(),
        KeyCode::Left => "left".into(),
        KeyCode::Right => "right".into(),
        KeyCode::Home => "home".into(),
        KeyCode::End => "end".into(),
        KeyCode::PageUp => "pageup".into(),
        KeyCode::PageDown => "pagedown".into(),
        KeyCode::Char(' ') => "space".into(),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::F(n) => format!("f{n}"),
        other => format!("{other:?}").to_lowercase(),
    }
}

impl FromStr for KeyChord {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            return Err("empty key chord".into());
        }
        let parts: Vec<&str> = trimmed.split('+').map(str::trim).collect();
        let (key_part, modifier_parts) = parts
            .split_last()
            .ok_or_else(|| "empty key chord".to_string())?;
        let mut modifiers = KeyModifiers::NONE;
        for m in modifier_parts {
            let lower = m.to_lowercase();
            modifiers |= match lower.as_str() {
                "ctrl" | "control" => KeyModifiers::CONTROL,
                "alt" | "meta" => KeyModifiers::ALT,
                "shift" => KeyModifiers::SHIFT,
                // `cmd` is the macOS-native name for the same key crossterm
                // calls SUPER. Both spellings parse to the same chord; the
                // help screen renders `super` for consistency.
                "super" | "cmd" | "win" => KeyModifiers::SUPER,
                other => return Err(format!("unknown modifier: {other}")),
            };
        }
        let code = parse_key_code(key_part)?;
        Ok(Self { code, modifiers })
    }
}

fn parse_key_code(raw: &str) -> Result<KeyCode, String> {
    let lower = raw.to_lowercase();
    match lower.as_str() {
        "enter" | "return" => Ok(KeyCode::Enter),
        "esc" | "escape" => Ok(KeyCode::Esc),
        "tab" => Ok(KeyCode::Tab),
        "backtab" => Ok(KeyCode::BackTab),
        "backspace" | "bs" => Ok(KeyCode::Backspace),
        "delete" | "del" => Ok(KeyCode::Delete),
        "insert" | "ins" => Ok(KeyCode::Insert),
        "up" => Ok(KeyCode::Up),
        "down" => Ok(KeyCode::Down),
        "left" => Ok(KeyCode::Left),
        "right" => Ok(KeyCode::Right),
        "home" => Ok(KeyCode::Home),
        "end" => Ok(KeyCode::End),
        "pageup" | "pgup" => Ok(KeyCode::PageUp),
        "pagedown" | "pgdown" | "pgdn" => Ok(KeyCode::PageDown),
        "space" => Ok(KeyCode::Char(' ')),
        s if s.starts_with('f') && s.len() > 1 => {
            let n: u8 = s[1..]
                .parse()
                .map_err(|_| format!("bad function key: {raw}"))?;
            Ok(KeyCode::F(n))
        }
        // Single character — preserve the original case so '?' vs '/' is
        // unambiguous; the chord-matcher folds SHIFT for char keys.
        _ => {
            let mut chars = raw.chars();
            let c = chars
                .next()
                .ok_or_else(|| "empty key code".to_string())?;
            if chars.next().is_some() {
                return Err(format!("unknown key code: {raw}"));
            }
            Ok(KeyCode::Char(c))
        }
    }
}

impl<'de> Deserialize<'de> for KeyChord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct V;
        impl Visitor<'_> for V {
            type Value = KeyChord;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a key chord like `ctrl+b`, `enter`, or `q`")
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<KeyChord, E> {
                value.parse().map_err(E::custom)
            }
        }
        deserializer.deserialize_str(V)
    }
}

// ---------- Action enums (per scope) ----------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrefixAction {
    Quit,
    SpawnAgent,
    FocusNext,
    FocusPrev,
    ToggleNav,
    OpenSwitcher,
    Help,
}

impl PrefixAction {
    pub const ALL: &'static [PrefixAction] = &[
        Self::Quit,
        Self::SpawnAgent,
        Self::FocusNext,
        Self::FocusPrev,
        Self::ToggleNav,
        Self::OpenSwitcher,
        Self::Help,
    ];

    pub const fn description(self) -> &'static str {
        match self {
            Self::Quit => "exit codemux",
            Self::SpawnAgent => "open the spawn modal",
            Self::FocusNext => "focus the next agent",
            Self::FocusPrev => "focus the previous agent",
            Self::ToggleNav => "toggle navigator style",
            Self::OpenSwitcher => "open the agent switcher popup",
            Self::Help => "show this help",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PopupAction {
    Confirm,
    Cancel,
    Next,
    Prev,
}

impl PopupAction {
    pub const ALL: &'static [PopupAction] =
        &[Self::Next, Self::Prev, Self::Confirm, Self::Cancel];

    pub const fn description(self) -> &'static str {
        match self {
            Self::Confirm => "focus the highlighted agent",
            Self::Cancel => "dismiss the popup",
            Self::Next => "highlight the next agent",
            Self::Prev => "highlight the previous agent",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModalAction {
    Confirm,
    Cancel,
    SwapField,
    SwapToHost,
    NextCompletion,
    PrevCompletion,
}

impl ModalAction {
    pub const ALL: &'static [ModalAction] = &[
        Self::Confirm,
        Self::Cancel,
        Self::SwapField,
        Self::SwapToHost,
        Self::NextCompletion,
        Self::PrevCompletion,
    ];

    pub const fn description(self) -> &'static str {
        match self {
            Self::Confirm => "spawn the agent",
            Self::Cancel => "close the modal",
            Self::SwapField => "swap focused field, or apply highlighted completion",
            Self::SwapToHost => "(in path field) jump to host field",
            Self::NextCompletion => "highlight next path completion",
            Self::PrevCompletion => "highlight previous path completion",
        }
    }
}

// ---------- Bindings (POD, deserialized from TOML) ----------

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct PrefixBindings {
    pub quit: KeyChord,
    pub spawn_agent: KeyChord,
    pub focus_next: KeyChord,
    pub focus_prev: KeyChord,
    pub toggle_nav: KeyChord,
    pub open_switcher: KeyChord,
    pub help: KeyChord,
}

impl Default for PrefixBindings {
    fn default() -> Self {
        Self {
            quit: KeyChord::plain(KeyCode::Char('q')),
            spawn_agent: KeyChord::plain(KeyCode::Char('c')),
            focus_next: KeyChord::plain(KeyCode::Char('n')),
            focus_prev: KeyChord::plain(KeyCode::Char('p')),
            toggle_nav: KeyChord::plain(KeyCode::Char('v')),
            open_switcher: KeyChord::plain(KeyCode::Char('w')),
            help: KeyChord::plain(KeyCode::Char('?')),
        }
    }
}

impl PrefixBindings {
    pub fn lookup(&self, key: &KeyEvent) -> Option<PrefixAction> {
        let table: [(KeyChord, PrefixAction); 7] = [
            (self.quit, PrefixAction::Quit),
            (self.spawn_agent, PrefixAction::SpawnAgent),
            (self.focus_next, PrefixAction::FocusNext),
            (self.focus_prev, PrefixAction::FocusPrev),
            (self.toggle_nav, PrefixAction::ToggleNav),
            (self.open_switcher, PrefixAction::OpenSwitcher),
            (self.help, PrefixAction::Help),
        ];
        table
            .iter()
            .find(|(c, _)| c.matches(key))
            .map(|(_, a)| *a)
    }

    pub fn binding_for(&self, action: PrefixAction) -> KeyChord {
        match action {
            PrefixAction::Quit => self.quit,
            PrefixAction::SpawnAgent => self.spawn_agent,
            PrefixAction::FocusNext => self.focus_next,
            PrefixAction::FocusPrev => self.focus_prev,
            PrefixAction::ToggleNav => self.toggle_nav,
            PrefixAction::OpenSwitcher => self.open_switcher,
            PrefixAction::Help => self.help,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct PopupBindings {
    pub confirm: KeyChord,
    pub cancel: KeyChord,
    pub next: KeyChord,
    pub prev: KeyChord,
}

impl Default for PopupBindings {
    fn default() -> Self {
        Self {
            confirm: KeyChord::plain(KeyCode::Enter),
            cancel: KeyChord::plain(KeyCode::Esc),
            next: KeyChord::plain(KeyCode::Down),
            prev: KeyChord::plain(KeyCode::Up),
        }
    }
}

impl PopupBindings {
    pub fn lookup(&self, key: &KeyEvent) -> Option<PopupAction> {
        let table: [(KeyChord, PopupAction); 4] = [
            (self.next, PopupAction::Next),
            (self.prev, PopupAction::Prev),
            (self.confirm, PopupAction::Confirm),
            (self.cancel, PopupAction::Cancel),
        ];
        table
            .iter()
            .find(|(c, _)| c.matches(key))
            .map(|(_, a)| *a)
    }

    pub fn binding_for(&self, action: PopupAction) -> KeyChord {
        match action {
            PopupAction::Confirm => self.confirm,
            PopupAction::Cancel => self.cancel,
            PopupAction::Next => self.next,
            PopupAction::Prev => self.prev,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct ModalBindings {
    pub confirm: KeyChord,
    pub cancel: KeyChord,
    pub swap_field: KeyChord,
    pub swap_to_host: KeyChord,
    pub next_completion: KeyChord,
    pub prev_completion: KeyChord,
}

impl Default for ModalBindings {
    fn default() -> Self {
        Self {
            confirm: KeyChord::plain(KeyCode::Enter),
            cancel: KeyChord::plain(KeyCode::Esc),
            swap_field: KeyChord::plain(KeyCode::Tab),
            swap_to_host: KeyChord::plain(KeyCode::Char('@')),
            next_completion: KeyChord::plain(KeyCode::Down),
            prev_completion: KeyChord::plain(KeyCode::Up),
        }
    }
}

impl ModalBindings {
    pub fn lookup(&self, key: &KeyEvent) -> Option<ModalAction> {
        let table: [(KeyChord, ModalAction); 6] = [
            (self.confirm, ModalAction::Confirm),
            (self.cancel, ModalAction::Cancel),
            (self.swap_field, ModalAction::SwapField),
            (self.swap_to_host, ModalAction::SwapToHost),
            (self.next_completion, ModalAction::NextCompletion),
            (self.prev_completion, ModalAction::PrevCompletion),
        ];
        table
            .iter()
            .find(|(c, _)| c.matches(key))
            .map(|(_, a)| *a)
    }

    pub fn binding_for(&self, action: ModalAction) -> KeyChord {
        match action {
            ModalAction::Confirm => self.confirm,
            ModalAction::Cancel => self.cancel,
            ModalAction::SwapField => self.swap_field,
            ModalAction::SwapToHost => self.swap_to_host,
            ModalAction::NextCompletion => self.next_completion,
            ModalAction::PrevCompletion => self.prev_completion,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct Bindings {
    pub prefix: KeyChord,
    pub on_prefix: PrefixBindings,
    pub on_popup: PopupBindings,
    pub on_modal: ModalBindings,
}

impl Default for Bindings {
    fn default() -> Self {
        Self {
            prefix: KeyChord::ctrl(KeyCode::Char('b')),
            on_prefix: PrefixBindings::default(),
            on_popup: PopupBindings::default(),
            on_modal: ModalBindings::default(),
        }
    }
}

impl Bindings {
    /// True if any loaded chord (prefix or any per-scope action) requires the
    /// SUPER modifier. The runtime uses this to decide whether to negotiate
    /// the Kitty Keyboard Protocol with the terminal — without it, terminals
    /// usually swallow Cmd / Super before the application can see it.
    ///
    /// We only check what the user actually bound. Defaults never use SUPER,
    /// so a user who never touches the config never pays the protocol cost.
    pub fn uses_super_modifier(&self) -> bool {
        let chords = [
            self.prefix,
            self.on_prefix.quit,
            self.on_prefix.spawn_agent,
            self.on_prefix.focus_next,
            self.on_prefix.focus_prev,
            self.on_prefix.toggle_nav,
            self.on_prefix.open_switcher,
            self.on_prefix.help,
            self.on_popup.confirm,
            self.on_popup.cancel,
            self.on_popup.next,
            self.on_popup.prev,
            self.on_modal.confirm,
            self.on_modal.cancel,
            self.on_modal.swap_field,
            self.on_modal.swap_to_host,
            self.on_modal.next_completion,
            self.on_modal.prev_completion,
        ];
        chords
            .iter()
            .any(|c| c.modifiers.contains(KeyModifiers::SUPER))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn ev(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    // --- KeyChord parser ---

    #[test]
    fn parse_plain_char() {
        assert_eq!(
            "q".parse::<KeyChord>().unwrap(),
            KeyChord::plain(KeyCode::Char('q')),
        );
    }

    #[test]
    fn parse_ctrl_char() {
        assert_eq!(
            "ctrl+b".parse::<KeyChord>().unwrap(),
            KeyChord::ctrl(KeyCode::Char('b')),
        );
    }

    #[test]
    fn parse_named_key() {
        assert_eq!(
            "enter".parse::<KeyChord>().unwrap(),
            KeyChord::plain(KeyCode::Enter),
        );
        assert_eq!(
            "esc".parse::<KeyChord>().unwrap(),
            KeyChord::plain(KeyCode::Esc),
        );
        assert_eq!(
            "pgup".parse::<KeyChord>().unwrap(),
            KeyChord::plain(KeyCode::PageUp),
        );
    }

    #[test]
    fn parse_function_key() {
        assert_eq!(
            "f12".parse::<KeyChord>().unwrap(),
            KeyChord::plain(KeyCode::F(12)),
        );
    }

    #[test]
    fn parse_multi_modifier() {
        let chord: KeyChord = "ctrl+alt+x".parse().unwrap();
        assert_eq!(chord.code, KeyCode::Char('x'));
        assert!(chord.modifiers.contains(KeyModifiers::CONTROL));
        assert!(chord.modifiers.contains(KeyModifiers::ALT));
    }

    #[test]
    fn parse_cmd_aliases_to_super() {
        let chord: KeyChord = "cmd+b".parse().unwrap();
        assert_eq!(chord.code, KeyCode::Char('b'));
        assert!(chord.modifiers.contains(KeyModifiers::SUPER));
        // `super` and `win` are the other two spellings of the same modifier.
        assert_eq!(chord, "super+b".parse::<KeyChord>().unwrap());
        assert_eq!(chord, "win+b".parse::<KeyChord>().unwrap());
    }

    #[test]
    fn parse_unknown_modifier_errors() {
        assert!("foo+x".parse::<KeyChord>().is_err());
    }

    #[test]
    fn parse_empty_errors() {
        assert!("".parse::<KeyChord>().is_err());
    }

    #[test]
    fn parse_question_mark() {
        // '?' should be reachable as a literal symbol; user does not need to
        // know it is shift+/ underneath.
        assert_eq!(
            "?".parse::<KeyChord>().unwrap(),
            KeyChord::plain(KeyCode::Char('?')),
        );
    }

    // --- KeyChord display roundtrip ---

    #[test]
    fn display_roundtrips_for_common_chords() {
        for raw in [
            "q", "ctrl+b", "ctrl+alt+x", "enter", "esc", "tab", "f5", "?",
            "up", "down", "space",
        ] {
            let chord: KeyChord = raw.parse().unwrap();
            let rendered = chord.to_string();
            let reparsed: KeyChord = rendered.parse().unwrap();
            assert_eq!(chord, reparsed, "roundtrip failed for {raw}");
        }
    }

    // --- KeyChord matching ---

    #[test]
    fn matches_strict_modifiers() {
        let chord = KeyChord::ctrl(KeyCode::Char('b'));
        assert!(chord.matches(&ev(KeyCode::Char('b'), KeyModifiers::CONTROL)));
        assert!(!chord.matches(&ev(KeyCode::Char('b'), KeyModifiers::NONE)));
        assert!(!chord.matches(&ev(KeyCode::Char('c'), KeyModifiers::CONTROL)));
    }

    #[test]
    fn matches_folds_shift_on_char_keys() {
        // Crossterm reports `?` as Char('?') with SHIFT on most platforms.
        // A user-written chord `?` (no modifiers) should still match it.
        let chord = KeyChord::plain(KeyCode::Char('?'));
        assert!(chord.matches(&ev(KeyCode::Char('?'), KeyModifiers::SHIFT)));
        assert!(chord.matches(&ev(KeyCode::Char('?'), KeyModifiers::NONE)));
    }

    #[test]
    fn matches_does_not_fold_shift_on_named_keys() {
        // shift+enter is a real distinct chord from enter.
        let chord = KeyChord::plain(KeyCode::Enter);
        assert!(chord.matches(&ev(KeyCode::Enter, KeyModifiers::NONE)));
        assert!(!chord.matches(&ev(KeyCode::Enter, KeyModifiers::SHIFT)));
    }

    // --- TOML deserialization of Bindings ---

    #[test]
    fn missing_config_returns_default_bindings() {
        let bindings: Bindings = toml::from_str("").unwrap();
        let defaults = Bindings::default();
        assert_eq!(bindings.prefix, defaults.prefix);
        assert_eq!(bindings.on_prefix.quit, defaults.on_prefix.quit);
    }

    #[test]
    fn user_can_override_just_the_prefix() {
        let bindings: Bindings = toml::from_str(r#"prefix = "ctrl+a""#).unwrap();
        assert_eq!(bindings.prefix, KeyChord::ctrl(KeyCode::Char('a')));
        // Per-action defaults remain.
        assert_eq!(bindings.on_prefix.quit, KeyChord::plain(KeyCode::Char('q')));
    }

    #[test]
    fn user_can_override_one_action() {
        let toml_text = r#"
            [on_prefix]
            quit = "x"
        "#;
        let bindings: Bindings = toml::from_str(toml_text).unwrap();
        assert_eq!(bindings.on_prefix.quit, KeyChord::plain(KeyCode::Char('x')));
        // Other actions still default.
        assert_eq!(
            bindings.on_prefix.spawn_agent,
            KeyChord::plain(KeyCode::Char('c')),
        );
    }

    #[test]
    fn invalid_chord_in_config_is_an_error() {
        let toml_text = r#"
            [on_prefix]
            quit = "ctrl+nonsense"
        "#;
        assert!(toml::from_str::<Bindings>(toml_text).is_err());
    }

    // --- Lookup ---

    #[test]
    fn prefix_lookup_finds_default_bindings() {
        let b = PrefixBindings::default();
        assert_eq!(
            b.lookup(&ev(KeyCode::Char('q'), KeyModifiers::NONE)),
            Some(PrefixAction::Quit),
        );
        assert_eq!(
            b.lookup(&ev(KeyCode::Char('?'), KeyModifiers::SHIFT)),
            Some(PrefixAction::Help),
        );
    }

    #[test]
    fn prefix_lookup_returns_none_for_unbound_key() {
        let b = PrefixBindings::default();
        assert_eq!(b.lookup(&ev(KeyCode::Char('z'), KeyModifiers::NONE)), None);
    }

    #[test]
    fn popup_lookup_round_trip() {
        let b = PopupBindings::default();
        for action in PopupAction::ALL {
            let chord = b.binding_for(*action);
            let event = KeyEvent::new(chord.code, chord.modifiers);
            assert_eq!(b.lookup(&event), Some(*action));
        }
    }

    #[test]
    fn modal_lookup_round_trip() {
        let b = ModalBindings::default();
        for action in ModalAction::ALL {
            let chord = b.binding_for(*action);
            let event = KeyEvent::new(chord.code, chord.modifiers);
            assert_eq!(b.lookup(&event), Some(*action));
        }
    }

    // --- uses_super_modifier ---

    #[test]
    fn defaults_do_not_use_super_modifier() {
        assert!(!Bindings::default().uses_super_modifier());
    }

    #[test]
    fn cmd_prefix_in_config_triggers_super_detection() {
        let toml_text = r#"prefix = "cmd+b""#;
        let bindings: Bindings = toml::from_str(toml_text).unwrap();
        assert!(bindings.uses_super_modifier());
    }

    #[test]
    fn cmd_action_in_config_triggers_super_detection() {
        let toml_text = r#"
            [on_prefix]
            quit = "cmd+q"
        "#;
        let bindings: Bindings = toml::from_str(toml_text).unwrap();
        assert!(bindings.uses_super_modifier());
    }

    #[test]
    fn ctrl_only_overrides_do_not_trigger_super_detection() {
        let toml_text = r#"
            prefix = "ctrl+a"
            [on_prefix]
            quit = "x"
        "#;
        let bindings: Bindings = toml::from_str(toml_text).unwrap();
        assert!(!bindings.uses_super_modifier());
    }
}
