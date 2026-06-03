// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Encoding edge-case tests (ticket 21).
//!
//! Lattice assumes valid UTF-8 input, but within valid UTF-8 several
//! categories of input stress the byte-offset machinery:
//!
//! - **Line endings.** `\n` (Unix), `\r\n` (Windows), and bare `\r` (legacy
//!   Mac), including all three mixed in one document. The parser never
//!   normalizes the buffer, so every span must still refer to the original
//!   bytes regardless of ending style.
//! - **Multi-byte characters** at structural boundaries (heading text, link
//!   URLs, table cells, code-fence info strings, list items, frontmatter
//!   keys) — a 2-, 3-, or 4-byte character straddling a boundary must not
//!   produce an off-by-one or a non-char-boundary span.
//! - **Zero-width and control characters** (ZWSP, ZWJ, soft hyphen, bidi
//!   marks, replacement character) — invisible but valid; structure
//!   recognition must not break.
//! - **BOM positions.** A UTF-8 BOM is stripped only when it is the very
//!   first bytes of the file; anywhere else it is passthrough content.
//!
//! These tests drive the same pipeline as `workspace::parse_content` and the
//! public frontmatter parsers, asserting both structural recognition and
//! that every node span lands on a UTF-8 char boundary within bounds.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests use expect and panic on unreachable match arms for clarity"
)]

use crate::block::{self, ElementKind, Syntax, Tree};
use crate::fm::{FmNode, FmValue, FrontmatterBlock};
use crate::{json, toml, yaml};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Detect frontmatter (YAML → TOML → JSON) with the workspace's precedence.
fn detect_frontmatter(source: &str) -> (Option<FrontmatterBlock>, Syntax) {
    yaml::parse_frontmatter_block(source).map_or_else(
        || {
            toml::parse_frontmatter_block(source).map_or_else(
                || {
                    json::parse_frontmatter_block(source)
                        .map_or((None, Syntax::Yaml), |b| (Some(b), Syntax::Json))
                },
                |b| (Some(b), Syntax::Toml),
            )
        },
        |b| (Some(b), Syntax::Yaml),
    )
}

/// Run the full parse pipeline exactly as `workspace::parse_content` does:
/// detect frontmatter, then build the tree (the inline pass runs inside
/// `parse_tree_with_entries`).
fn parse_full(source: &str) -> Tree {
    let (fm_block, fm_syntax) = detect_frontmatter(source);
    let fm_span = fm_block.as_ref().map(|b| b.span);
    let fm_entries = fm_block.as_ref().map(|b| b.entries.as_slice());
    block::parse_tree_with_entries(source, fm_span, fm_syntax, fm_entries)
}

/// Assert every node span is ordered, within bounds, char-boundary aligned,
/// and contained in its parent — i.e. every span is a valid byte range of the
/// original source, independent of line-ending style or character width.
fn assert_spans_wellformed(tree: &Tree) {
    let source = tree.source();
    let len = source.len();
    let nodes = tree.nodes();

    for (id, node) in nodes.iter().enumerate() {
        assert!(
            node.span.start <= node.span.end,
            "node {id} ({:?}) span start {} after end {}",
            node.kind,
            node.span.start,
            node.span.end
        );
        assert!(
            node.span.end <= len,
            "node {id} ({:?}) span end {} exceeds source length {len}",
            node.kind,
            node.span.end
        );
        assert!(
            source.is_char_boundary(node.span.start),
            "node {id} ({:?}) span start {} is not a UTF-8 char boundary",
            node.kind,
            node.span.start
        );
        assert!(
            source.is_char_boundary(node.span.end),
            "node {id} ({:?}) span end {} is not a UTF-8 char boundary",
            node.kind,
            node.span.end
        );

        if let Some(parent) = node.parent {
            let p = &nodes[parent];
            assert!(
                p.span.start <= node.span.start && node.span.end <= p.span.end,
                "node {id} ({:?}) span {:?} escapes parent {parent} ({:?}) span {:?}",
                node.kind,
                node.span,
                p.kind,
                p.span
            );
        }
    }
}

/// Count nodes matching a predicate.
fn count_kind(tree: &Tree, pred: impl Fn(&ElementKind) -> bool) -> usize {
    tree.nodes().iter().filter(|n| pred(&n.kind)).count()
}

/// Slice the original source for a span.
fn slice(tree: &Tree, span: crate::span::Span) -> &str {
    &tree.source()[span.start..span.end]
}

/// Extract the top-level mapping key text and span from a frontmatter block.
fn first_key(block: &FrontmatterBlock) -> (&str, crate::span::Span) {
    match &block.entries[0] {
        FmNode::Mapping { key, .. } => (key.text.as_str(), key.span),
        other @ FmNode::SequenceItem { .. } => panic!("expected a mapping entry, got {other:?}"),
    }
}

/// Extract the scalar value text of the first top-level mapping entry.
fn first_scalar_value(block: &FrontmatterBlock) -> &str {
    match &block.entries[0] {
        FmNode::Mapping {
            value: FmValue::Scalar(s),
            ..
        } => s.text.as_str(),
        other => panic!("expected a scalar-valued mapping, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Line endings
// ---------------------------------------------------------------------------

#[test]
fn crlf_document_spans_wellformed() {
    let source =
        "---\r\ntitle: Doc\r\n---\r\n# Héading\r\n\r\nA paragraph with a [link](./t.md).\r\n";
    let tree = parse_full(source);
    assert_spans_wellformed(&tree);

    let headings = tree.headings();
    assert_eq!(headings.len(), 1, "one heading expected");
    assert_eq!(
        headings[0].text, "Héading",
        "CRLF must not leak a trailing \\r into the heading text"
    );
    assert_eq!(
        slice(&tree, headings[0].text_span),
        "Héading",
        "heading text span must slice exactly to the text under CRLF"
    );
}

#[test]
fn bare_cr_document_spans_wellformed() {
    // Pure legacy-Mac line endings.
    let source = "# One\r\rSome text.\r\r## Two\r";
    let tree = parse_full(source);
    assert_spans_wellformed(&tree);
    assert_eq!(
        tree.headings().len(),
        2,
        "bare CR must separate the two headings"
    );
    assert!(
        count_kind(&tree, |k| matches!(k, ElementKind::Paragraph)) >= 1,
        "the middle paragraph must be recognized across bare CRs"
    );
}

#[test]
fn mixed_line_endings_one_document() {
    let source = "# Title\n\nFirst.\r\nSecond.\rThird.\n\n- a\r\n- b\r- c\n";
    let tree = parse_full(source);
    assert_spans_wellformed(&tree);
    assert_eq!(tree.headings().len(), 1, "the single heading is recognized");
    // The three list markers must each open an item regardless of which
    // ending precedes them.
    assert_eq!(
        count_kind(&tree, |k| matches!(k, ElementKind::ListItem { .. })),
        3,
        "all three list items recognized across LF / CRLF / CR"
    );
}

#[test]
fn crlf_table_cells_split_correctly() {
    let source = "| a | b |\r\n| --- | --- |\r\n| 1 | 2 |\r\n";
    let tree = parse_full(source);
    assert_spans_wellformed(&tree);
    let cells: Vec<&str> = tree
        .nodes()
        .iter()
        .filter(|n| matches!(n.kind, ElementKind::TableCell))
        .map(|n| slice(&tree, n.span))
        .collect();
    assert_eq!(
        cells,
        vec!["a", "b", "1", "2"],
        "CRLF rows must not pull the next line into the last cell"
    );
}

// ---------------------------------------------------------------------------
// Multi-byte characters at structural boundaries
// ---------------------------------------------------------------------------

#[test]
fn heading_multibyte_span() {
    let tree = parse_full("# Héllo 日本語\n");
    assert_spans_wellformed(&tree);
    let headings = tree.headings();
    assert_eq!(headings.len(), 1, "one heading");
    assert_eq!(headings[0].text, "Héllo 日本語", "full multi-byte text");
    assert_eq!(
        slice(&tree, headings[0].text_span),
        "Héllo 日本語",
        "text span covers all multi-byte characters"
    );
}

#[test]
fn link_url_multibyte_span() {
    let tree = parse_full("See [text](pâth-日本.md).\n");
    assert_spans_wellformed(&tree);
    let url = tree
        .nodes()
        .iter()
        .find_map(|n| match &n.kind {
            ElementKind::Link { url, .. } => Some(url.clone()),
            _ => None,
        })
        .expect("a link node should exist");
    assert_eq!(
        url, "pâth-日本.md",
        "the URL retains every multi-byte character"
    );
}

#[test]
fn table_cells_multibyte_split() {
    let source = "| café | résumé |\n| --- | --- |\n| naïve | 日本 |\n";
    let tree = parse_full(source);
    assert_spans_wellformed(&tree);
    let cells: Vec<&str> = tree
        .nodes()
        .iter()
        .filter(|n| matches!(n.kind, ElementKind::TableCell))
        .map(|n| slice(&tree, n.span))
        .collect();
    assert_eq!(
        cells,
        vec!["café", "résumé", "naïve", "日本"],
        "cell boundaries must not split a multi-byte character"
    );
}

#[test]
fn code_fence_multibyte_info_wellformed() {
    let tree = parse_full("```日本語\nlet x = 1;\n```\n");
    assert_spans_wellformed(&tree);
    assert_eq!(
        count_kind(&tree, |k| matches!(k, ElementKind::CodeBlock)),
        1,
        "the fenced code block is recognized despite a multi-byte info string"
    );
}

#[test]
fn nested_list_multibyte_parent_wellformed() {
    let source = "- 日本語 parent\n    - child\n";
    let tree = parse_full(source);
    assert_spans_wellformed(&tree);
    let parent_item = tree
        .nodes()
        .iter()
        .find(|n| matches!(n.kind, ElementKind::ListItem { .. }))
        .expect("a list item exists");
    assert!(
        slice(&tree, parent_item.span).contains("日本語 parent"),
        "the parent item span covers its multi-byte text"
    );
    assert_eq!(
        count_kind(&tree, |k| matches!(k, ElementKind::ListItem { .. })),
        2,
        "both the parent and the nested child item are recognized"
    );
}

// ---------------------------------------------------------------------------
// Zero-width and control characters
// ---------------------------------------------------------------------------

#[test]
fn zero_width_chars_do_not_break_structure() {
    // ZWSP, ZWJ, ZWNJ embedded in heading and link text.
    let source = "# Ti\u{200B}tle\u{200D}\n\n[li\u{200C}nk](./t.md)\n";
    let tree = parse_full(source);
    assert_spans_wellformed(&tree);
    assert_eq!(
        tree.headings().len(),
        1,
        "zero-width characters must not prevent heading recognition"
    );
    assert_eq!(
        count_kind(&tree, |k| matches!(k, ElementKind::Link { .. })),
        1,
        "zero-width characters must not prevent link recognition"
    );
}

#[test]
fn soft_hyphen_and_bidi_marks_passthrough() {
    // Soft hyphen (U+00AD), LTR mark (U+200E), RTL mark (U+200F).
    let tree = parse_full("# Be\u{00AD}fore \u{200E}mid\u{200F}dle\n");
    assert_spans_wellformed(&tree);
    let headings = tree.headings();
    assert_eq!(headings.len(), 1, "heading recognized with invisible marks");
    assert_eq!(
        slice(&tree, headings[0].text_span),
        "Be\u{00AD}fore \u{200E}mid\u{200F}dle",
        "invisible marks are preserved verbatim in the text span"
    );
}

#[test]
fn replacement_character_passthrough() {
    let tree = parse_full("# Bad\u{FFFD}char\n\nText \u{FFFD} more.\n");
    assert_spans_wellformed(&tree);
    assert_eq!(
        tree.headings().len(),
        1,
        "U+FFFD is just another multi-byte character; structure is unaffected"
    );
}

// ---------------------------------------------------------------------------
// Four-byte emoji in every structural position
// ---------------------------------------------------------------------------

#[test]
fn emoji_in_heading_list_and_link() {
    let source = "# Title 😀\n\n- 😀 item\n\n[😀](./t.md)\n";
    let tree = parse_full(source);
    assert_spans_wellformed(&tree);
    let headings = tree.headings();
    assert_eq!(headings.len(), 1, "emoji heading recognized");
    assert_eq!(headings[0].text, "Title 😀", "4-byte emoji kept in heading");
    assert_eq!(
        count_kind(&tree, |k| matches!(k, ElementKind::ListItem { .. })),
        1,
        "emoji list item recognized"
    );
    assert_eq!(
        count_kind(&tree, |k| matches!(k, ElementKind::Link { .. })),
        1,
        "emoji link text recognized"
    );
}

#[test]
fn emoji_in_yaml_key() {
    let block = yaml::parse_frontmatter_block("---\n😀: value\n---\n")
        .expect("YAML frontmatter should parse");
    let (key, span) = first_key(&block);
    assert_eq!(key, "😀", "the 4-byte emoji key is read whole");
    let source = "---\n😀: value\n---\n";
    assert_eq!(
        &source[span.start..span.end],
        "😀",
        "the key span slices exactly to the emoji"
    );
}

// ---------------------------------------------------------------------------
// BOM positions
// ---------------------------------------------------------------------------

#[test]
fn bom_at_byte_zero_stripped_no_frontmatter() {
    let source = "\u{FEFF}# Heading\n\nText.\n";
    let tree = parse_full(source);
    assert_spans_wellformed(&tree);
    let headings = tree.headings();
    assert_eq!(
        headings.len(),
        1,
        "a leading BOM must not prevent the first heading from being recognized"
    );
    assert_eq!(
        slice(&tree, headings[0].text_span),
        "Heading",
        "the heading span starts after the stripped BOM"
    );
}

#[test]
fn bom_at_byte_zero_stripped_with_yaml_frontmatter() {
    let source = "\u{FEFF}---\ntitle: Doc\n---\n# H\n";
    let tree = parse_full(source);
    assert_spans_wellformed(&tree);
    assert_eq!(
        count_kind(&tree, |k| matches!(k, ElementKind::Frontmatter)),
        1,
        "frontmatter is recognized after a leading BOM"
    );
    // The Frontmatter node begins after the 3-byte BOM.
    let fm = tree
        .nodes()
        .iter()
        .find(|n| matches!(n.kind, ElementKind::Frontmatter))
        .expect("frontmatter node");
    assert_eq!(
        fm.span.start, 3,
        "the frontmatter span starts after the stripped BOM bytes"
    );
}

#[test]
fn bom_mid_document_is_passthrough() {
    let source = "# A\n\nPara \u{FEFF} with mid-document BOM.\n";
    let tree = parse_full(source);
    assert_spans_wellformed(&tree);
    assert_eq!(
        tree.headings().len(),
        1,
        "a mid-document BOM is content and does not disturb structure"
    );
}

#[test]
fn bom_after_frontmatter_is_passthrough() {
    // A BOM at the start of the body (not byte 0 of the file) is not stripped;
    // the document must still parse cleanly with well-formed spans.
    let source = "---\nkey: v\n---\n\u{FEFF}# Body\n";
    let tree = parse_full(source);
    assert_spans_wellformed(&tree);
    assert_eq!(
        count_kind(&tree, |k| matches!(k, ElementKind::Frontmatter)),
        1,
        "frontmatter is still recognized"
    );
}

#[test]
fn bom_in_yaml_value_preserved() {
    let block = yaml::parse_frontmatter_block("---\nkey: a\u{FEFF}b\n---\n")
        .expect("YAML frontmatter should parse");
    assert_eq!(
        first_scalar_value(&block),
        "a\u{FEFF}b",
        "a BOM inside a value is preserved as content, not stripped"
    );
}

// ---------------------------------------------------------------------------
// Frontmatter format encoding: keys, escapes, and bare-CR forward progress
// ---------------------------------------------------------------------------

#[test]
fn yaml_cjk_key_span() {
    let source = "---\n日本語: value\n---\n";
    let block = yaml::parse_frontmatter_block(source).expect("YAML should parse");
    let (key, span) = first_key(&block);
    assert_eq!(key, "日本語", "the CJK key text is read whole");
    assert_eq!(
        &source[span.start..span.end],
        "日本語",
        "the key span slices exactly to the CJK key"
    );
}

#[test]
fn yaml_bare_cr_makes_forward_progress() {
    // Mixed endings inside YAML frontmatter: a bare CR separates two entries.
    // Before the fix this spun forever in `skip_blanks_and_comments`.
    let block = yaml::parse_frontmatter_block("---\na: 1\rb: 2\n---\n").expect("YAML should parse");
    assert_eq!(
        block.entries.len(),
        2,
        "bare CR is a YAML line break: two entries, no hang"
    );
}

#[test]
fn toml_multibyte_quoted_key() {
    let source = "+++\n\"日本語\" = \"v\"\n+++\n";
    let block = toml::parse_frontmatter_block(source).expect("TOML should parse");
    let (key, _) = first_key(&block);
    assert_eq!(key, "日本語", "a quoted multi-byte TOML key is read whole");
}

#[test]
fn toml_bare_cr_makes_forward_progress() {
    // Bare CR between two TOML key-value pairs. Before the fix this spun
    // forever in `skip_blanks`.
    let block =
        toml::parse_frontmatter_block("+++\na = 1\rb = 2\n+++\n").expect("TOML should parse");
    assert_eq!(
        block.entries.len(),
        2,
        "bare CR separates the two TOML entries without hanging"
    );
}

#[test]
fn json_multibyte_key_and_unicode_escape() {
    let source = "{\"日本語\": \"caf\\u00e9\"}\n";
    let block = json::parse_frontmatter_block(source).expect("JSON should parse");
    let (key, _) = first_key(&block);
    assert_eq!(key, "日本語", "the multi-byte JSON key is read whole");
    assert_eq!(
        first_scalar_value(&block),
        "café",
        "a \\uXXXX escape decodes to the multi-byte character"
    );
}

#[test]
fn json_surrogate_pair_escape_decodes_emoji() {
    let source = "{\"k\": \"\\uD83D\\uDE00\"}\n";
    let block = json::parse_frontmatter_block(source).expect("JSON should parse");
    assert_eq!(
        first_scalar_value(&block),
        "😀",
        "a UTF-16 surrogate pair escape decodes to a 4-byte emoji"
    );
}

#[test]
fn json_bare_cr_between_members() {
    // JSON treats CR as insignificant whitespace; a bare CR must not stall.
    let source = "{\"a\": 1,\r\"b\": 2}\n";
    let block = json::parse_frontmatter_block(source).expect("JSON should parse");
    assert_eq!(
        block.entries.len(),
        2,
        "bare CR between JSON members is whitespace; both members parse"
    );
}
