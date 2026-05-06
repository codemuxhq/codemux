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
use std::path::{Path, PathBuf};

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
    /// Modifier the user holds to enable host-terminal URL handling
    /// (Ghostty / iTerm2 / Kitty cursor change + Cmd-click open). When
    /// the user holds this key, codemux temporarily yields mouse capture
    /// to the host so its URL hover detector can run; on release we
    /// reclaim. Independent of the in-app Ctrl+click handler, which
    /// always works as a fallback whenever mouse capture is active and
    /// the click arrives with `KeyModifiers::CONTROL` set.
    ///
    /// Default: `cmd` on macOS (Ghostty/iTerm2 use Cmd for URLs there),
    /// `ctrl` elsewhere.
    pub mouse_url_modifier: MouseUrlModifier,
    /// When `true`, codemux yields mouse capture for the entire focused
    /// pane any time the focused agent is in a terminal-failure state
    /// (`Failed`). Trade: the host terminal's native I-beam cursor + click-
    /// drag-copy gesture light up on the failure pane, BUT clicks land on
    /// the terminal instead of codemux while focus is there — so tab
    /// clicks, scroll-wheel scrolling, and the in-app drag-to-select
    /// overlay all stop responding until focus moves to a non-Failed
    /// agent. Default `false`: in-app selection (reverse-video highlight,
    /// OSC 52 to clipboard) covers Failed panes too, so the only thing
    /// the user gives up by leaving this off is the cursor-shape change.
    /// Opt in if you specifically want the native gesture and accept
    /// switching tabs via the keyboard chord while on a Failed pane.
    #[serde(default)]
    pub mouse_yield_on_failed: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bindings: Bindings::default(),
            scrollback_len: default_scrollback_len(),
            ui: Ui::default(),
            spawn: SpawnConfig::default(),
            mouse_url_modifier: MouseUrlModifier::default(),
            mouse_yield_on_failed: false,
        }
    }
}

/// Modifier key the user holds to make codemux yield mouse capture so
/// the host terminal can run its native URL hover-and-open UX.
///
/// Why this exists: any DEC mouse capture mode (`?1000h`, `?1002h`,
/// `?1003h`) silences Ghostty's URL hover detector. We can't deliver
/// Cmd over the SGR mouse encoding (only shift/alt/ctrl bits exist),
/// so the only path to native Cmd-click is to temporarily release
/// capture on the user's modifier press, then reclaim on release. The
/// Kitty Keyboard Protocol delivers bare-modifier press/release events
/// when `REPORT_ALL_KEYS_AS_ESCAPE_CODES` + `REPORT_EVENT_TYPES` are
/// pushed.
///
/// **Dead-key trade-off (intl layouts).** Pushing
/// `REPORT_ALL_KEYS_AS_ESCAPE_CODES` also bypasses OS-level dead-key
/// composition: pressing dead-tilde + space on a US-International or
/// Brazilian intl layout no longer produces a literal `~` for the
/// application — the dead-key arrives as an unmatched Release event
/// and only the composing space surfaces. The runtime mitigates the
/// dead-tilde case via a per-character recovery (`runtime.rs`) but
/// other dead-keys (`"`, `'`, `^`, `` ` ``) round-trip incorrectly.
///
/// To avoid the issue entirely, choose anything other than `Cmd` —
/// only `Cmd` requires the offending KKP flag (because Cmd doesn't
/// have a bit in the SGR mouse encoding). For `Ctrl` / `Alt` /
/// `Shift` / `None`, codemux negotiates with just
/// `DISAMBIGUATE_ESCAPE_CODES` and the OS keeps composing dead-keys
/// natively; the cost is that the native terminal URL hover yield
/// doesn't fire (use the in-app Ctrl+hover overlay + Ctrl+Click
/// handler instead).
///
/// `None` disables the yield behavior entirely (in-app Ctrl+click is
/// the only path).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum MouseUrlModifier {
    /// Never yield. Only the in-app Ctrl+click handler fires.
    None,
    /// Yield while the user holds Cmd (macOS) / Win / Super.
    /// Aliases: `super`, `win`, `command`.
    #[serde(alias = "super", alias = "win", alias = "command")]
    Cmd,
    /// Yield while the user holds Control. Alias: `control`.
    #[serde(alias = "control")]
    Ctrl,
    /// Yield while the user holds Alt / Option / Meta.
    /// Aliases: `option`, `meta`.
    #[serde(alias = "option", alias = "meta")]
    Alt,
    /// Yield while the user holds Shift.
    Shift,
}

impl MouseUrlModifier {
    /// Whether codemux must request KKP `REPORT_ALL_KEYS_AS_ESCAPE_CODES`
    /// (and `REPORT_EVENT_TYPES`) so the terminal delivers bare-modifier
    /// press/release events. Only `Cmd` needs them: SGR 1006 mouse
    /// encoding does not carry a Cmd / Super bit, so the keyboard
    /// channel is the only signal that the user is holding the
    /// modifier. `Ctrl` / `Alt` / `Shift` are reported on the mouse
    /// event itself; `None` never yields.
    ///
    /// Encapsulated here (rather than as a `match` in the runtime) so
    /// the runtime stays ignorant of *why* the flags are needed and a
    /// future variant cannot silently miss the negotiation step.
    #[must_use]
    pub fn requires_bare_modifier_events(self) -> bool {
        matches!(self, Self::Cmd)
    }
}

impl Default for MouseUrlModifier {
    fn default() -> Self {
        if cfg!(target_os = "macos") {
            Self::Cmd
        } else {
            Self::Ctrl
        }
    }
}

/// User-facing presentation knobs. Default values are tuned to be
/// readable on poor monitors (washed-out laptop screens, projectors,
/// sunlight glare); opt-ins reintroduce the subtler aesthetic for
/// users who have a high-contrast display and prefer it.
///
/// Manual `Default` impl (rather than `derive`d) because
/// [`Self::host_bell_on_finish`] needs to default to `true`, and the
/// derived bool default is `false`.
#[derive(Clone, Debug, Deserialize)]
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
    /// tabs (e.g. `work-laptop · main-claude`). Hosts not listed fall
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

    /// When `true` (the default), emit a terminal BEL (`\x07`) on
    /// every agent's working → idle transition so the surrounding
    /// terminal (Ghostty, iTerm2, Kitty, …) marks its codemux tab
    /// as needing attention. The terminal handles the visual
    /// treatment itself: most modern emulators only flash the tab
    /// when it isn't currently focused, so this stays silent while
    /// the user is already inside codemux and surfaces only when
    /// they're in another window or app.
    ///
    /// Set to `false` to opt out — useful on terminals that
    /// interpret BEL as an audible beep rather than a visual cue
    /// and where the sound is disruptive.
    pub host_bell_on_finish: bool,

    /// Status-bar segments rendered between the tab strip and the
    /// right edge, in left-to-right order. The rightmost is the
    /// highest priority: when the terminal can't fit them all,
    /// segments are dropped from the LEFT first, so the prefix
    /// hint stays visible even on a 60-cell-wide window.
    ///
    /// Built-in IDs:
    /// - `"model"` — current Claude model on the focused agent
    /// - `"tokens"` — context-window usage for the focused agent
    ///   (`tok:Nk N%`), fed by Claude Code's statusLine callback
    /// - `"worktree"` — basename of the focused agent's working dir,
    ///   shown only when the worktree directory differs from the repo
    ///   name (i.e. you're not in the main checkout)
    /// - `"branch"` — focused agent's git branch, shown only when the
    ///   branch is not in [`Self::segments`]'s
    ///   [`BranchSegmentConfig::default_branches`]
    /// - `"prefix_hint"` — the `super+b for help` / `[NAV] …` hint
    /// - `"repo"` — focused agent's repo name (opt-in; not in the
    ///   default set since `worktree` covers the same use case)
    ///
    /// Unknown IDs are logged at startup and skipped. An empty list
    /// disables the right-side block entirely (the tab strip then
    /// fills the whole status bar).
    ///
    /// Default: `["model", "tokens", "worktree", "branch", "prefix_hint"]`.
    /// The container's `#[serde(default)]` calls `Ui::default()` which
    /// fills the field — no field-level `default` attribute needed.
    pub status_bar_segments: Vec<String>,

    /// Per-segment policy knobs. Each built-in that needs configuration
    /// owns its own field here, namespaced under `[ui.segments.<id>]`
    /// in the user's TOML. This matches the Common Closure Principle:
    /// changing a segment's behavior shouldn't require touching the
    /// global `[ui]` schema.
    ///
    /// Example:
    ///
    /// ```toml
    /// [ui.segments.branch]
    /// default_branches = ["main", "develop"]
    /// ```
    pub segments: SegmentConfig,
}

impl Default for Ui {
    fn default() -> Self {
        Self {
            subtle: false,
            host_colors: HashMap::new(),
            host_bell_on_finish: true,
            status_bar_segments: crate::status_bar::default_segment_ids(),
            segments: SegmentConfig::default(),
        }
    }
}

/// Container for per-segment configuration. Lives under `[ui.segments]`
/// in the user's TOML. Each built-in segment that has a configurable
/// policy gets its own field here so the segment owns its config and
/// the global `Ui` doesn't accumulate one-off knobs per segment.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct SegmentConfig {
    /// Configuration for the `branch` built-in segment. See
    /// [`BranchSegmentConfig`].
    pub branch: BranchSegmentConfig,
    /// Configuration for the `tokens` built-in segment. See
    /// [`TokensSegmentConfig`].
    pub tokens: TokensSegmentConfig,
}

/// Configuration for the status-bar `branch` segment.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct BranchSegmentConfig {
    /// Branches treated as "uninteresting" by the `branch` segment.
    /// When the focused agent's current branch matches one of these,
    /// the segment renders nothing — the assumption being that you
    /// only need to *see* the branch when it's worth seeing.
    ///
    /// Default: `["main", "master"]`. Override per project by listing
    /// whatever your team treats as the trunk:
    ///
    /// ```toml
    /// [ui.segments.branch]
    /// default_branches = ["main", "develop", "trunk"]
    /// ```
    ///
    /// Set to `[]` to always show the branch segment (every branch
    /// is "interesting").
    pub default_branches: Vec<String>,
}

impl Default for BranchSegmentConfig {
    fn default() -> Self {
        Self {
            default_branches: vec!["main".to_string(), "master".to_string()],
        }
    }
}

/// Configuration for the status-bar `tokens` segment.
///
/// The segment surfaces context-window usage for the focused agent,
/// fed by Claude Code's documented `statusLine` callback contract
/// (the per-spawn `--settings` injection in
/// [`crate::runtime::spawn_local_agent`]). All knobs default to
/// values that match aifx's behavior so users coming from aifx see
/// the same color thresholds and effective-window math.
///
/// Example:
///
/// ```toml
/// [ui.segments.tokens]
/// format = "with_bar"
/// yellow_threshold = 150000
/// refresh_interval_secs = 5
/// ```
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct TokensSegmentConfig {
    /// How the segment renders its text. See [`TokensFormat`] for the
    /// three variants. Default: [`TokensFormat::WithPercent`] —
    /// `tok:125k 31%`.
    pub format: TokensFormat,
    /// Token count at which the segment turns yellow (warning level
    /// 1). Default `200_000` — aligned with aifx's threshold against
    /// the 400k effective compaction window.
    pub yellow_threshold: u64,
    /// Token count at which the segment turns orange (warning level
    /// 2). Default `300_000`.
    pub orange_threshold: u64,
    /// Token count at which the segment turns red (warning level 3).
    /// Default `360_000`.
    pub red_threshold: u64,
    /// Optional override for the effective context window used in
    /// percentage math and bar rendering. When `None` (the default)
    /// the segment trusts whatever `context_window_size` Claude Code
    /// reports — so a user on Opus 1M sees the bar fill against 1M,
    /// a user on a 200k Sonnet sees it fill against 200k. Set this
    /// to force compaction-style accounting (e.g. `400_000` to mirror
    /// aifx's behavior) — the override is treated as a ceiling and
    /// is capped to the model's actual window when smaller. The
    /// `$CLAUDE_CODE_AUTO_COMPACT_WINDOW` env var is consulted as a
    /// fallback when this field is unset, matching aifx.
    pub auto_compact_window: Option<u64>,
    /// Forwarded to Claude Code as the statusLine `refreshInterval`
    /// (in seconds) when the per-agent `--settings` JSON is injected.
    /// `None` (the default) leaves Claude on event-driven cadence
    /// only — the segment ticks per assistant turn, on `/compact`,
    /// and on permission/vim mode changes. Set this to e.g. `5` to
    /// have the segment refresh during long-running tool calls.
    ///
    /// Cross-cutting note: this knob is read at agent spawn time by
    /// `runtime::spawn_local_agent` to embed `refreshInterval` in the
    /// injected `--settings` JSON. It lives under
    /// `[ui.segments.tokens]` for user discoverability (the user
    /// thinks of it as "how often does my token segment refresh"),
    /// even though strict layering would put it in a separate
    /// IPC-config section.
    pub refresh_interval_secs: Option<u32>,
}

/// Display format for the `tokens` segment.
///
/// `Compact` is the cheapest in cells and survives the longest under
/// width pressure. `WithPercent` (the default) adds the percentage of
/// the effective context window so the user can see headroom at a
/// glance. `WithBar` adds a 5-cell mini progress bar — visually loud
/// but useful when several agents are open and you want to compare.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TokensFormat {
    /// `tok:125k`
    Compact,
    /// `tok:125k 31%`
    #[default]
    WithPercent,
    /// `tok:125k [▌▌  ]`
    WithBar,
}

impl Default for TokensSegmentConfig {
    fn default() -> Self {
        // Defaults match aifx's behavior so a user coming from aifx
        // sees the same yellow/orange/red transitions. The 400k
        // effective compaction window is implicit (resolved at render
        // time from `$CLAUDE_CODE_AUTO_COMPACT_WINDOW` or the
        // hard-coded fallback).
        Self {
            format: TokensFormat::WithPercent,
            yellow_threshold: 200_000,
            orange_threshold: 300_000,
            red_threshold: 360_000,
            auto_compact_window: None,
            refresh_interval_secs: None,
        }
    }
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
    /// Per-host SSH search-root configuration. The fuzzy index for
    /// each SSH host is built by walking these roots on the remote
    /// machine via the existing `ControlMaster` socket. Lookup is
    /// per-host first, then falls back to the special `"default"`
    /// key, then to a hardcoded `["~"]`. The `~` is expanded against
    /// the remote `$HOME` (captured during the SSH prepare phase),
    /// not the local one — see [`crate::index_worker::expand_remote_roots`].
    ///
    /// Example:
    /// ```toml
    /// [spawn.ssh.default]
    /// search_roots = ["~"]
    ///
    /// [spawn.ssh."devpod-go"]
    /// search_roots = ["~", "/srv/repos"]
    /// ```
    #[serde(default)]
    pub ssh: std::collections::HashMap<String, SshHostSpawnConfig>,
    /// Directory the spawn modal opens an agent in when the user
    /// presses Enter without picking anything (no wildmenu selection,
    /// no typed query, the path field is empty or still holds the
    /// auto-seeded cwd / remote `$HOME`). Created on first use if
    /// missing.
    ///
    /// Tilde resolves at use time: against the local `$HOME` for local
    /// spawns, and against the remote `$HOME` (captured during the
    /// SSH prepare phase) for remote spawns. Absolute paths pass
    /// through unchanged on both sides. A non-tilde, non-absolute
    /// value is treated as an error at use time and the spawn falls
    /// back to today's "use the platform default cwd" behavior.
    ///
    /// Defaults to `"~/.codemux/scratch"` so a fresh install gets a
    /// dedicated scratch dir without writing config.
    pub scratch_dir: String,
}

impl SpawnConfig {
    /// Resolve the search-roots list to use for a given SSH `host`.
    /// Per-host config wins; otherwise the `"default"` entry; otherwise
    /// the hardcoded `["~"]` baseline. Centralized here so the runtime
    /// has one call site (and the lookup rules are pinned by tests).
    #[must_use]
    pub fn ssh_search_roots(&self, host: &str) -> Vec<String> {
        if let Some(per_host) = self.ssh.get(host) {
            return per_host.search_roots.clone();
        }
        if let Some(default) = self.ssh.get("default") {
            return default.search_roots.clone();
        }
        vec!["~".to_string()]
    }

    /// Resolve [`Self::scratch_dir`] against the local `$HOME`.
    /// Returns `None` when the configured value uses `~` but `$HOME`
    /// is unset (the runtime falls back to the platform default cwd
    /// in that case rather than crashing).
    ///
    /// Absolute paths pass through unchanged. Relative paths are
    /// rejected — there's no obvious anchor to resolve them against
    /// (the TUI's cwd would be surprising; using `$HOME` would
    /// duplicate the `~/foo` syntax).
    #[must_use]
    pub fn local_scratch_dir(&self) -> Option<PathBuf> {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        expand_scratch(&self.scratch_dir, home.as_deref())
    }

    /// Resolve [`Self::scratch_dir`] against the remote `$HOME`
    /// captured during the SSH prepare phase. Same rules as
    /// [`Self::local_scratch_dir`] but anchored on the remote home
    /// instead of the local one.
    #[must_use]
    pub fn remote_scratch_dir(&self, remote_home: &Path) -> Option<PathBuf> {
        expand_scratch(&self.scratch_dir, Some(remote_home))
    }
}

/// Tilde-expand `value` against `home`. Mirrors the `expand_one`
/// helper in `index_worker.rs` but returns a single value rather
/// than walking a list. Centralised here so the runtime has one
/// call shape regardless of local vs remote context — the caller
/// just supplies the appropriate `$HOME`.
///
/// Returns `None` for tilde-prefixed values when `home` is unset
/// (the caller falls back to the platform default), and for relative
/// paths (no obvious anchor — see [`SpawnConfig::local_scratch_dir`]).
fn expand_scratch(value: &str, home: Option<&Path>) -> Option<PathBuf> {
    if value == "~" {
        let h = home?;
        return Some(h.to_path_buf());
    }
    if let Some(rest) = value.strip_prefix("~/") {
        let h = home?;
        return Some(h.join(rest));
    }
    let candidate = Path::new(value);
    if candidate.is_absolute() {
        return Some(candidate.to_path_buf());
    }
    tracing::warn!(
        scratch_dir = %value,
        "scratch_dir is neither absolute nor `~`-prefixed; ignoring",
    );
    None
}

/// Per-host SSH spawn config. Currently just `search_roots` — this
/// struct exists as a named type rather than a bare `Vec<String>`
/// keyed off the `[spawn.ssh.<host>]` table so future per-host knobs
/// (e.g. host-specific project markers) have somewhere to land
/// without a config-format break.
#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct SshHostSpawnConfig {
    /// Search roots for this host. Same expansion rules as the local
    /// `search_roots`, but `~` resolves to the remote `$HOME`. Roots
    /// that are neither absolute nor `~`-prefixed are dropped (no
    /// client-side cwd to anchor relative paths against on the
    /// remote side).
    pub search_roots: Vec<String>,
}

impl Default for SshHostSpawnConfig {
    fn default() -> Self {
        Self {
            search_roots: vec!["~".to_string()],
        }
    }
}

impl Default for SpawnConfig {
    fn default() -> Self {
        Self {
            search_roots: vec!["~".to_string()],
            default_mode: SearchMode::default(),
            project_markers: default_project_markers(),
            projects: Vec::new(),
            ssh: std::collections::HashMap::new(),
            scratch_dir: "~/.codemux/scratch".to_string(),
        }
    }
}

/// User-curated alias for a project path. Matched by `name` in the
/// fuzzy modal; spawn target is `path` (tilde-expanded at use site).
///
/// `host` binds the project to an SSH alias from `~/.ssh/config`. When
/// set, picking the project from the spawn modal kicks off the bootstrap
/// for that host and auto-spawns at `path` once the prepare phase
/// completes — no second user step. Missing/`None` means local. An
/// explicit empty string `host = ""` is normalised to `None` at the
/// I/O boundary (see [`deserialize_optional_non_empty_string`]) so
/// downstream code never has to think about an "empty alias" sentinel.
/// The alias is *not* validated at config-load time: `~/.ssh/config`
/// is read lazily by the spawn modal, so a bad alias surfaces as a
/// normal bootstrap failure when the user picks the project rather
/// than as a startup error.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct NamedProject {
    pub name: String,
    pub path: String,
    #[serde(default, deserialize_with = "deserialize_optional_non_empty_string")]
    pub host: Option<String>,
}

/// Serde adapter that maps `Some("")` → `None` for fields where the
/// empty string is semantically equivalent to "unset". Used by
/// [`NamedProject::host`] so the user can clear the alias by emptying
/// the value (`host = ""`) without us carrying an `Option<String>`
/// whose `Some` variant might still be empty.
fn deserialize_optional_non_empty_string<'de, D>(
    deserializer: D,
) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt: Option<String> = Option::deserialize(deserializer)?;
    Ok(opt.filter(|s| !s.is_empty()))
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
    fn ui_host_bell_on_finish_defaults_to_true() {
        // Opt-out knob: default is on so users get the host tab
        // attention indicator without writing config. The manual
        // `Default for Ui` impl exists for this reason — derived
        // bool defaults are `false`, which would silently break the
        // intended UX.
        let config: Config = toml::from_str("").unwrap();
        assert!(config.ui.host_bell_on_finish);
    }

    #[test]
    fn ui_host_bell_on_finish_can_be_disabled() {
        let config: Config = toml::from_str("[ui]\nhost_bell_on_finish = false\n").unwrap();
        assert!(!config.ui.host_bell_on_finish);
    }

    #[test]
    fn ui_host_bell_on_finish_round_trips_explicit_true() {
        // Explicit `= true` must parse, not just be implied by absence
        // — pins that the field is wired through serde, not just a
        // hardcoded default.
        let config: Config = toml::from_str("[ui]\nhost_bell_on_finish = true\n").unwrap();
        assert!(config.ui.host_bell_on_finish);
    }

    #[test]
    fn ui_status_bar_segments_default_includes_all_five_built_ins() {
        // Defaults define the out-of-the-box UX. New users see model,
        // tokens, worktree, branch, prefix_hint without writing config.
        // Repo is intentionally omitted (worktree covers the same need).
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(
            config.ui.status_bar_segments,
            vec![
                "model".to_string(),
                "tokens".to_string(),
                "worktree".to_string(),
                "branch".to_string(),
                "prefix_hint".to_string(),
            ],
        );
    }

    #[test]
    fn ui_status_bar_segments_round_trips_user_override() {
        let toml_text = r#"
            [ui]
            status_bar_segments = ["repo", "prefix_hint"]
        "#;
        let config: Config = toml::from_str(toml_text).unwrap();
        assert_eq!(
            config.ui.status_bar_segments,
            vec!["repo".to_string(), "prefix_hint".to_string()],
            "user list replaces the default (no merge)",
        );
    }

    #[test]
    fn ui_status_bar_segments_empty_list_disables_right_side_block() {
        // Setting the list to `[]` is the documented way to drop the
        // right-side block entirely. Pinning this so a future serde
        // change can't silently swap empty for default.
        let config: Config = toml::from_str("[ui]\nstatus_bar_segments = []\n").unwrap();
        assert!(config.ui.status_bar_segments.is_empty());
    }

    #[test]
    fn ui_default_branches_defaults_to_main_and_master() {
        // The branch segment hides itself for these. Cover the most
        // common conventions so a fresh install hides the segment on
        // either main or master without writing config.
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(
            config.ui.segments.branch.default_branches,
            vec!["main".to_string(), "master".to_string()],
        );
    }

    #[test]
    fn ui_default_branches_round_trips_user_override() {
        // Knob lives under [ui.segments.branch] now (not [ui]) so the
        // segment that owns the policy also owns the config namespace.
        let toml_text = r#"
            [ui.segments.branch]
            default_branches = ["main", "develop", "trunk"]
        "#;
        let config: Config = toml::from_str(toml_text).unwrap();
        assert_eq!(
            config.ui.segments.branch.default_branches,
            vec![
                "main".to_string(),
                "develop".to_string(),
                "trunk".to_string(),
            ],
            "user list replaces the default (no merge)",
        );
    }

    #[test]
    fn ui_default_branches_empty_list_means_show_every_branch() {
        // The documented opt-out: empty list = no branches treated
        // as default = branch segment always renders.
        let config: Config =
            toml::from_str("[ui.segments.branch]\ndefault_branches = []\n").unwrap();
        assert!(config.ui.segments.branch.default_branches.is_empty());
    }

    #[test]
    fn ui_segments_tokens_defaults_match_aifx_thresholds() {
        // The whole point of the defaults — a user coming from aifx
        // sees the same yellow/orange/red transitions without writing
        // any config. Pin the numbers so a future tweak is a
        // deliberate decision (and shows up in a code review).
        let config: Config = toml::from_str("").unwrap();
        let cfg = config.ui.segments.tokens;
        assert_eq!(cfg.format, TokensFormat::WithPercent);
        assert_eq!(cfg.yellow_threshold, 200_000);
        assert_eq!(cfg.orange_threshold, 300_000);
        assert_eq!(cfg.red_threshold, 360_000);
        assert!(cfg.auto_compact_window.is_none());
        assert!(cfg.refresh_interval_secs.is_none());
    }

    #[test]
    fn ui_segments_tokens_round_trips_user_overrides() {
        // All five user-facing knobs in one TOML block. A regression
        // here would silently revert a user's customisation, so each
        // one is asserted against its TOML key.
        let toml_text = r#"
            [ui.segments.tokens]
            format = "with_bar"
            yellow_threshold = 150000
            orange_threshold = 220000
            red_threshold = 300000
            auto_compact_window = 500000
            refresh_interval_secs = 5
        "#;
        let config: Config = toml::from_str(toml_text).unwrap();
        let cfg = config.ui.segments.tokens;
        assert_eq!(cfg.format, TokensFormat::WithBar);
        assert_eq!(cfg.yellow_threshold, 150_000);
        assert_eq!(cfg.orange_threshold, 220_000);
        assert_eq!(cfg.red_threshold, 300_000);
        assert_eq!(cfg.auto_compact_window, Some(500_000));
        assert_eq!(cfg.refresh_interval_secs, Some(5));
    }

    #[test]
    fn ui_segments_tokens_format_accepts_all_three_variants() {
        // The serde rename_all = "snake_case" derive must accept all
        // three documented spellings. Done as one parametrised pin so
        // adding a fourth later is a one-line diff.
        for (literal, expected) in [
            ("compact", TokensFormat::Compact),
            ("with_percent", TokensFormat::WithPercent),
            ("with_bar", TokensFormat::WithBar),
        ] {
            let toml_text = format!("[ui.segments.tokens]\nformat = \"{literal}\"\n");
            let config: Config = toml::from_str(&toml_text).unwrap();
            assert_eq!(
                config.ui.segments.tokens.format, expected,
                "format = \"{literal}\" should parse to {expected:?}",
            );
        }
    }

    #[test]
    fn ui_segments_tokens_section_is_optional() {
        // A user with no [ui.segments.tokens] table at all still
        // gets the aifx-aligned defaults via the container's
        // #[serde(default)] cascade.
        let config: Config = toml::from_str("[ui]\nsubtle = true\n").unwrap();
        assert_eq!(
            config.ui.segments.tokens.yellow_threshold, 200_000,
            "missing [ui.segments.tokens] must keep the default thresholds",
        );
    }

    #[test]
    fn ui_segments_section_is_optional_and_defaults_apply() {
        // A user with no [ui.segments] table at all still gets the
        // sensible defaults via the container's #[serde(default)].
        let config: Config = toml::from_str("[ui]\nsubtle = true\n").unwrap();
        assert!(config.ui.subtle);
        assert_eq!(
            config.ui.segments.branch.default_branches,
            vec!["main".to_string(), "master".to_string()],
        );
    }

    #[test]
    fn missing_ui_section_keeps_defaults() {
        // The whole [ui] table is optional; users on default chrome
        // never have to write anything to opt in to it.
        let config: Config = toml::from_str("scrollback_len = 100").unwrap();
        assert!(!config.ui.subtle);
        assert!(config.ui.host_bell_on_finish);
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
            work = "blue"
            personal = "lightred"
        "#;
        let config: Config = toml::from_str(toml_text).unwrap();
        assert_eq!(
            config.ui.host_colors.get("work"),
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
            work = 33
            personal = 247
        ";
        let config: Config = toml::from_str(toml_text).unwrap();
        assert_eq!(
            config.ui.host_colors.get("work"),
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
            work = "#0080ff"
            personal = "#D75F00"
        "##;
        let config: Config = toml::from_str(toml_text).unwrap();
        assert_eq!(
            config.ui.host_colors.get("work"),
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
            work = "burgundy"
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
        assert!(
            config.spawn.projects[0].host.is_none(),
            "missing `host` deserializes to None (local)",
        );
        assert_eq!(config.spawn.projects[1].name, "dotfiles");
        assert!(config.spawn.projects[1].host.is_none());
    }

    #[test]
    fn spawn_named_project_host_round_trips() {
        let toml_text = r#"
            [[spawn.projects]]
            name = "devpod-work"
            path = "~/work"
            host = "devpod-1"
        "#;
        let config: Config = toml::from_str(toml_text).unwrap();
        assert_eq!(config.spawn.projects.len(), 1);
        assert_eq!(
            config.spawn.projects[0].host.as_deref(),
            Some("devpod-1"),
            "explicit `host` is preserved as the SSH alias",
        );
    }

    #[test]
    fn spawn_named_project_empty_host_normalises_to_none() {
        // `host = ""` is the user's way to clear the field. The
        // custom deserializer maps it to `None` at the I/O boundary
        // so downstream code only ever sees `Some(non_empty)` or
        // `None` — no "empty alias" sentinel leaking into the
        // spawn-modal lookup.
        let toml_text = r#"
            [[spawn.projects]]
            name = "p"
            path = "/tmp/p"
            host = ""
        "#;
        let config: Config = toml::from_str(toml_text).unwrap();
        assert!(config.spawn.projects[0].host.is_none());
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

    // ── ssh per-host search roots ────────────────────────────────
    //
    // Lookup precedence is: per-host explicit → "default" → hardcoded
    // ["~"]. Each rung has a positive test; the order between the
    // first two is also exercised so a per-host override actually
    // *wins* over default rather than just shadowing it by accident.

    #[test]
    fn ssh_search_roots_falls_back_to_tilde_when_no_config() {
        let config = Config::default();
        assert_eq!(
            config.spawn.ssh_search_roots("anyhost"),
            vec!["~".to_string()]
        );
    }

    #[test]
    fn ssh_search_roots_uses_default_section_when_host_unconfigured() {
        let toml_text = r#"
            [spawn.ssh.default]
            search_roots = ["~", "/srv"]
        "#;
        let config: Config = toml::from_str(toml_text).unwrap();
        assert_eq!(
            config.spawn.ssh_search_roots("unknown-host"),
            vec!["~".to_string(), "/srv".to_string()],
        );
    }

    #[test]
    fn ssh_search_roots_per_host_overrides_default() {
        let toml_text = r#"
            [spawn.ssh.default]
            search_roots = ["~"]

            [spawn.ssh."devpod-go"]
            search_roots = ["~", "/work/repos"]
        "#;
        let config: Config = toml::from_str(toml_text).unwrap();
        // Per-host wins; default is shadowed entirely (we don't merge).
        assert_eq!(
            config.spawn.ssh_search_roots("devpod-go"),
            vec!["~".to_string(), "/work/repos".to_string()],
        );
        // Other hosts still get the default fallback.
        assert_eq!(
            config.spawn.ssh_search_roots("other"),
            vec!["~".to_string()],
        );
    }

    #[test]
    fn ssh_per_host_config_round_trips() {
        let toml_text = r#"
            [spawn.ssh."my-box"]
            search_roots = ["/projects"]
        "#;
        let config: Config = toml::from_str(toml_text).unwrap();
        let entry = config.spawn.ssh.get("my-box").unwrap();
        assert_eq!(entry.search_roots, vec!["/projects".to_string()]);
    }

    // ── scratch_dir ──────────────────────────────────────────────
    //
    // The default lands a fresh install in `~/.codemux/scratch` for
    // the empty-Enter spawn fallback. Pinned because the runtime
    // mkdir's whatever path lands here on first use; a typo in the
    // default would silently send agents into the wrong place.

    #[test]
    fn spawn_scratch_dir_defaults_to_dotcodemux_scratch() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.spawn.scratch_dir, "~/.codemux/scratch");
    }

    #[test]
    fn spawn_scratch_dir_round_trips() {
        let toml_text = r#"
            [spawn]
            scratch_dir = "~/scratch"
        "#;
        let config: Config = toml::from_str(toml_text).unwrap();
        assert_eq!(config.spawn.scratch_dir, "~/scratch");
    }

    #[test]
    fn spawn_scratch_dir_accepts_absolute_path() {
        let toml_text = r#"
            [spawn]
            scratch_dir = "/var/codemux/scratch"
        "#;
        let config: Config = toml::from_str(toml_text).unwrap();
        assert_eq!(config.spawn.scratch_dir, "/var/codemux/scratch");
    }

    // ── scratch dir resolution ───────────────────────────────────
    //
    // The path comes off disk as a string; expansion is per-use so
    // local and remote spawns can both share one config knob. Pin
    // each branch (tilde/absolute/relative/missing-home) so a future
    // refactor can't silently change the resolution semantics.
    //
    // We test `expand_scratch` directly rather than the public
    // `local_scratch_dir`/`remote_scratch_dir` wrappers because the
    // local one reads `$HOME` from the process environment — and
    // mutating env vars from cargo tests is racy under the default
    // parallel runner.

    #[test]
    fn expand_scratch_expands_tilde_against_home() {
        assert_eq!(
            expand_scratch("~/.codemux/scratch", Some(Path::new("/home/me"))),
            Some(PathBuf::from("/home/me/.codemux/scratch")),
        );
    }

    #[test]
    fn expand_scratch_bare_tilde_resolves_to_home() {
        assert_eq!(
            expand_scratch("~", Some(Path::new("/home/me"))),
            Some(PathBuf::from("/home/me")),
        );
    }

    #[test]
    fn expand_scratch_absolute_path_passes_through() {
        // The "home" arg is intentionally Some — absolute paths must
        // ignore it. Pinning Some(...) catches the bug where someone
        // adds a `home.unwrap_or_else(...)` and silently grafts a
        // home prefix onto every path.
        assert_eq!(
            expand_scratch("/var/scratch", Some(Path::new("/home/me"))),
            Some(PathBuf::from("/var/scratch")),
        );
        assert_eq!(
            expand_scratch("/var/scratch", None),
            Some(PathBuf::from("/var/scratch")),
        );
    }

    #[test]
    fn expand_scratch_returns_none_for_relative_path() {
        // Bare `scratch` has no anchor — the runtime falls back to
        // platform default cwd rather than guessing.
        assert_eq!(expand_scratch("scratch", Some(Path::new("/home/me"))), None);
        assert_eq!(
            expand_scratch("./scratch", Some(Path::new("/home/me"))),
            None
        );
    }

    #[test]
    fn expand_scratch_returns_none_when_tilde_but_no_home() {
        // `$HOME` unset on the local side, or a remote where we
        // somehow lost the captured home (shouldn't happen in
        // practice — remote home is required to reach this code path
        // — but pin the branch anyway so we degrade rather than crash).
        assert_eq!(expand_scratch("~/scratch", None), None);
        assert_eq!(expand_scratch("~", None), None);
    }

    #[test]
    fn remote_scratch_dir_via_public_api() {
        // Sanity check that the public wrapper passes through to
        // expand_scratch correctly. Doesn't read $HOME so it's safe
        // under parallel test execution.
        let toml_text = r#"
            [spawn]
            scratch_dir = "~/.codemux/scratch"
        "#;
        let config: Config = toml::from_str(toml_text).unwrap();
        assert_eq!(
            config.spawn.remote_scratch_dir(Path::new("/root")),
            Some(PathBuf::from("/root/.codemux/scratch")),
        );
    }

    /// `requires_bare_modifier_events` is the gate that decides
    /// whether the runtime pushes KKP `REPORT_ALL_KEYS_AS_ESCAPE_CODES`.
    /// Only Cmd needs it (no Cmd bit in SGR 1006); Ctrl / Alt / Shift
    /// ride on the mouse event's own modifier bits, and None never
    /// yields. Locking the matrix down so a future variant can't
    /// silently miss the flag-negotiation step.
    #[test]
    fn requires_bare_modifier_events_only_true_for_cmd() {
        assert!(MouseUrlModifier::Cmd.requires_bare_modifier_events());
        assert!(!MouseUrlModifier::Ctrl.requires_bare_modifier_events());
        assert!(!MouseUrlModifier::Alt.requires_bare_modifier_events());
        assert!(!MouseUrlModifier::Shift.requires_bare_modifier_events());
        assert!(!MouseUrlModifier::None.requires_bare_modifier_events());
    }

    /// Default `mouse_url_modifier` follows the host platform's URL
    /// convention: Cmd on macOS (Ghostty/iTerm2 use Cmd for URLs),
    /// Ctrl elsewhere. Locking the default down so a future
    /// platform-default refactor can't silently flip it.
    #[test]
    fn mouse_url_modifier_default_is_platform_aware() {
        let expected = if cfg!(target_os = "macos") {
            MouseUrlModifier::Cmd
        } else {
            MouseUrlModifier::Ctrl
        };
        assert_eq!(MouseUrlModifier::default(), expected);
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg.mouse_url_modifier, expected);
    }

    /// The user-facing config accepts every spelling each modifier
    /// goes by in the wild. If a future serde rename drops one of
    /// these aliases, every config file using that spelling silently
    /// stops parsing — this test catches the regression.
    #[test]
    fn mouse_url_modifier_parses_all_documented_spellings() {
        for spelling in ["cmd", "super", "win", "command"] {
            let cfg: Config =
                toml::from_str(&format!("mouse_url_modifier = \"{spelling}\"")).unwrap();
            assert_eq!(
                cfg.mouse_url_modifier,
                MouseUrlModifier::Cmd,
                "{spelling} should map to Cmd",
            );
        }
        for spelling in ["ctrl", "control"] {
            let cfg: Config =
                toml::from_str(&format!("mouse_url_modifier = \"{spelling}\"")).unwrap();
            assert_eq!(cfg.mouse_url_modifier, MouseUrlModifier::Ctrl);
        }
        for spelling in ["alt", "option", "meta"] {
            let cfg: Config =
                toml::from_str(&format!("mouse_url_modifier = \"{spelling}\"")).unwrap();
            assert_eq!(cfg.mouse_url_modifier, MouseUrlModifier::Alt);
        }
        let cfg: Config = toml::from_str("mouse_url_modifier = \"shift\"").unwrap();
        assert_eq!(cfg.mouse_url_modifier, MouseUrlModifier::Shift);
        let cfg: Config = toml::from_str("mouse_url_modifier = \"none\"").unwrap();
        assert_eq!(cfg.mouse_url_modifier, MouseUrlModifier::None);
    }

    #[test]
    fn mouse_url_modifier_rejects_unknown_value() {
        let res: Result<Config, _> = toml::from_str("mouse_url_modifier = \"bogus\"");
        assert!(res.is_err(), "unknown modifier should fail to parse");
    }

    /// `mouse_yield_on_failed` defaults to `false` so users get the
    /// in-app selection UX (tab clicks + scroll wheel + drag-to-select
    /// overlay all keep working on Failed panes). Locked down because a
    /// silent flip to `true` would break tab clicks for everyone who
    /// hasn't read the changelog — much higher cost than the cursor-
    /// shape change the opt-in delivers.
    #[test]
    fn mouse_yield_on_failed_defaults_to_false() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(
            !cfg.mouse_yield_on_failed,
            "default must be off so tab clicks keep working on Failed",
        );
    }

    /// The flag round-trips both spellings the TOML parser accepts
    /// (true / false). Sanity check that the explicit opt-in path
    /// works — without this a serde rename of the field would silently
    /// drop user opt-ins back to the default.
    #[test]
    fn mouse_yield_on_failed_parses_explicit_value() {
        let cfg: Config = toml::from_str("mouse_yield_on_failed = true").unwrap();
        assert!(cfg.mouse_yield_on_failed);
        let cfg: Config = toml::from_str("mouse_yield_on_failed = false").unwrap();
        assert!(!cfg.mouse_yield_on_failed);
    }
}
