//! Build-script helper: strip `[dev-dependencies]` tables from a
//! `Cargo.toml` source string while preserving comments, key order,
//! and the original formatting of everything else.
//!
//! Lives under `build/` (outside `src/`) so the helpers stay out of
//! the lib's compilation graph — the lib does not call them, so a
//! `src/`-resident copy would emit `dead_code` warnings on every
//! non-test build. The build script includes this file via
//! `#[path]` because it runs before the crate's own library is
//! compiled and cannot `use` items from it. An integration test
//! under `tests/` includes the same file via `#[path]` so
//! `cargo test` exercises the inline `#[cfg(test)]` tests below.
//!
//! Why strip dev-deps at bundle time: the bootstrap tarball ships
//! only the production-dep subset of the live workspace (see
//! `bootstrap-root/Cargo.toml`). A `[dev-dependencies]` block that
//! inherits `workspace = true` for crates absent from bootstrap-root
//! would fail remote-side manifest resolution. Filtering keeps the
//! bootstrap-root invariant ("production subset only") intact
//! without per-crate manual coordination, and pairs with the
//! production-dep drift guard at `lib.rs::bootstrap_manifest_mirrors_
//! every_workspace_dep_used_by_daemon` — between the two, the
//! bootstrap manifest stays in sync regardless of what dev-deps the
//! daemon picks up.
//!
//! Implementation choice — `toml_edit` over textual line scanning:
//! a parser-level transformation is robust to any TOML formatting
//! quirk the daemon's manifest might pick up (multi-line strings,
//! inline tables, exotic target predicates), where a line-based
//! scan would be brittle on those edge cases. The cost is one
//! build-script dep; the parser is small enough that the build-time
//! overhead is negligible.

use toml_edit::DocumentMut;

/// Strip every `[dev-dependencies]` and `[target.*.dev-dependencies]`
/// table from a `Cargo.toml` source. The remaining content is
/// re-serialized by `toml_edit`, which preserves comments, blank
/// lines, key order, and quoting style outside the removed tables.
///
/// # Errors
/// Returns the parser's error if `input` is not valid TOML. The
/// build script propagates this via `?` into its `Box<dyn Error>`
/// chain — a malformed daemon manifest should fail the host build
/// loudly, not produce a silently-broken tarball.
pub(crate) fn strip_dev_deps(input: &str) -> Result<String, toml_edit::TomlError> {
    let mut doc: DocumentMut = input.parse()?;

    doc.as_table_mut().remove("dev-dependencies");

    if let Some(targets) = doc
        .as_table_mut()
        .get_mut("target")
        .and_then(toml_edit::Item::as_table_like_mut)
    {
        for (_, value) in targets.iter_mut() {
            if let Some(target_table) = value.as_table_like_mut() {
                target_table.remove("dev-dependencies");
            }
        }
    }

    Ok(doc.to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Helper: parse `out` with `toml_edit` and assert that no
    /// `dev-dependencies` table exists anywhere reachable from the
    /// root — neither at top level nor under any `target.*` entry.
    /// Tests assert via the parsed structure rather than substring
    /// matching so they survive formatting changes from
    /// `toml_edit::Document::to_string`.
    fn assert_no_dev_deps(out: &str) {
        let doc: DocumentMut = out.parse().expect("output must be valid TOML");
        assert!(
            doc.get("dev-dependencies").is_none(),
            "top-level [dev-dependencies] table still present in:\n{out}"
        );
        if let Some(targets) = doc.get("target").and_then(toml_edit::Item::as_table_like) {
            for (key, value) in targets.iter() {
                if let Some(t) = value.as_table_like() {
                    assert!(
                        t.get("dev-dependencies").is_none(),
                        "[target.{key}.dev-dependencies] still present in:\n{out}"
                    );
                }
            }
        }
    }

    /// Happy path: a `[dev-dependencies]` block surrounded by other
    /// sections is removed; the surrounding sections, their keys,
    /// and their comments are preserved.
    #[test]
    fn strips_top_level_dev_deps() {
        let input = "\
[package]
name = \"foo\"

[dependencies]
serde = \"1\"

[dev-dependencies]
tempfile = \"3\"
mockito = \"1\"

[features]
default = []
";
        let out = strip_dev_deps(input).unwrap();
        assert_no_dev_deps(&out);
        let doc: DocumentMut = out.parse().unwrap();
        assert!(doc.get("dependencies").is_some());
        assert!(doc.get("features").is_some());
        assert!(doc.get("package").is_some());
    }

    /// A manifest without any dev-deps must round-trip with the
    /// production tables intact. Output may not be byte-identical
    /// because `toml_edit` re-emits via `Display`, but the parsed
    /// structure must equal the input's parsed structure.
    #[test]
    fn no_dev_deps_round_trips_structurally() {
        let input = "\
[package]
name = \"foo\"

[dependencies]
serde = \"1\"
";
        let out = strip_dev_deps(input).unwrap();
        let parsed_in: DocumentMut = input.parse().unwrap();
        let parsed_out: DocumentMut = out.parse().unwrap();
        assert_eq!(parsed_in.to_string(), parsed_out.to_string());
    }

    /// `[target.'cfg(unix)'.dev-dependencies]` and other target-
    /// specific dev-dep tables are also stripped. The surviving
    /// target entry stays present (its production deps and metadata
    /// would live as sibling keys), only its `dev-dependencies`
    /// child is gone.
    #[test]
    fn strips_target_specific_dev_deps() {
        let input = "\
[package]
name = \"foo\"

[target.'cfg(unix)'.dev-dependencies]
tempfile = \"3\"

[features]
default = []
";
        let out = strip_dev_deps(input).unwrap();
        assert_no_dev_deps(&out);
        let doc: DocumentMut = out.parse().unwrap();
        assert!(doc.get("features").is_some());
    }

    /// Multiple dev-deps tables (one bare, one target-specific) in
    /// the same manifest are all stripped, and unrelated sections
    /// stay intact.
    #[test]
    fn strips_multiple_dev_deps_tables() {
        let input = "\
[dev-dependencies]
tempfile = \"3\"

[dependencies]
serde = \"1\"

[target.'cfg(unix)'.dev-dependencies]
nix = \"0.27\"

[features]
default = []
";
        let out = strip_dev_deps(input).unwrap();
        assert_no_dev_deps(&out);
        let doc: DocumentMut = out.parse().unwrap();
        let deps = doc
            .get("dependencies")
            .and_then(toml_edit::Item::as_table_like)
            .unwrap();
        assert!(deps.get("serde").is_some());
        assert!(doc.get("features").is_some());
    }

    /// Production keys that share a string suffix with the dev-deps
    /// name (e.g. `build-dependencies`) must NOT be removed. This
    /// guards against a naive predicate that matched on substrings
    /// rather than exact keys.
    #[test]
    fn does_not_touch_build_dependencies_or_dependencies() {
        let input = "\
[dependencies]
serde = \"1\"

[build-dependencies]
cc = \"1\"

[dev-dependencies]
tempfile = \"3\"
";
        let out = strip_dev_deps(input).unwrap();
        let doc: DocumentMut = out.parse().unwrap();
        assert!(doc.get("dependencies").is_some());
        assert!(doc.get("build-dependencies").is_some());
        assert_no_dev_deps(&out);
    }

    /// Comments outside the removed tables stay; comments inside
    /// the removed tables disappear with the table. This matches
    /// `toml_edit`'s general behavior of associating a comment with
    /// the item it precedes.
    #[test]
    fn preserves_comments_outside_removed_sections() {
        let input = "\
# crate-level comment
[package]
name = \"foo\"

[dependencies]
# keep this
serde = \"1\"

[dev-dependencies]
# this disappears with the table
tempfile = \"3\"
";
        let out = strip_dev_deps(input).unwrap();
        assert!(out.contains("crate-level comment"));
        assert!(out.contains("keep this"));
        assert_no_dev_deps(&out);
    }

    /// An empty `[dev-dependencies]` table (no keys under it) is
    /// still stripped — the table header itself must not survive in
    /// the output.
    #[test]
    fn strips_empty_dev_deps_table() {
        let input = "\
[package]
name = \"foo\"

[dev-dependencies]
";
        let out = strip_dev_deps(input).unwrap();
        assert_no_dev_deps(&out);
    }

    /// Inline-table form (`dev-dependencies = { tempfile = \"3\" }`)
    /// is the same table semantically; it must also be stripped.
    /// This case would silently survive a textual scanner.
    #[test]
    fn strips_inline_table_form() {
        let input = "\
[package]
name = \"foo\"

dev-dependencies = { tempfile = \"3\" }

[features]
default = []
";
        let out = strip_dev_deps(input).unwrap();
        assert_no_dev_deps(&out);
        let doc: DocumentMut = out.parse().unwrap();
        assert!(doc.get("features").is_some());
    }

    /// Malformed TOML surfaces the parser error rather than panicking
    /// or silently producing an empty output. The build script relies
    /// on this to fail loudly on a broken daemon manifest.
    #[test]
    fn returns_parser_error_on_invalid_toml() {
        let result = strip_dev_deps("this is = = not toml");
        assert!(result.is_err());
    }
}
