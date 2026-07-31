// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Tests for the block-structure parser.

use super::parser::*;
use super::scan::*;
use super::*;

/// Helper: parse a tree with no frontmatter.
fn parse(source: &str) -> Tree {
    parse_tree(source, None)
}

#[test]
fn list_continuation_multibyte_whitespace_wellformed() {
    // Regression (fuzz_parse_tree / fuzz_inlines, ticket 22): an
    // all-whitespace list-continuation line containing a multi-byte
    // whitespace char (e.g. U+00A0 NBSP, U+2001 EM QUAD — both counted as
    // whitespace by `str::trim`, so the early-return guard is bypassed)
    // made `expanded_to_raw` return a byte offset inside the char, panicking
    // when the indentation was sliced off. Column->byte mapping must land on
    // a char boundary.
    for src in [
        "1. x\n  \u{a0}\n",   // ordered marker, content column 3, NBSP
        "-  x\n  \u{a0}\n",   // wide bullet, NBSP straddles the slice point
        "1. x\n  \u{2001}\n", // 3-byte multi-byte whitespace
        "- x\n\t\u{a0}\n",    // tab + NBSP in the continuation indent
    ] {
        crate::invariants::assert_tree_wellformed(&parse_tree(src, None));
    }
}

/// Helper: get the text of a span from source.
fn span_text<'a>(source: &'a str, span: &Span) -> &'a str {
    &source[span.start..span.end]
}

/// Helper: collect children of the root.
fn root_children(tree: &Tree) -> Vec<NodeId> {
    tree.children(tree.root()).to_vec()
}

/// Helper: assert a node is a specific kind and return it.
fn assert_kind<'a>(tree: &'a Tree, id: NodeId, expected: &ElementKind) -> &'a Node {
    let node = tree.node(id);
    assert_eq!(
        &node.kind, expected,
        "node {id} should be {expected:?}, got {:?}",
        node.kind
    );
    node
}

// --- Line splitting (encoding edge cases, ticket 21) ---

#[test]
fn split_lines_unix() {
    assert_eq!(
        split_lines("a\nb\nc"),
        vec!["a\n", "b\n", "c"],
        "LF lines retain their trailing newline; last line has none"
    );
    assert_eq!(
        split_lines("a\nb\n"),
        vec!["a\n", "b\n"],
        "a trailing LF does not produce an empty final line"
    );
}

#[test]
fn split_lines_crlf() {
    assert_eq!(
        split_lines("a\r\nb\r\n"),
        vec!["a\r\n", "b\r\n"],
        "CRLF is kept whole in each slice"
    );
}

#[test]
fn split_lines_bare_cr() {
    assert_eq!(
        split_lines("a\rb\rc"),
        vec!["a\r", "b\r", "c"],
        "bare CR (legacy Mac) is recognized as a line break"
    );
}

#[test]
fn split_lines_mixed_endings() {
    assert_eq!(
        split_lines("a\nb\r\nc\rd"),
        vec!["a\n", "b\r\n", "c\r", "d"],
        "LF, CRLF, and bare CR coexist in one document"
    );
}

#[test]
fn split_lines_reconstructs_source() {
    for src in [
        "a\nb\r\nc\rd",
        "\r\n\n\r",
        "no endings",
        "trailing\r\n",
        "中\r日\n本\r\n",
    ] {
        let joined: String = split_lines(src).concat();
        assert_eq!(
            joined, src,
            "concatenating the slices must reproduce the source exactly: {src:?}"
        );
    }
}

#[test]
fn line_content_end_all_endings() {
    assert_eq!(line_content_end("ab\ncd", 0), 2, "stops at the LF byte");
    assert_eq!(
        line_content_end("ab\r\ncd", 0),
        2,
        "stops at the CR of a CRLF pair (the content boundary)"
    );
    assert_eq!(line_content_end("ab\rcd", 0), 2, "stops at a bare CR");
    assert_eq!(
        line_content_end("abcd", 0),
        4,
        "runs to end of input when there is no line ending"
    );
}

#[test]
fn first_line_breaks_on_all_endings() {
    assert_eq!(first_line("ab\ncd"), "ab", "breaks on LF");
    assert_eq!(first_line("ab\r\ncd"), "ab", "breaks on CRLF");
    assert_eq!(first_line("ab\rcd"), "ab", "breaks on bare CR");
    assert_eq!(first_line("ab"), "ab", "whole string when no ending");
    assert_eq!(first_line(""), "", "empty input yields empty first line");
}

#[test]
fn content_lines_matches_str_lines_plus_bare_cr() {
    fn collect(s: &str) -> Vec<&str> {
        content_lines(s).collect()
    }

    // Matches `str::lines` for the common cases.
    assert_eq!(collect(""), Vec::<&str>::new(), "empty yields no lines");
    assert_eq!(collect("a"), vec!["a"], "single line, no ending");
    assert_eq!(
        collect("a\n"),
        vec!["a"],
        "trailing LF yields no empty line"
    );
    assert_eq!(collect("a\nb"), vec!["a", "b"], "LF separates lines");
    assert_eq!(collect("a\n\n"), vec!["a", ""], "interior blank line kept");
    assert_eq!(collect("a\r\nb"), vec!["a", "b"], "CRLF separates lines");

    // Unlike `str::lines`, a bare CR also splits.
    assert_eq!(
        collect("a\rb\rc"),
        vec!["a", "b", "c"],
        "bare CR separates lines (str::lines would not)"
    );
    assert_eq!(
        collect("a\r"),
        vec!["a"],
        "trailing bare CR yields no empty line"
    );
}

#[test]
fn bare_cr_splits_block_structure() {
    // Three ATX headings separated only by bare CRs must be recognized
    // as three separate headings, not one run-on line.
    let tree = parse("# A\r# B\r# C");
    let headings = tree.headings();
    assert_eq!(
        headings.len(),
        3,
        "bare CR must separate the three headings, got {}",
        headings.len()
    );
}

#[test]
fn anchors_harvest_a_id_and_name_block_and_inline() {
    // Issue 025: `Tree::anchors()` harvests `id`/`name` from `<a>` tags in
    // both opaque HTML blocks and inline raw HTML, and ignores `<a>` tags
    // without an anchor-defining attribute.
    let tree = parse(
        "<a id=\"block-id\"></a>\n\n\
             <a name=\"block-name\"></a>\n\n\
             A paragraph with an inline <a id=\"inline-id\"></a> anchor.\n\n\
             <a href=\"https://example.com\">a link, not a target</a>\n",
    );
    let anchors = tree.anchors();
    let ids: Vec<&str> = anchors.iter().map(|a| a.id.as_str()).collect();
    assert!(
        ids.contains(&"block-id"),
        "block-level `<a id>` is harvested: {ids:?}"
    );
    assert!(
        ids.contains(&"block-name"),
        "block-level `<a name>` is harvested: {ids:?}"
    );
    assert!(
        ids.contains(&"inline-id"),
        "inline `<a id>` is harvested: {ids:?}"
    );
    assert_eq!(
        ids.len(),
        3,
        "an `<a href>` with no id/name contributes no anchor: {ids:?}"
    );
}

#[test]
fn anchors_harvest_both_id_and_name_on_one_tag_and_skip_empty() {
    // A single `<a id="x" name="y">` yields both; an empty value is skipped.
    let tree = parse("<a id=\"x\" name=\"y\"></a>\n\n<a id=\"\"></a>\n");
    let anchors = tree.anchors();
    let ids: Vec<&str> = anchors.iter().map(|a| a.id.as_str()).collect();
    assert!(
        ids.contains(&"x") && ids.contains(&"y"),
        "both id and name on one tag are harvested: {ids:?}"
    );
    assert_eq!(
        ids.len(),
        2,
        "an empty `id` value contributes no anchor: {ids:?}"
    );
}

#[test]
fn anchors_harvest_id_from_any_element_but_name_only_from_a() {
    // Issue 025 (broadened to GitHub parity): a fragment `#x` resolves
    // against any element bearing `id="x"`, so `id` is harvested from a
    // `<div>`, `<span>`, and `<section>` — not only `<a>`. The legacy
    // `name`-as-anchor idiom stays `<a>`-specific: a `name` on a non-`<a>`
    // element is not an anchor.
    let tree = parse(
        "<div id=\"div-id\">\n\ncontent\n\n</div>\n\n\
             <span id=\"span-id\"></span>\n\n\
             <section id=\"section-id\">\n\nmore\n\n</section>\n\n\
             <div name=\"div-name\"></div>\n",
    );
    let anchors = tree.anchors();
    let ids: Vec<&str> = anchors.iter().map(|a| a.id.as_str()).collect();
    assert!(
        ids.contains(&"div-id"),
        "a `<div id>` is harvested as an anchor: {ids:?}"
    );
    assert!(
        ids.contains(&"span-id"),
        "a `<span id>` is harvested as an anchor: {ids:?}"
    );
    assert!(
        ids.contains(&"section-id"),
        "a `<section id>` is harvested as an anchor: {ids:?}"
    );
    assert!(
        !ids.contains(&"div-name"),
        "a `name` on a non-`<a>` element is not harvested: {ids:?}"
    );
    assert_eq!(
        ids.len(),
        3,
        "only the three element `id`s are harvested, not the `<div name>`: {ids:?}"
    );
}

#[test]
fn anchors_harvest_mid_paragraph_inline_element_id() {
    // Issue 026: a non-`<a>` `id` that appears mid-paragraph (inline raw
    // HTML, not a standalone block) is now materialized as an `InlineHtml`
    // node and harvested — closing the gap left by issue 025, which only
    // covered `<a>` and standalone HTML blocks.
    let tree = parse("Paragraph with an <span id=\"inline-anchor\"></span> target.\n");
    let anchors = tree.anchors();
    let ids: Vec<&str> = anchors.iter().map(|a| a.id.as_str()).collect();
    assert!(
        ids.contains(&"inline-anchor"),
        "a mid-paragraph `<span id>` is harvested as an anchor: {ids:?}"
    );
    assert_eq!(
        ids.len(),
        1,
        "exactly the one mid-paragraph inline id is harvested: {ids:?}"
    );
}

// --- Document root ---

#[test]
fn document_is_always_root() {
    let tree = parse("");
    assert_eq!(tree.root(), 0, "root is always node 0");
    assert_eq!(tree.node(0).kind, ElementKind::Document, "root is Document");
    assert!(tree.node(0).parent.is_none(), "root has no parent");
}

#[test]
fn empty_document_has_no_children() {
    let tree = parse("");
    assert!(
        root_children(&tree).is_empty(),
        "empty document has no children"
    );
}

// --- ATX headings ---

#[test]
fn atx_heading_levels() {
    let source = "# H1\n## H2\n### H3\n#### H4\n##### H5\n###### H6\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 6, "should find six headings");
    for (i, &id) in children.iter().enumerate() {
        let expected_level = (i + 1) as u8;
        assert_kind(
            &tree,
            id,
            &ElementKind::Heading {
                level: expected_level,
            },
        );
    }
}

#[test]
fn atx_heading_text_span() {
    let source = "## Hello World\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one heading");
    let node = assert_kind(&tree, children[0], &ElementKind::Heading { level: 2 });
    let line = &source[node.span.start..node.span.end]
        .lines()
        .next()
        .expect("heading should have a line");
    let (text_span, _) = extract_atx_content(line, node.span.start);
    assert_eq!(
        span_text(source, &text_span),
        "Hello World",
        "text span content"
    );
}

#[test]
fn atx_heading_with_explicit_id() {
    let source = "## My Heading {#custom-id}\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one heading");
    let node = assert_kind(&tree, children[0], &ElementKind::Heading { level: 2 });
    let line = &source[node.span.start..node.span.end]
        .lines()
        .next()
        .expect("should have a line");
    let (text_span, id) = extract_atx_content(line, node.span.start);
    assert_eq!(
        span_text(source, &text_span),
        "My Heading",
        "text span without id attribute"
    );
    let attr = id.expect("should have id attribute");
    assert_eq!(attr.id, "custom-id", "id text");
}

#[test]
fn atx_heading_trailing_hashes() {
    let source = "## Heading ##\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one heading");
    let node = assert_kind(&tree, children[0], &ElementKind::Heading { level: 2 });
    let line = &source[node.span.start..node.span.end]
        .lines()
        .next()
        .expect("should have a line");
    let (text_span, _) = extract_atx_content(line, node.span.start);
    assert_eq!(
        span_text(source, &text_span),
        "Heading",
        "trailing hashes stripped"
    );
}

#[test]
fn atx_heading_empty() {
    let source = "#\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one heading");
    let node = assert_kind(&tree, children[0], &ElementKind::Heading { level: 1 });
    let line = &source[node.span.start..node.span.end]
        .lines()
        .next()
        .expect("should have a line");
    let (text_span, _) = extract_atx_content(line, node.span.start);
    assert!(text_span.is_empty(), "empty heading has empty text span");
}

#[test]
fn atx_heading_with_leading_spaces() {
    let source = "   ## Indented\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one heading");
    let node = assert_kind(&tree, children[0], &ElementKind::Heading { level: 2 });
    let line = &source[node.span.start..node.span.end]
        .lines()
        .next()
        .expect("should have a line");
    let (text_span, _) = extract_atx_content(line, node.span.start);
    assert_eq!(span_text(source, &text_span), "Indented", "text content");
}

#[test]
fn four_leading_spaces_not_heading() {
    let source = "    ## Not a heading\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert!(
        !matches!(tree.node(children[0]).kind, ElementKind::Heading { .. }),
        "4+ spaces should not be a heading"
    );
}

// --- Setext headings ---

#[test]
fn setext_heading_level_1() {
    let source = "Heading\n=======\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert_kind(&tree, children[0], &ElementKind::Heading { level: 1 });
}

#[test]
fn setext_heading_level_2() {
    let source = "Heading\n-------\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert_kind(&tree, children[0], &ElementKind::Heading { level: 2 });
}

#[test]
fn setext_heading_multiline() {
    let source = "Line one\nLine two\n=========\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert_kind(&tree, children[0], &ElementKind::Heading { level: 1 });
    let node = tree.node(children[0]);
    assert_eq!(
        node.span,
        Span::new(0, source.len()),
        "setext heading span covers all lines"
    );
}

// --- Setext vs thematic break ---

#[test]
fn dashes_after_paragraph_is_setext() {
    let source = "Paragraph\n---\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert_kind(&tree, children[0], &ElementKind::Heading { level: 2 });
}

#[test]
fn dashes_after_blank_is_thematic_break() {
    let source = "\n---\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one non-blank block");
    assert_kind(&tree, children[0], &ElementKind::Rules);
}

#[test]
fn dashes_at_document_start_is_thematic_break() {
    let source = "---\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert_kind(&tree, children[0], &ElementKind::Rules);
}

// --- Thematic breaks ---

#[test]
fn thematic_break_stars() {
    let source = "***\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert_kind(&tree, children[0], &ElementKind::Rules);
}

#[test]
fn thematic_break_underscores() {
    let source = "___\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert_kind(&tree, children[0], &ElementKind::Rules);
}

#[test]
fn thematic_break_with_spaces() {
    let source = "* * * *\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert_kind(&tree, children[0], &ElementKind::Rules);
}

#[test]
fn thematic_break_with_many_chars() {
    let source = "----------\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert_kind(&tree, children[0], &ElementKind::Rules);
}

// --- Fenced code blocks ---

#[test]
fn fenced_code_backticks() {
    let source = "```\ncode here\n```\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert_kind(&tree, children[0], &ElementKind::CodeBlock);
}

#[test]
fn fenced_code_tildes() {
    let source = "~~~\ncode here\n~~~\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert_kind(&tree, children[0], &ElementKind::CodeBlock);
}

#[test]
fn fenced_code_with_info_string() {
    let source = "```rust\nfn main() {}\n```\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert_kind(&tree, children[0], &ElementKind::CodeBlock);
}

#[test]
fn fenced_code_unclosed() {
    let source = "```\ncode here\nmore code\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert_kind(&tree, children[0], &ElementKind::CodeBlock);
    assert_eq!(tree.diagnostics().len(), 1, "should emit one diagnostic");
    assert!(
        tree.diagnostics()[0].message.contains("unclosed"),
        "diagnostic mentions unclosed"
    );
}

#[test]
fn fenced_code_longer_close() {
    let source = "```\ncode\n`````\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert_kind(&tree, children[0], &ElementKind::CodeBlock);
}

#[test]
fn fenced_code_shorter_close_not_accepted() {
    let source = "````\ncode\n```\nmore\n````\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert_kind(&tree, children[0], &ElementKind::CodeBlock);
    let node = tree.node(children[0]);
    assert_eq!(
        node.span,
        Span::new(0, source.len()),
        "shorter fence is content, span covers entire block"
    );
}

// --- Block math ---

#[test]
fn block_math_basic() {
    let source = "$$\nx + y = z\n$$\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert_kind(&tree, children[0], &ElementKind::Math);
}

#[test]
fn block_math_unclosed() {
    let source = "$$\nmath content\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert_kind(&tree, children[0], &ElementKind::Math);
    assert_eq!(tree.diagnostics().len(), 1, "should emit one diagnostic");
    assert!(
        tree.diagnostics()[0].message.contains("unclosed"),
        "diagnostic mentions unclosed"
    );
}

// --- Indented code blocks ---

#[test]
fn indented_code_block() {
    let source = "    code line 1\n    code line 2\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert_kind(&tree, children[0], &ElementKind::CodeBlock);
}

#[test]
fn indented_code_not_after_paragraph() {
    let source = "Paragraph\n    continuation\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert_kind(&tree, children[0], &ElementKind::Paragraph);
}

// --- Block quotes ---

#[test]
fn block_quote_simple() {
    let source = "> quoted text\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    let node = assert_kind(&tree, children[0], &ElementKind::QuoteBlock);
    assert!(node.parent == Some(0), "block quote parent is Document");
    let quote_children = tree.children(children[0]);
    assert_eq!(quote_children.len(), 1, "block quote has one child");
    assert_kind(&tree, quote_children[0], &ElementKind::Paragraph);
}

#[test]
fn block_quote_multiline() {
    let source = "> line one\n> line two\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert_kind(&tree, children[0], &ElementKind::QuoteBlock);
}

#[test]
fn block_quote_lazy_continuation() {
    let source = "> first line\nlazy continuation\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert_kind(&tree, children[0], &ElementKind::QuoteBlock);
}

#[test]
fn block_quote_nested() {
    let source = "> > nested\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one outer block quote");
    assert_kind(&tree, children[0], &ElementKind::QuoteBlock);

    let outer_children = tree.children(children[0]);
    assert_eq!(outer_children.len(), 1, "outer has one child");
    assert_kind(&tree, outer_children[0], &ElementKind::QuoteBlock);

    let inner_children = tree.children(outer_children[0]);
    assert_eq!(inner_children.len(), 1, "inner has one child");
    assert_kind(&tree, inner_children[0], &ElementKind::Paragraph);
}

#[test]
fn block_quote_with_heading() {
    let source = "> # Heading\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert_kind(&tree, children[0], &ElementKind::QuoteBlock);

    let quote_children = tree.children(children[0]);
    assert_eq!(quote_children.len(), 1, "block quote has one child");
    assert_kind(&tree, quote_children[0], &ElementKind::Heading { level: 1 });
}

#[test]
fn block_quote_with_code_block() {
    let source = "> ```\n> code\n> ```\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert_kind(&tree, children[0], &ElementKind::QuoteBlock);

    let quote_children = tree.children(children[0]);
    assert_eq!(quote_children.len(), 1, "block quote has one child");
    assert_kind(&tree, quote_children[0], &ElementKind::CodeBlock);
}

#[test]
fn block_quote_with_thematic_break() {
    let source = "> ***\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert_kind(&tree, children[0], &ElementKind::QuoteBlock);

    let quote_children = tree.children(children[0]);
    assert_eq!(quote_children.len(), 1, "block quote has one child");
    assert_kind(&tree, quote_children[0], &ElementKind::Rules);
}

// --- Admonitions ---

#[test]
fn gfm_admonition_warning() {
    let source = "> [!WARNING]\n> Be careful!\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert_kind(
        &tree,
        children[0],
        &ElementKind::Admonition {
            kind: "WARNING".to_string(),
        },
    );
}

#[test]
fn gfm_admonition_note() {
    let source = "> [!NOTE]\n> Some note text\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert_kind(
        &tree,
        children[0],
        &ElementKind::Admonition {
            kind: "NOTE".to_string(),
        },
    );
}

#[test]
fn gfm_admonition_case_insensitive() {
    let source = "> [!tip]\n> Some tip\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert_kind(
        &tree,
        children[0],
        &ElementKind::Admonition {
            kind: "TIP".to_string(),
        },
    );
}

#[test]
fn plain_blockquote_not_admonition() {
    let source = "> Just a quote\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert_kind(&tree, children[0], &ElementKind::QuoteBlock);
}

#[test]
fn admonition_has_paragraph_children() {
    let source = "> [!WARNING]\n> Be careful!\n";
    let tree = parse(source);
    let children = root_children(&tree);
    let adm_children = tree.children(children[0]);

    assert!(
        adm_children
            .iter()
            .any(|&c| matches!(tree.node(c).kind, ElementKind::Paragraph)),
        "admonition should contain paragraph children"
    );
}

// --- HTML blocks ---

#[test]
fn html_block_type1_pre() {
    let source = "<pre>\ncode\n</pre>\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert_kind(&tree, children[0], &ElementKind::HtmlBlock);
}

#[test]
fn html_block_type2_comment() {
    let source = "<!-- comment -->\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert_kind(&tree, children[0], &ElementKind::HtmlBlock);
}

#[test]
fn html_block_type6_div() {
    let source = "<div>\ncontent\n</div>\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one container");
    let node = assert_kind(&tree, children[0], &ElementKind::Container);
    assert_eq!(node.syntax, Syntax::Html, "syntax is Html");
}

#[test]
fn html_block_type7_cannot_interrupt_paragraph() {
    let source = "Paragraph\n<span>inline</span>\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert_kind(&tree, children[0], &ElementKind::Paragraph);
}

// --- Paragraphs ---

#[test]
fn simple_paragraph() {
    let source = "Hello world.\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert_kind(&tree, children[0], &ElementKind::Paragraph);
}

#[test]
fn multiline_paragraph() {
    let source = "Line one.\nLine two.\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert_kind(&tree, children[0], &ElementKind::Paragraph);
}

// --- Mixed constructs ---

#[test]
fn mixed_blocks() {
    let source = "# Heading\n\nParagraph text.\n\n---\n\n```\ncode\n```\n";
    let tree = parse(source);
    let children = root_children(&tree);

    // Blank lines are not nodes.
    assert_eq!(children.len(), 4, "should find four non-blank blocks");
    assert_kind(&tree, children[0], &ElementKind::Heading { level: 1 });
    assert_kind(&tree, children[1], &ElementKind::Paragraph);
    assert_kind(&tree, children[2], &ElementKind::Rules);
    assert_kind(&tree, children[3], &ElementKind::CodeBlock);
}

// --- Tab expansion ---

#[test]
fn tab_expansion_basic() {
    let (expanded, _) = expand_leading_tabs("\tcode");
    assert_eq!(expanded, "    code", "tab at column 0 expands to 4 spaces");
}

#[test]
fn tab_expansion_partial() {
    let (expanded, _) = expand_leading_tabs(" \tcode");
    assert_eq!(expanded, "    code", "tab at column 1 expands to 3 spaces");
}

#[test]
fn tab_expansion_aligned() {
    let (expanded, _) = expand_leading_tabs("    \tcode");
    assert_eq!(
        expanded, "        code",
        "tab at column 4 expands to 4 spaces"
    );
}

#[test]
fn tab_indented_code_block() {
    let source = "\tcode line\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert_kind(&tree, children[0], &ElementKind::CodeBlock);
}

#[test]
fn expanded_to_raw_no_tabs() {
    let raw = "- item";
    let (_, mappings) = expand_leading_tabs(raw);
    assert_eq!(
        expanded_to_raw(2, raw, &mappings),
        2,
        "no tabs: offset unchanged"
    );
}

#[test]
fn expanded_to_raw_single_tab() {
    // "\t- item" → "    - item" (tab at byte 0, 4 spaces)
    let raw = "\t- item";
    let (expanded, mappings) = expand_leading_tabs(raw);
    assert_eq!(expanded, "    - item", "expansion sanity check");
    // Offset 4 in expanded is `-`, which is byte 1 in raw
    assert_eq!(
        expanded_to_raw(4, raw, &mappings),
        1,
        "offset past tab maps to byte after tab"
    );
    // Offset 6 in expanded is `i`, which is byte 3 in raw
    assert_eq!(
        expanded_to_raw(6, raw, &mappings),
        3,
        "offset well past tab maps correctly"
    );
    // Offset 2 is inside the tab expansion → maps to byte 1 (past tab)
    assert_eq!(
        expanded_to_raw(2, raw, &mappings),
        1,
        "offset inside tab expansion maps past tab byte"
    );
}

#[test]
fn expanded_to_raw_two_tabs() {
    // "\t\t- x" → "        - x" (8 spaces, then "- x")
    let raw = "\t\t- x";
    let (expanded, mappings) = expand_leading_tabs(raw);
    assert_eq!(expanded, "        - x", "expansion sanity check");
    // Offset 8 in expanded is `-`, which is byte 2 in raw
    assert_eq!(
        expanded_to_raw(8, raw, &mappings),
        2,
        "offset past both tabs"
    );
    // Offset 5 is inside second tab → maps to byte 2 (past second tab)
    assert_eq!(
        expanded_to_raw(5, raw, &mappings),
        2,
        "offset inside second tab expansion"
    );
    // Offset 0 is before any tab
    assert_eq!(expanded_to_raw(0, raw, &mappings), 0, "offset 0 stays at 0");
}

#[test]
fn expanded_to_raw_partial_tab() {
    // " \t- item" → "    - item" (space + tab at col 1 → 3 spaces)
    let raw = " \t- item";
    let (expanded, mappings) = expand_leading_tabs(raw);
    assert_eq!(expanded, "    - item", "expansion sanity check");
    // Offset 4 is `-`, byte 2 in raw
    assert_eq!(
        expanded_to_raw(4, raw, &mappings),
        2,
        "offset past partial tab"
    );
    // Offset 1 is at expanded_col of the tab → inside expansion
    assert_eq!(
        expanded_to_raw(1, raw, &mappings),
        1,
        "offset at tab start maps to tab byte"
    );
}

#[test]
fn expanded_to_raw_clamped_to_raw_len() {
    let raw = "ab";
    let (_, mappings) = expand_leading_tabs(raw);
    assert_eq!(
        expanded_to_raw(100, raw, &mappings),
        2,
        "offset beyond raw len is clamped"
    );
}

#[test]
fn tab_not_expanded_inside_content() {
    let (expanded, _) = expand_leading_tabs("text\there");
    assert_eq!(expanded, "text\there", "tab inside content is preserved");
}

// --- Frontmatter ---

#[test]
fn frontmatter_is_first_child() {
    let source = "---\ntitle: test\n---\n# Heading\n";
    let fm_end = source.find("# Heading").expect("should find heading");
    let tree = parse_tree(source, Some(Span::new(0, fm_end)));
    let children = root_children(&tree);

    assert_eq!(children.len(), 2, "should find frontmatter + heading");
    assert_kind(&tree, children[0], &ElementKind::Frontmatter);
    assert_kind(&tree, children[1], &ElementKind::Heading { level: 1 });

    assert_eq!(
        tree.node(children[0]).syntax,
        Syntax::Yaml,
        "frontmatter has Yaml syntax"
    );
}

#[test]
fn body_offset_shifts_spans() {
    let source = "---\ntitle: test\n---\n# Heading\n";
    let body_offset = source.find("# Heading").expect("should find heading");
    let tree = parse_tree(source, Some(Span::new(0, body_offset)));
    let children = root_children(&tree);

    let heading_id = children
        .iter()
        .find(|&&id| matches!(tree.node(id).kind, ElementKind::Heading { .. }))
        .expect("should find heading");
    let node = tree.node(*heading_id);
    let line = &source[node.span.start..node.span.end]
        .lines()
        .next()
        .expect("should have a line");
    let (text_span, _) = extract_atx_content(line, node.span.start);
    assert_eq!(
        span_text(source, &text_span),
        "Heading",
        "text span in original source with offset"
    );
}

// --- Span correctness ---

#[test]
fn spans_cover_original_source() {
    let source = "# Heading\n\nParagraph\n";
    let tree = parse(source);

    for node in tree.nodes() {
        let text = span_text(source, &node.span);
        assert!(
            !text.is_empty() || matches!(node.kind, ElementKind::Document),
            "node span should reference source text: {:?}",
            node.kind
        );
    }
}

#[test]
fn no_text_copied() {
    let source = "## Title\n\n> Quote\n\n```\ncode\n```\n\n---\n";
    let tree = parse(source);

    for node in tree.nodes() {
        assert!(
            node.span.start <= node.span.end,
            "span start <= end: {:?}",
            node.kind
        );
        assert!(
            node.span.end <= source.len(),
            "span end <= source length: {:?}",
            node.kind
        );
    }
}

// --- Parent/children ---

#[test]
fn parent_children_consistency() {
    let source = "# Heading\n\nParagraph\n\n> Quote\n";
    let tree = parse(source);

    for (id, node) in tree.nodes().iter().enumerate() {
        for &child_id in &node.children {
            assert_eq!(
                tree.node(child_id).parent,
                Some(id),
                "child {child_id} should have parent {id}"
            );
        }
        if let Some(pid) = node.parent {
            assert!(
                tree.node(pid).children.contains(&id),
                "node {id} should be in parent {pid}'s children"
            );
        }
    }
}

#[test]
fn children_in_document_order() {
    let source = "# First\n\n## Second\n\nParagraph\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 3, "should have three children");
    for window in children.windows(2) {
        let a = tree.node(window[0]);
        let b = tree.node(window[1]);
        assert!(
            a.span.start < b.span.start,
            "children should be in document order: {:?} before {:?}",
            a.kind,
            b.kind
        );
    }
}

// --- HTML block types ---

#[test]
fn html_block_type3_processing_instruction() {
    let source = "<?xml version=\"1.0\"?>\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert_kind(&tree, children[0], &ElementKind::HtmlBlock);
}

#[test]
fn html_block_type4_declaration() {
    let source = "<!DOCTYPE html>\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert_kind(&tree, children[0], &ElementKind::HtmlBlock);
}

#[test]
fn html_block_type5_cdata() {
    let source = "<![CDATA[\nsome data\n]]>\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert_kind(&tree, children[0], &ElementKind::HtmlBlock);
}

#[test]
fn html_block_multiline_comment() {
    let source = "<!-- start\nmiddle\nend -->\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one block");
    assert_kind(&tree, children[0], &ElementKind::HtmlBlock);
}

// --- Blank lines ---

#[test]
fn blank_lines_are_not_nodes() {
    let source = "\n\n\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert!(
        children.is_empty(),
        "blank lines should not produce child nodes"
    );
}

// --- Nested block quote tests ---

#[test]
fn nested_block_quotes_produce_nested_containers() {
    let source = "> > > deeply nested\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "one top-level quote");
    let l1 = children[0];
    assert_kind(&tree, l1, &ElementKind::QuoteBlock);

    let l1_children = tree.children(l1);
    assert_eq!(l1_children.len(), 1, "one child at level 1");
    let l2 = l1_children[0];
    assert_kind(&tree, l2, &ElementKind::QuoteBlock);

    let l2_children = tree.children(l2);
    assert_eq!(l2_children.len(), 1, "one child at level 2");
    let l3 = l2_children[0];
    assert_kind(&tree, l3, &ElementKind::QuoteBlock);

    let l3_children = tree.children(l3);
    assert_eq!(l3_children.len(), 1, "leaf content at level 3");
    assert_kind(&tree, l3_children[0], &ElementKind::Paragraph);
}

#[test]
fn every_node_has_span() {
    let source = "# H\n\n> text\n\n```\ncode\n```\n";
    let tree = parse(source);

    for node in tree.nodes() {
        if matches!(node.kind, ElementKind::Document) {
            assert_eq!(node.span, Span::new(0, source.len()), "document span");
        } else {
            assert!(
                node.span.start < node.span.end,
                "non-document node should have non-empty span: {:?}",
                node.kind
            );
        }
    }
}

#[test]
fn block_quote_child_span_excludes_markers() {
    let source = "> # Heading\n";
    let tree = parse(source);
    let children = root_children(&tree);
    let quote_children = tree.children(children[0]);
    let heading = tree.node(quote_children[0]);

    // Heading span starts after "> ", not at the raw line start.
    assert_eq!(
        heading.span.start, 2,
        "heading span starts after quote marker"
    );
    assert_eq!(
        &source[heading.span.start..heading.span.end],
        "# Heading\n",
        "heading span content excludes marker"
    );
}

#[test]
fn nested_quote_child_spans_exclude_all_markers() {
    let source = "> > text\n";
    let tree = parse(source);

    // Outer QuoteBlock starts at 0 (owns the first `>`).
    let outer = root_children(&tree)[0];
    assert_eq!(
        tree.node(outer).span.start,
        0,
        "outer quote starts at raw line start"
    );

    // Inner QuoteBlock starts at 2 (owns the second `>`).
    let inner = tree.children(outer)[0];
    assert_eq!(
        tree.node(inner).span.start,
        2,
        "inner quote starts after first marker"
    );

    // Paragraph starts at 4 (after both `> >`).
    let para = tree.children(inner)[0];
    assert_eq!(
        tree.node(para).span.start,
        4,
        "paragraph starts after all markers"
    );
    assert_eq!(
        &source[tree.node(para).span.start..tree.node(para).span.end],
        "text\n",
        "paragraph content excludes all markers"
    );
}

// --- Lists: basic ---

#[test]
fn single_unordered_item() {
    let source = "- item\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one list");
    let list_id = children[0];
    assert!(
        matches!(
            tree.node(list_id).kind,
            ElementKind::List {
                ordered: false,
                tight: true,
                ..
            }
        ),
        "should be an unordered tight list"
    );

    let items = tree.children(list_id);
    assert_eq!(items.len(), 1, "list has one item");
    assert_kind(&tree, items[0], &ElementKind::ListItem { task: None });

    let item_children = tree.children(items[0]);
    assert_eq!(item_children.len(), 1, "item has one child");
    assert_kind(&tree, item_children[0], &ElementKind::Paragraph);
}

#[test]
fn multi_item_unordered() {
    let source = "- a\n- b\n- c\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one list");
    let items = tree.children(children[0]);
    assert_eq!(items.len(), 3, "list has three items");
    for &item in items {
        assert!(
            matches!(tree.node(item).kind, ElementKind::ListItem { task: None }),
            "each item is a regular ListItem"
        );
    }
}

#[test]
fn unordered_marker_star() {
    let source = "* item\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one list");
    assert!(
        matches!(
            tree.node(children[0]).kind,
            ElementKind::List { ordered: false, .. }
        ),
        "star marker produces unordered list"
    );
}

#[test]
fn unordered_marker_plus() {
    let source = "+ item\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one list");
    assert!(
        matches!(
            tree.node(children[0]).kind,
            ElementKind::List { ordered: false, .. }
        ),
        "plus marker produces unordered list"
    );
}

#[test]
fn ordered_list_dot() {
    let source = "1. first\n2. second\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one list");
    assert!(
        matches!(
            tree.node(children[0]).kind,
            ElementKind::List {
                ordered: true,
                start: 1,
                ..
            }
        ),
        "ordered list with dot delimiter"
    );
    let items = tree.children(children[0]);
    assert_eq!(items.len(), 2, "list has two items");
}

#[test]
fn ordered_list_paren() {
    let source = "1) first\n2) second\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one list");
    assert!(
        matches!(
            tree.node(children[0]).kind,
            ElementKind::List {
                ordered: true,
                start: 1,
                ..
            }
        ),
        "ordered list with paren delimiter"
    );
}

#[test]
fn ordered_list_start_number() {
    let source = "3. third\n4. fourth\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "should find one list");
    assert!(
        matches!(
            tree.node(children[0]).kind,
            ElementKind::List {
                ordered: true,
                start: 3,
                ..
            }
        ),
        "ordered list preserves start number"
    );
}

// --- Lists: structure ---

#[test]
fn list_items_are_children_of_list() {
    let source = "- a\n- b\n";
    let tree = parse(source);
    let list_id = root_children(&tree)[0];
    let items = tree.children(list_id);

    for &item_id in items {
        assert_eq!(
            tree.node(item_id).parent,
            Some(list_id),
            "item parent is the list"
        );
    }
}

#[test]
fn list_span_covers_all_items() {
    let source = "- a\n- b\n- c\n";
    let tree = parse(source);
    let list = tree.node(root_children(&tree)[0]);

    assert_eq!(
        list.span,
        Span::new(0, source.len()),
        "list span covers entire content"
    );
}

// --- Lists: nested ---

#[test]
fn nested_list_two_levels() {
    let source = "- outer\n  - inner\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "one top-level list");
    let outer_items = tree.children(children[0]);
    assert_eq!(outer_items.len(), 1, "one outer item");

    // Outer item contains: paragraph + nested list
    let outer_item_children = tree.children(outer_items[0]);
    assert!(
        outer_item_children.len() >= 2,
        "outer item has paragraph + nested list, got {}",
        outer_item_children.len()
    );

    // Find the nested list
    let nested_list = outer_item_children
        .iter()
        .find(|&&id| matches!(tree.node(id).kind, ElementKind::List { .. }))
        .expect("should find nested list");
    let nested_items = tree.children(*nested_list);
    assert_eq!(nested_items.len(), 1, "nested list has one item");
}

#[test]
fn nested_list_three_levels() {
    let source = "- a\n  - b\n    - c\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "one top-level list");
    let l1_items = tree.children(children[0]);
    let l1_item_children = tree.children(l1_items[0]);

    // Find level 2 list
    let l2_list = l1_item_children
        .iter()
        .find(|&&id| matches!(tree.node(id).kind, ElementKind::List { .. }))
        .expect("should find level 2 list");
    let l2_items = tree.children(*l2_list);
    let l2_item_children = tree.children(l2_items[0]);

    // Find level 3 list
    let l3_list = l2_item_children
        .iter()
        .find(|&&id| matches!(tree.node(id).kind, ElementKind::List { .. }))
        .expect("should find level 3 list");
    let l3_items = tree.children(*l3_list);
    assert_eq!(l3_items.len(), 1, "level 3 has one item");
}

// --- Lists: tight vs loose ---

#[test]
fn tight_list_no_blanks() {
    let source = "- a\n- b\n- c\n";
    let tree = parse(source);
    let list = tree.node(root_children(&tree)[0]);

    assert!(
        matches!(list.kind, ElementKind::List { tight: true, .. }),
        "no blank lines → tight"
    );
}

#[test]
fn loose_list_blank_between_items() {
    let source = "- a\n\n- b\n";
    let tree = parse(source);
    let list = tree.node(root_children(&tree)[0]);

    assert!(
        matches!(list.kind, ElementKind::List { tight: false, .. }),
        "blank between items → loose"
    );
}

#[test]
fn blank_within_item_makes_loose() {
    let source = "- a\n\n  b\n- c\n";
    let tree = parse(source);
    let children = root_children(&tree);
    let list = tree.node(children[0]);

    // Per CommonMark, a blank line within any list item makes
    // the entire list loose — all items get paragraph wrappers.
    assert!(
        matches!(list.kind, ElementKind::List { tight: false, .. }),
        "blank within item makes list loose"
    );
}

// --- Lists: task items ---

#[test]
fn task_item_unchecked() {
    let source = "- [ ] todo\n";
    let tree = parse(source);
    let list_id = root_children(&tree)[0];
    let items = tree.children(list_id);

    assert_eq!(items.len(), 1, "one item");
    assert_kind(
        &tree,
        items[0],
        &ElementKind::ListItem { task: Some(false) },
    );
}

#[test]
fn task_item_checked() {
    let source = "- [x] done\n";
    let tree = parse(source);
    let list_id = root_children(&tree)[0];
    let items = tree.children(list_id);

    assert_kind(&tree, items[0], &ElementKind::ListItem { task: Some(true) });
}

#[test]
fn task_item_checked_uppercase() {
    let source = "- [X] done\n";
    let tree = parse(source);
    let list_id = root_children(&tree)[0];
    let items = tree.children(list_id);

    assert_kind(&tree, items[0], &ElementKind::ListItem { task: Some(true) });
}

#[test]
fn mixed_task_and_regular() {
    let source = "- [ ] todo\n- regular\n- [x] done\n";
    let tree = parse(source);
    let list_id = root_children(&tree)[0];
    let items = tree.children(list_id);

    assert_eq!(items.len(), 3, "three items");
    assert_kind(
        &tree,
        items[0],
        &ElementKind::ListItem { task: Some(false) },
    );
    assert_kind(&tree, items[1], &ElementKind::ListItem { task: None });
    assert_kind(&tree, items[2], &ElementKind::ListItem { task: Some(true) });
}

// --- Lists: continuation ---

#[test]
fn multiline_item_continuation() {
    let source = "- line one\n  line two\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "one list");
    let items = tree.children(children[0]);
    assert_eq!(items.len(), 1, "one item");
    let item_children = tree.children(items[0]);
    assert_eq!(item_children.len(), 1, "item has one paragraph");
    assert_kind(&tree, item_children[0], &ElementKind::Paragraph);
}

#[test]
fn lazy_continuation_no_indent() {
    let source = "- first\nlazy line\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "one list");
    let items = tree.children(children[0]);
    assert_eq!(items.len(), 1, "one item");
    let item_children = tree.children(items[0]);
    assert_eq!(item_children.len(), 1, "item has one paragraph");
    assert_kind(&tree, item_children[0], &ElementKind::Paragraph);
}

#[test]
fn lazy_continuation_broken_by_blank() {
    let source = "- first\n\nnot in list\n";
    let tree = parse(source);
    let children = root_children(&tree);

    // Blank line + unindented line closes the list.
    assert!(children.len() >= 2, "list + paragraph");
    assert!(
        matches!(tree.node(children[0]).kind, ElementKind::List { .. }),
        "first child is list"
    );
    assert_kind(&tree, children[children.len() - 1], &ElementKind::Paragraph);
}

#[test]
fn lazy_continuation_broken_by_list_marker() {
    let source = "- first\n+ second\n";
    let tree = parse(source);
    let children = root_children(&tree);

    // `+ second` is a different marker → new list, not lazy continuation.
    assert_eq!(children.len(), 2, "two lists");
}

#[test]
fn blockquote_list_closed_by_lazy_list_marker() {
    // `> - foo` opens a list inside a block quote; the unmarked `- bar`
    // cannot lazily continue (a list marker is a block construct), so the
    // quote and its list close and a new top-level list begins.
    //
    // Regression: closing the quote must keep `list_stack` in sync with
    // `scope_stack` so the subsequent item transition does not spin
    // popping a list item that was already removed.
    let source = "> - foo\n- bar\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 2, "block quote then a top-level list");
    assert!(
        matches!(tree.node(children[0]).kind, ElementKind::QuoteBlock),
        "first child is the block quote"
    );
    assert!(
        matches!(tree.node(children[1]).kind, ElementKind::List { .. }),
        "second child is a new top-level list"
    );
    // The quoted list is nested inside the block quote, not the top list.
    let quoted_lists = tree
        .children(children[0])
        .iter()
        .filter(|&&id| matches!(tree.node(id).kind, ElementKind::List { .. }))
        .count();
    assert_eq!(quoted_lists, 1, "one list nested in the block quote");
}

// --- Lists: marker changes ---

#[test]
fn different_marker_starts_new_list() {
    let source = "* item a\n- item b\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 2, "two separate lists");
    assert!(
        matches!(tree.node(children[0]).kind, ElementKind::List { .. }),
        "first is a list"
    );
    assert!(
        matches!(tree.node(children[1]).kind, ElementKind::List { .. }),
        "second is a list"
    );
}

// --- Lists: items with block constructs ---

#[test]
fn item_containing_fenced_code() {
    let source = "- code:\n  ```\n  fn main() {}\n  ```\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "one list");
    let items = tree.children(children[0]);
    assert_eq!(items.len(), 1, "one item");
    let item_children = tree.children(items[0]);

    let has_code = item_children
        .iter()
        .any(|&id| matches!(tree.node(id).kind, ElementKind::CodeBlock));
    assert!(has_code, "item should contain a code block");
}

#[test]
fn item_containing_block_quote() {
    let source = "- text\n  > quoted\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "one list");
    let items = tree.children(children[0]);
    let item_children = tree.children(items[0]);

    let has_quote = item_children
        .iter()
        .any(|&id| matches!(tree.node(id).kind, ElementKind::QuoteBlock));
    assert!(has_quote, "item should contain a block quote");
}

#[test]
fn fence_at_list_boundary_closes_code_block() {
    // Closing fence at indent 0 while code block is inside a list
    // item (content_column=2). The fence should close the code block,
    // not produce an unclosed diagnostic.
    let source = "- ```\n  code\n```\n";
    let tree = parse(source);

    assert!(
        tree.diagnostics().is_empty(),
        "no unclosed diagnostic: {:?}",
        tree.diagnostics()
    );

    let children = root_children(&tree);
    assert_eq!(children.len(), 1, "one list");
    let items = tree.children(children[0]);
    let item_children = tree.children(items[0]);

    let has_code = item_children
        .iter()
        .any(|&id| matches!(tree.node(id).kind, ElementKind::CodeBlock));
    assert!(has_code, "item should contain a code block");
}

// --- Lists: interactions ---

#[test]
fn thematic_break_not_list_dashes() {
    let source = "---\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "one block");
    assert_kind(&tree, children[0], &ElementKind::Rules);
}

#[test]
fn thematic_break_not_list_spaced_dashes() {
    let source = "- - -\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "one block");
    assert_kind(&tree, children[0], &ElementKind::Rules);
}

#[test]
fn list_after_paragraph() {
    let source = "Paragraph\n- item\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 2, "paragraph + list");
    assert_kind(&tree, children[0], &ElementKind::Paragraph);
    assert!(
        matches!(tree.node(children[1]).kind, ElementKind::List { .. }),
        "second child is a list"
    );
}

#[test]
fn ordered_start_not_1_cannot_interrupt_paragraph() {
    let source = "Paragraph\n3. item\n";
    let tree = parse(source);
    let children = root_children(&tree);

    // "3. item" cannot interrupt a paragraph, so it's part of the
    // paragraph continuation.
    assert_eq!(children.len(), 1, "single paragraph");
    assert_kind(&tree, children[0], &ElementKind::Paragraph);
}

// --- Tables: basic ---

#[test]
fn basic_table() {
    let source = "| A | B |\n| --- | --- |\n| 1 | 2 |\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "one table");
    let table = tree.node(children[0]);
    assert!(
        matches!(&table.kind, ElementKind::Table { alignments } if alignments.len() == 2),
        "table with 2 columns"
    );

    let rows = tree.children(children[0]);
    assert_eq!(rows.len(), 2, "header row + 1 body row");

    // Header row.
    assert_kind(&tree, rows[0], &ElementKind::TableRow { header: true });
    let header_cells = tree.children(rows[0]);
    assert_eq!(header_cells.len(), 2, "header has 2 cells");
    assert_kind(&tree, header_cells[0], &ElementKind::TableCell);
    assert_kind(&tree, header_cells[1], &ElementKind::TableCell);
    assert_eq!(
        tree.text(&tree.node(header_cells[0]).span),
        "A",
        "first header cell text"
    );
    assert_eq!(
        tree.text(&tree.node(header_cells[1]).span),
        "B",
        "second header cell text"
    );

    // Body row.
    assert_kind(&tree, rows[1], &ElementKind::TableRow { header: false });
    let body_cells = tree.children(rows[1]);
    assert_eq!(body_cells.len(), 2, "body has 2 cells");
    assert_eq!(
        tree.text(&tree.node(body_cells[0]).span),
        "1",
        "first body cell text"
    );
    assert_eq!(
        tree.text(&tree.node(body_cells[1]).span),
        "2",
        "second body cell text"
    );
}

// =======================================================================
// HTML tag integration
// =======================================================================

// --- Equivalence: same ElementKind for markdown and HTML syntax ---

#[test]
fn html_blockquote_same_kind_as_markdown() {
    let md = parse("> quoted\n");
    let html = parse("<blockquote>\n\nquoted\n\n</blockquote>\n");

    let md_kind = &md.node(root_children(&md)[0]).kind;
    let html_kind = &html.node(root_children(&html)[0]).kind;
    assert_eq!(md_kind, html_kind, "both produce QuoteBlock");
}

#[test]
fn html_heading_same_kind_as_markdown() {
    let md = parse("# Heading\n");
    let html = parse("<h1>Heading</h1>\n");

    let md_kind = &md.node(root_children(&md)[0]).kind;
    let html_kind = &html.node(root_children(&html)[0]).kind;
    assert_eq!(md_kind, html_kind, "both produce Heading level 1");
}

#[test]
fn html_hr_same_kind_as_markdown() {
    let md = parse("---\n");
    let html = parse("<hr>\n");

    let md_kind = &md.node(root_children(&md)[0]).kind;
    let html_kind = &html.node(root_children(&html)[0]).kind;
    assert_eq!(md_kind, html_kind, "both produce Rules");
}

// --- HTML syntax produces Syntax::Html ---

#[test]
fn html_blockquote_has_html_syntax() {
    let tree = parse("<blockquote>\n\nquoted\n\n</blockquote>\n");
    let children = root_children(&tree);
    let node = tree.node(children[0]);
    assert_eq!(node.syntax, Syntax::Html, "HTML blockquote has Html syntax");
    assert_eq!(node.kind, ElementKind::QuoteBlock, "kind is QuoteBlock");
}

#[test]
fn html_heading_has_html_syntax() {
    let tree = parse("<h1>Heading</h1>\n");
    let children = root_children(&tree);
    let node = tree.node(children[0]);
    assert_eq!(node.syntax, Syntax::Html, "HTML heading has Html syntax");
    assert_eq!(
        node.kind,
        ElementKind::Heading { level: 1 },
        "kind is Heading level 1"
    );
}

#[test]
fn html_h2_through_h6() {
    for level in 2..=6u8 {
        let source = format!("<h{level}>text</h{level}>\n");
        let tree = parse(&source);
        let children = root_children(&tree);
        assert_eq!(children.len(), 1, "h{level} produces one node");
        assert_kind(&tree, children[0], &ElementKind::Heading { level });
    }
}

#[test]
fn table_multiple_body_rows() {
    let source = "| H |\n| --- |\n| a |\n| b |\n| c |\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "one table");
    let rows = tree.children(children[0]);
    assert_eq!(rows.len(), 4, "header + 3 body rows");
    assert_kind(&tree, rows[0], &ElementKind::TableRow { header: true });
    for &row_id in &rows[1..] {
        assert_kind(&tree, row_id, &ElementKind::TableRow { header: false });
    }
}

#[test]
fn table_header_only() {
    let source = "| H1 | H2 |\n| --- | --- |\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "one table");
    let rows = tree.children(children[0]);
    assert_eq!(rows.len(), 1, "header row only");
}

// --- Tables: alignment ---

#[test]
fn table_alignment_left() {
    let source = "| A |\n| --- |\n| x |\n";
    let tree = parse(source);
    let table = tree.node(root_children(&tree)[0]);

    assert!(
        matches!(&table.kind, ElementKind::Table { alignments }
                if alignments == &[TableAlignment::Left]),
        "default left alignment"
    );
}

#[test]
fn table_alignment_left_colon() {
    let source = "| A |\n| :--- |\n| x |\n";
    let tree = parse(source);
    let table = tree.node(root_children(&tree)[0]);

    assert!(
        matches!(&table.kind, ElementKind::Table { alignments }
                if alignments == &[TableAlignment::Left]),
        "explicit left alignment"
    );
}

#[test]
fn table_alignment_center() {
    let source = "| A |\n| :---: |\n| x |\n";
    let tree = parse(source);
    let table = tree.node(root_children(&tree)[0]);

    assert!(
        matches!(&table.kind, ElementKind::Table { alignments }
                if alignments == &[TableAlignment::Center]),
        "center alignment"
    );
}

#[test]
fn table_alignment_right() {
    let source = "| A |\n| ---: |\n| x |\n";
    let tree = parse(source);
    let table = tree.node(root_children(&tree)[0]);

    assert!(
        matches!(&table.kind, ElementKind::Table { alignments }
                if alignments == &[TableAlignment::Right]),
        "right alignment"
    );
}

#[test]
fn table_mixed_alignment() {
    let source = "| L | C | R |\n| --- | :---: | ---: |\n| a | b | c |\n";
    let tree = parse(source);
    let table = tree.node(root_children(&tree)[0]);

    assert!(
        matches!(&table.kind, ElementKind::Table { alignments }
        if alignments == &[
            TableAlignment::Left,
            TableAlignment::Center,
            TableAlignment::Right,
        ]),
        "mixed alignment"
    );
}

// --- Tables: column count mismatches ---

#[test]
fn table_fewer_cells_padded() {
    let source = "| A | B | C |\n| --- | --- | --- |\n| 1 |\n";
    let tree = parse(source);
    let children = root_children(&tree);
    let rows = tree.children(children[0]);

    // Body row should be padded to 3 cells.
    let body_cells = tree.children(rows[1]);
    assert_eq!(body_cells.len(), 3, "padded to 3 cells");

    // First cell has content, rest are empty.
    assert_eq!(
        tree.text(&tree.node(body_cells[0]).span),
        "1",
        "first cell has content"
    );
    assert!(
        tree.node(body_cells[1]).span.is_empty(),
        "second cell is empty"
    );
    assert!(
        tree.node(body_cells[2]).span.is_empty(),
        "third cell is empty"
    );
}

#[test]
fn table_excess_cells_ignored() {
    let source = "| A |\n| --- |\n| 1 | 2 | 3 |\n";
    let tree = parse(source);
    let children = root_children(&tree);
    let rows = tree.children(children[0]);

    // Body row should have only 1 cell (excess ignored).
    let body_cells = tree.children(rows[1]);
    assert_eq!(body_cells.len(), 1, "excess cells ignored");
}

#[test]
fn table_mismatch_diagnostic() {
    let source = "| A | B |\n| --- | --- |\n| 1 |\n";
    let tree = parse(source);

    let mismatch_diags: Vec<_> = tree
        .diagnostics()
        .iter()
        .filter(|d| d.message.contains("cells"))
        .collect();
    assert_eq!(mismatch_diags.len(), 1, "one mismatch diagnostic");
    assert!(
        mismatch_diags[0].message.contains("1 cells, expected 2"),
        "diagnostic message: {}",
        mismatch_diags[0].message
    );
}

// --- Tables: pipes in inline code ---

#[test]
fn table_pipe_in_inline_code() {
    let source = "| A | B |\n| --- | --- |\n| `a|b` | c |\n";
    let tree = parse(source);
    let children = root_children(&tree);
    let rows = tree.children(children[0]);

    let body_cells = tree.children(rows[1]);
    assert_eq!(body_cells.len(), 2, "pipe in code does not split");
    assert_eq!(
        tree.text(&tree.node(body_cells[0]).span),
        "`a|b`",
        "code span preserved"
    );
}

#[test]
fn table_pipe_in_double_backtick_code() {
    let source = "| A |\n| --- |\n| ``a | b`` |\n";
    let tree = parse(source);
    let children = root_children(&tree);
    let rows = tree.children(children[0]);

    let body_cells = tree.children(rows[1]);
    assert_eq!(
        body_cells.len(),
        1,
        "pipe in double-backtick code does not split"
    );
}

#[test]
fn table_cell_double_backtick_wraps_longer_run() {
    // A `` span containing a longer ``` run closes at the next `` — the
    // inner triple-backtick run is literal content. The `|` delimiters
    // outside the span must still split all three cells. Regression for the
    // splitter matching the first N backticks of a longer run and merging
    // the trailing cells.
    let source = "| A | B | C |\n|---|---|---|\n| Code block | `` ``` `` | `Object` |\n";
    let tree = parse(source);
    let children = root_children(&tree);
    let rows = tree.children(children[0]);

    let body_cells = tree.children(rows[1]);
    assert_eq!(
        body_cells.len(),
        3,
        "double-backtick span wrapping a longer run must not swallow pipes"
    );
    assert_eq!(
        tree.text(&tree.node(body_cells[1]).span),
        "`` ``` ``",
        "middle cell is the full code span, not merged with the next cell"
    );
}

// --- Tables: links in cells ---

#[test]
fn table_with_links() {
    let source = "| Name |\n| --- |\n| [foo](bar.md) |\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "one table");
    let rows = tree.children(children[0]);
    let body_cells = tree.children(rows[1]);

    // The cell should have inline children from the inline parser.
    let cell_children = tree.children(body_cells[0]);
    let has_link = cell_children
        .iter()
        .any(|&id| matches!(tree.node(id).kind, ElementKind::Link { .. }));
    assert!(has_link, "cell should contain a link from inline parsing");
}

// --- Root-relative `/` link classification (issue 028) ---

#[test]
fn root_relative_link_target_is_workspace_root_anchored() {
    // `[x](/README.md)` from a nested file resolves at the workspace root,
    // so the stored target is the workspace-relative `README.md` (not an
    // absolute `/README.md`), independent of the source file's depth.
    let tree = parse("[x](/README.md)\n");
    let links = tree.links(Path::new("a/b/c.md"));
    assert_eq!(links.len(), 1, "one link extracted: {links:?}");
    match &links[0].kind {
        LinkKind::IntraProject { target, .. } => {
            assert_eq!(
                target,
                Path::new("README.md"),
                "root-relative `/README.md` resolves to the workspace-relative `README.md`",
            );
        }
        other => panic!("expected an intra-project markdown link, got {other:?}"),
    }
}

#[test]
fn root_relative_link_does_not_escape_workspace() {
    // A `/`-rooted target must resolve under the workspace root, never to a
    // real filesystem-absolute path: the result carries no `RootDir`
    // component.
    let tree = parse("[x](/etc/passwd.md)\n");
    let links = tree.links(Path::new("a/b/c.md"));
    assert_eq!(links.len(), 1, "one link extracted: {links:?}");
    match &links[0].kind {
        LinkKind::IntraProject { target, .. } => {
            assert!(
                !target.has_root(),
                "root-relative target stays workspace-relative (no filesystem root): {target:?}",
            );
            assert_eq!(
                target,
                Path::new("etc/passwd.md"),
                "the `/` is stripped to a workspace-relative path: {target:?}",
            );
        }
        other => panic!("expected an intra-project markdown link, got {other:?}"),
    }
}

#[test]
fn protocol_relative_link_classifies_as_external() {
    // `//host/path` is protocol-relative — a URL, not a workspace path —
    // so it classifies as External and is never resolved against the root.
    let tree = parse("[x](//cdn.example.com/lib.md)\n");
    let links = tree.links(Path::new("a/b/c.md"));
    assert_eq!(links.len(), 1, "one link extracted: {links:?}");
    assert!(
        matches!(&links[0].kind, LinkKind::External { .. }),
        "protocol-relative `//host` is external, not a workspace path: {:?}",
        links[0].kind,
    );
}

// --- URI-scheme recognition (issue 071) ---

/// Every destination `is_external` must recognize as a URI, and the four
/// forms the pre-071 prefix list already knew (pinned byte-identical).
const EXTERNAL_DESTINATIONS: &[&str] = &[
    // The pre-071 list: unchanged.
    "http://example.com/a.md",
    "https://example.com/a.md",
    "mailto:contact@example.com",
    "//cdn.example.com/lib.md",
    // Recognized by grammar since 071 — each was resolved as a workspace
    // path, and diagnosed as a missing file, before it.
    "data:text/plain;base64,SGVsbG8=",
    "tel:+15551234567",
    "sms:+15551234567",
    "ftp://example.com/pub/notes.md",
    "file:///etc/hosts",
    "javascript:void",
    "custom-scheme:some/thing",
    "x+y.z-w:thing",
    // Schemes are case-insensitive (RFC 3986 §3.1); the prefix list was
    // case-sensitive, so this one used to resolve as a path.
    "HTTPS://EXAMPLE.COM/a.md",
    // Two characters is the floor, and a digit satisfies it.
    "s3://bucket/key.md",
    "c9:thing",
];

/// Destinations that stay workspace paths: no scheme, or a colon that the
/// grammar does not read as one.
const PATH_DESTINATIONS: &[&str] = &[
    "notes.md",
    "./notes.md",
    "../notes.md",
    "/docs/notes.md",
    "img/logo.png",
    "#fragment",
    "",
    // A `/` before the `:` breaks the scheme run.
    "docs/a:b.md",
    // A scheme must start with ALPHA.
    "12:30",
    ":x",
    "-x:y",
    // A single ALPHA before the `:` is a Windows drive letter, not a
    // scheme — the boundary 071 decided.
    "C:\\notes.md",
    "C:/notes.md",
    // Scheme-shaped, but never terminated by a `:`.
    "a.b.c",
    "http",
];

#[test]
fn is_external_recognizes_schemes_by_grammar() {
    for url in EXTERNAL_DESTINATIONS {
        assert!(
            is_external(url),
            "a URI-scheme (or protocol-relative) destination is external: {url}"
        );
    }
    for url in PATH_DESTINATIONS {
        assert!(
            !is_external(url),
            "a workspace path is never external: {url}"
        );
    }
}

#[test]
fn uri_scheme_links_and_embeds_classify_as_external() {
    // Both surfaces route through the one oracle (issue 071), so a link and
    // an embed of the same destination classify identically — neither is
    // ever resolved against the source document.
    for url in EXTERNAL_DESTINATIONS {
        for source in [format!("[x]({url})\n"), format!("![x]({url})\n")] {
            let kind = only_link_kind(&source);
            assert!(
                matches!(kind, LinkKind::External { .. }),
                "expected External for `{source}`, got {kind:?}"
            );
        }
    }
}

#[test]
fn base64_data_uri_embed_is_not_a_workspace_target() {
    // The shape that motivated issue 071: a self-contained document inlines
    // its image as base64, and the embed existence check added by issue 058
    // named the payload as a missing file.
    let source = "![](data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAAAAAA6\
                      fptVAAAACklEQVR4nGMAAQAABQABDQottAAAAABJRU5ErkJggg==)\n";
    let kind = only_link_kind(source);
    assert!(
        matches!(kind, LinkKind::External { .. }),
        "a base64 data URI is a URI, never a filename: {kind:?}"
    );
}

#[test]
fn windows_drive_shaped_destination_stays_a_workspace_path() {
    // The decided boundary (issue 071): a one-letter scheme is read as a
    // Windows drive letter, so `C:\…` keeps resolving as a path — a
    // one-letter URI scheme does not exist in the wild, the drive spelling
    // does.
    for (source, expected) in [
        ("[x](C:\\notes.md)\n", "docs/C:\\notes.md"),
        ("[x](C:/notes.md)\n", "docs/C:/notes.md"),
    ] {
        match only_link_kind(source) {
            LinkKind::IntraProject { target, .. } => assert_eq!(
                target,
                Path::new(expected),
                "the drive-shaped target resolves as a path: {source}"
            ),
            other => panic!("expected an intra-project link for {source}, got {other:?}"),
        }
    }
}

#[test]
fn two_character_scheme_shaped_destination_is_a_uri() {
    // The other side of the same boundary: above the one-letter floor,
    // `CommonMark`'s reading governs — a scheme-looking prefix makes it a
    // URI, which is how a renderer resolves the `href`, so `foo:bar` is not
    // a relative path that happens to carry a colon.
    for source in ["[x](foo:bar)\n", "[x](notes:draft.md)\n"] {
        let kind = only_link_kind(source);
        assert!(
            matches!(kind, LinkKind::External { .. }),
            "a scheme-shaped destination is a URI: {source} -> {kind:?}"
        );
    }
}

#[test]
fn external_namespace_link_keeps_strict_resolution() {
    // Issue 030: the `{Name}/…` external-namespace escape applies only to
    // *citations* (backtick/quoted/bare), never to markdown *links*. A
    // clickable cross-repo link is not navigable on GitHub, so a
    // `[x]({Archive}/…)` link keeps strict intra-project resolution — the
    // literal `{Archive}` is dir-joined as an ordinary `.md` target, not
    // exempted by any alias.
    let tree = parse("[x]({Archive}/docs/configuration.md)\n");
    let links = tree.links(Path::new("a/b/c.md"));
    assert_eq!(links.len(), 1, "one link extracted: {links:?}");
    match &links[0].kind {
        LinkKind::IntraProject { target, .. } => {
            assert_eq!(
                target,
                Path::new("a/b/{Archive}/docs/configuration.md"),
                "the `{{Archive}}` link target is dir-joined verbatim, not aliased",
            );
        }
        other => panic!("expected a strict intra-project markdown link, got {other:?}"),
    }
}

// --- link_destination_span (the move-engine edit primitive, ticket mv/01) ---

/// Resolve the single link's destination span and return the slice it
/// covers, asserting exactly one link is present.
fn only_link_dest_slice(source: &str) -> (Span, String) {
    let tree = parse(source);
    let links = tree.links(Path::new("doc.md"));
    assert_eq!(links.len(), 1, "expected exactly one link: {links:?}");
    let span = link_destination_span(source, links[0].span)
        .expect("link should carry an editable inline destination");
    (span, source[span.start..span.end].to_string())
}

#[test]
fn dest_span_inline_bare() {
    let (_span, slice) = only_link_dest_slice("see [x](sub/other.md) here\n");
    assert_eq!(
        slice, "sub/other.md",
        "the bare inline destination is the path run"
    );
}

#[test]
fn dest_span_inline_with_title() {
    let (_span, slice) = only_link_dest_slice("[x](other.md \"references\")\n");
    assert_eq!(
        slice, "other.md",
        "the title is excluded from the destination span"
    );
}

#[test]
fn dest_span_inline_fragment_excluded() {
    let source = "[x](guide.md#heading)\n";
    let (span, slice) = only_link_dest_slice(source);
    assert_eq!(slice, "guide.md", "the `#fragment` is never in the span");
    assert_eq!(
        &source[span.end..span.end + 8],
        "#heading",
        "the span ends exactly before the `#`"
    );
}

#[test]
fn dest_span_angle_bracket_inside_brackets() {
    let source = "[x](<a b.md> \"references\")\n";
    let (span, slice) = only_link_dest_slice(source);
    assert_eq!(
        slice, "a b.md",
        "the angle-bracketed destination is the run inside `<>`"
    );
    assert_eq!(
        source.as_bytes()[span.start - 1],
        b'<',
        "the edit range starts after the `<`"
    );
    assert_eq!(
        source.as_bytes()[span.end],
        b'>',
        "the edit range ends before the `>`"
    );
}

#[test]
fn dest_span_import() {
    let (_span, slice) = only_link_dest_slice("@sub/partial.md\n");
    assert_eq!(
        slice, "sub/partial.md",
        "the import destination is the path after `@`"
    );
}

#[test]
fn dest_span_html_anchor_href() {
    let (_span, slice) = only_link_dest_slice("<a href=\"other.md\">x</a>\n");
    assert_eq!(
        slice, "other.md",
        "the HTML anchor destination is the `href` value"
    );
}

#[test]
fn dest_span_html_anchor_href_fragment_excluded() {
    let (_span, slice) = only_link_dest_slice("<a href=\"g.md#h\">x</a>\n");
    assert_eq!(
        slice, "g.md",
        "the HTML anchor `#fragment` is excluded from the span"
    );
}

#[test]
fn dest_span_reference_style_is_none() {
    // A reference-style link's destination lives in a separate ReferenceDef
    // node, not the link span, so the primitive declines it.
    let source = "[x][ref]\n\n[ref]: other.md\n";
    let tree = parse(source);
    let links = tree.links(Path::new("doc.md"));
    assert_eq!(
        links.len(),
        1,
        "reference link resolves to one link: {links:?}"
    );
    assert!(
        link_destination_span(source, links[0].span).is_none(),
        "a reference-style link carries no inline destination span"
    );
    // The definition's URL is what the move engine edits instead.
    let (_id, node) = tree.find_ref_def("ref").expect("ref def resolves");
    assert!(
        matches!(&node.kind, ElementKind::ReferenceDef { url, .. } if url == "other.md"),
        "the ReferenceDef carries the destination: {:?}",
        node.kind
    );
}

// --- link_fragment_span (the heading-rename edit primitive, issue 057) ---

/// Resolve the single link's fragment span and return the slice it covers,
/// asserting exactly one link is present.
fn only_link_fragment_slice(source: &str) -> (Span, String) {
    let tree = parse(source);
    let links = tree.links(Path::new("doc.md"));
    assert_eq!(links.len(), 1, "expected exactly one link: {links:?}");
    let span = link_fragment_span(source, links[0].span)
        .expect("link should carry an editable fragment span");
    (span, source[span.start..span.end].to_string())
}

#[test]
fn fragment_span_inline_after_the_hash() {
    let source = "[x](guide.md#old-section)\n";
    let (span, slice) = only_link_fragment_slice(source);
    assert_eq!(slice, "old-section", "the fragment excludes the `#`");
    assert_eq!(
        source.as_bytes()[span.start - 1],
        b'#',
        "the edit range starts just after the `#`"
    );
    assert_eq!(
        source.as_bytes()[span.end],
        b')',
        "the edit range ends before the closing paren"
    );
}

#[test]
fn fragment_span_intra_document_anchor() {
    // A fragment-only destination carries no path, so the path-axis
    // primitive declines it while the fragment-axis one still answers.
    let source = "[x](#old-section)\n";
    let (_span, slice) = only_link_fragment_slice(source);
    assert_eq!(slice, "old-section", "the same-document anchor is editable");
    let tree = parse(source);
    let links = tree.links(Path::new("doc.md"));
    assert!(
        link_destination_span(source, links[0].span).is_none(),
        "a fragment-only destination denotes no file, so it has no path span"
    );
}

#[test]
fn fragment_span_excludes_the_title() {
    let (_span, slice) = only_link_fragment_slice("[x](g.md#old \"references\")\n");
    assert_eq!(slice, "old", "the title is not part of the fragment");
}

#[test]
fn fragment_span_angle_bracketed_stays_inside_the_brackets() {
    let source = "[x](<a b.md#old>)\n";
    let (span, slice) = only_link_fragment_slice(source);
    assert_eq!(
        slice, "old",
        "the angle-bracketed fragment is the run after `#`"
    );
    assert_eq!(
        source.as_bytes()[span.end],
        b'>',
        "the edit range ends before the `>`"
    );
}

#[test]
fn fragment_span_html_anchor_href() {
    let (_span, slice) = only_link_fragment_slice("<a href=\"g.md#old\">x</a>\n");
    assert_eq!(
        slice, "old",
        "the HTML anchor fragment is the `href` remainder"
    );
}

#[test]
fn fragment_span_absent_without_a_hash() {
    let source = "[x](guide.md)\n";
    let tree = parse(source);
    let links = tree.links(Path::new("doc.md"));
    assert!(
        link_fragment_span(source, links[0].span).is_none(),
        "a destination with no `#` carries no fragment span"
    );
}

#[test]
fn fragment_span_reference_style_is_none() {
    // A reference-style link's fragment lives in its ReferenceDef URL, not
    // the link span — the rename engine edits the definition instead.
    let source = "[x][ref]\n\n[ref]: other.md#old\n";
    let tree = parse(source);
    let links = tree.links(Path::new("doc.md"));
    assert_eq!(links.len(), 1, "one link extracted: {links:?}");
    assert!(
        link_fragment_span(source, links[0].span).is_none(),
        "a reference-style link carries no inline fragment span"
    );
}

// --- Embeds are extracted edges (issue 058) ---

/// The single extracted link's kind for a source, asserting exactly one.
fn only_link_kind(source: &str) -> LinkKind {
    let tree = parse(source);
    let mut links = tree.links(Path::new("docs/doc.md"));
    assert_eq!(links.len(), 1, "expected exactly one link: {links:?}");
    links.remove(0).kind
}

#[test]
fn markdown_image_extracts_as_embed() {
    match only_link_kind("![logo](img/logo.png)\n") {
        LinkKind::Embed { target } => assert_eq!(
            target,
            Path::new("docs/img/logo.png"),
            "an image embed resolves against the document's directory"
        ),
        other => panic!("expected an Embed, got {other:?}"),
    }
}

#[test]
fn markdown_video_and_audio_extract_as_embeds() {
    // `classify_media` splits `![](*.mp4)` / `![](*.mp3)` into distinct node
    // kinds; all three embed kinds must reach the same `Embed` edge.
    for (source, expected) in [
        ("![clip](media/demo.mp4)\n", "docs/media/demo.mp4"),
        ("![tune](media/track.mp3)\n", "docs/media/track.mp3"),
    ] {
        match only_link_kind(source) {
            LinkKind::Embed { target } => assert_eq!(
                target,
                Path::new(expected),
                "the embed target resolves: {source}"
            ),
            other => panic!("expected an Embed for {source}, got {other:?}"),
        }
    }
}

#[test]
fn html_embed_tags_extract_as_embeds() {
    for (source, expected) in [
        ("<img src=\"img/logo.png\">\n", "docs/img/logo.png"),
        (
            "<video src=\"media/demo.mp4\"></video>\n",
            "docs/media/demo.mp4",
        ),
        (
            "<audio src=\"media/track.mp3\"></audio>\n",
            "docs/media/track.mp3",
        ),
    ] {
        match only_link_kind(source) {
            LinkKind::Embed { target } => assert_eq!(
                target,
                Path::new(expected),
                "the HTML embed `src` resolves: {source}"
            ),
            other => panic!("expected an Embed for {source}, got {other:?}"),
        }
    }
}

#[test]
fn embed_carries_no_predicate_edge() {
    // An embed of a markdown file is still an `Embed`, never an
    // `IntraProject` edge: it renders a resource, it asserts no relation, so
    // it must derive no predicate and no backlink obligation.
    match only_link_kind("![inline](other.md)\n") {
        LinkKind::Embed { target } => assert_eq!(
            target,
            Path::new("docs/other.md"),
            "a markdown embed is still an embed edge"
        ),
        other => panic!("expected an Embed, got {other:?}"),
    }
}

#[test]
fn external_embed_is_not_a_workspace_target() {
    assert!(
        matches!(
            only_link_kind("![remote](https://example.com/a.png)\n"),
            LinkKind::External { .. }
        ),
        "a remote embed source is external, never a workspace path"
    );
}

#[test]
fn embed_with_empty_or_fragment_only_source_is_dropped() {
    for source in ["![alt]()\n", "![alt](#x)\n"] {
        let tree = parse(source);
        let links = tree.links(Path::new("docs/doc.md"));
        assert!(
            links.is_empty(),
            "an embed denoting no file forms no edge: {source} -> {links:?}"
        );
    }
}

#[test]
fn dest_span_markdown_embed() {
    let (_span, slice) = only_link_dest_slice("![alt](img/logo.png)\n");
    assert_eq!(
        slice, "img/logo.png",
        "the embed destination is the path run after the `!`"
    );
}

#[test]
fn dest_span_markdown_embed_title_and_fragment_excluded() {
    let source = "![alt](pic.svg#view \"Caption\")\n";
    let (span, slice) = only_link_dest_slice(source);
    assert_eq!(
        slice, "pic.svg",
        "neither the `#fragment` nor the title is in the embed's span"
    );
    assert_eq!(
        &source[span.end..span.end + 5],
        "#view",
        "the span ends exactly before the `#`"
    );
}

#[test]
fn dest_span_markdown_embed_angle_bracketed() {
    let source = "![alt](<a pic.png>)\n";
    let (span, slice) = only_link_dest_slice(source);
    assert_eq!(
        slice, "a pic.png",
        "the angle-bracketed embed destination is the run inside `<>`"
    );
    assert_eq!(
        source.as_bytes()[span.start - 1],
        b'<',
        "the edit range starts after the `<`"
    );
}

#[test]
fn dest_span_html_embed_src() {
    for (source, expected) in [
        ("<img src=\"img/logo.png\">\n", "img/logo.png"),
        ("<video src=\"media/demo.mp4\"></video>\n", "media/demo.mp4"),
        (
            "<audio src=\"media/track.mp3\"></audio>\n",
            "media/track.mp3",
        ),
    ] {
        let (_span, slice) = only_link_dest_slice(source);
        assert_eq!(
            slice, expected,
            "the HTML embed destination is the `src` value: {source}"
        );
    }
}

#[test]
fn dest_span_reference_style_embed_is_none() {
    // Like a reference-style link, a reference-style embed's destination
    // lives in its ReferenceDef — the move engine edits that instead.
    let source = "![alt][pic]\n\n[pic]: img/logo.png\n";
    let tree = parse(source);
    let links = tree.links(Path::new("doc.md"));
    assert_eq!(links.len(), 1, "one embed extracted: {links:?}");
    assert!(
        link_destination_span(source, links[0].span).is_none(),
        "a reference-style embed carries no inline destination span"
    );
}

// --- Tables: edge cases ---

#[test]
fn table_single_column() {
    let source = "| A |\n| --- |\n| x |\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "one table");
    let table = tree.node(children[0]);
    assert!(
        matches!(&table.kind, ElementKind::Table { alignments } if alignments.len() == 1),
        "single column table"
    );
}

#[test]
fn table_no_leading_trailing_pipes() {
    let source = "A | B\n--- | ---\n1 | 2\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "one table");
    let rows = tree.children(children[0]);
    assert_eq!(rows.len(), 2, "header + body");

    let body_cells = tree.children(rows[1]);
    assert_eq!(
        body_cells.len(),
        2,
        "2 cells without leading/trailing pipes"
    );
    assert_eq!(tree.text(&tree.node(body_cells[0]).span), "1", "first cell");
    assert_eq!(
        tree.text(&tree.node(body_cells[1]).span),
        "2",
        "second cell"
    );
}

#[test]
fn table_empty_cells() {
    let source = "| A | B |\n| --- | --- |\n| | |\n";
    let tree = parse(source);
    let children = root_children(&tree);
    let rows = tree.children(children[0]);

    let body_cells = tree.children(rows[1]);
    assert_eq!(body_cells.len(), 2, "two empty cells");
    assert!(tree.node(body_cells[0]).span.is_empty(), "first cell empty");
    assert!(
        tree.node(body_cells[1]).span.is_empty(),
        "second cell empty"
    );
}

#[test]
fn table_ends_at_blank_line() {
    let source = "| A |\n| --- |\n| x |\n\nParagraph\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 2, "table + paragraph");
    assert!(
        matches!(&tree.node(children[0]).kind, ElementKind::Table { .. }),
        "first is table"
    );
    assert_kind(&tree, children[1], &ElementKind::Paragraph);
}

#[test]
fn table_ends_at_non_row_line() {
    let source = "| A |\n| --- |\n| x |\n# Heading\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 2, "table + heading");
    assert!(
        matches!(&tree.node(children[0]).kind, ElementKind::Table { .. }),
        "first is table"
    );
    assert_kind(&tree, children[1], &ElementKind::Heading { level: 1 });
}

#[test]
fn dashes_after_paragraph_is_setext_not_table() {
    // `---` after a paragraph line is a setext heading, not a table
    // delimiter, because the first line has no pipes.
    let source = "Heading\n---\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "one heading");
    assert_kind(&tree, children[0], &ElementKind::Heading { level: 2 });
}

#[test]
fn not_a_table_without_delimiter() {
    let source = "| A | B |\n| C | D |\n";
    let tree = parse(source);
    let children = root_children(&tree);

    // Second line is not a delimiter row, so this is a paragraph.
    assert_eq!(children.len(), 1, "one paragraph");
    assert_kind(&tree, children[0], &ElementKind::Paragraph);
}

#[test]
fn table_in_list_item() {
    let source = "- | A |\n  | --- |\n  | x |\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "one list");
    let items = tree.children(children[0]);
    assert_eq!(items.len(), 1, "one item");

    let item_children = tree.children(items[0]);
    assert!(
        item_children
            .iter()
            .any(|&id| matches!(&tree.node(id).kind, ElementKind::Table { .. })),
        "list item contains table"
    );
}

#[test]
fn table_in_block_quote() {
    let source = "> | A |\n> | --- |\n> | x |\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "one block quote");
    assert_kind(&tree, children[0], &ElementKind::QuoteBlock);

    let quote_children = tree.children(children[0]);
    assert!(
        quote_children
            .iter()
            .any(|&id| matches!(&tree.node(id).kind, ElementKind::Table { .. })),
        "block quote contains table"
    );
}

#[test]
fn html_heading_multiline_span() {
    let source = "<h2>\nHeading Text\n</h2>\n";
    let tree = parse(source);
    let children = root_children(&tree);
    assert_eq!(children.len(), 1, "one heading");
    let node = assert_kind(&tree, children[0], &ElementKind::Heading { level: 2 });
    assert_eq!(
        node.span,
        Span::new(0, source.len()),
        "span covers opening through closing tag"
    );
}

#[test]
fn html_hr_has_html_syntax() {
    let tree = parse("<hr>\n");
    let children = root_children(&tree);
    let node = tree.node(children[0]);
    assert_eq!(node.syntax, Syntax::Html, "HTML hr has Html syntax");
    assert_eq!(node.kind, ElementKind::Rules, "kind is Rules");
}

#[test]
fn html_hr_self_closing() {
    let tree = parse("<hr/>\n");
    let children = root_children(&tree);
    assert_eq!(children.len(), 1, "one node");
    assert_kind(&tree, children[0], &ElementKind::Rules);
}

// --- Void elements ---

#[test]
fn void_element_never_pushed_to_scope() {
    let tree = parse("<hr>\n<br>\n");
    let children = root_children(&tree);
    // Void elements are leaves, not containers.
    assert_eq!(children.len(), 2, "two void element leaves");
    assert_kind(&tree, children[0], &ElementKind::Rules);
    // <br> has no structural mapping so falls through to HtmlBlock.
}

#[test]
fn img_void_element() {
    let tree = parse("<img src=\"photo.jpg\" />\n");
    let children = root_children(&tree);
    assert_eq!(children.len(), 1, "one image node");
    let node = tree.node(children[0]);
    assert_eq!(node.syntax, Syntax::Html, "Html syntax");
    assert!(
        matches!(node.kind, ElementKind::Image { .. }),
        "kind is Image"
    );
}

// --- Container scoping ---

#[test]
fn details_container_scope() {
    let tree = parse("<details>\n\ncontent\n\n</details>\n");
    let children = root_children(&tree);
    assert_eq!(children.len(), 1, "one details container");
    assert_kind(&tree, children[0], &ElementKind::Details);
    let inner = tree.children(children[0]);
    assert!(
        !inner.is_empty(),
        "details has children (content parsed as markdown)"
    );
}

#[test]
fn nested_html_containers() {
    let source = "<div>\n\n<blockquote>\n\ntext\n\n</blockquote>\n\n</div>\n";
    let tree = parse(source);
    let children = root_children(&tree);
    assert_eq!(children.len(), 1, "one div container");
    assert_kind(&tree, children[0], &ElementKind::Container);
}

// --- HTML inside block quotes ---

#[test]
fn html_container_inside_blockquote() {
    let source = "> <div>\n> content\n> </div>\n";
    let tree = parse(source);
    let children = root_children(&tree);
    assert_eq!(children.len(), 1, "one block quote");
    assert_kind(&tree, children[0], &ElementKind::QuoteBlock);
    // The div container should be a child of the block quote.
    let quote_children = tree.children(children[0]);
    assert!(
        quote_children
            .iter()
            .any(|&id| tree.node(id).kind == ElementKind::Container),
        "div container inside block quote: {quote_children:?}"
    );
    // The container should be properly closed (no unclosed diagnostic).
    assert!(
        !tree
            .diagnostics()
            .iter()
            .any(|d| d.message.contains("unclosed")),
        "no unclosed tag diagnostic: {:?}",
        tree.diagnostics()
    );
}

#[test]
fn html_heading_inside_blockquote() {
    let source = "> <h2>Title</h2>\n";
    let tree = parse(source);
    let children = root_children(&tree);
    assert_eq!(children.len(), 1, "one block quote");
    let quote_children = tree.children(children[0]);
    assert!(
        quote_children
            .iter()
            .any(|&id| matches!(tree.node(id).kind, ElementKind::Heading { level: 2 })),
        "heading inside block quote: {quote_children:?}"
    );
}

// --- Error recovery ---

#[test]
fn unclosed_html_tag_diagnostic() {
    let tree = parse("<div>\n\ncontent\n");
    let diags = tree.diagnostics();
    assert!(
        diags.iter().any(|d| d.message.contains("unclosed")),
        "should have unclosed tag diagnostic: {diags:?}"
    );
}

#[test]
fn unexpected_close_tag_diagnostic() {
    let tree = parse("</div>\n");
    let diags = tree.diagnostics();
    assert!(
        diags
            .iter()
            .any(|d| d.message.contains("unexpected closing tag")),
        "should have unexpected close tag diagnostic: {diags:?}"
    );
}

#[test]
fn unexpected_close_tag_span_with_trailing_multibyte() {
    // A nested close tag with no matching open, followed by trailing
    // content containing multi-byte characters, used to get its span
    // back-computed from the end-of-line offset — splitting a UTF-8
    // character (fuzz_structural soak finding). The span must land on
    // char boundaries and cover the close tag's own bytes.
    let src = "<details>\n</div>x\u{feff}\u{feff}\n";
    let tree = parse(src);
    let diag = tree
        .diagnostics()
        .iter()
        .find(|d| d.message.contains("unexpected closing tag"))
        .expect("unexpected-close diagnostic emitted");
    assert!(
        src.is_char_boundary(diag.span.start) && src.is_char_boundary(diag.span.end),
        "span must land on char boundaries: {:?}",
        diag.span
    );
    assert_eq!(
        &src[diag.span.start..diag.span.end],
        "</div>",
        "span must cover the close tag itself"
    );
}

#[test]
fn mismatched_nesting_recovery() {
    // <div><section></div> should close section implicitly
    let tree = parse("<div>\n\n<section>\n\ntext\n\n</div>\n");
    let diags = tree.diagnostics();
    assert!(
        diags
            .iter()
            .any(|d| d.message.contains("unclosed `<section>`")),
        "should flag unclosed section: {diags:?}"
    );
    // The div should still be properly closed.
    let children = root_children(&tree);
    assert_eq!(children.len(), 1, "one root container");
}

// --- Markdown inside HTML blocks ---

#[test]
fn markdown_in_html_with_blank_lines() {
    let source = "<div>\n\n## Heading\n\n</div>\n";
    let tree = parse(source);
    let children = root_children(&tree);
    assert_eq!(children.len(), 1, "one container");
    assert_kind(&tree, children[0], &ElementKind::Container);
    // The heading should be a child of the container.
    let inner = tree.children(children[0]);
    assert!(
        inner
            .iter()
            .any(|&id| matches!(tree.node(id).kind, ElementKind::Heading { level: 2 })),
        "heading parsed inside container"
    );
}

#[test]
fn raw_html_without_blank_lines() {
    let source = "<div>\n## Not a heading\n</div>\n";
    let tree = parse(source);
    let children = root_children(&tree);
    assert_eq!(children.len(), 1, "one container");
    // Content without blank lines is raw: no heading child.
    let inner = tree.children(children[0]);
    assert!(
        !inner
            .iter()
            .any(|&id| matches!(tree.node(id).kind, ElementKind::Heading { .. })),
        "no heading in raw mode"
    );
}

// --- <pre><code> → CodeBlock ---

#[test]
fn pre_code_produces_code_block() {
    let tree = parse("<pre><code>\nfn main() {}\n</code></pre>\n");
    let children = root_children(&tree);
    assert_eq!(children.len(), 1, "one block");
    let node = assert_kind(&tree, children[0], &ElementKind::CodeBlock);
    assert_eq!(node.syntax, Syntax::Html, "Html syntax");
}

#[test]
fn pre_code_with_language() {
    let tree = parse("<pre><code class=\"language-rust\">\nfn main() {}\n</code></pre>\n");
    let children = root_children(&tree);
    assert_eq!(children.len(), 1, "one block");
    assert_kind(&tree, children[0], &ElementKind::CodeBlock);
}

#[test]
fn pre_code_same_kind_as_fenced() {
    let md = parse("```\ncode\n```\n");
    let html = parse("<pre><code>\ncode\n</code></pre>\n");

    let md_kind = &md.node(root_children(&md)[0]).kind;
    let html_kind = &html.node(root_children(&html)[0]).kind;
    assert_eq!(md_kind, html_kind, "both produce CodeBlock");
}

#[test]
fn pre_code_span_covers_full_block() {
    let source = "<pre><code>\nline1\nline2\n</code></pre>\n";
    let tree = parse(source);
    let children = root_children(&tree);
    let node = tree.node(children[0]);
    assert_eq!(
        node.span,
        Span::new(0, source.len()),
        "span covers opening through closing tag"
    );
}

// --- Standalone <pre> stays opaque ---

#[test]
fn html_block_type1_pre_stays_opaque() {
    let tree = parse("<pre>\ncode\n</pre>\n");
    let children = root_children(&tree);
    // Standalone <pre> (without <code>) stays as HtmlBlock.
    assert_eq!(children.len(), 1, "one block");
    assert_kind(&tree, children[0], &ElementKind::HtmlBlock);
}

#[test]
fn html_block_type2_comment_stays_opaque() {
    let tree = parse("<!-- comment -->\n");
    let children = root_children(&tree);
    assert_eq!(children.len(), 1, "one block");
    assert_kind(&tree, children[0], &ElementKind::HtmlBlock);
}

// --- Table (HTML) elements ---

#[test]
fn html_table_container() {
    let tree = parse("<table>\n\n<tr><td>cell</td></tr>\n\n</table>\n");
    let children = root_children(&tree);
    assert_eq!(children.len(), 1, "one table container");
    assert!(
        matches!(&tree.node(children[0]).kind, ElementKind::Table { .. }),
        "kind is Table"
    );
    assert_eq!(tree.node(children[0]).syntax, Syntax::Html, "Html syntax");
}

// --- Section/article/aside all map to Container ---

#[test]
fn section_maps_to_container() {
    let tree = parse("<section>\n\ncontent\n\n</section>\n");
    let children = root_children(&tree);
    assert_eq!(children.len(), 1, "one container");
    assert_kind(&tree, children[0], &ElementKind::Container);
}

// --- HTML admonition containers ---

#[test]
fn html_div_warning_is_admonition() {
    let tree = parse("<div class=\"warning\">\n\nBe careful!\n\n</div>\n");
    let children = root_children(&tree);
    assert_eq!(children.len(), 1, "one admonition container");
    assert_kind(
        &tree,
        children[0],
        &ElementKind::Admonition {
            kind: "WARNING".to_string(),
        },
    );
}

#[test]
fn html_div_note_is_admonition() {
    let tree = parse("<div class=\"note\">\n\nNote text.\n\n</div>\n");
    let children = root_children(&tree);
    assert_eq!(children.len(), 1, "one admonition container");
    assert_kind(
        &tree,
        children[0],
        &ElementKind::Admonition {
            kind: "NOTE".to_string(),
        },
    );
}

#[test]
fn html_div_plain_is_container() {
    let tree = parse("<div>\n\ncontent\n\n</div>\n");
    let children = root_children(&tree);
    assert_eq!(children.len(), 1, "one container");
    assert_kind(&tree, children[0], &ElementKind::Container);
}

// --- Media elements ---

#[test]
fn html_video_produces_video() {
    let tree = parse("<video src=\"vid.mp4\"></video>\n");
    let children = root_children(&tree);
    assert_eq!(children.len(), 1, "one media element");
    let kind = &tree.node(children[0]).kind;
    assert!(
        matches!(kind, ElementKind::Video { url, .. } if url == "vid.mp4"),
        "video should produce Video with src extracted"
    );
}

#[test]
fn html_audio_produces_audio() {
    let tree = parse("<audio src=\"song.mp3\"></audio>\n");
    let children = root_children(&tree);
    assert_eq!(children.len(), 1, "one media element");
    let kind = &tree.node(children[0]).kind;
    assert!(
        matches!(kind, ElementKind::Audio { url, .. } if url == "song.mp3"),
        "audio should produce Audio with src extracted"
    );
}

#[test]
fn html_iframe_produces_image() {
    let tree = parse("<iframe src=\"page.html\"></iframe>\n");
    let children = root_children(&tree);
    assert_eq!(children.len(), 1, "one media element");
    let kind = &tree.node(children[0]).kind;
    assert!(
        matches!(kind, ElementKind::Image { url, .. } if url == "page.html"),
        "iframe should produce Image with src extracted"
    );
}

#[test]
fn markdown_image_mp4_produces_video() {
    let tree = parse("![demo](demo.mp4)\n");
    let children = root_children(&tree);
    assert_eq!(children.len(), 1, "one paragraph");
    let para_children = tree.children(children[0]);
    let kind = &tree.node(para_children[0]).kind;
    assert!(
        matches!(kind, ElementKind::Video { url, .. } if url == "demo.mp4"),
        "![](*.mp4) should produce Video"
    );
}

#[test]
fn markdown_image_mp3_produces_audio() {
    let tree = parse("![song](track.mp3)\n");
    let children = root_children(&tree);
    assert_eq!(children.len(), 1, "one paragraph");
    let para_children = tree.children(children[0]);
    let kind = &tree.node(para_children[0]).kind;
    assert!(
        matches!(kind, ElementKind::Audio { url, .. } if url == "track.mp3"),
        "![](*.mp3) should produce Audio"
    );
}

#[test]
fn markdown_image_png_stays_image() {
    let tree = parse("![photo](pic.png)\n");
    let children = root_children(&tree);
    let para_children = tree.children(children[0]);
    let kind = &tree.node(para_children[0]).kind;
    assert!(
        matches!(kind, ElementKind::Image { url, .. } if url == "pic.png"),
        "![](*.png) should stay Image"
    );
}

// --- Form elements ---

#[test]
fn html_input_produces_form_control() {
    let tree = parse("<input type=\"text\">\n");
    let children = root_children(&tree);
    assert_eq!(children.len(), 1, "one form element");
    assert_kind(&tree, children[0], &ElementKind::FormControl);
}

#[test]
fn html_select_produces_form_control() {
    let tree = parse("<select>\n<option>A</option>\n</select>\n");
    let children = root_children(&tree);
    assert_eq!(children.len(), 1, "one form element");
    assert_kind(&tree, children[0], &ElementKind::FormControl);
}

#[test]
fn html_textarea_produces_form_control() {
    let tree = parse("<textarea>content</textarea>\n");
    let children = root_children(&tree);
    assert_eq!(children.len(), 1, "one form element");
    assert_kind(&tree, children[0], &ElementKind::FormControl);
}

// --- Table structure (main's table tests) ---

#[test]
fn table_tree_structure() {
    // Verify parent-child relationships throughout.
    let source = "| A | B |\n| --- | --- |\n| 1 | 2 |\n";
    let tree = parse(source);
    let table_id = root_children(&tree)[0];
    let rows = tree.children(table_id);

    for &row_id in rows {
        assert_eq!(
            tree.node(row_id).parent,
            Some(table_id),
            "row parent is table"
        );
        for &cell_id in tree.children(row_id) {
            assert_eq!(
                tree.node(cell_id).parent,
                Some(row_id),
                "cell parent is row"
            );
        }
    }
}

#[test]
fn table_span_covers_all_content() {
    let source = "| A |\n| --- |\n| x |\n";
    let tree = parse(source);
    let table = tree.node(root_children(&tree)[0]);

    assert_eq!(
        tree.text(&table.span),
        source,
        "table span covers all rows including delimiter"
    );
}

// --- Tables: delimiter row validation ---

#[test]
fn delimiter_row_requires_dashes() {
    // Spaces-only cells are not valid delimiter rows.
    let source = "| A |\n|   |\n| x |\n";
    let tree = parse(source);
    let children = root_children(&tree);

    // Should be a paragraph (no valid delimiter row).
    assert_kind(&tree, children[0], &ElementKind::Paragraph);
}

#[test]
fn delimiter_row_minimum_one_dash() {
    let source = "| A |\n| - |\n| x |\n";
    let tree = parse(source);
    let children = root_children(&tree);

    assert_eq!(children.len(), 1, "one table");
    assert!(
        matches!(&tree.node(children[0]).kind, ElementKind::Table { .. }),
        "single dash is valid delimiter"
    );
}

// --- Nested HTML containers without blank lines (ticket 15) ---

#[test]
fn compact_dl_produces_children() {
    // <dl> with <dt>/<dd> on separate lines, no blank lines.
    let source = "<dl>\n<dt>API</dt>\n<dd>Description</dd>\n</dl>\n";
    let tree = parse(source);
    let children = root_children(&tree);
    assert_eq!(children.len(), 1, "one definition list");
    assert_kind(&tree, children[0], &ElementKind::DefinitionList);

    let dl_children = tree.children(children[0]);
    assert_eq!(dl_children.len(), 2, "dt and dd children");
    assert_kind(&tree, dl_children[0], &ElementKind::DefinitionTerm);
    assert_kind(&tree, dl_children[1], &ElementKind::DefinitionDesc);

    assert!(
        tree.diagnostics()
            .iter()
            .all(|d| !d.message.contains("unclosed")),
        "no unclosed diagnostics: {:?}",
        tree.diagnostics()
    );
}

#[test]
fn compact_details_summary() {
    let source = "<details>\n<summary>Title</summary>\n<p>content</p>\n</details>\n";
    let tree = parse(source);
    let children = root_children(&tree);
    assert_eq!(children.len(), 1, "one details container");
    assert_kind(&tree, children[0], &ElementKind::Details);

    let inner = tree.children(children[0]);
    assert!(
        inner
            .iter()
            .any(|&id| tree.node(id).kind == ElementKind::DetailsSummary),
        "has DetailsSummary child: {inner:?}"
    );
    assert!(
        inner
            .iter()
            .any(|&id| tree.node(id).kind == ElementKind::Paragraph),
        "has Paragraph child: {inner:?}"
    );

    assert!(
        tree.diagnostics()
            .iter()
            .all(|d| !d.message.contains("unclosed")),
        "no unclosed diagnostics: {:?}",
        tree.diagnostics()
    );
}

#[test]
fn compact_ul_with_li_children() {
    let source = "<ul>\n<li>item 1</li>\n<li>item 2</li>\n</ul>\n";
    let tree = parse(source);
    let children = root_children(&tree);
    assert_eq!(children.len(), 1, "one list");
    assert!(
        matches!(&tree.node(children[0]).kind, ElementKind::List { .. }),
        "kind is List"
    );

    let list_children = tree.children(children[0]);
    assert_eq!(list_children.len(), 2, "two list items");
    assert!(
        matches!(
            &tree.node(list_children[0]).kind,
            ElementKind::ListItem { .. }
        ),
        "first child is ListItem"
    );
    assert!(
        matches!(
            &tree.node(list_children[1]).kind,
            ElementKind::ListItem { .. }
        ),
        "second child is ListItem"
    );

    assert!(
        tree.diagnostics()
            .iter()
            .all(|d| !d.message.contains("unclosed")),
        "no unclosed diagnostics: {:?}",
        tree.diagnostics()
    );
}

#[test]
fn compact_html_mixed_with_blank_lines() {
    // Some content with blank lines, some without.
    let source = "<dl>\n<dt>Term 1</dt>\n\nSome markdown\n\n<dd>Desc</dd>\n</dl>\n";
    let tree = parse(source);
    let children = root_children(&tree);
    assert_eq!(children.len(), 1, "one definition list");
    assert_kind(&tree, children[0], &ElementKind::DefinitionList);

    let dl_children = tree.children(children[0]);
    assert!(
        dl_children
            .iter()
            .any(|&id| tree.node(id).kind == ElementKind::DefinitionTerm),
        "has DefinitionTerm child"
    );
    assert!(
        dl_children
            .iter()
            .any(|&id| tree.node(id).kind == ElementKind::DefinitionDesc),
        "has DefinitionDesc child"
    );
}

#[test]
fn compact_html_preserves_raw_non_html() {
    // Non-HTML content without blank lines is still opaque.
    let source = "<div>\n## Not a heading\n<p>also raw</p>\n</div>\n";
    let tree = parse(source);
    let children = root_children(&tree);
    assert_eq!(children.len(), 1, "one container");
    let inner = tree.children(children[0]);
    // The ## line is opaque, but <p> IS dispatched as a child.
    assert!(
        !inner
            .iter()
            .any(|&id| matches!(tree.node(id).kind, ElementKind::Heading { .. })),
        "heading is raw, not parsed"
    );
    assert!(
        inner
            .iter()
            .any(|&id| tree.node(id).kind == ElementKind::Paragraph),
        "<p> dispatched as Paragraph child"
    );
}

#[test]
fn compact_nested_close_tag() {
    // Close tag for inner container dispatched from raw mode.
    let source = "<div>\n<section>\n<p>text</p>\n</section>\n</div>\n";
    let tree = parse(source);
    let children = root_children(&tree);
    assert_eq!(children.len(), 1, "one root container");
    assert_kind(&tree, children[0], &ElementKind::Container);

    let div_children = tree.children(children[0]);
    assert!(
        div_children
            .iter()
            .any(|&id| tree.node(id).kind == ElementKind::Container),
        "section child dispatched inside div"
    );

    assert!(
        tree.diagnostics()
            .iter()
            .all(|d| !d.message.contains("unclosed")),
        "no unclosed diagnostics: {:?}",
        tree.diagnostics()
    );
}

// --- Pathological input limits (ticket 20) ---

use crate::limits;
use cpu_time::ThreadTime;

/// Parsing must always terminate quickly; this generous per-thread
/// **CPU-time** bound catches quadratic or runaway behavior while remaining
/// immune to scheduling delay. Because CPU time accrues only while the
/// thread is actually executing, it excludes time spent descheduled, so
/// cross-process contention (e.g. a concurrent full-suite run saturating
/// every core) cannot inflate it — unlike a wall-clock bound. The parse is
/// single-threaded, so the calling thread's CPU time captures all the work.
/// Set generously to tolerate slower CI hardware (GitHub-hosted runners are
/// markedly slower per core than the self-hosted box this was tuned on);
/// genuine quadratic blowup is orders of magnitude worse and still trips it.
const SLOW_BOUND: std::time::Duration = std::time::Duration::from_secs(30);

#[test]
fn deeply_nested_block_quotes_hit_limit() {
    // 10,000 `>` markers on one line. Block quotes are parsed iteratively,
    // but the nesting cap must still fire so node growth is bounded.
    let source = format!("{} text\n", ">".repeat(10_000));
    let start = ThreadTime::now();
    let tree = parse(&source);
    assert!(
        start.elapsed() < SLOW_BOUND,
        "block quote nesting must not hang"
    );

    let quotes = tree
        .nodes()
        .iter()
        .filter(|n| matches!(n.kind, ElementKind::QuoteBlock))
        .count();
    assert!(
        quotes <= limits::MAX_QUOTE_NESTING,
        "quote nesting capped at {}, got {quotes}",
        limits::MAX_QUOTE_NESTING
    );
    assert!(
        tree.diagnostics()
            .iter()
            .any(|d| d.message.contains("block quote nesting exceeds")),
        "expected a block quote nesting diagnostic: {:?}",
        tree.diagnostics()
    );
}

#[test]
fn same_line_nested_list_markers_hit_limit() {
    // `- - - - ... x` recurses through `classify_item_content`; without a
    // cap this overflows the stack.
    let source = format!("{}x\n", "- ".repeat(10_000));
    let start = ThreadTime::now();
    let tree = parse(&source);
    assert!(
        start.elapsed() < SLOW_BOUND,
        "list marker recursion must not hang"
    );

    let lists = tree
        .nodes()
        .iter()
        .filter(|n| matches!(n.kind, ElementKind::List { .. }))
        .count();
    assert!(
        lists <= limits::MAX_LIST_NESTING,
        "list nesting capped at {}, got {lists}",
        limits::MAX_LIST_NESTING
    );
    assert!(
        tree.diagnostics()
            .iter()
            .any(|d| d.message.contains("list nesting exceeds")),
        "expected a list nesting diagnostic: {:?}",
        tree.diagnostics()
    );
}

#[test]
fn deeply_nested_lists_across_lines_hit_limit() {
    // Each line indents two more spaces, opening a new nested list level.
    let mut source = String::new();
    for depth in 0..2_000 {
        source.push_str(&" ".repeat(depth * 2));
        source.push_str("- item\n");
    }
    let start = ThreadTime::now();
    let tree = parse(&source);
    assert!(start.elapsed() < SLOW_BOUND, "nested lists must not hang");

    let lists = tree
        .nodes()
        .iter()
        .filter(|n| matches!(n.kind, ElementKind::List { .. }))
        .count();
    assert!(
        lists <= limits::MAX_LIST_NESTING,
        "list nesting capped at {}, got {lists}",
        limits::MAX_LIST_NESTING
    );
    assert!(
        tree.diagnostics()
            .iter()
            .any(|d| d.message.contains("list nesting exceeds")),
        "expected a list nesting diagnostic: {:?}",
        tree.diagnostics()
    );
}

#[test]
fn deeply_nested_html_containers_hit_limit() {
    // Nested `<div>` containers are parsed recursively
    // (`consume_html_raw` -> `handle_html_open`); the cap bounds recursion
    // depth and prevents stack overflow.
    let source = "<div>\n".repeat(10_000);
    let start = ThreadTime::now();
    let tree = parse(&source);
    assert!(start.elapsed() < SLOW_BOUND, "nested HTML must not hang");

    assert!(
        tree.diagnostics()
            .iter()
            .any(|d| d.message.contains("HTML container nesting exceeds")),
        "expected an HTML nesting diagnostic: {:?}",
        tree.diagnostics()
    );
}

#[test]
fn scope_stack_depth_is_hard_limited() {
    // 90 block quotes (under the quote cap) then a deep same-line list.
    // Each list level adds two scopes (List + ListItem), so the scope
    // stack reaches its hard cap before the list cap — exercising the
    // cross-container backstop.
    let source = format!("{}{}x\n", "> ".repeat(90), "- ".repeat(100));
    let start = ThreadTime::now();
    let tree = parse(&source);
    assert!(
        start.elapsed() < SLOW_BOUND,
        "mixed deep nesting must not hang"
    );
    assert!(
        tree.diagnostics()
            .iter()
            .any(|d| d.message.contains("maximum scope depth")),
        "expected a scope-depth diagnostic: {:?}",
        tree.diagnostics()
    );
}

#[test]
fn node_count_limit_is_enforced() {
    // More headings than the node cap; the parser must stop allocating
    // nodes, emit a diagnostic, and still return a tree.
    let source = "# h\n".repeat(limits::MAX_NODES + 100);
    let tree = parse(&source);
    assert!(
        tree.len() <= limits::MAX_NODES,
        "tree node count capped at {}, got {}",
        limits::MAX_NODES,
        tree.len()
    );
    assert!(
        tree.diagnostics()
            .iter()
            .any(|d| d.message.contains("-node limit")),
        "expected a node-count diagnostic: {:?}",
        tree.diagnostics()
    );
}

#[test]
fn table_row_with_many_pipes_is_linear() {
    // A 10,000-cell row must split linearly.
    let header = format!("{}|\n", "|a".repeat(10_000));
    let delim = format!("{}|\n", "|-".repeat(10_000));
    let row = format!("{}|\n", "|b".repeat(10_000));
    let source = format!("{header}{delim}{row}");
    let start = ThreadTime::now();
    let tree = parse(&source);
    assert!(
        start.elapsed() < SLOW_BOUND,
        "table cell splitting must be linear"
    );
    assert!(
        tree.nodes()
            .iter()
            .any(|n| matches!(n.kind, ElementKind::Table { .. })),
        "a table should be recognized"
    );
}

#[test]
fn many_reference_definitions_are_bounded() {
    // Thousands of reference definitions: label normalization and lookup
    // must stay near-linear.
    use std::fmt::Write as _;
    let mut source = String::new();
    for i in 0..10_000 {
        let _ = writeln!(source, "[ref{i}]: https://example.com/{i}");
    }
    let start = ThreadTime::now();
    let _tree = parse(&source);
    assert!(
        start.elapsed() < SLOW_BOUND,
        "reference definitions must not be quadratic"
    );
}

#[test]
fn large_mixed_document_parses_quickly() {
    // ~1 MB of mixed structure parses well within the bound.
    let unit = "# Heading\n\nSome [text](./target.md \"references\") and `code`.\n\n\
                    - item one\n- item two\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n\
                    > a quote\n\n```rust\nlet x = 1;\n```\n\n";
    let mut source = String::with_capacity(1_100_000);
    while source.len() < 1_000_000 {
        source.push_str(unit);
    }
    let start = ThreadTime::now();
    let tree = parse(&source);
    let elapsed = start.elapsed();
    assert!(
        elapsed < SLOW_BOUND,
        "1 MB document should parse quickly, took {elapsed:?}"
    );
    assert!(tree.len() > 1, "tree should contain structure");
}

mod commonmark_spec {
    include!("commonmark_spec_tests.rs");
}
