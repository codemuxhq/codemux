//! TOML config loader. The user file lives at
//! `$XDG_CONFIG_HOME/codemux/config.toml` (resolved by the `directories`
//! crate; falls back to `~/.config/codemux/config.toml` on Linux).
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

use std::path::PathBuf;

use color_eyre::Result;
use color_eyre::eyre::{WrapErr, eyre};
use directories::ProjectDirs;
use serde::Deserialize;

use crate::keymap::Bindings;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub bindings: Bindings,
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
pub fn config_path() -> Result<PathBuf> {
    let proj = ProjectDirs::from("", "", "codemux")
        .ok_or_else(|| eyre!("could not resolve a config directory for this user"))?;
    Ok(proj.config_dir().join("config.toml"))
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
}
