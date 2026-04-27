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
use serde::Deserialize;
use serde::de::{self, Deserializer, Visitor};

// ---------- KeyChord ----------

/// A keystroke as the user writes it in config: code + modifiers, no kind/state.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct KeyChord {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl KeyChord {
    pub const fn plain(code: KeyCode) -> Self {
        Self {
            code,
            modifiers: KeyModifiers::NONE,
        }
    }

    pub const fn ctrl(code: KeyCode) -> Self {
        Self {
            code,
            modifiers: KeyModifiers::CONTROL,
        }
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
            let c = chars.next().ok_or_else(|| "empty key code".to_string())?;
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

/// Deserialize a list of chords from TOML accepting either a single
/// string (`focus_next = "n"`) or an array (`focus_next = ["n", "l",
/// "j"]`). Used for the focus actions where vim (`h`/`l`) and
/// tmux-style (`n`/`p`) aliases to the same action are genuinely
/// useful; every other binding stays single-chord. Keeping
/// single-string syntax working means existing configs are forward-
/// compatible — no migration step.
fn deserialize_chord_list<'de, D>(d: D) -> Result<Vec<KeyChord>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum SingleOrList {
        Single(String),
        List(Vec<String>),
    }
    match SingleOrList::deserialize(d)? {
        SingleOrList::Single(s) => s.parse().map(|c| vec![c]).map_err(de::Error::custom),
        SingleOrList::List(v) => v
            .into_iter()
            .map(|s| s.parse().map_err(de::Error::custom))
            .collect(),
    }
}

// ---------- Action enums (per scope) ----------

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrefixAction {
    Quit,
    SpawnAgent,
    FocusNext,
    FocusPrev,
    FocusLast,
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
        Self::FocusLast,
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
            Self::FocusLast => "bounce to the previously-focused agent",
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
    pub const ALL: &'static [PopupAction] = &[Self::Next, Self::Prev, Self::Confirm, Self::Cancel];

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

/// Actions reachable while the focused agent is scrolled back through
/// its PTY history (i.e. `vt100::Screen::scrollback() > 0`). Bindings
/// in this scope are *only* consulted when scroll mode is active; on
/// the live view, the same chords are forwarded to the agent normally.
/// `ExitScroll` snaps to the bottom; any non-scroll keystroke also
/// snaps to the bottom and then forwards through (non-sticky), so the
/// user never gets trapped in scroll mode by typing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScrollAction {
    LineUp,
    LineDown,
    PageUp,
    PageDown,
    Top,
    Bottom,
    ExitScroll,
}

impl ScrollAction {
    pub const ALL: &'static [ScrollAction] = &[
        Self::LineUp,
        Self::LineDown,
        Self::PageUp,
        Self::PageDown,
        Self::Top,
        Self::Bottom,
        Self::ExitScroll,
    ];

    pub const fn description(self) -> &'static str {
        match self {
            Self::LineUp => "scroll up one line",
            Self::LineDown => "scroll down one line",
            Self::PageUp => "scroll up one page",
            Self::PageDown => "scroll down one page",
            Self::Top => "jump to the top of scrollback",
            Self::Bottom => "snap to the live view",
            Self::ExitScroll => "exit scroll mode (snap to live view)",
        }
    }
}

// ---------- Bindings (POD, deserialized from TOML) ----------

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct PrefixBindings {
    pub quit: KeyChord,
    pub spawn_agent: KeyChord,
    /// Multi-chord: defaults map both `n` (tmux) and `l`/`j` (vim) to
    /// "next agent" so neither muscle memory has to fight the other.
    /// Single string in TOML still parses as a one-element list.
    #[serde(deserialize_with = "deserialize_chord_list")]
    pub focus_next: Vec<KeyChord>,
    /// Multi-chord; mirrors `focus_next` with `p`, `h`, `k`.
    #[serde(deserialize_with = "deserialize_chord_list")]
    pub focus_prev: Vec<KeyChord>,
    /// Multi-chord; default is just `Tab` (the canonical alt-tab move
    /// in tmux/zellij). Multi-chord support is here for symmetry with
    /// the other focus actions, not because the default needs aliases.
    #[serde(deserialize_with = "deserialize_chord_list")]
    pub focus_last: Vec<KeyChord>,
    pub toggle_nav: KeyChord,
    pub open_switcher: KeyChord,
    pub help: KeyChord,
}

impl Default for PrefixBindings {
    fn default() -> Self {
        Self {
            quit: KeyChord::plain(KeyCode::Char('q')),
            spawn_agent: KeyChord::plain(KeyCode::Char('c')),
            focus_next: vec![
                // tmux convention
                KeyChord::plain(KeyCode::Char('n')),
                // vim horizontal motion
                KeyChord::plain(KeyCode::Char('l')),
                // vim vertical motion (down)
                KeyChord::plain(KeyCode::Char('j')),
                // arrow-key motion (right + down) — for users who
                // think in arrows; works the same way in sticky
                // prefix mode as hjkl does.
                KeyChord::plain(KeyCode::Right),
                KeyChord::plain(KeyCode::Down),
            ],
            focus_prev: vec![
                KeyChord::plain(KeyCode::Char('p')),
                KeyChord::plain(KeyCode::Char('h')),
                KeyChord::plain(KeyCode::Char('k')),
                KeyChord::plain(KeyCode::Left),
                KeyChord::plain(KeyCode::Up),
            ],
            focus_last: vec![KeyChord::plain(KeyCode::Tab)],
            toggle_nav: KeyChord::plain(KeyCode::Char('v')),
            open_switcher: KeyChord::plain(KeyCode::Char('w')),
            help: KeyChord::plain(KeyCode::Char('?')),
        }
    }
}

impl PrefixBindings {
    pub fn lookup(&self, key: &KeyEvent) -> Option<PrefixAction> {
        // Single-chord actions checked first via a small fixed-size
        // table — these don't need aliasing and the table form makes
        // the registry read as data declaration.
        let single: [(KeyChord, PrefixAction); 5] = [
            (self.quit, PrefixAction::Quit),
            (self.spawn_agent, PrefixAction::SpawnAgent),
            (self.toggle_nav, PrefixAction::ToggleNav),
            (self.open_switcher, PrefixAction::OpenSwitcher),
            (self.help, PrefixAction::Help),
        ];
        if let Some((_, action)) = single.iter().find(|(c, _)| c.matches(key)) {
            return Some(*action);
        }
        // Multi-chord focus actions: linear scan across each Vec.
        // With ~3 chords per action and 3 actions, this is 9 ops max
        // per keystroke — negligible at the 50ms FRAME_POLL cadence.
        let multi: [(&[KeyChord], PrefixAction); 3] = [
            (&self.focus_next, PrefixAction::FocusNext),
            (&self.focus_prev, PrefixAction::FocusPrev),
            (&self.focus_last, PrefixAction::FocusLast),
        ];
        for (chords, action) in multi {
            if chords.iter().any(|c| c.matches(key)) {
                return Some(action);
            }
        }
        None
    }

    /// Primary chord for an action — the first one in the user's
    /// list. Used by the help screen for a single-line summary;
    /// aliases are config-discoverable rather than help-rendered to
    /// keep the help screen scannable.
    pub fn binding_for(&self, action: PrefixAction) -> KeyChord {
        match action {
            PrefixAction::Quit => self.quit,
            PrefixAction::SpawnAgent => self.spawn_agent,
            PrefixAction::FocusNext => first_or_default(&self.focus_next, KeyCode::Char('n')),
            PrefixAction::FocusPrev => first_or_default(&self.focus_prev, KeyCode::Char('p')),
            PrefixAction::FocusLast => first_or_default(&self.focus_last, KeyCode::Tab),
            PrefixAction::ToggleNav => self.toggle_nav,
            PrefixAction::OpenSwitcher => self.open_switcher,
            PrefixAction::Help => self.help,
        }
    }
}

/// Return the first chord in `list` if non-empty, else fall back to a
/// plain-modifier chord on `default_code`. The fallback path only
/// fires when the user wrote `focus_next = []` in their config —
/// surprising but valid TOML — so we degrade to "binding shows
/// default in help; lookup returns None" rather than crash.
fn first_or_default(list: &[KeyChord], default_code: KeyCode) -> KeyChord {
    list.first()
        .copied()
        .unwrap_or(KeyChord::plain(default_code))
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
        table.iter().find(|(c, _)| c.matches(key)).map(|(_, a)| *a)
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
        table.iter().find(|(c, _)| c.matches(key)).map(|(_, a)| *a)
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
pub struct ScrollBindings {
    pub line_up: KeyChord,
    pub line_down: KeyChord,
    pub page_up: KeyChord,
    pub page_down: KeyChord,
    pub top: KeyChord,
    pub bottom: KeyChord,
    pub exit: KeyChord,
}

impl Default for ScrollBindings {
    fn default() -> Self {
        Self {
            line_up: KeyChord::plain(KeyCode::Up),
            line_down: KeyChord::plain(KeyCode::Down),
            page_up: KeyChord::plain(KeyCode::PageUp),
            page_down: KeyChord::plain(KeyCode::PageDown),
            top: KeyChord::plain(KeyCode::Char('g')),
            bottom: KeyChord::plain(KeyCode::Char('G')),
            exit: KeyChord::plain(KeyCode::Esc),
        }
    }
}

impl ScrollBindings {
    pub fn lookup(&self, key: &KeyEvent) -> Option<ScrollAction> {
        let table: [(KeyChord, ScrollAction); 7] = [
            (self.line_up, ScrollAction::LineUp),
            (self.line_down, ScrollAction::LineDown),
            (self.page_up, ScrollAction::PageUp),
            (self.page_down, ScrollAction::PageDown),
            (self.top, ScrollAction::Top),
            (self.bottom, ScrollAction::Bottom),
            (self.exit, ScrollAction::ExitScroll),
        ];
        table
            .into_iter()
            .find_map(|(c, a)| c.matches(key).then_some(a))
    }

    pub fn binding_for(&self, action: ScrollAction) -> KeyChord {
        match action {
            ScrollAction::LineUp => self.line_up,
            ScrollAction::LineDown => self.line_down,
            ScrollAction::PageUp => self.page_up,
            ScrollAction::PageDown => self.page_down,
            ScrollAction::Top => self.top,
            ScrollAction::Bottom => self.bottom,
            ScrollAction::ExitScroll => self.exit,
        }
    }
}

// ---------- DirectBindings (no-prefix, fast-path navigation) ----------

/// Actions reachable without first arming the prefix-key state
/// machine. Same semantics as the matching `PrefixAction` variants —
/// this enum exists for the help screen and the dispatch table, not
/// because the underlying state mutation differs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
// All three variants are intentionally `Focus*` to mirror the
// matching `PrefixAction` names — clippy's enum_variant_names lint
// would have us drop the prefix, which would create a `Next` /
// `Prev` / `Last` enum that is impossible to read in isolation.
#[allow(clippy::enum_variant_names)]
pub enum DirectAction {
    FocusNext,
    FocusPrev,
    FocusLast,
}

impl DirectAction {
    pub const ALL: &'static [DirectAction] = &[Self::FocusNext, Self::FocusPrev, Self::FocusLast];

    pub const fn description(self) -> &'static str {
        match self {
            Self::FocusNext => "focus the next agent",
            Self::FocusPrev => "focus the previous agent",
            Self::FocusLast => "bounce to the previously-focused agent",
        }
    }
}

/// Direct (no-prefix) navigation chords. The whole point of this
/// scope is the fast path: the user pays one chord (e.g. `Cmd-;`)
/// instead of the two of `Ctrl-B p`. Defaults use the `SUPER` (Cmd
/// on macOS, Win on most Linux DEs) modifier; the runtime
/// auto-enables the Kitty Keyboard Protocol whenever any binding
/// uses `SUPER`, which is what makes Cmd deliverable to a TUI.
///
/// **Two chords only by default**: `Cmd+;` for prev and `Cmd+'` for
/// next. The wider sticky-mode navigation (hjkl with no modifier)
/// lives behind the prefix instead — see `PrefixState` in the
/// runtime. This is deliberate: the prior multi-chord defaults
/// (`Cmd+]`, `Cmd+[`, `Cmd+L`, `Cmd+H`, `` Cmd+` ``) ran into a
/// mess of OS reservations (`Cmd+H` = Hide, `` Cmd+` `` = window
/// cycle) and terminal claims (Ghostty owns `Cmd+]`/`Cmd+[`). The
/// two surviving defaults are verified-working and on the right
/// side of every layout we tested.
///
/// Multi-chord per action — same shape as `PrefixBindings.focus_*` —
/// so users on different terminals can add aliases without forking
/// the schema (`focus_next = ["cmd+'", "cmd+l"]`). Single-string
/// TOML still parses.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
// Same rationale as DirectAction: the `focus_` prefix makes each
// field self-describing and matches the names used in PrefixBindings.
// Dropping it would land us with `next` / `prev` / `last` which read
// as ambiguous list-navigation rather than tab-focus moves.
#[allow(clippy::struct_field_names)]
pub struct DirectBindings {
    #[serde(deserialize_with = "deserialize_chord_list")]
    pub focus_next: Vec<KeyChord>,
    #[serde(deserialize_with = "deserialize_chord_list")]
    pub focus_prev: Vec<KeyChord>,
    /// Empty by default. The bounce ("alt-tab") move is covered by
    /// `Ctrl-B Tab` in the prefix-mode scope; a Cmd-modifier chord
    /// for it would either fight the OS (`` Cmd+` ``) or add
    /// learning surface for a move people use less often than
    /// straight next/prev. Users who want one can add it via
    /// config.
    #[serde(deserialize_with = "deserialize_chord_list")]
    pub focus_last: Vec<KeyChord>,
}

impl Default for DirectBindings {
    fn default() -> Self {
        // `Cmd+'` and `Cmd+;` are adjacent on the home row, both
        // verified free in Ghostty + macOS, both unclaimed across
        // common terminals (iTerm2, WezTerm, Terminal.app). The
        // semicolon was the user-confirmed working chord; the
        // apostrophe rides next to it.
        Self {
            focus_next: vec![KeyChord {
                code: KeyCode::Char('\''),
                modifiers: KeyModifiers::SUPER,
            }],
            focus_prev: vec![KeyChord {
                code: KeyCode::Char(';'),
                modifiers: KeyModifiers::SUPER,
            }],
            focus_last: Vec::new(),
        }
    }
}

impl DirectBindings {
    pub fn lookup(&self, key: &KeyEvent) -> Option<DirectAction> {
        // Linear scan over each Vec; with ~1 chord per action and
        // 3 actions, it's 3 ops max per keystroke at the 50ms frame
        // cadence — invisible.
        let table: [(&[KeyChord], DirectAction); 3] = [
            (&self.focus_next, DirectAction::FocusNext),
            (&self.focus_prev, DirectAction::FocusPrev),
            (&self.focus_last, DirectAction::FocusLast),
        ];
        for (chords, action) in table {
            if chords.iter().any(|c| c.matches(key)) {
                return Some(action);
            }
        }
        None
    }

    /// Primary chord for an action — the first one in the user's
    /// list. Used by the help screen for a single-line summary;
    /// aliases are config-discoverable rather than help-rendered to
    /// keep the help screen scannable.
    ///
    /// `focus_last` falls back to `Tab` for the help-screen display
    /// even though it's unbound by default — the help line still
    /// renders, just dimmed by the runtime to indicate "configure
    /// to enable" (rendering policy is the renderer's call, not
    /// this method's).
    pub fn binding_for(&self, action: DirectAction) -> KeyChord {
        match action {
            DirectAction::FocusNext => first_or_default(&self.focus_next, KeyCode::Char('\'')),
            DirectAction::FocusPrev => first_or_default(&self.focus_prev, KeyCode::Char(';')),
            DirectAction::FocusLast => first_or_default(&self.focus_last, KeyCode::Tab),
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
    pub on_direct: DirectBindings,
    pub on_scroll: ScrollBindings,
}

impl Default for Bindings {
    fn default() -> Self {
        Self {
            prefix: KeyChord::ctrl(KeyCode::Char('b')),
            on_prefix: PrefixBindings::default(),
            on_popup: PopupBindings::default(),
            on_modal: ModalBindings::default(),
            on_direct: DirectBindings::default(),
            on_scroll: ScrollBindings::default(),
        }
    }
}

impl Bindings {
    /// True if any loaded chord (prefix or any per-scope action) requires the
    /// SUPER modifier. The runtime uses this to decide whether to negotiate
    /// the Kitty Keyboard Protocol with the terminal — without it, terminals
    /// usually swallow Cmd / Super before the application can see it.
    ///
    /// Defaults DO use SUPER for the `on_direct` scope (the whole
    /// point of direct binds is fast Cmd-key access), so the
    /// protocol negotiation runs on the default config. Users who
    /// don't want it can override the direct chords to non-SUPER
    /// alternatives in their config.
    pub fn uses_super_modifier(&self) -> bool {
        let single = [
            self.prefix,
            self.on_prefix.quit,
            self.on_prefix.spawn_agent,
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
            self.on_scroll.line_up,
            self.on_scroll.line_down,
            self.on_scroll.page_up,
            self.on_scroll.page_down,
            self.on_scroll.top,
            self.on_scroll.bottom,
            self.on_scroll.exit,
        ];
        if single
            .iter()
            .any(|c| c.modifiers.contains(KeyModifiers::SUPER))
        {
            return true;
        }
        // Multi-chord scopes — flatten across each Vec. Includes
        // both prefix-mode focus aliases and the entire direct-bind
        // scope (which now stores Vec<KeyChord> per action).
        let multi: [&[KeyChord]; 6] = [
            &self.on_prefix.focus_next,
            &self.on_prefix.focus_prev,
            &self.on_prefix.focus_last,
            &self.on_direct.focus_next,
            &self.on_direct.focus_prev,
            &self.on_direct.focus_last,
        ];
        multi
            .iter()
            .flat_map(|v| v.iter())
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
            "q",
            "ctrl+b",
            "ctrl+alt+x",
            "enter",
            "esc",
            "tab",
            "f5",
            "?",
            "up",
            "down",
            "space",
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

    #[test]
    fn focus_next_accepts_a_single_string_in_toml() {
        // Backwards-compat with the original single-chord syntax —
        // the deserializer accepts a string OR an array.
        let toml_text = r#"
            [on_prefix]
            focus_next = "x"
        "#;
        let bindings: Bindings = toml::from_str(toml_text).unwrap();
        assert_eq!(
            bindings.on_prefix.focus_next,
            vec![KeyChord::plain(KeyCode::Char('x'))],
        );
    }

    #[test]
    fn focus_next_accepts_an_array_in_toml() {
        let toml_text = r#"
            [on_prefix]
            focus_next = ["n", "l", "j"]
        "#;
        let bindings: Bindings = toml::from_str(toml_text).unwrap();
        assert_eq!(
            bindings.on_prefix.focus_next,
            vec![
                KeyChord::plain(KeyCode::Char('n')),
                KeyChord::plain(KeyCode::Char('l')),
                KeyChord::plain(KeyCode::Char('j')),
            ],
        );
    }

    #[test]
    fn focus_next_array_with_an_invalid_chord_is_an_error() {
        let toml_text = r#"
            [on_prefix]
            focus_next = ["n", "ctrl+nonsense"]
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
    fn prefix_focus_next_aliases_all_resolve_to_focus_next() {
        // Default `focus_next` includes n (tmux), l + j (vim),
        // and Right + Down (arrows).
        let b = PrefixBindings::default();
        for c in ['n', 'l', 'j'] {
            assert_eq!(
                b.lookup(&ev(KeyCode::Char(c), KeyModifiers::NONE)),
                Some(PrefixAction::FocusNext),
                "char {c} should map to FocusNext",
            );
        }
        for code in [KeyCode::Right, KeyCode::Down] {
            assert_eq!(
                b.lookup(&ev(code, KeyModifiers::NONE)),
                Some(PrefixAction::FocusNext),
                "{code:?} should map to FocusNext",
            );
        }
    }

    #[test]
    fn prefix_focus_prev_aliases_all_resolve_to_focus_prev() {
        let b = PrefixBindings::default();
        for c in ['p', 'h', 'k'] {
            assert_eq!(
                b.lookup(&ev(KeyCode::Char(c), KeyModifiers::NONE)),
                Some(PrefixAction::FocusPrev),
                "char {c} should map to FocusPrev",
            );
        }
        for code in [KeyCode::Left, KeyCode::Up] {
            assert_eq!(
                b.lookup(&ev(code, KeyModifiers::NONE)),
                Some(PrefixAction::FocusPrev),
                "{code:?} should map to FocusPrev",
            );
        }
    }

    #[test]
    fn direct_lookup_finds_default_cmd_bindings() {
        let b = DirectBindings::default();
        // Two chords by default: Cmd+' for next, Cmd+; for prev.
        // No focus_last default — that move lives behind the prefix
        // (Ctrl-B Tab).
        assert_eq!(
            b.lookup(&ev(KeyCode::Char('\''), KeyModifiers::SUPER)),
            Some(DirectAction::FocusNext),
        );
        assert_eq!(
            b.lookup(&ev(KeyCode::Char(';'), KeyModifiers::SUPER)),
            Some(DirectAction::FocusPrev),
        );
    }

    #[test]
    fn direct_lookup_focus_last_unbound_by_default() {
        let b = DirectBindings::default();
        // No default chord. The bounce move is reachable via
        // `Ctrl-B Tab` (prefix mode); a Cmd default would have to
        // pick a chord that doesn't fight the OS, and none of the
        // candidates were worth the learning surface.
        assert!(b.focus_last.is_empty());
    }

    #[test]
    fn direct_lookup_does_not_match_os_reserved_or_terminal_claimed_chords() {
        // Sanity check: codemux must NOT bind chords that macOS or
        // common terminals claim. This catches regressions if the
        // defaults grow back the kitchen-sink alias list we just
        // removed.
        let b = DirectBindings::default();
        for c in ['h', '`', ']', '['] {
            assert_eq!(
                b.lookup(&ev(KeyCode::Char(c), KeyModifiers::SUPER)),
                None,
                "Cmd+{c} should not be bound by default",
            );
        }
    }

    #[test]
    fn direct_lookup_round_trip_for_bound_actions() {
        // FocusLast is unbound by default (empty Vec), so it
        // doesn't round-trip — that's intentional, see
        // `direct_lookup_focus_last_unbound_by_default`.
        let b = DirectBindings::default();
        for action in [DirectAction::FocusNext, DirectAction::FocusPrev] {
            let chord = b.binding_for(action);
            let event = KeyEvent::new(chord.code, chord.modifiers);
            assert_eq!(b.lookup(&event), Some(action));
        }
    }

    #[test]
    fn direct_lookup_returns_none_when_modifier_does_not_match() {
        // Plain `;` (no SUPER) must NOT trigger the direct bind —
        // otherwise the user couldn't type the character into a
        // focused PTY without it stealing focus.
        let b = DirectBindings::default();
        assert_eq!(b.lookup(&ev(KeyCode::Char(';'), KeyModifiers::NONE)), None,);
    }

    #[test]
    fn direct_binding_for_focus_last_falls_back_to_tab_when_unbound() {
        // FocusLast is empty in the default config (see
        // direct_lookup_focus_last_unbound_by_default). The help
        // screen still renders a row for it, so binding_for must
        // produce a printable chord rather than panicking. Tab is
        // the chosen fallback because it mirrors the prefix-mode
        // alt-tab move (`Ctrl-B Tab`).
        let b = DirectBindings::default();
        assert_eq!(
            b.binding_for(DirectAction::FocusLast),
            KeyChord::plain(KeyCode::Tab),
        );
    }

    #[test]
    fn user_can_swap_direct_modifier_to_alt() {
        // The chord vocabulary is the same everywhere — users on
        // terminals that can't deliver Cmd swap to Alt by editing
        // the chord, no separate "modifier" config required.
        let toml_text = r#"
            [on_direct]
            focus_next = "alt+l"
            focus_prev = "alt+h"
            focus_last = "alt+`"
        "#;
        let bindings: Bindings = toml::from_str(toml_text).unwrap();
        assert_eq!(
            bindings
                .on_direct
                .lookup(&ev(KeyCode::Char('l'), KeyModifiers::ALT)),
            Some(DirectAction::FocusNext),
        );
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

    #[test]
    fn scroll_lookup_round_trip() {
        let b = ScrollBindings::default();
        for action in ScrollAction::ALL {
            let chord = b.binding_for(*action);
            let event = KeyEvent::new(chord.code, chord.modifiers);
            assert_eq!(b.lookup(&event), Some(*action));
        }
    }

    #[test]
    fn scroll_defaults_cover_arrow_pgup_pgdn_g_capital_g_esc() {
        // Pin the defaults explicitly. The runtime's interception is
        // gated on these chords matching the user's keypress; if
        // someone "tidies" the defaults to e.g. `j`/`k` they'd silently
        // break wheel-then-arrow navigation in scroll mode.
        let b = ScrollBindings::default();
        assert_eq!(b.line_up.code, KeyCode::Up);
        assert_eq!(b.line_down.code, KeyCode::Down);
        assert_eq!(b.page_up.code, KeyCode::PageUp);
        assert_eq!(b.page_down.code, KeyCode::PageDown);
        assert_eq!(b.top.code, KeyCode::Char('g'));
        assert_eq!(b.bottom.code, KeyCode::Char('G'));
        assert_eq!(b.exit.code, KeyCode::Esc);
    }

    // --- uses_super_modifier ---

    #[test]
    fn defaults_use_super_modifier_via_direct_binds() {
        // Direct binds default to `cmd+l` / `cmd+h` / `cmd+grave`,
        // so out-of-the-box codemux DOES request the Kitty Keyboard
        // Protocol. This is intentional — the fast-path navigation
        // is the headline of the direct-bind layer; making it work
        // by default outweighs the cost of the protocol negotiation.
        // Users who don't want it override the `on_direct` chords.
        assert!(Bindings::default().uses_super_modifier());
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
    fn cmd_scroll_chord_triggers_super_detection() {
        // A user who binds Cmd-PgUp to scroll-page-up needs Kitty
        // Keyboard Protocol negotiation just like any other Cmd
        // binding. If `on_scroll` is missed in the enumeration, this
        // test fails — the user's Cmd-PgUp would be silently swallowed
        // by the terminal.
        let toml_text = r#"
            [on_scroll]
            page_up = "cmd+pageup"
        "#;
        let bindings: Bindings = toml::from_str(toml_text).unwrap();
        assert!(bindings.uses_super_modifier());
    }

    #[test]
    fn ctrl_only_overrides_do_not_trigger_super_detection() {
        // Override every default that uses SUPER — including the
        // `on_direct` defaults — to a non-SUPER chord. Without
        // overriding `on_direct`, the default Cmd chords would keep
        // detection true, masking the actual scenario being tested
        // (a user who has explicitly opted out of all Cmd bindings).
        let toml_text = r#"
            prefix = "ctrl+a"
            [on_prefix]
            quit = "x"
            [on_direct]
            focus_next = "ctrl+l"
            focus_prev = "ctrl+h"
            focus_last = "ctrl+t"
        "#;
        let bindings: Bindings = toml::from_str(toml_text).unwrap();
        assert!(!bindings.uses_super_modifier());
    }
}
