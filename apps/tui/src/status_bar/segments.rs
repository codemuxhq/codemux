//! Built-in status-bar segments. Each is a tiny stateless type that
//! reads its data off [`SegmentCtx`] and renders a styled `Line`.
//!
//! Adding a new segment: implement [`StatusSegment`], add a unique ID
//! constant in `mod.rs`, register it in `build_segments`, and document
//! it in the `Ui::status_bar_segments` doc comment.
//!
//! Each segment returns a `Line` whose **spans** carry the style
//! directly. We do not use `Line::styled(...)` — that style lives on
//! the line wrapper, and `render_segments` flattens per-segment lines
//! into a single line by extracting the spans, which would drop the
//! line-level style and render the text in the terminal default
//! color. Style on the span survives the extraction.
//!
//! ## Visibility is the segment's call
//!
//! Segments returning `None` from `render` is the universal "skip
//! me this frame" mechanism — that's what makes the segment list
//! pluggable (`Box<dyn StatusSegment>`) without a separate
//! `should_render` method. Built-ins use this to hide themselves
//! when their value is the project default:
//!
//! - [`WorktreeSegment`] hides when the cwd basename matches the
//!   repo name (you're in the main checkout, no worktree to
//!   announce).
//! - [`BranchSegment`] hides when the branch is in its instance
//!   `default_branches` (you're on the trunk).
//!
//! New segments inherit the same option: any custom logic for "should
//! I render right now?" goes in `render` and returns `None` to opt
//! out. The renderer collapses the slot cleanly — no separator drawn,
//! no width consumed.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::{SegmentCtx, StatusSegment};
use crate::runtime::PrefixState;

// ─── ModelSegment ─────────────────────────────────────────────────

/// Renders the user's currently-selected Claude model paired with the
/// reasoning effort level. Both fields are read by
/// [`crate::agent_meta_worker`] from `~/.claude/settings.json` —
/// the only architecturally-sanctioned place to peek at claude
/// state, see AD-1's amended prose in `docs/architecture.md`.
///
/// Display:
///
/// - `model:opus-4-7` when effort is default (or absent)
/// - `model:opus-4-7 [xhigh]` when effort is non-default
///
/// The model alias is run through [`shorten_model_name`] for display:
/// `claude-opus-4-7` → `opus-4-7`, `opus[1m]` → `opus`. Pass-through
/// for anything we don't recognise (better to show the raw alias than
/// guess).
pub(crate) struct ModelSegment;

impl StatusSegment for ModelSegment {
    fn id(&self) -> &'static str {
        super::SEGMENT_MODEL
    }

    fn render(&self, ctx: &SegmentCtx<'_>) -> Option<Line<'static>> {
        let me = ctx.agent.model_effort?;
        let short = shorten_model_name(&me.model);
        // Effort badge is suppressed when the level is "default" or
        // "medium" (claude's default). Otherwise we render a
        // bracketed badge so the user knows they're on a non-default
        // setting at a glance — this mirrors `BranchSegment` hiding
        // when on the trunk.
        let badge = me.effort.as_deref().and_then(format_effort_badge);
        let text = match badge {
            Some(b) => format!("model:{short} [{b}]"),
            None => format!("model:{short}"),
        };
        Some(Line::from(Span::styled(text, ctx.secondary)))
    }
}

/// Strip the `claude-` prefix and the trailing `[<digits><a-z>]`
/// context-window suffix that claude code's aliases carry. So:
///
/// - `claude-opus-4-7`     → `opus-4-7`
/// - `claude-opus-4-7[1m]` → `opus-4-7`
/// - `opus[1m]`            → `opus`
/// - `sonnet`              → `sonnet`
///
/// The bracketed suffix is the user's chosen context-window option
/// (`1m` for 1M tokens, etc.) and is surfaced separately in the
/// `/model` picker UI; it doesn't add information once you know
/// which model you picked. Anything else (custom names, non-Anthropic
/// models reached through a proxy) passes through unchanged.
#[must_use]
fn shorten_model_name(raw: &str) -> &str {
    let no_prefix = raw.strip_prefix("claude-").unwrap_or(raw);
    strip_bracketed_suffix(no_prefix)
}

/// Return the input with a trailing `[<digits><a-z>+]` suffix removed
/// (`opus[1m]` → `opus`). Returns the input unchanged when there's
/// no such suffix, when the bracket payload doesn't look like a
/// context-window code, or when stripping would leave an empty
/// string. Pulled out of [`shorten_model_name`] so the predicate
/// is testable in isolation and re-usable for any future suffix
/// claude introduces (`[200k]`, `[2m]`, etc.).
#[must_use]
fn strip_bracketed_suffix(s: &str) -> &str {
    let Some(rest) = s.strip_suffix(']') else {
        return s;
    };
    let Some((base, suffix)) = rest.rsplit_once('[') else {
        return s;
    };
    if base.is_empty() {
        return s;
    }
    // Single pass: split the suffix at the first non-digit. The
    // prefix part must be non-empty (saw a digit), the trailing
    // part must be non-empty and all-lowercase-or-digit (saw the
    // unit code). Rejects `opus[]`, `opus[ABC]`, and friends —
    // we'd rather pass them through and surface the weirdness
    // than silently swallow it.
    let split_idx = suffix
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(suffix.len());
    let (digits, letters) = suffix.split_at(split_idx);
    let saw_digit = !digits.is_empty();
    let saw_alpha = !letters.is_empty()
        && letters
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    if saw_digit && saw_alpha { base } else { s }
}

/// Decide whether to render an effort badge, and what its text
/// should be (without brackets — those are added by `ModelSegment`
/// during composition). Returns `None` when the effort is default;
/// `Some(borrowed_text)` otherwise. The default set is intentionally
/// narrow — `"medium"` is claude's documented default and `"default"`
/// is the legacy spelling — anything else is shown verbatim so a
/// future `"max"` or `"thinking"` flag surfaces immediately. The
/// borrowed return avoids a per-frame allocation in the hot render
/// path; bracketing is folded into the segment's single `format!`.
#[must_use]
fn format_effort_badge(effort: &str) -> Option<&str> {
    match effort {
        "" | "medium" | "default" => None,
        other => Some(other),
    }
}

// ─── RepoSegment ──────────────────────────────────────────────────

/// Renders the focused agent's repo name (git root basename or cwd
/// basename). Sourced directly from [`SegmentCtx::repo`], which
/// mirrors `RuntimeAgent.repo`. Not in the default segment list as
/// of v1 — `WorktreeSegment` covers the same use case more directly
/// — but kept available for users who want to opt in via
/// `[ui] status_bar_segments`.
pub(crate) struct RepoSegment;

impl StatusSegment for RepoSegment {
    fn id(&self) -> &'static str {
        super::SEGMENT_REPO
    }

    fn render(&self, ctx: &SegmentCtx<'_>) -> Option<Line<'static>> {
        let repo = ctx.agent.repo?;
        Some(Line::from(Span::styled(
            format!("repo:{repo}"),
            ctx.secondary,
        )))
    }
}

// ─── WorktreeSegment ──────────────────────────────────────────────

/// Renders the basename of the focused agent's working directory as
/// `wt:<name>` — but **only when it differs from the repo name**. In
/// a regular checkout the cwd basename equals the repo basename and
/// the segment renders nothing (no use restating the repo). In a git
/// worktree (e.g. `~/Workbench/worktrees/feature-x`) the cwd
/// basename is the worktree directory's name, which is the value
/// worth surfacing.
pub(crate) struct WorktreeSegment;

impl StatusSegment for WorktreeSegment {
    fn id(&self) -> &'static str {
        super::SEGMENT_WORKTREE
    }

    fn render(&self, ctx: &SegmentCtx<'_>) -> Option<Line<'static>> {
        let cwd = ctx.agent.cwd_basename?;
        // In the main checkout, cwd basename matches the repo name —
        // the worktree label would just restate the repo. Skip.
        if ctx.agent.repo == Some(cwd) {
            return None;
        }
        Some(Line::from(Span::styled(format!("wt:{cwd}"), ctx.secondary)))
    }
}

// ─── BranchSegment ────────────────────────────────────────────────

/// Renders the focused agent's git branch as `branch:<name>` — but
/// **only when the branch is not in [`Self::default_branches`]**
/// (typically `main` / `master`). The whole point of the segment is
/// to flag "you're not on the trunk;" once you're back on it, the
/// label would be ambient noise.
///
/// The hide-list is held on the segment instance (rather than passed
/// per-frame via [`SegmentCtx`]) so segment policy travels with the
/// segment. Setting up another built-in with its own config follows
/// the same shape: add a field, take it in `new`, store it.
///
/// Data is supplied by [`crate::agent_meta_worker`] which reads
/// `<cwd>/.git/HEAD`. `None` outside a git repo.
pub(crate) struct BranchSegment {
    default_branches: Vec<String>,
}

impl BranchSegment {
    /// Construct with the user-configured list of branches treated as
    /// "default" (typically `main` / `master`). The segment hides
    /// itself when the focused agent's branch matches one of these.
    pub(crate) fn new(default_branches: Vec<String>) -> Self {
        Self { default_branches }
    }
}

impl StatusSegment for BranchSegment {
    fn id(&self) -> &'static str {
        super::SEGMENT_BRANCH
    }

    fn render(&self, ctx: &SegmentCtx<'_>) -> Option<Line<'static>> {
        let branch = ctx.agent.branch?;
        if self.default_branches.iter().any(|b| b == branch) {
            return None;
        }
        Some(Line::from(Span::styled(
            format!("branch:{branch}"),
            ctx.secondary,
        )))
    }
}

// ─── TokenSegment ─────────────────────────────────────────────────

/// Renders the focused agent's context-window usage as
/// `tok:<count> <pct>%` (or one of two other configurable formats —
/// see [`crate::config::TokensFormat`]). Color-coded against
/// configurable thresholds so the user notices context pressure at a
/// glance:
///
/// - default (below `yellow_threshold`)  → ambient (chrome.secondary)
/// - at/above `yellow_threshold`         → yellow
/// - at/above `orange_threshold`         → orange (256-color 208)
/// - at/above `red_threshold`            → red
///
/// Default thresholds (200k/300k/360k against a 400k effective
/// compaction window) match aifx so a user coming from aifx sees the
/// same transitions without writing config.
///
/// Data is supplied by [`crate::agent_meta_worker`] which reads the
/// per-agent statusLine JSON snapshot written by the
/// `codemux statusline-tee` subcommand (one file per `AgentId`, written
/// atomically by Claude Code's own statusLine callback). See
/// `apps/tui/src/statusline_ipc.rs` for the on-disk layout and the
/// AD-1 carve-out rationale.
pub(crate) struct TokenSegment {
    cfg: crate::config::TokensSegmentConfig,
}

impl TokenSegment {
    /// Construct with the user-configured policy (format choice,
    /// color thresholds, optional auto-compact window override).
    pub(crate) fn new(cfg: crate::config::TokensSegmentConfig) -> Self {
        Self { cfg }
    }

    /// Resolve the effective context window used in percentage math
    /// and bar rendering. Production wrapper around
    /// [`Self::effective_window_pure`] that reads the
    /// `$CLAUDE_CODE_AUTO_COMPACT_WINDOW` env var. Tests call the
    /// pure version directly so they don't depend on the test
    /// environment's env state (the user's shell may have it set
    /// from aifx, or any other tool that injects it).
    fn effective_window(&self, context_window_size: u64) -> u64 {
        let env_window = std::env::var("CLAUDE_CODE_AUTO_COMPACT_WINDOW")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|v| *v > 0);
        Self::effective_window_pure(
            self.cfg.auto_compact_window,
            env_window,
            context_window_size,
        )
    }

    /// Pure resolution of the effective context window. Mirrors
    /// aifx's `effectiveContextWindow` exactly:
    ///
    /// - With no override (config OR env) → trust the model's
    ///   reported `context_window_size`. A user on Opus 1M sees the
    ///   bar fill against 1M, not against a hidden 400k floor.
    /// - With an override (config wins over env) → use it, but cap
    ///   to `context_window_size` when smaller. The override is the
    ///   auto-compact ceiling, not a floor; if your model can only
    ///   do 200k there's no point pretending the ceiling is 400k.
    /// - With no override AND no reported window → fall back to 400k
    ///   as a last-resort guard so the percentage isn't 0/0.
    #[must_use]
    fn effective_window_pure(
        configured: Option<u64>,
        env_window: Option<u64>,
        context_window_size: u64,
    ) -> u64 {
        let override_window = configured.filter(|v| *v > 0).or(env_window);
        match (override_window, context_window_size > 0) {
            (Some(override_w), true) => override_w.min(context_window_size),
            (Some(override_w), false) => override_w,
            (None, true) => context_window_size,
            (None, false) => 400_000,
        }
    }

    /// Color picker keyed on the **headline** token count (input +
    /// output + cache). Returns `None` for the green/below-threshold
    /// bucket so the caller can fall back to the chrome's ambient
    /// secondary style — keeps the segment looking like the rest of
    /// the bar at idle and drawing the eye only when there's actual
    /// pressure.
    fn color_for(&self, total_tokens: u64) -> Option<Color> {
        if total_tokens >= self.cfg.red_threshold {
            Some(Color::Red)
        } else if total_tokens >= self.cfg.orange_threshold {
            Some(Color::Indexed(208))
        } else if total_tokens >= self.cfg.yellow_threshold {
            Some(Color::Yellow)
        } else {
            None
        }
    }
}

impl StatusSegment for TokenSegment {
    fn id(&self) -> &'static str {
        super::SEGMENT_TOKENS
    }

    fn render(&self, ctx: &SegmentCtx<'_>) -> Option<Line<'static>> {
        let usage = ctx.agent.token_usage?;
        let cache_total = usage.cache_creation.saturating_add(usage.cache_read);
        let total = usage
            .input
            .saturating_add(usage.output)
            .saturating_add(cache_total);
        if total == 0 {
            // First turn hasn't reported any tokens yet. Render
            // nothing — the slot collapses cleanly until the second
            // turn lands a non-zero count.
            return None;
        }
        // Numerator for the percentage uses the same definition aifx
        // does: input + cache_creation + cache_read (excludes output,
        // which doesn't contribute to the next turn's prompt
        // budget).
        let context_used = usage
            .input
            .saturating_add(usage.cache_creation)
            .saturating_add(usage.cache_read);
        let window = self.effective_window(usage.context_window);
        let pct = if window > 0 {
            // Saturating arithmetic guards against exotic scenarios
            // where a snapshot reports context_used > window (mid-
            // /compact race, e.g.). Clamping to 0..=100 keeps the bar
            // render and the percentage label in the documented range.
            context_used
                .saturating_mul(100)
                .saturating_div(window.max(1))
                .min(100)
        } else {
            0
        };

        let count = format_token_count(total);
        let style = match self.color_for(total) {
            Some(c) => Style::default().fg(c),
            None => ctx.secondary,
        };

        let text = match self.cfg.format {
            crate::config::TokensFormat::Compact => format!("tok:{count}"),
            crate::config::TokensFormat::WithPercent => format!("tok:{count} {pct}%"),
            crate::config::TokensFormat::WithBar => {
                format!("tok:{count} {}", render_mini_bar(pct))
            }
        };
        Some(Line::from(Span::styled(text, style)))
    }
}

/// Render a compact 5-cell progress bar like `[▌▌  ]`. Width is
/// fixed (5 cells of fill, plus the two `[` `]` brackets) so the
/// `WithBar` format is always 9 cells wide regardless of percentage —
/// keeps the drop-from-the-left algorithm's width math stable.
#[must_use]
fn render_mini_bar(pct: u64) -> String {
    const WIDTH: u64 = 5;
    let pct_clamped = pct.min(100);
    let filled = pct_clamped.saturating_mul(WIDTH) / 100;
    let filled_usize = usize::try_from(filled).unwrap_or(0);
    let empty_usize = usize::try_from(WIDTH)
        .unwrap_or(0)
        .saturating_sub(filled_usize);
    let mut s = String::with_capacity(2 + filled_usize + empty_usize);
    s.push('[');
    for _ in 0..filled_usize {
        s.push('▌');
    }
    for _ in 0..empty_usize {
        s.push(' ');
    }
    s.push(']');
    s
}

/// Format a token count as `1.2k` / `1.5m` / `42` for compact display
/// in the status bar. Matches aifx's `formatTokenCount` (floor for
/// `k`, one-decimal for `m`).
#[must_use]
fn format_token_count(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        // Floor-then-format keeps "1.0m" out of the very-low-million
        // bucket. Divide by 100k first so we keep one decimal place
        // without floating-point math.
        let tenths = tokens / 100_000;
        let whole = tenths / 10;
        let frac = tenths % 10;
        format!("{whole}.{frac}m")
    } else if tokens >= 1_000 {
        format!("{}k", tokens / 1_000)
    } else {
        format!("{tokens}")
    }
}

// ─── PrefixHintSegment ────────────────────────────────────────────

/// The right-edge hint that's lived on the status bar since day one:
/// `super+b for help` when idle, `[NAV] h/l prev/next esc exit` when
/// the prefix key has been pressed and we're awaiting a command.
///
/// Wraps the existing logic verbatim (the `[NAV]` badge stays bold
/// yellow) so users don't notice the segment refactor in the prefix-
/// mode UX. Exists as a segment so the user can drop it from the
/// status bar entirely (`status_bar_segments = ["repo"]`) without a
/// special config knob.
pub(crate) struct PrefixHintSegment;

impl StatusSegment for PrefixHintSegment {
    fn id(&self) -> &'static str {
        super::SEGMENT_PREFIX_HINT
    }

    fn render(&self, ctx: &SegmentCtx<'_>) -> Option<Line<'static>> {
        match ctx.prefix_state {
            PrefixState::Idle => {
                let text = format!(
                    "{} {} for help",
                    ctx.bindings.prefix, ctx.bindings.on_prefix.help,
                );
                Some(Line::from(Span::styled(text, ctx.secondary)))
            }
            PrefixState::AwaitingCommand => {
                // Yellow + bold on the badge, dim chrome on the body.
                // Matches the pre-refactor prefix-mode rendering exactly.
                let badge = "[NAV] ";
                let body = "h/l prev/next  esc exit";
                Some(Line::from(vec![
                    Span::styled(
                        badge,
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(body, ctx.secondary),
                ]))
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::agent_meta_worker::ModelEffort;
    use crate::keymap::Bindings;
    use crate::status_bar::AgentView;

    fn ctx_with<'a>(
        bindings: &'a Bindings,
        repo: Option<&'a str>,
        branch: Option<&'a str>,
        cwd_basename: Option<&'a str>,
        prefix_state: PrefixState,
    ) -> SegmentCtx<'a> {
        SegmentCtx {
            agent: AgentView {
                repo,
                branch,
                cwd_basename,
                ..AgentView::empty()
            },
            prefix_state,
            bindings,
            secondary: Style::default(),
        }
    }

    /// Variant of [`ctx_with`] that carries a [`ModelEffort`] —
    /// used by the `ModelSegment` tests. The caller owns the
    /// `ModelEffort` so the borrow lives the length of the test.
    fn ctx_with_me<'a>(bindings: &'a Bindings, me: Option<&'a ModelEffort>) -> SegmentCtx<'a> {
        SegmentCtx {
            agent: AgentView {
                model_effort: me,
                ..AgentView::empty()
            },
            prefix_state: PrefixState::Idle,
            bindings,
            secondary: Style::default(),
        }
    }

    /// Build a `ModelEffort` from string literals — single-line
    /// constructor for the tests below.
    fn me(model: &str, effort: Option<&str>) -> ModelEffort {
        ModelEffort {
            model: model.into(),
            effort: effort.map(str::to_string),
        }
    }

    /// Test ctx for [`BranchSegment`]'s "branch is set" path. The
    /// "is this branch a default" decision now lives on the segment
    /// instance, not in the ctx.
    fn ctx_with_branch<'a>(bindings: &'a Bindings, branch: Option<&'a str>) -> SegmentCtx<'a> {
        SegmentCtx {
            agent: AgentView {
                branch,
                ..AgentView::empty()
            },
            prefix_state: PrefixState::Idle,
            bindings,
            secondary: Style::default(),
        }
    }

    /// Test ctx for [`TokenSegment`]'s "usage is set" path. Mirrors
    /// `ctx_with_me` for symmetry with the other meta-worker-fed
    /// segment.
    fn ctx_with_tu<'a>(
        bindings: &'a Bindings,
        usage: Option<&'a crate::agent_meta_worker::TokenUsage>,
    ) -> SegmentCtx<'a> {
        SegmentCtx {
            agent: AgentView {
                token_usage: usage,
                ..AgentView::empty()
            },
            prefix_state: PrefixState::Idle,
            bindings,
            secondary: Style::default(),
        }
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn line_style(line: &Line<'_>) -> Option<Style> {
        // Segments must put the style on the span (not the line wrapper)
        // so it survives the span-extraction flatten in render_segments.
        // Each segment renders one styled span; assert against that.
        // Returning Option (not unwrap_or_default) so a test can tell
        // "no span at all" apart from "span exists with default style."
        line.spans.first().map(|s| s.style)
    }

    // ─── ModelSegment ──────────────────────────────────────────────

    #[test]
    fn model_segment_shortens_anthropic_model_ids() {
        let bindings = Bindings::default();
        let model = me("claude-opus-4-7", None);
        let ctx = ctx_with_me(&bindings, Some(&model));
        let line = ModelSegment.render(&ctx).unwrap();
        assert_eq!(line_text(&line), "model:opus-4-7");
    }

    #[test]
    fn model_segment_passes_through_unknown_model_names() {
        // Custom models reached through a proxy may not start with
        // `claude-`. Better to show the raw id than mangle it.
        let bindings = Bindings::default();
        let model = me("internal/llama-7b", None);
        let ctx = ctx_with_me(&bindings, Some(&model));
        let line = ModelSegment.render(&ctx).unwrap();
        assert_eq!(line_text(&line), "model:internal/llama-7b");
    }

    #[test]
    fn model_segment_returns_none_when_model_is_unknown() {
        // Worker hasn't reported yet, or focused agent is SSH-backed.
        // Segment must be skipped (not rendered as `model:?`).
        let bindings = Bindings::default();
        let ctx = ctx_with_me(&bindings, None);
        assert!(ModelSegment.render(&ctx).is_none());
    }

    #[test]
    fn model_segment_strips_bracketed_context_window_suffix() {
        // Aliases like `opus[1m]` carry a trailing `[1m]` context-
        // window flag that's redundant once the user has picked the
        // model. Pin that the segment shortens to `opus` (not `opus[1m]`).
        let bindings = Bindings::default();
        let model = me("opus[1m]", None);
        let ctx = ctx_with_me(&bindings, Some(&model));
        let line = ModelSegment.render(&ctx).unwrap();
        assert_eq!(line_text(&line), "model:opus");
    }

    #[test]
    fn model_segment_strips_both_prefix_and_suffix_when_present() {
        // `claude-opus-4-7[1m]` — both the `claude-` prefix and the
        // `[1m]` suffix come off, leaving `opus-4-7`.
        let bindings = Bindings::default();
        let model = me("claude-opus-4-7[1m]", None);
        let ctx = ctx_with_me(&bindings, Some(&model));
        let line = ModelSegment.render(&ctx).unwrap();
        assert_eq!(line_text(&line), "model:opus-4-7");
    }

    #[test]
    fn model_segment_appends_bracketed_effort_badge_when_non_default() {
        // The whole point of this rewrite: model + effort together,
        // bracketed-suffix style. `opus-4-7 [xhigh]` per the user's
        // chosen UX.
        let bindings = Bindings::default();
        let model = me("claude-opus-4-7", Some("xhigh"));
        let ctx = ctx_with_me(&bindings, Some(&model));
        let line = ModelSegment.render(&ctx).unwrap();
        assert_eq!(line_text(&line), "model:opus-4-7 [xhigh]");
    }

    #[test]
    fn model_segment_hides_effort_badge_when_medium_default() {
        // `medium` is claude's documented default — surfacing it
        // would just be noise on the status bar. Hide it the same
        // way `BranchSegment` hides the trunk branch.
        let bindings = Bindings::default();
        let model = me("opus[1m]", Some("medium"));
        let ctx = ctx_with_me(&bindings, Some(&model));
        let line = ModelSegment.render(&ctx).unwrap();
        assert_eq!(line_text(&line), "model:opus");
    }

    #[test]
    fn model_segment_hides_effort_badge_when_legacy_default_spelling() {
        // The legacy spelling `"default"` shows up in older
        // settings.json files; hide it the same as `"medium"`.
        let bindings = Bindings::default();
        let model = me("opus[1m]", Some("default"));
        let ctx = ctx_with_me(&bindings, Some(&model));
        let line = ModelSegment.render(&ctx).unwrap();
        assert_eq!(line_text(&line), "model:opus");
    }

    #[test]
    fn model_segment_hides_effort_badge_when_field_empty_or_absent() {
        // Older settings.json files may not have the field at all
        // (None) or carry an empty string. Both must hide the
        // badge — the segment shouldn't render `model:opus []`.
        let bindings = Bindings::default();
        let absent_me = me("opus[1m]", None);
        let empty_me = me("opus[1m]", Some(""));
        let absent = ctx_with_me(&bindings, Some(&absent_me));
        let empty = ctx_with_me(&bindings, Some(&empty_me));
        assert_eq!(
            line_text(&ModelSegment.render(&absent).unwrap()),
            "model:opus"
        );
        assert_eq!(
            line_text(&ModelSegment.render(&empty).unwrap()),
            "model:opus"
        );
    }

    #[test]
    fn model_segment_passes_unknown_effort_through_verbatim() {
        // A future claude flag like `"max"` or `"thinking"` should
        // surface immediately as `[max]` rather than being silently
        // hidden by an over-aggressive default-detection list.
        let bindings = Bindings::default();
        let model = me("opus[1m]", Some("max"));
        let ctx = ctx_with_me(&bindings, Some(&model));
        let line = ModelSegment.render(&ctx).unwrap();
        assert_eq!(line_text(&line), "model:opus [max]");
    }

    #[test]
    fn shorten_model_name_passes_through_garbage_bracketed_suffix() {
        assert_eq!(shorten_model_name("opus[]"), "opus[]");
        assert_eq!(shorten_model_name("opus[ABC]"), "opus[ABC]");
        assert_eq!(shorten_model_name("opus[2m]"), "opus");
        assert_eq!(shorten_model_name("opus[200k]"), "opus");
    }

    #[test]
    fn shorten_model_name_passes_through_when_no_open_bracket() {
        // Input ends with `]` but contains no `[`. The rsplit_once
        // returns None and the function passes through. Pinning the
        // edge case so a future "trim trailing `]`" shortcut doesn't
        // silently mangle exotic aliases.
        assert_eq!(shorten_model_name("foo]"), "foo]");
        assert_eq!(shorten_model_name("opus-4-7]"), "opus-4-7]");
    }

    #[test]
    fn shorten_model_name_passes_through_when_base_is_empty() {
        // The whole alias is `[<payload>]` with nothing before the
        // bracket. Stripping would leave an empty string — the
        // function passes through instead so the rendered segment
        // still has something visible (`model:[1m]`).
        assert_eq!(shorten_model_name("[1m]"), "[1m]");
        assert_eq!(shorten_model_name("[200k]"), "[200k]");
    }

    #[test]
    fn model_segment_styles_the_span_so_color_survives_flattening() {
        // The reason this test exists: `Line::styled(text, style)` puts
        // the style on the line wrapper, not the span. When
        // render_segments extracts spans into a unified line, that
        // line-level style is dropped and the text renders in the
        // terminal's default color. Pinning that the style lives on
        // the span guards against that regression.
        let bindings = Bindings::default();
        let secondary = Style::default().fg(Color::Indexed(247));
        let model = me("claude-opus-4-7", None);
        let ctx = SegmentCtx {
            agent: AgentView {
                model_effort: Some(&model),
                ..AgentView::empty()
            },
            prefix_state: PrefixState::Idle,
            bindings: &bindings,
            secondary,
        };
        let line = ModelSegment.render(&ctx).unwrap();
        assert_eq!(line_style(&line), Some(secondary));
    }

    // ─── RepoSegment ───────────────────────────────────────────────

    #[test]
    fn repo_segment_renders_repo_name() {
        let bindings = Bindings::default();
        let ctx = ctx_with(&bindings, Some("codemux"), None, None, PrefixState::Idle);
        let line = RepoSegment.render(&ctx).unwrap();
        assert_eq!(line_text(&line), "repo:codemux");
    }

    #[test]
    fn repo_segment_returns_none_when_no_repo() {
        let bindings = Bindings::default();
        let ctx = ctx_with(&bindings, None, None, None, PrefixState::Idle);
        assert!(RepoSegment.render(&ctx).is_none());
    }

    // ─── WorktreeSegment ───────────────────────────────────────────

    #[test]
    fn worktree_segment_hides_in_main_checkout() {
        // Plain checkout: cwd basename matches the repo name. The
        // segment hides itself rather than restate the repo.
        let bindings = Bindings::default();
        let ctx = ctx_with(
            &bindings,
            Some("codemux"),
            None,
            Some("codemux"),
            PrefixState::Idle,
        );
        assert!(WorktreeSegment.render(&ctx).is_none());
    }

    #[test]
    fn worktree_segment_renders_when_cwd_differs_from_repo() {
        // Worktree: cwd basename differs from repo. Segment shows
        // the worktree name so the user knows which checkout they're
        // in.
        let bindings = Bindings::default();
        let ctx = ctx_with(
            &bindings,
            Some("codemux"),
            None,
            Some("feature-x"),
            PrefixState::Idle,
        );
        let line = WorktreeSegment.render(&ctx).unwrap();
        assert_eq!(line_text(&line), "wt:feature-x");
    }

    #[test]
    fn worktree_segment_renders_when_repo_is_unknown() {
        // We have a cwd basename but the repo couldn't be resolved
        // (rare — agent spawned outside any repo). With nothing to
        // compare against, render the cwd basename so the user at
        // least sees where they are.
        let bindings = Bindings::default();
        let ctx = ctx_with(&bindings, None, None, Some("scratch"), PrefixState::Idle);
        let line = WorktreeSegment.render(&ctx).unwrap();
        assert_eq!(line_text(&line), "wt:scratch");
    }

    #[test]
    fn worktree_segment_returns_none_when_cwd_basename_unknown() {
        // SSH agent or a path with no resolvable basename.
        let bindings = Bindings::default();
        let ctx = ctx_with(&bindings, None, None, None, PrefixState::Idle);
        assert!(WorktreeSegment.render(&ctx).is_none());
    }

    // ─── BranchSegment ─────────────────────────────────────────────

    #[test]
    fn branch_segment_renders_when_branch_is_not_default() {
        let bindings = Bindings::default();
        let segment = BranchSegment::new(vec!["main".into(), "master".into()]);
        let ctx = ctx_with_branch(&bindings, Some("feat/x"));
        let line = segment.render(&ctx).unwrap();
        assert_eq!(line_text(&line), "branch:feat/x");
    }

    #[test]
    fn branch_segment_hides_when_branch_is_in_default_list() {
        // Trunk: branch matches the configured default. Hide.
        let bindings = Bindings::default();
        let segment = BranchSegment::new(vec!["main".into(), "master".into()]);
        let ctx = ctx_with_branch(&bindings, Some("main"));
        assert!(segment.render(&ctx).is_none());
        let ctx = ctx_with_branch(&bindings, Some("master"));
        assert!(segment.render(&ctx).is_none());
    }

    #[test]
    fn branch_segment_renders_main_when_default_list_is_empty() {
        // Documented opt-out: empty default_branches means every
        // branch is "interesting." Even `main` renders.
        let bindings = Bindings::default();
        let segment = BranchSegment::new(vec![]);
        let ctx = ctx_with_branch(&bindings, Some("main"));
        let line = segment.render(&ctx).unwrap();
        assert_eq!(line_text(&line), "branch:main");
    }

    #[test]
    fn branch_segment_returns_none_when_no_branch() {
        let bindings = Bindings::default();
        let segment = BranchSegment::new(vec!["main".into()]);
        let ctx = ctx_with_branch(&bindings, None);
        assert!(segment.render(&ctx).is_none());
    }

    // ─── TokenSegment ──────────────────────────────────────────────

    use crate::agent_meta_worker::TokenUsage;

    #[test]
    fn token_segment_returns_none_when_no_usage_yet() {
        // First-render path: the worker hasn't seen a snapshot file
        // yet (the agent hasn't completed turn 1). The segment slot
        // collapses cleanly — no `tok:0` placeholder.
        let bindings = Bindings::default();
        let segment = TokenSegment::new(crate::config::TokensSegmentConfig::default());
        let ctx = ctx_with_tu(&bindings, None);
        assert!(segment.render(&ctx).is_none());
    }

    #[test]
    fn token_segment_returns_none_when_total_is_zero() {
        // Defensive: if Claude Code ever fires the statusLine
        // callback before any tokens have flowed (e.g. a `/compact`
        // immediately after spawn), render nothing rather than `tok:0`.
        let bindings = Bindings::default();
        let segment = TokenSegment::new(crate::config::TokensSegmentConfig::default());
        let usage = TokenUsage {
            context_window: 200_000,
            ..Default::default()
        };
        let ctx = ctx_with_tu(&bindings, Some(&usage));
        assert!(segment.render(&ctx).is_none());
    }

    #[test]
    fn token_segment_with_percent_renders_tok_count_and_percentage() {
        // The default format. Headline number sums input+output+cache;
        // the percentage divides input+cache by the effective window.
        // 100k input + 50k output + 0 cache, against the 200k window
        // reported in the snapshot: total = 150k → "150k",
        // context_used = 100k, effective_window = min(200k, 400k) = 200k
        // → 100/200 = 50%.
        let bindings = Bindings::default();
        let segment = TokenSegment::new(crate::config::TokensSegmentConfig::default());
        let usage = TokenUsage {
            input: 100_000,
            output: 50_000,
            context_window: 200_000,
            ..Default::default()
        };
        let ctx = ctx_with_tu(&bindings, Some(&usage));
        let line = segment.render(&ctx).unwrap();
        let text = line_text(&line);
        assert!(text.starts_with("tok:150k"), "got {text:?}");
        assert!(text.ends_with("50%"), "got {text:?}");
    }

    #[test]
    fn token_segment_compact_format_omits_percentage() {
        let bindings = Bindings::default();
        let cfg = crate::config::TokensSegmentConfig {
            format: crate::config::TokensFormat::Compact,
            ..Default::default()
        };
        let segment = TokenSegment::new(cfg);
        let usage = TokenUsage {
            input: 100_000,
            output: 50_000,
            context_window: 200_000,
            ..Default::default()
        };
        let ctx = ctx_with_tu(&bindings, Some(&usage));
        let line = segment.render(&ctx).unwrap();
        assert_eq!(line_text(&line), "tok:150k");
    }

    #[test]
    fn token_segment_with_bar_renders_fixed_width_progress_bar() {
        // The mini-bar is fixed at 5 fill cells + brackets so the
        // segment width is stable across percentages — the
        // drop-from-the-left algorithm relies on width staying put.
        let bindings = Bindings::default();
        let cfg = crate::config::TokensSegmentConfig {
            format: crate::config::TokensFormat::WithBar,
            ..Default::default()
        };
        let segment = TokenSegment::new(cfg);
        // 100k input + 0 output + 0 cache, 400k effective window
        // → 25% → 1 of 5 fill cells.
        let usage = TokenUsage {
            input: 100_000,
            context_window: 400_000,
            ..Default::default()
        };
        let ctx = ctx_with_tu(&bindings, Some(&usage));
        let line = segment.render(&ctx).unwrap();
        assert_eq!(line_text(&line), "tok:100k [▌    ]");
    }

    #[test]
    fn token_segment_color_threshold_yellow_at_200k() {
        // Default yellow threshold is 200k. A total at exactly the
        // boundary must turn yellow — pin the comparison sense (>=).
        let bindings = Bindings::default();
        let segment = TokenSegment::new(crate::config::TokensSegmentConfig::default());
        let usage = TokenUsage {
            input: 200_000,
            context_window: 400_000,
            ..Default::default()
        };
        let ctx = ctx_with_tu(&bindings, Some(&usage));
        let line = segment.render(&ctx).unwrap();
        assert_eq!(
            line.spans.first().map(|s| s.style.fg),
            Some(Some(Color::Yellow)),
            "at-threshold token count must paint the segment yellow",
        );
    }

    #[test]
    fn token_segment_color_threshold_orange_at_300k() {
        let bindings = Bindings::default();
        let segment = TokenSegment::new(crate::config::TokensSegmentConfig::default());
        let usage = TokenUsage {
            input: 300_000,
            context_window: 400_000,
            ..Default::default()
        };
        let ctx = ctx_with_tu(&bindings, Some(&usage));
        let line = segment.render(&ctx).unwrap();
        assert_eq!(
            line.spans.first().map(|s| s.style.fg),
            Some(Some(Color::Indexed(208))),
        );
    }

    #[test]
    fn token_segment_color_threshold_red_at_360k() {
        let bindings = Bindings::default();
        let segment = TokenSegment::new(crate::config::TokensSegmentConfig::default());
        let usage = TokenUsage {
            input: 360_000,
            context_window: 400_000,
            ..Default::default()
        };
        let ctx = ctx_with_tu(&bindings, Some(&usage));
        let line = segment.render(&ctx).unwrap();
        assert_eq!(
            line.spans.first().map(|s| s.style.fg),
            Some(Some(Color::Red)),
        );
    }

    #[test]
    fn token_segment_below_yellow_inherits_chrome_secondary_style() {
        // Below the warning thresholds the segment must read as
        // ambient chrome, not pop. That's what makes the color shift
        // at 200k actually catch the eye.
        let bindings = Bindings::default();
        let segment = TokenSegment::new(crate::config::TokensSegmentConfig::default());
        let secondary = Style::default().fg(Color::Indexed(247));
        let usage = TokenUsage {
            input: 50_000,
            context_window: 400_000,
            ..Default::default()
        };
        let ctx = SegmentCtx {
            agent: AgentView {
                token_usage: Some(&usage),
                ..AgentView::empty()
            },
            prefix_state: PrefixState::Idle,
            bindings: &bindings,
            secondary,
        };
        let line = segment.render(&ctx).unwrap();
        assert_eq!(line.spans.first().map(|s| s.style), Some(secondary));
    }

    #[test]
    fn token_segment_thresholds_are_configurable() {
        // Lower the yellow threshold dramatically — what would have
        // been an ambient render at 60k must now turn yellow. Pin
        // that the per-instance config is actually consulted (and
        // not, say, a hardcoded constant the implementor forgot to
        // wire to the config).
        let bindings = Bindings::default();
        let cfg = crate::config::TokensSegmentConfig {
            yellow_threshold: 50_000,
            ..Default::default()
        };
        let segment = TokenSegment::new(cfg);
        let usage = TokenUsage {
            input: 60_000,
            context_window: 400_000,
            ..Default::default()
        };
        let ctx = ctx_with_tu(&bindings, Some(&usage));
        let line = segment.render(&ctx).unwrap();
        assert_eq!(
            line.spans.first().map(|s| s.style.fg),
            Some(Some(Color::Yellow)),
        );
    }

    #[test]
    fn token_segment_uses_cache_tokens_in_total_and_percentage() {
        // Cache reads are part of the prompt budget — they count
        // toward the headline number AND the percentage. Output
        // tokens count toward the headline but NOT the percentage
        // (output doesn't carry into the next turn's prompt budget).
        // 50k input + 30k output + 20k cache_creation + 100k cache_read,
        // window 400k → total = 200k, context_used = 170k → 42%.
        let bindings = Bindings::default();
        let segment = TokenSegment::new(crate::config::TokensSegmentConfig::default());
        let usage = TokenUsage {
            input: 50_000,
            output: 30_000,
            cache_creation: 20_000,
            cache_read: 100_000,
            context_window: 400_000,
        };
        let ctx = ctx_with_tu(&bindings, Some(&usage));
        let line = segment.render(&ctx).unwrap();
        let text = line_text(&line);
        assert!(text.starts_with("tok:200k"), "got {text:?}");
        assert!(text.ends_with("42%"), "got {text:?}");
    }

    #[test]
    fn token_segment_trusts_reported_window_for_large_context_models() {
        // Regression for "418k 100% on a 1M-context model": the
        // segment must NOT pin the denominator to a hidden 400k
        // floor. Aifx clamps to 400k only because it sets the
        // CLAUDE_CODE_AUTO_COMPACT_WINDOW env var itself; codemux
        // doesn't, so the 1M window from the JSON should win.
        //
        // Pure-function call so the test isn't influenced by whatever
        // the runner's shell sets — the user's terminal may export
        // the env var from aifx and that would skew the math.
        assert_eq!(
            TokenSegment::effective_window_pure(None, None, 1_000_000),
            1_000_000,
            "no override, big window → trust the JSON window",
        );
    }

    #[test]
    fn token_segment_override_caps_to_reported_window_when_smaller() {
        // User sets `auto_compact_window = 500_000` but the model is
        // a 200k Sonnet. The override must NOT inflate the
        // denominator past what the model can actually do — aifx's
        // semantics: the override is a ceiling, capped by the real
        // window.
        assert_eq!(
            TokenSegment::effective_window_pure(Some(500_000), None, 200_000),
            200_000,
            "config override capped by smaller reported window",
        );
    }

    #[test]
    fn token_segment_falls_back_to_400k_when_no_window_reported() {
        // Defensive: if a future Claude Code version omits
        // `context_window_size`, the segment must still render a
        // sensible percentage rather than 0/0 = 0% or NaN.
        assert_eq!(
            TokenSegment::effective_window_pure(None, None, 0),
            400_000,
            "no override, no window → 400k last-resort guard",
        );
    }

    #[test]
    fn token_segment_config_override_wins_over_env() {
        // Config takes priority over the env var when both are set.
        // The end-to-end render test would race against whatever the
        // shell exports; this pure check is hermetic.
        assert_eq!(
            TokenSegment::effective_window_pure(Some(300_000), Some(900_000), 1_000_000),
            300_000,
            "config override beats env override",
        );
    }

    #[test]
    fn token_segment_env_override_used_when_config_unset() {
        // The env var is a hard ceiling honored by Claude Code's own
        // compactor; the segment should respect it when the user
        // hasn't provided an explicit config override.
        assert_eq!(
            TokenSegment::effective_window_pure(None, Some(400_000), 1_000_000),
            400_000,
            "env override used when config is None",
        );
    }

    #[test]
    fn token_segment_format_token_count_units_match_aifx() {
        assert_eq!(format_token_count(0), "0");
        assert_eq!(format_token_count(999), "999");
        assert_eq!(format_token_count(1_000), "1k");
        assert_eq!(format_token_count(125_000), "125k");
        // 1.25M floors to 1.2M with the one-decimal scheme.
        assert_eq!(format_token_count(1_250_000), "1.2m");
        assert_eq!(format_token_count(2_000_000), "2.0m");
    }

    // ─── PrefixHintSegment ─────────────────────────────────────────

    #[test]
    fn prefix_hint_segment_renders_help_label_when_idle() {
        let bindings = Bindings::default();
        let ctx = ctx_with(&bindings, None, None, None, PrefixState::Idle);
        let line = PrefixHintSegment.render(&ctx).unwrap();
        assert!(
            line_text(&line).ends_with("for help"),
            "got {:?}",
            line_text(&line)
        );
    }

    #[test]
    fn prefix_hint_segment_renders_nav_badge_when_awaiting_command() {
        let bindings = Bindings::default();
        let ctx = ctx_with(&bindings, None, None, None, PrefixState::AwaitingCommand);
        let line = PrefixHintSegment.render(&ctx).unwrap();
        assert_eq!(line_text(&line), "[NAV] h/l prev/next  esc exit");
    }

    #[test]
    fn prefix_hint_segment_styles_idle_text_via_span() {
        // Same regression test as ModelSegment's: idle hint must put
        // its style on the span so the gray color survives the
        // render_segments flatten.
        let bindings = Bindings::default();
        let secondary = Style::default().fg(Color::Indexed(247));
        let ctx = SegmentCtx {
            agent: AgentView::empty(),
            prefix_state: PrefixState::Idle,
            bindings: &bindings,
            secondary,
        };
        let line = PrefixHintSegment.render(&ctx).unwrap();
        assert_eq!(line_style(&line), Some(secondary));
    }

    #[test]
    fn prefix_hint_segment_never_returns_none() {
        // Always renders something — that's the contract that lets
        // it stay rightmost (highest priority) by default.
        let bindings = Bindings::default();
        for state in [PrefixState::Idle, PrefixState::AwaitingCommand] {
            let ctx = ctx_with(&bindings, None, None, None, state);
            assert!(PrefixHintSegment.render(&ctx).is_some());
        }
    }
}
