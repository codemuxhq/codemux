//! Status-bar right-side segments.
//!
//! The bottom status bar is split into two zones: the agent tab strip
//! on the left, and a stack of context segments on the right
//! (model · repo · branch · prefix-hint by default). This module owns
//! the right-side stack — the trait, the four built-ins, and the
//! drop-from-the-left algorithm that handles narrow terminals.
//!
//! ## Plugin shape
//!
//! Segments are a closed set of built-ins, selected and ordered by the
//! user via `[ui] status_bar_segments = [...]` in `config.toml`. Same
//! pattern as `host_colors` and the search engine: built-in `impl
//! StatusSegment` values, user picks IDs. Adding a new segment is
//! "add a new built-in"; the config layer follows automatically.
//!
//! Not dynamic plugins: no shell-out, no scripting, no FFI. AD-29
//! covers the rationale.
//!
//! ## Drop algorithm
//!
//! Segments are ordered left-to-right in [`SegmentCtx`]'s view. The
//! rightmost segment is the highest priority — when we can't fit
//! everything, segments are dropped from the LEFT. So the prefix hint
//! (rightmost by default) stays visible even on a 60-cell-wide screen,
//! and the worktree/branch (second-from-right) is the next-most-likely
//! to survive. See [`render_segments`] for the implementation.
//!
//! ## AD-1 carve-out
//!
//! [`segments::ModelSegment`] reads the focused agent's most recent
//! Claude session JSONL to surface the live model. This is the single
//! sanctioned exception to AD-1's "never semantically parse Claude
//! Code" rule — bounded to one specific file shape, one specific
//! field, focused local agent only. See AD-1's amended prose in
//! `docs/architecture.md`.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::keymap::Bindings;
use crate::runtime::PrefixState;

pub(crate) mod segments;

/// Stable IDs the user types in `config.toml`. Tests reference these
/// constants too — the same string in two places without a source of
/// truth would silently drift.
pub(crate) const SEGMENT_MODEL: &str = "model";
pub(crate) const SEGMENT_REPO: &str = "repo";
pub(crate) const SEGMENT_BRANCH: &str = "branch";
pub(crate) const SEGMENT_PREFIX_HINT: &str = "prefix_hint";

/// Default segment list when the user hasn't set `status_bar_segments`
/// in their config. Order is left-to-right; the rightmost is the
/// highest priority and is the last to be dropped under width pressure.
#[must_use]
pub(crate) fn default_segment_ids() -> Vec<String> {
    vec![
        SEGMENT_MODEL.into(),
        SEGMENT_REPO.into(),
        SEGMENT_BRANCH.into(),
        SEGMENT_PREFIX_HINT.into(),
    ]
}

/// Read-only data view passed to every segment's `render`. Flat (no
/// `RuntimeAgent` reference) so the `status_bar` module doesn't need
/// access to the runtime's internal types — segments only see what
/// they need to render. Built fresh each frame in
/// `render_status_bar`.
pub(crate) struct SegmentCtx<'a> {
    /// Repo name resolved for the focused agent (git root basename, or
    /// cwd basename outside a repo). `None` for failed/unresolvable
    /// agents — the segment renders nothing in that case.
    pub repo: Option<&'a str>,
    /// Current git branch for the focused local agent. Updated by
    /// [`crate::agent_meta_worker`]; `None` until the worker's first
    /// successful read, on non-git directories, and for SSH agents
    /// (worker only handles local in v1).
    pub branch: Option<&'a str>,
    /// Current Claude model for the focused local agent. Updated by
    /// [`crate::agent_meta_worker`] from
    /// `~/.claude/projects/<encoded-cwd>/*.jsonl`. `None` until the
    /// worker's first successful read and for SSH agents.
    pub model: Option<&'a str>,
    /// Basename of the focused agent's cwd, used by [`segments::BranchSegment`]
    /// to distinguish a worktree directory from a regular checkout
    /// (when the cwd basename differs from the repo name, the segment
    /// renders both: `worktree-foo:feat/bar` instead of just `:bar`).
    pub cwd_basename: Option<&'a str>,
    /// Idle vs in-prefix-mode. Drives [`segments::PrefixHintSegment`]'s
    /// label/badge swap.
    pub prefix_state: PrefixState,
    /// User key bindings — the prefix-hint segment renders the
    /// configured prefix chord verbatim ("super+b for help").
    pub bindings: &'a Bindings,
    /// Pre-computed dim chrome style (separators, hint text, repo/model
    /// labels). Built by [`crate::runtime::ChromeStyle::from_ui`] once
    /// at startup — segments use it for ambient text so the chrome
    /// reads as a single visual unit.
    pub secondary: Style,
}

/// One segment in the status bar's right-side stack.
///
/// Implementations are stateless — all data comes from `ctx`. A
/// segment that has nothing to show this frame returns `None`; the
/// renderer then collapses its slot (no width consumed, no separator
/// drawn). This is what makes "model unknown yet" render cleanly
/// rather than as `model: ?`.
pub(crate) trait StatusSegment {
    /// Stable ID used to match config entries to built-in
    /// implementations. Rendered into trace logs when a frame's
    /// width pressure forces the drop algorithm to skip a segment,
    /// so debugging "why isn't `model` showing up" doesn't require
    /// printf-trace-driving the drop loop.
    fn id(&self) -> &'static str;

    /// Render the segment for the given context. Return `None` to
    /// skip (no width consumed, no separator drawn). The returned
    /// `Line` carries its own styling.
    fn render(&self, ctx: &SegmentCtx<'_>) -> Option<Line<'static>>;
}

/// Construct the live segment list from a config-supplied ID list.
/// Unknown IDs are logged at startup (loud) and skipped — better than
/// failing the whole load over a typo.
#[must_use]
pub(crate) fn build_segments(ids: &[String]) -> Vec<Box<dyn StatusSegment>> {
    ids.iter()
        .filter_map(|id| match id.as_str() {
            SEGMENT_MODEL => Some(Box::new(segments::ModelSegment) as Box<dyn StatusSegment>),
            SEGMENT_REPO => Some(Box::new(segments::RepoSegment) as Box<dyn StatusSegment>),
            SEGMENT_BRANCH => Some(Box::new(segments::BranchSegment) as Box<dyn StatusSegment>),
            SEGMENT_PREFIX_HINT => {
                Some(Box::new(segments::PrefixHintSegment) as Box<dyn StatusSegment>)
            }
            other => {
                tracing::warn!(
                    segment = %other,
                    "unknown status_bar_segments id; skipping. \
                     Known ids: model, repo, branch, prefix_hint",
                );
                None
            }
        })
        .collect()
}

/// Width of the separator span drawn between adjacent segments. The
/// glyph is `" │ "` (three cells: space, vertical bar, space) — same
/// separator the tab strip uses. Hoisted to a constant so the drop
/// algorithm and the renderer agree exactly.
const SEPARATOR: &str = " │ ";
const SEPARATOR_WIDTH: u16 = 3;

/// Render the right-side segment stack into a single styled `Line`,
/// dropping segments from the LEFT until the result fits in
/// `available` cells.
///
/// Returns the line plus the consumed width (the number of cells the
/// caller should reserve for it). When no segment fits at all (or the
/// list is empty), returns an empty line and a zero width.
pub(crate) fn render_segments(
    segments: &[Box<dyn StatusSegment>],
    ctx: &SegmentCtx<'_>,
    available: u16,
) -> (Line<'static>, u16) {
    if segments.is_empty() || available == 0 {
        return (Line::default(), 0);
    }
    // Render every segment first; widths come from the styled spans
    // (unicode-aware). `None` results are skipped — the kept indices
    // form a sparse list that the drop pass walks right-to-left.
    let rendered: Vec<(usize, Line<'static>, u16)> = segments
        .iter()
        .enumerate()
        .filter_map(|(idx, seg)| {
            let line = seg.render(ctx)?;
            let width = line_width(&line);
            (width > 0).then_some((idx, line, width))
        })
        .collect();
    if rendered.is_empty() {
        return (Line::default(), 0);
    }

    // Walk right-to-left, accumulating width + a separator between
    // adjacent kept segments. Stop the moment the next segment would
    // overflow `available`. The set of indices we accept becomes the
    // surviving stack; render them in original (left-to-right) order.
    let mut keep: Vec<usize> = Vec::with_capacity(rendered.len());
    let mut used: u16 = 0;
    for (i, (orig_idx, _line, width)) in rendered.iter().enumerate().rev() {
        let extra = if i == rendered.len() - 1 {
            *width
        } else {
            // We're prepending another segment to the left of an
            // already-kept one, so a separator slot is needed too.
            width.saturating_add(SEPARATOR_WIDTH)
        };
        if used.saturating_add(extra) > available {
            // Anything from here leftward is dropped. Trace once per
            // dropped segment so debug builds can answer "why isn't
            // model showing up" by grepping for the segment id.
            tracing::trace!(
                segment = %segments[*orig_idx].id(),
                used,
                available,
                "status_bar: dropping segment under width pressure",
            );
            break;
        }
        used = used.saturating_add(extra);
        keep.push(i);
    }
    if keep.is_empty() {
        return (Line::default(), 0);
    }
    keep.reverse(); // back into render order

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(keep.len() * 2);
    for (pos, kept) in keep.iter().enumerate() {
        if pos > 0 {
            spans.push(Span::styled(SEPARATOR, ctx.secondary));
        }
        spans.extend(rendered[*kept].1.spans.iter().cloned());
    }
    (Line::from(spans), used)
}

/// Display-cell width of every span in `line`, summed and clamped to
/// `u16` (which is what `Rect` uses everywhere).
fn line_width(line: &Line<'_>) -> u16 {
    let total: usize = line.spans.iter().map(Span::width).sum();
    u16::try_from(total).unwrap_or(u16::MAX)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use ratatui::style::Style;

    /// Stub segment with a fixed plain-text label, used only by the
    /// drop-algorithm tests so we can vary widths without depending on
    /// any of the real built-ins' formatting rules.
    struct Stub {
        id: &'static str,
        text: &'static str,
    }

    impl StatusSegment for Stub {
        fn id(&self) -> &'static str {
            self.id
        }

        fn render(&self, _ctx: &SegmentCtx<'_>) -> Option<Line<'static>> {
            Some(Line::from(self.text.to_string()))
        }
    }

    /// Stub segment that always renders nothing — exercises the
    /// "skip-the-slot" path in `render_segments`. Hoisted to test-
    /// module scope so it has a single trait-impl site instead of
    /// being redefined inside two separate tests.
    struct Empty;

    impl StatusSegment for Empty {
        fn id(&self) -> &'static str {
            "empty"
        }

        fn render(&self, _: &SegmentCtx<'_>) -> Option<Line<'static>> {
            None
        }
    }

    fn ctx(bindings: &Bindings) -> SegmentCtx<'_> {
        SegmentCtx {
            repo: None,
            branch: None,
            model: None,
            cwd_basename: None,
            prefix_state: PrefixState::Idle,
            bindings,
            secondary: Style::default(),
        }
    }

    #[test]
    fn render_segments_renders_all_when_space_is_ample() {
        // 4 + 3 + 4 + 3 + 4 + 3 + 4 = 25 cells: three 4-cell stubs
        // joined by three 3-cell separators (no, two separators —
        // n-1 = 2). 4+3+4+3+4 = 18. Cap available well above that.
        let segs: Vec<Box<dyn StatusSegment>> = vec![
            Box::new(Stub {
                id: "a",
                text: "AAAA",
            }),
            Box::new(Stub {
                id: "b",
                text: "BBBB",
            }),
            Box::new(Stub {
                id: "c",
                text: "CCCC",
            }),
        ];
        let bindings = Bindings::default();
        let ctx = ctx(&bindings);
        let (line, width) = render_segments(&segs, &ctx, 100);
        assert_eq!(width, 18, "AAAA│BBBB│CCCC = 4+3+4+3+4 cells");
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "AAAA │ BBBB │ CCCC");
    }

    #[test]
    fn render_segments_drops_leftmost_first_when_space_is_tight() {
        // Same three 4-cell stubs. With `available = 11`:
        // - Try fit C (4) → used=4. OK.
        // - Try prepend B (3 sep + 4 = 7) → used=11. OK.
        // - Try prepend A (3 sep + 4 = 7) → used=18. Overflow → drop A.
        // Surviving stack: [B, C].
        let segs: Vec<Box<dyn StatusSegment>> = vec![
            Box::new(Stub {
                id: "a",
                text: "AAAA",
            }),
            Box::new(Stub {
                id: "b",
                text: "BBBB",
            }),
            Box::new(Stub {
                id: "c",
                text: "CCCC",
            }),
        ];
        let bindings = Bindings::default();
        let ctx = ctx(&bindings);
        let (line, width) = render_segments(&segs, &ctx, 11);
        assert_eq!(width, 11, "BBBB│CCCC = 4+3+4 cells");
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "BBBB │ CCCC", "leftmost A must be dropped first");
    }

    #[test]
    fn render_segments_keeps_only_the_rightmost_when_space_is_very_tight() {
        // available = 4: only the rightmost (C) fits, no separator.
        let segs: Vec<Box<dyn StatusSegment>> = vec![
            Box::new(Stub {
                id: "a",
                text: "AAAA",
            }),
            Box::new(Stub {
                id: "b",
                text: "BBBB",
            }),
            Box::new(Stub {
                id: "c",
                text: "CCCC",
            }),
        ];
        let bindings = Bindings::default();
        let ctx = ctx(&bindings);
        let (line, width) = render_segments(&segs, &ctx, 4);
        assert_eq!(width, 4);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "CCCC");
    }

    #[test]
    fn render_segments_returns_empty_when_nothing_fits() {
        // available = 3: even the rightmost 4-cell segment doesn't fit.
        let segs: Vec<Box<dyn StatusSegment>> = vec![Box::new(Stub {
            id: "c",
            text: "CCCC",
        })];
        let bindings = Bindings::default();
        let ctx = ctx(&bindings);
        let (line, width) = render_segments(&segs, &ctx, 3);
        assert_eq!(width, 0);
        assert!(line.spans.is_empty());
    }

    #[test]
    fn render_segments_returns_empty_for_empty_segment_list() {
        let segs: Vec<Box<dyn StatusSegment>> = vec![];
        let bindings = Bindings::default();
        let ctx = ctx(&bindings);
        let (line, width) = render_segments(&segs, &ctx, 100);
        assert_eq!(width, 0);
        assert!(line.spans.is_empty());
    }

    #[test]
    fn render_segments_skips_segments_that_render_none() {
        // A segment that returns None must contribute nothing — no
        // width, no separator. Verifies that the kept-set is built
        // from rendered output, not from the input list's length.
        let segs: Vec<Box<dyn StatusSegment>> = vec![
            Box::new(Empty),
            Box::new(Stub {
                id: "b",
                text: "BBBB",
            }),
            Box::new(Empty),
            Box::new(Stub {
                id: "c",
                text: "CCCC",
            }),
        ];
        let bindings = Bindings::default();
        let ctx = ctx(&bindings);
        let (line, width) = render_segments(&segs, &ctx, 100);
        assert_eq!(width, 11);
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "BBBB │ CCCC");
    }

    #[test]
    fn render_segments_returns_empty_when_all_segments_yield_none() {
        // Every segment returns None — the rendered list is empty
        // before the drop pass even starts. The early-return at the
        // top of the drop pass must produce a clean (empty Line, 0)
        // rather than panic on an empty `keep` set.
        let segs: Vec<Box<dyn StatusSegment>> = vec![Box::new(Empty), Box::new(Empty)];
        let bindings = Bindings::default();
        let ctx = ctx(&bindings);
        let (line, width) = render_segments(&segs, &ctx, 100);
        assert_eq!(width, 0);
        assert!(line.spans.is_empty());
    }

    #[test]
    fn build_segments_skips_unknown_ids_and_warns() {
        // The warning side-effect goes to tracing; we just assert the
        // returned list contains the recognised ones in the requested
        // order and skips the unknown.
        let ids = vec!["model".to_string(), "bogus".to_string(), "repo".to_string()];
        let built = build_segments(&ids);
        assert_eq!(built.len(), 2);
        assert_eq!(built[0].id(), SEGMENT_MODEL);
        assert_eq!(built[1].id(), SEGMENT_REPO);
    }

    #[test]
    fn build_segments_handles_empty_list() {
        let built = build_segments(&[]);
        assert!(built.is_empty());
    }

    #[test]
    fn build_segments_recognises_all_four_built_ins() {
        let built = build_segments(&default_segment_ids());
        let ids: Vec<&str> = built.iter().map(|s| s.id()).collect();
        assert_eq!(
            ids,
            vec![
                SEGMENT_MODEL,
                SEGMENT_REPO,
                SEGMENT_BRANCH,
                SEGMENT_PREFIX_HINT
            ],
        );
    }

    #[test]
    fn stub_segment_id_returns_configured_value() {
        // Test stubs implement the trait method; calling it both
        // pins the helper's contract and avoids dead-code warnings
        // for the impl since the trait method is `#[allow(dead_code)]`
        // for production but the test stubs still satisfy the trait.
        let s = Stub {
            id: "stub-id",
            text: "x",
        };
        assert_eq!(s.id(), "stub-id");
        assert_eq!(Empty.id(), "empty");
    }
}
