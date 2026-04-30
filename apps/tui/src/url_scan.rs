//! Find URLs in a vt100 row.
//!
//! Used by the Ctrl+hover (highlight) and Ctrl+click (open) handlers.
//! vt100 0.16 does not preserve OSC 8 hyperlinks (`osc_dispatch` drops
//! them on the floor), so we recover URLs by scanning rendered text —
//! the same trick iTerm2 / VS Code use.
//!
//! Single-row only for v1: Claude almost never wraps URLs, and a wrap-
//! aware variant would have to walk `screen.row_wrapped()` and reason
//! about the seam between the trailing cells of one row and the
//! leading cells of the next. Defer until we see real complaints.
//!
//! AD-1 note: this reads vt100 *cell glyphs*, not Claude's protocol.
//! Same posture as a host terminal recognising URLs in scrollback.

use std::ops::Range;

use vt100::Screen;

/// Schemes we recognise. Restricted to the four that show up in
/// developer-tool output; broadening to `mailto:` / `tel:` etc. would
/// trade a wider catch for more false positives in code samples.
const SCHEMES: &[&str] = &["https://", "http://", "file://", "ftp://"];

/// One URL discovered on a row. `cols` is pane-relative and inclusive
/// on the low side, exclusive on the high — same convention as
/// `Range`-based slicing elsewhere in the runtime.
#[derive(Clone, Eq, PartialEq, Debug)]
pub(crate) struct UrlSpan {
    pub url: String,
    pub cols: Range<u16>,
}

/// Returns the URL whose column span covers `target_col` on `target_row`,
/// or `None` if the cell isn't inside any URL.
///
/// Cheap: one row scan, early-returns the first match. No allocations
/// on the no-URL path beyond a per-row text buffer that's reused
/// across calls if the caller cares. Caller currently invokes per
/// mouse event, which is fine — typical row width is < 200 cells.
pub(crate) fn find_url_at(screen: &Screen, target_row: u16, target_col: u16) -> Option<UrlSpan> {
    let (rows, cols) = screen.size();
    if target_row >= rows || target_col >= cols {
        return None;
    }
    let row_text = collect_row(screen, target_row, cols);
    scan_for_url_at(&row_text, target_col)
}

/// One byte in the assembled row text and the cell column it came
/// from. Lets us map a URL byte range back to a column range without
/// re-walking the row.
struct RowText {
    text: String,
    /// Parallel to `text`'s bytes: `col_of_byte[i]` is the column the
    /// byte at index `i` was emitted from.
    col_of_byte: Vec<u16>,
    /// One past the last column that contributed to `text`. Lets the
    /// scanner produce a half-open `cols` range without an off-by-one.
    end_col: u16,
}

/// Walk a row left-to-right, concatenating cell contents and recording
/// the source column of every byte. Wide-character continuation cells
/// contribute nothing (their sibling already wrote the glyph). Empty
/// cells contribute a single space so URL termination at whitespace
/// works naturally.
fn collect_row(screen: &Screen, row: u16, cols: u16) -> RowText {
    let cap = usize::from(cols);
    let mut text = String::with_capacity(cap);
    let mut col_of_byte: Vec<u16> = Vec::with_capacity(cap);
    let mut end_col = 0u16;
    for col in 0..cols {
        let Some(cell) = screen.cell(row, col) else {
            break;
        };
        // Track the past-the-last column we've observed even when the
        // cell is a wide-char continuation: the continuation still
        // *consumes* a column, and end_col needs to reflect that so a
        // URL ending in (say) an emoji reports the correct closing col.
        end_col = col.saturating_add(1);
        if cell.is_wide_continuation() {
            continue;
        }
        let contents = cell.contents();
        if contents.is_empty() {
            text.push(' ');
            col_of_byte.push(col);
        } else {
            for _ in 0..contents.len() {
                col_of_byte.push(col);
            }
            text.push_str(contents);
        }
    }
    RowText {
        text,
        col_of_byte,
        end_col,
    }
}

/// Find the first URL on `row` whose column range covers `target_col`,
/// or `None` if no URL spans that cell. Walks the schemes once and
/// returns as soon as it finds a hit, so the no-URL path is cheap and
/// a row with several URLs only pays for the ones up to the target.
fn scan_for_url_at(row: &RowText, target_col: u16) -> Option<UrlSpan> {
    let bytes = row.text.as_bytes();
    let mut search_from = 0_usize;
    while let Some((scheme_start, scheme_len)) = next_scheme(bytes, search_from) {
        let url_start = scheme_start;
        let url_end = walk_url_end(bytes, scheme_start + scheme_len);
        let trimmed_end = trim_trailing_punct(bytes, url_start, url_end);
        if trimmed_end > url_start + scheme_len {
            let col_start = row.col_of_byte[url_start];
            let col_end = row
                .col_of_byte
                .get(trimmed_end)
                .copied()
                .unwrap_or(row.end_col);
            if (col_start..col_end).contains(&target_col) {
                // SAFETY: `url_start` and `trimmed_end` are byte
                // indices that came from scanning ASCII scheme prefixes
                // and ASCII terminator characters, so they always land
                // on UTF-8 character boundaries even when the row
                // contains multi-byte text. String-byte slicing here
                // can't panic for the inputs collect_row produces.
                return Some(UrlSpan {
                    url: row.text[url_start..trimmed_end].to_string(),
                    cols: col_start..col_end,
                });
            }
        }
        search_from = url_end.max(scheme_start + 1);
    }
    None
}

/// Find the next scheme prefix (`https://`, `http://`, …) at or after
/// `from`. Returns `(byte_index, scheme_length)`.
fn next_scheme(bytes: &[u8], from: usize) -> Option<(usize, usize)> {
    (from..bytes.len()).find_map(|i| {
        SCHEMES
            .iter()
            .find(|s| bytes[i..].starts_with(s.as_bytes()))
            .map(|s| (i, s.len()))
    })
}

/// Walk forward from the first character after the scheme, consuming
/// bytes that are plausibly part of a URL. Stops at whitespace, ASCII
/// control chars, or a non-URL character (`<`, `>`, `"`, backtick,
/// `{`, `}`, `|`, `\`, `^`).
///
/// RFC 3986 reserved set is more permissive (allows `[]`, `()`, etc.),
/// but those characters frequently appear adjacent to URLs in markdown
/// and prose. We optimise for "the URL the user actually meant" over
/// "the URL the RFC technically allows".
fn walk_url_end(bytes: &[u8], start: usize) -> usize {
    bytes[start..]
        .iter()
        .position(|&b| {
            b.is_ascii_whitespace()
                || b < 0x20
                || matches!(
                    b,
                    b'<' | b'>' | b'"' | b'`' | b'{' | b'}' | b'|' | b'\\' | b'^'
                )
        })
        .map_or(bytes.len(), |off| start + off)
}

/// Trim trailing characters that are technically valid in a URL but
/// almost always belong to surrounding prose (`.`, `,`, `;`, `:`, `!`,
/// `?`, `)`, `]`). Matches the linkify behaviour every modern terminal
/// uses.
fn trim_trailing_punct(bytes: &[u8], start: usize, end: usize) -> usize {
    bytes[start..end]
        .iter()
        .rposition(|&b| !matches!(b, b'.' | b',' | b';' | b':' | b'!' | b'?' | b')' | b']'))
        .map_or(start, |last_keep| start + last_keep + 1)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use vt100::Parser;

    use super::*;

    fn parser_with(text: &str) -> Parser {
        let mut p = Parser::new(1, u16::try_from(text.len() + 4).unwrap_or(80), 0);
        p.process(text.as_bytes());
        p
    }

    fn find(p: &Parser, row: u16, col: u16) -> Option<UrlSpan> {
        find_url_at(p.screen(), row, col)
    }

    #[test]
    fn finds_https_url_and_returns_pane_relative_columns() {
        let p = parser_with("see https://example.com here");
        let span = find(&p, 0, 5).expect("col 5 is the 'h' of https");
        assert_eq!(span.url, "https://example.com");
        assert_eq!(span.cols, 4..23);
    }

    #[test]
    fn covers_every_column_of_the_url() {
        let p = parser_with("https://example.com");
        for col in 0..19u16 {
            let span = find(&p, 0, col).expect("inside URL");
            assert_eq!(span.url, "https://example.com");
        }
    }

    #[test]
    fn returns_none_for_columns_outside_url() {
        let p = parser_with("see https://example.com here");
        assert!(find(&p, 0, 0).is_none(), "before URL");
        assert!(find(&p, 0, 24).is_none(), "after URL");
    }

    #[test]
    fn trims_trailing_punctuation() {
        let p = parser_with("see https://example.com.");
        let span = find(&p, 0, 4).expect("on the 'h'");
        assert_eq!(span.url, "https://example.com");
    }

    #[test]
    fn url_in_parentheses_drops_the_close_paren() {
        let p = parser_with("(https://example.com)");
        let span = find(&p, 0, 1).expect("on the 'h'");
        assert_eq!(span.url, "https://example.com");
    }

    #[test]
    fn ignores_bare_scheme_with_no_authority() {
        let p = parser_with("text https:// more");
        assert!(find(&p, 0, 5).is_none());
    }

    #[test]
    fn handles_multiple_schemes() {
        let p = parser_with("http://a.b and https://c.d");
        let first = find(&p, 0, 0).expect("on http");
        assert_eq!(first.url, "http://a.b");
        let second = find(&p, 0, 15).expect("on https");
        assert_eq!(second.url, "https://c.d");
    }

    #[test]
    fn out_of_bounds_target_returns_none() {
        let p = parser_with("https://example.com");
        assert!(find(&p, 99, 0).is_none(), "row out of range");
        assert!(find(&p, 0, 9999).is_none(), "col out of range");
    }

    #[test]
    fn file_and_ftp_schemes_are_recognised() {
        let p = parser_with("file:///etc/hosts");
        let span = find(&p, 0, 0).expect("file scheme");
        assert_eq!(span.url, "file:///etc/hosts");

        let p = parser_with("ftp://ftp.example.com/x");
        let span = find(&p, 0, 0).expect("ftp scheme");
        assert_eq!(span.url, "ftp://ftp.example.com/x");
    }

    #[test]
    fn respects_terminator_chars_in_markdown_links() {
        let p = parser_with("<https://example.com>");
        let span = find(&p, 0, 1).expect("on 'h'");
        assert_eq!(span.url, "https://example.com");
    }
}
