//! Built-in status-bar segments. Each is a tiny stateless type that
//! reads its data off [`SegmentCtx`] and renders a styled `Line`.
//!
//! Adding a new segment: implement [`StatusSegment`], add a unique ID
//! constant in `mod.rs`, register it in `build_segments`, and document
//! it in the `Ui::status_bar_segments` doc comment.

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
        Some(Line::styled(format!("model: {short}"), ctx.secondary))
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
/// mirrors `RuntimeAgent.repo`.
pub(crate) struct RepoSegment;

impl StatusSegment for RepoSegment {
    fn id(&self) -> &'static str {
        super::SEGMENT_REPO
    }

    fn render(&self, ctx: &SegmentCtx<'_>) -> Option<Line<'static>> {
        let repo = ctx.repo?;
        Some(Line::styled(format!("repo: {repo}"), ctx.secondary))
    }
}

// ─── BranchSegment ────────────────────────────────────────────────

/// Renders the focused agent's git branch, prefixed with the worktree
/// directory's basename **only when it differs from the repo name**.
/// This is the worktree-vs-checkout distinction:
///
/// - Plain checkout (cwd basename == repo): renders `:main`
/// - Worktree (cwd is `.../worktrees/feature-x`): renders
///   `feature-x:feat/x`
///
/// The leading `:` in the plain-checkout case is intentional — it
/// reads as "branch on this repo" without restating the repo name
/// (which `RepoSegment` already shows immediately to the left).
pub(crate) struct BranchSegment;

impl StatusSegment for BranchSegment {
    fn id(&self) -> &'static str {
        super::SEGMENT_BRANCH
    }

    fn render(&self, ctx: &SegmentCtx<'_>) -> Option<Line<'static>> {
        let branch = ctx.branch?;
        let label = match (ctx.cwd_basename, ctx.repo) {
            (Some(cwd), Some(repo)) if cwd != repo => format!("{cwd}:{branch}"),
            _ => format!(":{branch}"),
        };
        Some(Line::styled(label, ctx.secondary))
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
                Some(Line::styled(text, ctx.secondary))
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
        assert_eq!(line_text(&line), "model: opus-4-7");
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
        assert_eq!(line_text(&line), "model: internal/llama-7b");
    }

    #[test]
    fn model_segment_returns_none_when_model_is_unknown() {
        // Worker hasn't reported yet, or focused agent is SSH-backed.
        // Segment must be skipped (not rendered as `model: ?`).
        let bindings = Bindings::default();
        let ctx = ctx_with(&bindings, None, None, None, None, PrefixState::Idle);
        assert!(ModelSegment.render(&ctx).is_none());
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
        assert_eq!(line_text(&line), "repo: codemux");
    }

    #[test]
    fn repo_segment_returns_none_when_no_repo() {
        let bindings = Bindings::default();
        let ctx = ctx_with(&bindings, None, None, None, None, PrefixState::Idle);
        assert!(RepoSegment.render(&ctx).is_none());
    }

    // ─── BranchSegment ─────────────────────────────────────────────

    #[test]
    fn branch_segment_renders_just_branch_when_cwd_basename_equals_repo() {
        // Plain checkout: cwd basename matches the git root basename,
        // so we render `:main` (no need to repeat the repo name —
        // RepoSegment is right next door on screen).
        let bindings = Bindings::default();
        let ctx = ctx_with(
            &bindings,
            Some("codemux"),
            Some("main"),
            None,
            Some("codemux"),
            PrefixState::Idle,
        );
        let line = BranchSegment.render(&ctx).unwrap();
        assert_eq!(line_text(&line), ":main");
    }

    #[test]
    fn branch_segment_prefixes_worktree_name_when_cwd_differs_from_repo() {
        // Worktree: `cd ~/Workbench/worktrees/feature-x && claude`.
        // cwd basename is `feature-x`, repo basename is `codemux`,
        // branch is `feat/x`. Segment renders `feature-x:feat/x` so
        // the user can see at a glance which worktree they're in.
        let bindings = Bindings::default();
        let ctx = ctx_with(
            &bindings,
            Some("codemux"),
            Some("feat/x"),
            None,
            Some("feature-x"),
            PrefixState::Idle,
        );
        let line = BranchSegment.render(&ctx).unwrap();
        assert_eq!(line_text(&line), "feature-x:feat/x");
    }

    #[test]
    fn branch_segment_falls_back_to_just_branch_when_cwd_basename_is_unknown() {
        // Defensive: if the worker ever reports a branch but the
        // cwd basename couldn't be derived (shouldn't happen — they're
        // resolved together — but we don't crash if it does).
        let bindings = Bindings::default();
        let ctx = ctx_with(
            &bindings,
            Some("codemux"),
            Some("main"),
            None,
            None,
            PrefixState::Idle,
        );
        let line = BranchSegment.render(&ctx).unwrap();
        assert_eq!(line_text(&line), ":main");
    }

    #[test]
    fn branch_segment_returns_none_when_no_branch() {
        let bindings = Bindings::default();
        let ctx = ctx_with(
            &bindings,
            Some("codemux"),
            None,
            None,
            None,
            PrefixState::Idle,
        );
        assert!(BranchSegment.render(&ctx).is_none());
    }

    // ─── PrefixHintSegment ─────────────────────────────────────────

    #[test]
    fn prefix_hint_segment_renders_help_label_when_idle() {
        let bindings = Bindings::default();
        let ctx = ctx_with(&bindings, None, None, None, None, PrefixState::Idle);
        let line = PrefixHintSegment.render(&ctx).unwrap();
        // Default prefix is `ctrl+b`, default help binding is `?`.
        // Just check the suffix; the exact prefix glyph depends on
        // the default binding which other tests pin separately.
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
