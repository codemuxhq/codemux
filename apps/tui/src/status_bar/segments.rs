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

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::{SegmentCtx, StatusSegment};
use crate::runtime::PrefixState;

// ─── ModelSegment ─────────────────────────────────────────────────

/// Renders the focused agent's currently-selected Claude model. Data
/// is supplied by [`crate::agent_meta_worker`] which tails
/// `~/.claude/projects/<encoded-cwd>/*.jsonl` for the most recent
/// `model` field on an assistant turn — the only architecturally-
/// sanctioned exception to AD-1, see `docs/architecture.md`.
///
/// The raw model identifier is run through [`shorten_model_name`]
/// for display: `claude-opus-4-7` → `opus-4-7`. Pass-through for
/// anything we don't recognise (better to show the raw id than guess).
pub(crate) struct ModelSegment;

impl StatusSegment for ModelSegment {
    fn id(&self) -> &'static str {
        super::SEGMENT_MODEL
    }

    fn render(&self, ctx: &SegmentCtx<'_>) -> Option<Line<'static>> {
        let model = ctx.model?;
        let short = shorten_model_name(model);
        Some(Line::from(Span::styled(
            format!("model:{short}"),
            ctx.secondary,
        )))
    }
}

/// Strip the `claude-` prefix that every Anthropic model ID carries.
/// Anything else (custom names, non-Anthropic models reached through
/// a proxy) passes through unchanged. Returns a borrowed slice — the
/// caller folds it into a `format!` so there's no need to allocate
/// here.
#[must_use]
fn shorten_model_name(raw: &str) -> &str {
    raw.strip_prefix("claude-").unwrap_or(raw)
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
        let repo = ctx.repo?;
        Some(Line::from(Span::styled(
            format!("repo:{repo}"),
            ctx.secondary,
        )))
    }
}

// ─── WorktreeSegment ──────────────────────────────────────────────

/// Renders the basename of the focused agent's working directory as
/// `wt:<name>`. For a regular checkout this is the repo name; for a
/// git worktree (e.g. `~/Workbench/worktrees/feature-x`) it's the
/// worktree directory's name. Combined with `BranchSegment` this
/// gives the user a complete "where am I" view at a glance.
pub(crate) struct WorktreeSegment;

impl StatusSegment for WorktreeSegment {
    fn id(&self) -> &'static str {
        super::SEGMENT_WORKTREE
    }

    fn render(&self, ctx: &SegmentCtx<'_>) -> Option<Line<'static>> {
        let cwd = ctx.cwd_basename?;
        Some(Line::from(Span::styled(format!("wt:{cwd}"), ctx.secondary)))
    }
}

// ─── BranchSegment ────────────────────────────────────────────────

/// Renders the focused agent's git branch as `branch:<name>`. Data
/// is supplied by [`crate::agent_meta_worker`] which reads
/// `<cwd>/.git/HEAD`. `None` outside a git repo.
pub(crate) struct BranchSegment;

impl StatusSegment for BranchSegment {
    fn id(&self) -> &'static str {
        super::SEGMENT_BRANCH
    }

    fn render(&self, ctx: &SegmentCtx<'_>) -> Option<Line<'static>> {
        let branch = ctx.branch?;
        Some(Line::from(Span::styled(
            format!("branch:{branch}"),
            ctx.secondary,
        )))
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
    use crate::keymap::Bindings;

    fn ctx_with<'a>(
        bindings: &'a Bindings,
        repo: Option<&'a str>,
        branch: Option<&'a str>,
        model: Option<&'a str>,
        cwd_basename: Option<&'a str>,
        prefix_state: PrefixState,
    ) -> SegmentCtx<'a> {
        SegmentCtx {
            repo,
            branch,
            model,
            cwd_basename,
            prefix_state,
            bindings,
            secondary: Style::default(),
        }
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn line_style(line: &Line<'_>) -> Style {
        // Segments must put the style on the span (not the line wrapper)
        // so it survives the span-extraction flatten in render_segments.
        // Each segment renders one styled span; assert against that.
        line.spans.first().map(|s| s.style).unwrap_or_default()
    }

    // ─── ModelSegment ──────────────────────────────────────────────

    #[test]
    fn model_segment_shortens_anthropic_model_ids() {
        let bindings = Bindings::default();
        let ctx = ctx_with(
            &bindings,
            None,
            None,
            Some("claude-opus-4-7"),
            None,
            PrefixState::Idle,
        );
        let line = ModelSegment.render(&ctx).unwrap();
        assert_eq!(line_text(&line), "model:opus-4-7");
    }

    #[test]
    fn model_segment_passes_through_unknown_model_names() {
        // Custom models reached through a proxy may not start with
        // `claude-`. Better to show the raw id than mangle it.
        let bindings = Bindings::default();
        let ctx = ctx_with(
            &bindings,
            None,
            None,
            Some("internal/llama-7b"),
            None,
            PrefixState::Idle,
        );
        let line = ModelSegment.render(&ctx).unwrap();
        assert_eq!(line_text(&line), "model:internal/llama-7b");
    }

    #[test]
    fn model_segment_returns_none_when_model_is_unknown() {
        // Worker hasn't reported yet, or focused agent is SSH-backed.
        // Segment must be skipped (not rendered as `model:?`).
        let bindings = Bindings::default();
        let ctx = ctx_with(&bindings, None, None, None, None, PrefixState::Idle);
        assert!(ModelSegment.render(&ctx).is_none());
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
        let ctx = SegmentCtx {
            repo: None,
            branch: None,
            model: Some("claude-opus-4-7"),
            cwd_basename: None,
            prefix_state: PrefixState::Idle,
            bindings: &bindings,
            secondary,
        };
        let line = ModelSegment.render(&ctx).unwrap();
        assert_eq!(line_style(&line), secondary);
    }

    // ─── RepoSegment ───────────────────────────────────────────────

    #[test]
    fn repo_segment_renders_repo_name() {
        let bindings = Bindings::default();
        let ctx = ctx_with(
            &bindings,
            Some("codemux"),
            None,
            None,
            None,
            PrefixState::Idle,
        );
        let line = RepoSegment.render(&ctx).unwrap();
        assert_eq!(line_text(&line), "repo:codemux");
    }

    #[test]
    fn repo_segment_returns_none_when_no_repo() {
        let bindings = Bindings::default();
        let ctx = ctx_with(&bindings, None, None, None, None, PrefixState::Idle);
        assert!(RepoSegment.render(&ctx).is_none());
    }

    // ─── WorktreeSegment ───────────────────────────────────────────

    #[test]
    fn worktree_segment_renders_cwd_basename() {
        // Plain checkout: cwd basename is the repo basename.
        let bindings = Bindings::default();
        let ctx = ctx_with(
            &bindings,
            Some("codemux"),
            None,
            None,
            Some("codemux"),
            PrefixState::Idle,
        );
        let line = WorktreeSegment.render(&ctx).unwrap();
        assert_eq!(line_text(&line), "wt:codemux");
    }

    #[test]
    fn worktree_segment_renders_worktree_directory_name() {
        // Worktree: cwd basename differs from repo. Segment shows
        // the worktree name so the user knows which checkout they're
        // in.
        let bindings = Bindings::default();
        let ctx = ctx_with(
            &bindings,
            Some("codemux"),
            None,
            None,
            Some("feature-x"),
            PrefixState::Idle,
        );
        let line = WorktreeSegment.render(&ctx).unwrap();
        assert_eq!(line_text(&line), "wt:feature-x");
    }

    #[test]
    fn worktree_segment_returns_none_when_cwd_basename_unknown() {
        // SSH agent or a path with no resolvable basename.
        let bindings = Bindings::default();
        let ctx = ctx_with(&bindings, None, None, None, None, PrefixState::Idle);
        assert!(WorktreeSegment.render(&ctx).is_none());
    }

    // ─── BranchSegment ─────────────────────────────────────────────

    #[test]
    fn branch_segment_renders_branch_with_prefix() {
        let bindings = Bindings::default();
        let ctx = ctx_with(&bindings, None, Some("main"), None, None, PrefixState::Idle);
        let line = BranchSegment.render(&ctx).unwrap();
        assert_eq!(line_text(&line), "branch:main");
    }

    #[test]
    fn branch_segment_renders_slash_branches_verbatim() {
        let bindings = Bindings::default();
        let ctx = ctx_with(
            &bindings,
            None,
            Some("feat/x"),
            None,
            None,
            PrefixState::Idle,
        );
        let line = BranchSegment.render(&ctx).unwrap();
        assert_eq!(line_text(&line), "branch:feat/x");
    }

    #[test]
    fn branch_segment_returns_none_when_no_branch() {
        let bindings = Bindings::default();
        let ctx = ctx_with(&bindings, None, None, None, None, PrefixState::Idle);
        assert!(BranchSegment.render(&ctx).is_none());
    }

    // ─── PrefixHintSegment ─────────────────────────────────────────

    #[test]
    fn prefix_hint_segment_renders_help_label_when_idle() {
        let bindings = Bindings::default();
        let ctx = ctx_with(&bindings, None, None, None, None, PrefixState::Idle);
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
        let ctx = ctx_with(
            &bindings,
            None,
            None,
            None,
            None,
            PrefixState::AwaitingCommand,
        );
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
            repo: None,
            branch: None,
            model: None,
            cwd_basename: None,
            prefix_state: PrefixState::Idle,
            bindings: &bindings,
            secondary,
        };
        let line = PrefixHintSegment.render(&ctx).unwrap();
        assert_eq!(line_style(&line), secondary);
    }

    #[test]
    fn prefix_hint_segment_never_returns_none() {
        // Always renders something — that's the contract that lets
        // it stay rightmost (highest priority) by default.
        let bindings = Bindings::default();
        for state in [PrefixState::Idle, PrefixState::AwaitingCommand] {
            let ctx = ctx_with(&bindings, None, None, None, None, state);
            assert!(PrefixHintSegment.render(&ctx).is_some());
        }
    }
}
