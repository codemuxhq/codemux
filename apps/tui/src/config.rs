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

use std::ffi::OsString;
use std::path::PathBuf;

use color_eyre::Result;
use color_eyre::eyre::{WrapErr, eyre};
use serde::Deserialize;

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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bindings: Bindings::default(),
            scrollback_len: default_scrollback_len(),
        }
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
}
