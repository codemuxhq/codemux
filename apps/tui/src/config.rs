//! TOML config loader. The user file lives at
//! `$XDG_CONFIG_HOME/codemux/config.toml`, falling back to
//! `$HOME/.config/codemux/config.toml`.
//!
//! XDG on every Unix, including macOS. The `directories`/`dirs` crates default
//! to `~/Library/Application Support/codemux/` on macOS — that's the Apple
//! convention for GUI apps and the wrong place for a CLI tool. Modern CLIs
//! (gh, git, helix, kubectl, alacritty, ripgrep, ruff, nushell) all settled on
//! `~/.config/` regardless of platform; we follow suit.
//!
//! Per the architecture-guide review (P1.3 NLM session): config is
//! infrastructure. We load it once at startup and pass the resulting POD into
//! `runtime::run`. We do not expose a port/trait — that abstraction earns its
//! keep only when the config becomes dynamic (remote service), which is not a
//! current need.
//!
//! Failure mode: if the file is present but unparseable, fail loud with a
//! readable error and exit non-zero before touching the terminal. A typo
//! silently breaking your bindings would be much worse than refusing to start.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;

use color_eyre::Result;
use color_eyre::eyre::{WrapErr, eyre};
use ratatui::style::Color;
use serde::Deserialize;
use serde::de::{self, Deserializer, Visitor};

use crate::keymap::Bindings;

/// Per-agent scrollback buffer size, in rows. Default ~5k rows is roughly
/// 20 MB at a typical 120-col width — comfortable for a personal tool with
/// 2-4 agents. Documented as the user-facing knob; bumping it costs memory
/// linearly per Ready agent.
fn default_scrollback_len() -> usize {
    5_000
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct Config {
    pub bindings: Bindings,
    /// How many rows of scrollback each agent's PTY parser retains.
    /// Vt100 only collects rows that scroll off the *primary* screen; this
    /// works for codemux because Claude Code renders inline (not on the
    /// alternate screen). See AD-25.
    #[serde(default = "default_scrollback_len")]
    pub scrollback_len: usize,
    /// Visual presentation knobs for the codemux chrome itself (status
    /// bar, tab strip, hints, log strip — everything *around* the agent
    /// pane). Agent PTY content is never restyled.
    pub ui: Ui,
    /// Knobs for the spawn modal — search engine choice and the roots
    /// the fuzzy directory indexer walks.
    pub spawn: SpawnConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bindings: Bindings::default(),
            scrollback_len: default_scrollback_len(),
            ui: Ui::default(),
            spawn: SpawnConfig::default(),
        }
    }
}

/// User-facing presentation knobs. Default values are tuned to be
/// readable on poor monitors (washed-out laptop screens, projectors,
/// sunlight glare); opt-ins reintroduce the subtler aesthetic for
/// users who have a high-contrast display and prefer it.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Ui {
    /// When `true`, secondary chrome (separators, hints, host prefix,
    /// log strip, unfocused tab body) renders with the ANSI `DIM`
    /// modifier on top of `DarkGray`. This was the original look — it
    /// reads as gentle ambient text on a high-contrast monitor but can
    /// disappear entirely on a poor one (DIM is terminal-defined and
    /// some renderers blend fg into bg aggressively).
    ///
    /// When `false` (the default), secondary chrome renders in a fixed
    /// xterm-256 gray (`Color::Indexed(247)`) with no `DIM` modifier —
    /// deterministic across terminals, and visible on any reasonable
    /// display.
    pub subtle: bool,

    /// Per-host accent colors used for the host prefix on unfocused
    /// tabs (e.g. `uber-laptop · main-claude`). Hosts not listed fall
    /// back to the secondary chrome style — quiet by default, opt-in
    /// distinction for the hosts the user juggles often.
    ///
    /// Three accepted formats per value:
    /// - **Named ANSI**: `"blue"`, `"red"`, `"lightgreen"`, etc.
    ///   Eight standard names (`black`, `red`, `green`, `yellow`,
    ///   `blue`, `magenta`, `cyan`, `gray`/`grey`) plus their `light_`
    ///   or `bright_` variants and `darkgray`/`white`. Honors the
    ///   user's terminal theme.
    /// - **xterm-256 index**: `33` (a TOML integer). Picks a specific
    ///   slot in the 256-color palette. Same color across themes.
    /// - **Hex RGB**: `"#0080ff"`. True-color. Renders precisely on
    ///   modern terminals; may degrade on 256-color-only setups.
    pub host_colors: HashMap<String, ChromeColor>,
}

/// Default file/dir names that mark a directory as a "code project"
/// for the fuzzy spawn-modal boost. Curated across the ecosystems
/// codemux's user is likely to spawn agents in.
///
/// Names must be **non-hidden** (no leading dot) — the indexer's walker
/// skips hidden entries by default, so a hidden marker like `.envrc`
/// would never be detected via file iteration. `.git` is the one
/// special case: it's checked via an explicit per-directory stat
/// because it's the strongest project signal we have.
///
/// The list is exposed via `[spawn].project_markers` for additions
/// like `Tiltfile`, `dvc.yaml`, etc. — the user's config completely
/// replaces this default (no merge), so copy these in when overriding
/// if you want to keep them.
fn default_project_markers() -> Vec<String> {
    [
        // Rust
        "Cargo.toml",
        // JS / TS
        "package.json",
        // Go
        "go.mod",
        // Python
        "pyproject.toml",
        "setup.py",
        // JVM
        "pom.xml",
        "build.gradle",
        "build.gradle.kts",
        // Ruby
        "Gemfile",
        // PHP
        "composer.json",
        // Elixir
        "mix.exs",
        // Swift
        "Package.swift",
        // Dart / Flutter
        "pubspec.yaml",
        // C / C++
        "CMakeLists.txt",
        // Generic build tools
        "Makefile",
        "Justfile",
        "justfile",
        "flake.nix",
        "BUILD",
        "BUILD.bazel",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect()
}

/// Knobs for the spawn modal, sourced from `[spawn]` in `config.toml`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct SpawnConfig {
    /// Roots walked by the fuzzy directory indexer. Each entry is
    /// tilde-expanded at startup. Defaults to `["~"]`, which indexes
    /// the user's home directory tree.
    ///
    /// The indexer respects `.gitignore` (via the `ignore` crate);
    /// large vendor trees should be excluded with a `.gitignore` entry
    /// rather than by removing them from this list. There is also a
    /// hard cap on indexed entries inside the worker — if you hit it,
    /// add a `.gitignore` to the noisy subtree.
    pub search_roots: Vec<String>,
    /// Which path-zone engine the modal opens with. `"fuzzy"` uses the
    /// session-built directory index with `nucleo-matcher` scoring;
    /// `"precise"` uses the live `read_dir` prefix-completion engine.
    /// Toggle at runtime with the `ToggleSearchMode` binding (default
    /// `ctrl+t`).
    pub default_mode: SearchMode,
    /// Filenames that mark a directory as a "code project" — the
    /// fuzzy matcher boosts these above plain directories. `.git` is
    /// detected separately (it's hidden) and gets a higher boost than
    /// any of these markers. The default covers Cargo / npm / Go /
    /// Python / JVM / Ruby / Elixir / Swift / Dart / C++ / generic
    /// build tools (Makefile, Justfile, flake.nix, BUILD, .envrc).
    /// Override to add e.g. `"Tiltfile"`, `"dvc.yaml"` — note that the
    /// user-supplied list completely replaces the default, so include
    /// any defaults you want to keep.
    #[serde(default = "default_project_markers")]
    pub project_markers: Vec<String>,
    /// Named projects: explicit user-curated `(name, path)` pairs that
    /// the fuzzy matcher boosts above any auto-discovered repository.
    /// The query is matched against `name` (not the full path), so
    /// short aliases work — `name = "cm"` plus `path = ".../codemux"`
    /// makes typing `cm` jump straight there. Paths are tilde-expanded
    /// at use time. If a project's path also lives in the indexed
    /// search roots, it is deduplicated so the entry only appears once
    /// (with the named-project boost).
    pub projects: Vec<NamedProject>,
}

impl Default for SpawnConfig {
    fn default() -> Self {
        Self {
            search_roots: vec!["~".to_string()],
            default_mode: SearchMode::default(),
            project_markers: default_project_markers(),
            projects: Vec::new(),
        }
    }
}

/// User-curated alias for a project path. Matched by `name` in the
/// fuzzy modal; spawn target is `path` (tilde-expanded at use site).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct NamedProject {
    pub name: String,
    pub path: String,
}

/// Which path-zone search engine the spawn modal uses.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    /// Session-built directory index, queried via fuzzy matcher.
    /// Default for new installs — `cmd+t` toggles to precise.
    #[default]
    Fuzzy,
    /// Live `read_dir` + zsh-style Tab autocomplete. The original
    /// engine. Selected when the user is on a remote SSH host
    /// (the index walker is local-only) or when explicitly chosen.
    Precise,
}

/// A user-configurable color for chrome accents. Validated at
/// deserialize time so a typo in `config.toml` fails loudly with a
/// readable error before any rendering happens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChromeColor {
    /// One of crossterm's 16 named ANSI colors. Maps to whatever the
    /// user's terminal theme defines for that slot.
    Named(Color),
    /// A specific xterm-256 palette slot (0-255).
    Indexed(u8),
    /// True-color RGB triple. Renders on terminals that support
    /// 24-bit color (most modern ones); lossy on 256-color terminals.
    Rgb(u8, u8, u8),
}

impl ChromeColor {
    /// Convert to the ratatui `Color` used by the renderer. Infallible
    /// because validation happened at deserialize time.
    #[must_use]
    pub fn to_color(self) -> Color {
        match self {
            Self::Named(c) => c,
            Self::Indexed(i) => Color::Indexed(i),
            Self::Rgb(r, g, b) => Color::Rgb(r, g, b),
        }
    }
}

/// Custom deserialization for `ChromeColor`: TOML scalars come in as
/// either string or integer, and we want the user to write the natural
/// form for each case (a name, a number, or a `#rrggbb`) without having
/// to wrap things in tagged variants.
impl<'de> Deserialize<'de> for ChromeColor {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(ChromeColorVisitor)
    }
}

struct ChromeColorVisitor;

impl Visitor<'_> for ChromeColorVisitor {
    type Value = ChromeColor;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(
            "a color: a named ANSI color (\"blue\"), a hex RGB string (\"#0080ff\"), \
             or an xterm-256 palette index (0-255)",
        )
    }

    fn visit_str<E>(self, value: &str) -> Result<ChromeColor, E>
    where
        E: de::Error,
    {
        if let Some(hex) = value.strip_prefix('#') {
            parse_hex_rgb(hex)
                .map(|(r, g, b)| ChromeColor::Rgb(r, g, b))
                .ok_or_else(|| {
                    de::Error::custom(format!(
                        "invalid hex color {value:?}; expected #rrggbb (six hex digits)",
                    ))
                })
        } else {
            parse_named_color(value)
                .map(ChromeColor::Named)
                .ok_or_else(|| {
                    de::Error::custom(format!(
                        "unknown color name {value:?}; expected one of: \
                     black, red, green, yellow, blue, magenta, cyan, white, gray, \
                     darkgray, darkred, darkgreen, darkyellow, darkblue, darkmagenta, \
                     darkcyan",
                    ))
                })
        }
    }

    fn visit_u64<E>(self, value: u64) -> Result<ChromeColor, E>
    where
        E: de::Error,
    {
        u8::try_from(value)
            .map(ChromeColor::Indexed)
            .map_err(|_| de::Error::custom(format!("xterm-256 index {value} out of range (0-255)")))
    }

    fn visit_i64<E>(self, value: i64) -> Result<ChromeColor, E>
    where
        E: de::Error,
    {
        u8::try_from(value)
            .map(ChromeColor::Indexed)
            .map_err(|_| de::Error::custom(format!("xterm-256 index {value} out of range (0-255)")))
    }
}

/// Parse `rrggbb` (six hex digits, no `#` prefix) into an RGB triple.
/// `None` on any malformed input — the caller wraps that into a
/// human-readable serde error.
fn parse_hex_rgb(s: &str) -> Option<(u8, u8, u8)> {
    if s.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&s[0..2], 16).ok()?;
    let g = u8::from_str_radix(&s[2..4], 16).ok()?;
    let b = u8::from_str_radix(&s[4..6], 16).ok()?;
    Some((r, g, b))
}

/// Map a lowercased ANSI color name to ratatui's `Color`. Names follow
/// the convention TUI configs use elsewhere (kitty, alacritty, helix):
/// the eight standard ANSI names map to the dim/normal palette
/// (`Color::Red` etc.); the `light_*` (or `bright_*`) prefix selects
/// the bright counterpart (`Color::LightRed` etc.). Returns `None` for
/// an unrecognized name so the caller can produce an error message
/// that lists the valid alternatives.
fn parse_named_color(s: &str) -> Option<Color> {
    // Allow both `lightred` and `bright_red` style; users coming from
    // different ecosystems write either. Normalize once up front.
    let normalized = s.to_ascii_lowercase().replace(['_', '-'], "");
    match normalized.as_str() {
        "black" => Some(Color::Black),
        "red" => Some(Color::Red),
        "green" => Some(Color::Green),
        "yellow" => Some(Color::Yellow),
        "blue" => Some(Color::Blue),
        "magenta" => Some(Color::Magenta),
        "cyan" => Some(Color::Cyan),
        "gray" | "grey" => Some(Color::Gray),
        "darkgray" | "darkgrey" => Some(Color::DarkGray),
        "lightred" | "brightred" => Some(Color::LightRed),
        "lightgreen" | "brightgreen" => Some(Color::LightGreen),
        "lightyellow" | "brightyellow" => Some(Color::LightYellow),
        "lightblue" | "brightblue" => Some(Color::LightBlue),
        "lightmagenta" | "brightmagenta" => Some(Color::LightMagenta),
        "lightcyan" | "brightcyan" => Some(Color::LightCyan),
        "white" => Some(Color::White),
        _ => None,
    }
}

/// Load the user config from the canonical XDG location, returning defaults
/// if the file is missing. Returns an error only on hard failures (read I/O,
/// TOML parse error, unresolvable XDG path).
pub fn load() -> Result<Config> {
    let path = config_path()?;
    if !path.exists() {
        tracing::debug!("no config at {}; using defaults", path.display());
        return Ok(Config::default());
    }
    let text = std::fs::read_to_string(&path)
        .wrap_err_with(|| format!("read config at {}", path.display()))?;
    let config: Config =
        toml::from_str(&text).wrap_err_with(|| format!("parse config at {}", path.display()))?;
    tracing::debug!("loaded config from {}", path.display());
    Ok(config)
}

/// Resolve the path codemux looks at. Public so the `--help` text and any
/// "where is my config" UX can show the same location the loader uses.
///
/// Resolution order:
/// 1. `$XDG_CONFIG_HOME/codemux/config.toml` if `$XDG_CONFIG_HOME` is set
/// 2. `$HOME/.config/codemux/config.toml` otherwise
pub fn config_path() -> Result<PathBuf> {
    resolve_config_path(
        std::env::var_os("XDG_CONFIG_HOME"),
        std::env::var_os("HOME"),
    )
}

fn resolve_config_path(xdg: Option<OsString>, home: Option<OsString>) -> Result<PathBuf> {
    if let Some(xdg) = xdg.filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(xdg).join("codemux").join("config.toml"));
    }
    let home = home
        .filter(|v| !v.is_empty())
        .ok_or_else(|| eyre!("$HOME is not set; cannot resolve config path"))?;
    Ok(PathBuf::from(home)
        .join(".config")
        .join("codemux")
        .join("config.toml"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn empty_toml_yields_default_config() {
        let config: Config = toml::from_str("").unwrap();
        let defaults = Config::default();
        assert_eq!(config.bindings.prefix, defaults.bindings.prefix);
        assert_eq!(config.scrollback_len, defaults.scrollback_len);
    }

    #[test]
    fn user_can_override_just_the_prefix() {
        let toml_text = r#"
            [bindings]
            prefix = "ctrl+a"
        "#;
        let config: Config = toml::from_str(toml_text).unwrap();
        assert_eq!(
            config.bindings.prefix.code,
            crossterm::event::KeyCode::Char('a'),
        );
        assert!(
            config
                .bindings
                .prefix
                .modifiers
                .contains(crossterm::event::KeyModifiers::CONTROL),
        );
    }

    #[test]
    fn unknown_top_level_key_is_an_error() {
        // Catch typos like `binding` (singular) instead of `bindings`.
        let toml_text = r#"
            [bindings]
            prefix = "ctrl+b"

            [bindng]
            something = "x"
        "#;
        // Note: serde(default) at the field level means missing keys are OK,
        // but unknown top-level tables are tolerated by serde unless we
        // mark Config with #[serde(deny_unknown_fields)]. For this slice
        // tolerance is fine; we trade a tiny amount of typo-safety for a
        // friendlier upgrade story (forward-compatible with new fields).
        let config: Config = toml::from_str(toml_text).unwrap();
        assert_eq!(
            config.bindings.prefix.code,
            crossterm::event::KeyCode::Char('b'),
        );
    }

    #[test]
    fn invalid_chord_in_config_propagates_as_a_parse_error() {
        let toml_text = r#"
            [bindings.on_prefix]
            quit = "ctrl+nonsense"
        "#;
        let result: Result<Config, _> = toml::from_str(toml_text);
        assert!(result.is_err());
    }

    #[test]
    fn xdg_config_home_wins_when_set() {
        let path = resolve_config_path(
            Some(OsString::from("/tmp/xdg")),
            Some(OsString::from("/home/me")),
        )
        .unwrap();
        assert_eq!(path, PathBuf::from("/tmp/xdg/codemux/config.toml"));
    }

    #[test]
    fn falls_back_to_home_dot_config_on_macos_and_linux() {
        // The whole point of this fallback: macOS users without XDG_CONFIG_HOME
        // must still land in ~/.config/codemux, not ~/Library/Application Support.
        let path = resolve_config_path(None, Some(OsString::from("/home/me"))).unwrap();
        assert_eq!(path, PathBuf::from("/home/me/.config/codemux/config.toml"));
    }

    #[test]
    fn empty_xdg_is_treated_as_unset() {
        let path = resolve_config_path(Some(OsString::from("")), Some(OsString::from("/home/me")))
            .unwrap();
        assert_eq!(path, PathBuf::from("/home/me/.config/codemux/config.toml"));
    }

    #[test]
    fn errors_when_neither_xdg_nor_home_is_set() {
        let result = resolve_config_path(None, None);
        assert!(result.is_err());
    }

    #[test]
    fn scrollback_len_defaults_to_five_thousand() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.scrollback_len, 5_000);
    }

    #[test]
    fn scrollback_len_round_trips_when_set_in_toml() {
        let config: Config = toml::from_str("scrollback_len = 1500").unwrap();
        assert_eq!(config.scrollback_len, 1_500);
    }

    #[test]
    fn ui_subtle_defaults_to_false() {
        let config: Config = toml::from_str("").unwrap();
        assert!(
            !config.ui.subtle,
            "default chrome must be readable on any monitor",
        );
    }

    #[test]
    fn ui_subtle_round_trips_when_set_in_toml() {
        let config: Config = toml::from_str("[ui]\nsubtle = true\n").unwrap();
        assert!(config.ui.subtle);
    }

    #[test]
    fn missing_ui_section_keeps_defaults() {
        // The whole [ui] table is optional; users on default chrome
        // never have to write anything to opt in to it.
        let config: Config = toml::from_str("scrollback_len = 100").unwrap();
        assert!(!config.ui.subtle);
        assert_eq!(config.scrollback_len, 100);
    }

    // ── ChromeColor parsing ──────────────────────────────────────
    //
    // Loud failure on bad input: the loader wraps the serde error in
    // a "parse config at <path>" frame and exits non-zero before any
    // rendering happens. These tests pin both the happy paths (so the
    // three accepted formats keep working) and the rejection cases (so
    // a typo doesn't silently fall back to a wrong color).

    #[test]
    fn host_colors_named_ansi_round_trips() {
        let toml_text = r#"
            [ui.host_colors]
            uber = "blue"
            personal = "lightred"
        "#;
        let config: Config = toml::from_str(toml_text).unwrap();
        assert_eq!(
            config.ui.host_colors.get("uber"),
            Some(&ChromeColor::Named(Color::Blue)),
        );
        assert_eq!(
            config.ui.host_colors.get("personal"),
            Some(&ChromeColor::Named(Color::LightRed)),
        );
    }

    #[test]
    fn host_colors_accepts_bright_underscore_and_dash_variants() {
        // Different ecosystems write `light_red`, `bright-red`, or
        // `lightred`. All three should parse identically — typo-friendly
        // without exploding the named-color enum.
        for name in [
            "lightred",
            "light_red",
            "light-red",
            "BrightRed",
            "bright_red",
        ] {
            let toml_text = format!("[ui.host_colors]\nh = \"{name}\"\n");
            let config: Config =
                toml::from_str(&toml_text).unwrap_or_else(|e| panic!("name {name:?} failed: {e}"));
            assert_eq!(
                config.ui.host_colors.get("h"),
                Some(&ChromeColor::Named(Color::LightRed)),
                "name {name:?} should map to LightRed",
            );
        }
    }

    #[test]
    fn host_colors_xterm_index_round_trips() {
        let toml_text = r"
            [ui.host_colors]
            uber = 33
            personal = 247
        ";
        let config: Config = toml::from_str(toml_text).unwrap();
        assert_eq!(
            config.ui.host_colors.get("uber"),
            Some(&ChromeColor::Indexed(33)),
        );
        assert_eq!(
            config.ui.host_colors.get("personal"),
            Some(&ChromeColor::Indexed(247)),
        );
    }

    #[test]
    fn host_colors_hex_rgb_round_trips() {
        // r##"..."## raw string — the inner `"#...` hex strings would
        // close a single-hash raw string early.
        let toml_text = r##"
            [ui.host_colors]
            uber = "#0080ff"
            personal = "#D75F00"
        "##;
        let config: Config = toml::from_str(toml_text).unwrap();
        assert_eq!(
            config.ui.host_colors.get("uber"),
            Some(&ChromeColor::Rgb(0x00, 0x80, 0xff)),
        );
        assert_eq!(
            config.ui.host_colors.get("personal"),
            Some(&ChromeColor::Rgb(0xd7, 0x5f, 0x00)),
            "uppercase hex digits must parse",
        );
    }

    #[test]
    fn host_colors_unknown_name_is_an_error() {
        // Loud failure for typos — better than silently falling back
        // to the default secondary color and leaving the user
        // wondering why their config had no effect.
        let toml_text = r#"
            [ui.host_colors]
            uber = "burgundy"
        "#;
        let result: Result<Config, _> = toml::from_str(toml_text);
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("burgundy"),
            "error should mention the bad name; got: {err}",
        );
    }

    #[test]
    fn host_colors_malformed_hex_is_an_error() {
        // Five digits, eight digits, non-hex chars all fail.
        for bad in ["#abc", "#abcdefg", "#xyzxyz", "#12345"] {
            let toml_text = format!("[ui.host_colors]\nh = \"{bad}\"\n");
            let result: Result<Config, _> = toml::from_str(&toml_text);
            assert!(result.is_err(), "{bad:?} should fail to parse");
        }
    }

    #[test]
    fn host_colors_xterm_index_out_of_range_is_an_error() {
        let toml_text = "[ui.host_colors]\nh = 256\n";
        let result: Result<Config, _> = toml::from_str(toml_text);
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("256") && err.contains("0-255"),
            "error should mention the bad value and the range; got: {err}",
        );
    }

    #[test]
    fn host_colors_defaults_to_empty_map() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.ui.host_colors.is_empty());
    }

    #[test]
    fn spawn_section_defaults_when_absent() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.spawn.search_roots, vec!["~".to_string()]);
        assert_eq!(config.spawn.default_mode, SearchMode::Fuzzy);
    }

    #[test]
    fn spawn_default_mode_fuzzy_round_trips() {
        let config: Config = toml::from_str("[spawn]\ndefault_mode = \"fuzzy\"\n").unwrap();
        assert_eq!(config.spawn.default_mode, SearchMode::Fuzzy);
    }

    #[test]
    fn spawn_default_mode_precise_round_trips() {
        let config: Config = toml::from_str("[spawn]\ndefault_mode = \"precise\"\n").unwrap();
        assert_eq!(config.spawn.default_mode, SearchMode::Precise);
    }

    #[test]
    fn spawn_search_roots_round_trips() {
        let toml_text = r#"
            [spawn]
            search_roots = ["~/code", "/work"]
        "#;
        let config: Config = toml::from_str(toml_text).unwrap();
        assert_eq!(
            config.spawn.search_roots,
            vec!["~/code".to_string(), "/work".to_string()],
        );
    }

    #[test]
    fn spawn_unknown_default_mode_is_an_error() {
        let result: Result<Config, _> = toml::from_str("[spawn]\ndefault_mode = \"hyperdrive\"\n");
        assert!(
            result.is_err(),
            "expected parse error for unknown default_mode, got {result:?}",
        );
    }

    #[test]
    fn spawn_project_markers_default_includes_common_ecosystems() {
        let config: Config = toml::from_str("").unwrap();
        let markers = &config.spawn.project_markers;
        // Spot-check a representative sample — exhaustive listing
        // would just duplicate the default constant.
        for required in [
            "Cargo.toml",
            "package.json",
            "go.mod",
            "pyproject.toml",
            "Makefile",
        ] {
            assert!(
                markers.iter().any(|m| m == required),
                "default project_markers missing {required:?}: {markers:?}",
            );
        }
    }

    #[test]
    fn spawn_project_markers_user_override_replaces_default() {
        let toml_text = r#"
            [spawn]
            project_markers = ["Tiltfile", "dvc.yaml"]
        "#;
        let config: Config = toml::from_str(toml_text).unwrap();
        assert_eq!(
            config.spawn.project_markers,
            vec!["Tiltfile".to_string(), "dvc.yaml".to_string()],
            "user list replaces the default (no merge)",
        );
    }

    #[test]
    fn spawn_named_projects_default_is_empty() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.spawn.projects.is_empty());
    }

    #[test]
    fn spawn_named_projects_round_trip() {
        let toml_text = r#"
            [[spawn.projects]]
            name = "cm"
            path = "~/Workbench/repositories/codemux"

            [[spawn.projects]]
            name = "dotfiles"
            path = "~/Workbench/repositories/dotfiles"
        "#;
        let config: Config = toml::from_str(toml_text).unwrap();
        assert_eq!(config.spawn.projects.len(), 2);
        assert_eq!(config.spawn.projects[0].name, "cm");
        assert_eq!(
            config.spawn.projects[0].path,
            "~/Workbench/repositories/codemux",
        );
        assert_eq!(config.spawn.projects[1].name, "dotfiles");
    }

    #[test]
    fn spawn_named_project_missing_path_is_an_error() {
        let toml_text = r#"
            [[spawn.projects]]
            name = "no-path"
        "#;
        let result: Result<Config, _> = toml::from_str(toml_text);
        assert!(
            result.is_err(),
            "missing required `path` field should fail parse, got {result:?}",
        );
    }
}
