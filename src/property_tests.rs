// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Property-based tests for the hand-rolled parsers.
//!
//! Hand-written examples catch the cases we thought of. These tests catch
//! the ones we didn't: `proptest` generates thousands of inputs — from
//! fully random UTF-8 to structured markdown and frontmatter fragments —
//! and asserts invariants that must hold for *any* input.
//!
//! ## Invariants
//!
//! Universal (any input, via [`assert_tree_wellformed`]):
//!
//! - **No panics.** Every parser entry point returns normally; a panic
//!   fails the test with a shrunk counterexample.
//! - **Span bounds.** Every node span satisfies `start <= end <= len` and
//!   lands on UTF-8 char boundaries (so the span is always sliceable).
//! - **Tree acyclicity.** Every node's ancestor chain terminates at the
//!   single `Document` root without revisiting a node.
//! - **Parent containment.** Every child span is contained in its parent.
//! - **Root structure.** Exactly one `Document` node, at index 0, parentless.
//! - **Diagnostic validity.** Every diagnostic span is within source bounds.
//!
//! Format-specific (frontmatter, HTML tags, backlink extraction) are
//! checked by the dedicated properties below.
//!
//! ## Case count
//!
//! Each property runs [`PROPTEST_CASES`](proptest_cases) cases (default
//! 256, the floor required by ticket 19's acceptance criteria). Set the
//! `PROPTEST_CASES` environment variable to raise it for local hardening
//! runs, e.g. `PROPTEST_CASES=20000 make test T=property`.
//!
//! ## Failures are not flakes
//!
//! Inputs are random, but failures are deterministic on replay: `proptest`
//! saves the failing seed to `proptest-regressions/` and replays it first
//! on every subsequent run, and `nextest` retries are off. A red property
//! test is a real, reproducible bug with a shrunk counterexample — fix the
//! parser; do not re-run to make it pass.

#![allow(
    clippy::wildcard_imports,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    reason = "proptest's prelude is its conventional import; invariant assertions and the proptest! macro necessarily panic on failure, and generator combinators move owned strings by value"
)]

use proptest::prelude::*;

use crate::block::{self, ElementKind, Syntax, Tree};
use crate::fm::{self, FrontmatterBlock};
use crate::html::{self, HtmlTag};
use crate::{inline, json, toml, yaml};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Number of cases per property.
///
/// Defaults to 256 (ticket 19's CI floor); overridable via the
/// `PROPTEST_CASES` environment variable for extended local runs.
fn proptest_cases() -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(256)
}

/// Build a `proptest` config honouring [`proptest_cases`].
fn config() -> ProptestConfig {
    ProptestConfig::with_cases(proptest_cases())
}

// ---------------------------------------------------------------------------
// Full-pipeline helper
// ---------------------------------------------------------------------------

/// Run the full parse pipeline the way `workspace::parse_content` does:
/// detect frontmatter (YAML, then TOML, then JSON), build the block tree,
/// then run the inline pass.
fn parse_full(source: &str) -> Tree {
    let (fm_block, fm_syntax) = detect_frontmatter(source);
    let fm_span = fm_block.as_ref().map(|b| b.span);
    let fm_entries = fm_block.as_ref().map(|b| b.entries.as_slice());
    let mut tree = block::parse_tree_with_entries(source, fm_span, fm_syntax, fm_entries);
    inline::parse_inlines(&mut tree);
    tree
}

/// Detect frontmatter using the same precedence as the workspace loader.
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

// ---------------------------------------------------------------------------
// Reusable invariant assertions
// ---------------------------------------------------------------------------

/// Assert every universal structural invariant on a parsed [`Tree`].
fn assert_tree_wellformed(tree: &Tree) {
    let nodes = tree.nodes();
    let source = tree.source();
    let len = source.len();

    // Root structure: exactly one Document, at index 0, parentless.
    assert!(!nodes.is_empty(), "tree must contain the Document root");
    let doc_count = nodes
        .iter()
        .filter(|n| matches!(n.kind, ElementKind::Document))
        .count();
    assert_eq!(
        doc_count, 1,
        "tree must have exactly one Document node, found {doc_count}"
    );
    assert!(
        matches!(nodes[0].kind, ElementKind::Document),
        "root node (index 0) must be the Document, found {:?}",
        nodes[0].kind
    );
    assert!(
        nodes[0].parent.is_none(),
        "Document root must have no parent"
    );

    for (id, node) in nodes.iter().enumerate() {
        // Span ordering and bounds.
        assert!(
            node.span.start <= node.span.end,
            "node {id} ({:?}) has start {} after end {}",
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
        // Char boundaries: the span must be sliceable from the source.
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

        // Non-root nodes have a parent; the parent contains the child span.
        if id == 0 {
            continue;
        }
        let parent_id = node
            .parent
            .unwrap_or_else(|| panic!("non-root node {id} ({:?}) must have a parent", node.kind));
        assert!(
            parent_id < nodes.len(),
            "node {id} parent index {parent_id} is out of range ({} nodes)",
            nodes.len()
        );
        let parent = &nodes[parent_id];
        assert!(
            parent.span.start <= node.span.start && node.span.end <= parent.span.end,
            "node {id} ({:?}) span {:?} is not contained in parent {parent_id} ({:?}) span {:?}",
            node.kind,
            node.span,
            parent.kind,
            parent.span
        );
    }

    // Acyclicity: every ancestor chain terminates at the root within a
    // bounded number of hops (a cycle would loop past the node count).
    for id in 0..nodes.len() {
        let mut cursor = id;
        let mut hops = 0usize;
        while let Some(parent) = nodes[cursor].parent {
            assert!(
                parent < nodes.len(),
                "ancestor of node {id} has out-of-range parent index {parent}"
            );
            cursor = parent;
            hops += 1;
            assert!(
                hops <= nodes.len(),
                "ancestor chain from node {id} exceeds node count — cycle detected"
            );
        }
        assert_eq!(
            cursor, 0,
            "ancestor chain from node {id} must terminate at the Document root"
        );
    }

    // Diagnostics: spans within bounds.
    for diag in tree.diagnostics() {
        assert!(
            diag.span.start <= diag.span.end && diag.span.end <= len,
            "diagnostic span {:?} out of bounds for source length {len}",
            diag.span
        );
    }
}

/// Assert structural invariants on a parsed frontmatter block.
fn assert_block_wellformed(block: &FrontmatterBlock, source: &str) {
    let len = source.len();
    assert!(
        block.span.start <= block.span.end && block.span.end <= len,
        "frontmatter block span {:?} out of bounds for source length {len}",
        block.span
    );
    assert!(
        source.is_char_boundary(block.span.start) && source.is_char_boundary(block.span.end),
        "frontmatter block span {:?} not on UTF-8 char boundaries",
        block.span
    );
    assert!(
        block.content_span.start <= block.content_span.end && block.content_span.end <= len,
        "frontmatter content span {:?} out of bounds for source length {len}",
        block.content_span
    );
    // The body after the block must be a clean slice that does not reference
    // frontmatter content — i.e. `span.end` is a valid char boundary, which
    // the bounds/boundary checks above already guarantee.
    for diag in &block.diagnostics {
        assert!(
            diag.span.start <= diag.span.end && diag.span.end <= len,
            "frontmatter diagnostic span {:?} out of bounds for source length {len}",
            diag.span
        );
    }
}

/// Assert a tokenized HTML tag reports lengths and spans within `text`.
fn assert_html_tag_in_bounds(tag: &HtmlTag, text: &str) {
    let len = text.len();
    match tag {
        HtmlTag::Open {
            attrs,
            len: consumed,
            ..
        } => {
            assert!(
                *consumed <= len,
                "open tag len {consumed} exceeds text {len}"
            );
            for attr in attrs {
                assert!(
                    attr.name_span.start <= attr.name_span.end && attr.name_span.end <= len,
                    "attribute name span {:?} out of bounds for text length {len}",
                    attr.name_span
                );
                if let Some(value_span) = attr.value_span {
                    assert!(
                        value_span.start <= value_span.end && value_span.end <= len,
                        "attribute value span {value_span:?} out of bounds for text length {len}"
                    );
                }
            }
        }
        HtmlTag::Close { len: consumed, .. } | HtmlTag::Comment { len: consumed } => {
            assert!(
                *consumed <= len,
                "tag len {consumed} exceeds text length {len}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Generators — characters and raw strings
// ---------------------------------------------------------------------------

/// Fully arbitrary UTF-8 string bounded to `max` chars (each char may be
/// multi-byte, control, or whitespace). Tests the "any input" guarantee.
fn arbitrary_string(max: usize) -> impl Strategy<Value = String> {
    proptest::collection::vec(proptest::char::any(), 0..max).prop_map(|cs| cs.into_iter().collect())
}

/// A single inline character: mostly printable ASCII, with a deterministic
/// sprinkle of multi-byte characters to exercise char-boundary handling.
/// Never a newline, so line-oriented fragments stay on one line.
fn inline_char() -> impl Strategy<Value = char> {
    prop_oneof![
        6 => (0x20u8..0x7fu8).prop_map(char::from),
        1 => prop_oneof![
            Just('é'), Just('ü'), Just('ñ'), Just('日'), Just('語'),
            Just('🎉'), Just('—'), Just('•'), Just('𝄞'),
        ],
    ]
}

/// Inline text (no newlines) up to `max` characters.
fn inline_text(max: usize) -> impl Strategy<Value = String> {
    proptest::collection::vec(inline_char(), 0..max).prop_map(|cs| cs.into_iter().collect())
}

// ---------------------------------------------------------------------------
// Generators — markdown fragments
// ---------------------------------------------------------------------------

fn heading_fragment() -> impl Strategy<Value = String> {
    (1usize..=6, inline_text(24))
        .prop_map(|(level, text)| format!("{} {text}\n", "#".repeat(level)))
}

fn thematic_break_fragment() -> impl Strategy<Value = String> {
    prop_oneof![Just("---\n"), Just("***\n"), Just("___\n"), Just("- - -\n")].prop_map(String::from)
}

fn setext_fragment() -> impl Strategy<Value = String> {
    (inline_text(20), prop_oneof![Just("="), Just("-")])
        .prop_map(|(text, rule)| format!("{text}\n{}\n", rule.repeat(3)))
}

fn list_item_fragment() -> impl Strategy<Value = String> {
    (
        prop_oneof![Just("- "), Just("* "), Just("+ "), Just("1. "), Just("2) ")],
        inline_text(24),
    )
        .prop_map(|(marker, text)| format!("{marker}{text}\n"))
}

fn task_item_fragment() -> impl Strategy<Value = String> {
    (
        prop_oneof![Just("[ ]"), Just("[x]"), Just("[X]")],
        inline_text(20),
    )
        .prop_map(|(box_, text)| format!("- {box_} {text}\n"))
}

fn code_fence_fragment() -> impl Strategy<Value = String> {
    (inline_text(8), inline_text(30)).prop_map(|(lang, body)| format!("```{lang}\n{body}\n```\n"))
}

fn indented_code_fragment() -> impl Strategy<Value = String> {
    inline_text(24).prop_map(|text| format!("    {text}\n"))
}

fn blockquote_fragment() -> impl Strategy<Value = String> {
    (1usize..=3, inline_text(24))
        .prop_map(|(depth, text)| format!("{}{text}\n", "> ".repeat(depth)))
}

fn lazy_quote_fragment() -> impl Strategy<Value = String> {
    (inline_text(16), inline_text(16)).prop_map(|(first, lazy)| format!("> {first}\n{lazy}\n"))
}

fn table_fragment() -> impl Strategy<Value = String> {
    (
        inline_text(8),
        inline_text(8),
        inline_text(8),
        inline_text(8),
    )
        .prop_map(|(a, b, c, d)| format!("| {a} | {b} |\n| --- | --- |\n| {c} | {d} |\n"))
}

fn paragraph_fragment() -> impl Strategy<Value = String> {
    inline_text(48).prop_map(|text| format!("{text}\n"))
}

fn blank_fragment() -> impl Strategy<Value = String> {
    prop_oneof![Just("\n"), Just("   \n"), Just("\t\n")].prop_map(String::from)
}

/// One markdown block fragment of any supported kind.
fn markdown_fragment() -> impl Strategy<Value = String> {
    prop_oneof![
        heading_fragment(),
        setext_fragment(),
        thematic_break_fragment(),
        list_item_fragment(),
        task_item_fragment(),
        code_fence_fragment(),
        indented_code_fragment(),
        blockquote_fragment(),
        lazy_quote_fragment(),
        table_fragment(),
        paragraph_fragment(),
        blank_fragment(),
        html_tag_fragment(),
        link_fragment(),
    ]
}

/// A markdown document assembled from random fragments.
fn markdown_document() -> impl Strategy<Value = String> {
    proptest::collection::vec(markdown_fragment(), 0..25).prop_map(|frags| frags.concat())
}

// ---------------------------------------------------------------------------
// Generators — links
// ---------------------------------------------------------------------------

fn url_text() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("./foo.md".to_string()),
        Just("../bar/baz.md#frag".to_string()),
        Just("https://example.com/x?y=1".to_string()),
        Just("mailto:a@b.com".to_string()),
        Just("#section".to_string()),
        Just("image.png".to_string()),
        inline_text(18),
    ]
}

fn link_fragment() -> impl Strategy<Value = String> {
    prop_oneof![
        (inline_text(12), url_text()).prop_map(|(t, u)| format!("[{t}]({u})\n")),
        (inline_text(12), url_text(), inline_text(10))
            .prop_map(|(t, u, title)| format!("[{t}]({u} \"{title}\")\n")),
        (inline_text(12), inline_text(8)).prop_map(|(t, r)| format!("[{t}][{r}]\n")),
        (inline_text(8), url_text()).prop_map(|(label, u)| format!("[{label}]: {u}\n")),
        (inline_text(8), url_text(), inline_text(8))
            .prop_map(|(label, u, title)| format!("[{label}]: {u} \"{title}\"\n")),
        url_text().prop_map(|u| format!("<{u}>\n")),
    ]
}

// ---------------------------------------------------------------------------
// Generators — HTML tags
// ---------------------------------------------------------------------------

fn tag_name() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("div"),
        Just("span"),
        Just("br"),
        Just("img"),
        Just("details"),
        Just("a"),
        Just("DIV"),
        Just("xyz"),
        Just(""),
        Just("1bad"),
        Just("p"),
    ]
    .prop_map(String::from)
}

fn attr_text() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(String::new()),
        Just(" class=\"warning\"".to_string()),
        Just(" data-n=1".to_string()),
        Just(" disabled".to_string()),
        Just("   id='x'  title=\"t\"".to_string()),
        inline_text(14).prop_map(|s| format!(" {s}")),
    ]
}

/// A single HTML tag string, valid or malformed.
fn html_tag_string() -> impl Strategy<Value = String> {
    prop_oneof![
        (tag_name(), attr_text()).prop_map(|(n, a)| format!("<{n}{a}>")),
        (tag_name(), attr_text()).prop_map(|(n, a)| format!("<{n}{a}/>")),
        tag_name().prop_map(|n| format!("</{n}>")),
        inline_text(12).prop_map(|c| format!("<!--{c}-->")),
        inline_text(12).prop_map(|s| format!("<{s}")), // unterminated
        tag_name().prop_map(|n| format!("<{n}")),      // unterminated open
    ]
}

fn html_tag_fragment() -> impl Strategy<Value = String> {
    html_tag_string().prop_map(|tag| format!("{tag}\n"))
}

// ---------------------------------------------------------------------------
// Generators — nesting
// ---------------------------------------------------------------------------

/// Deeply nested block quotes (`> > > ...`) to stress the scope stack.
fn nested_quotes() -> impl Strategy<Value = String> {
    (1usize..16, inline_text(12))
        .prop_map(|(depth, text)| format!("{}{text}\n", "> ".repeat(depth)))
}

/// Increasingly indented list items to stress list-scope handling.
fn nested_lists() -> impl Strategy<Value = String> {
    (1usize..12, inline_text(8)).prop_map(|(depth, text)| {
        let mut out = String::new();
        for level in 0..depth {
            out.push_str(&" ".repeat(level * 2));
            out.push_str("- ");
            out.push_str(&text);
            out.push('\n');
        }
        out
    })
}

/// Deeply nested HTML containers (`<div><div>...`).
fn nested_html() -> impl Strategy<Value = String> {
    (1usize..16).prop_map(|depth| {
        let mut out = String::new();
        for _ in 0..depth {
            out.push_str("<div>");
        }
        out.push_str("\ntext\n");
        for _ in 0..depth {
            out.push_str("</div>");
        }
        out.push('\n');
        out
    })
}

/// Any of the nesting generators.
fn nested_document() -> impl Strategy<Value = String> {
    prop_oneof![nested_quotes(), nested_lists(), nested_html()]
}

// ---------------------------------------------------------------------------
// Generators — frontmatter
// ---------------------------------------------------------------------------

fn fm_key() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("title"),
        Just("tags"),
        Just("author"),
        Just("date"),
        Just("x")
    ]
    .prop_map(String::from)
}

fn predicate_name() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("referenced_by"),
        Just("superseded_by"),
        Just("implemented_by"),
        Just("weird_pred"),
    ]
    .prop_map(String::from)
}

fn path_text() -> impl Strategy<Value = String> {
    prop_oneof![Just("../README.md"), Just("foo/bar.md"), Just("a.md")].prop_map(String::from)
}

/// A YAML `backlinks:` mapping with random predicates and path lists.
fn yaml_backlinks_block() -> impl Strategy<Value = String> {
    proptest::collection::vec(
        (
            predicate_name(),
            proptest::collection::vec(path_text(), 1..4),
        ),
        1..4,
    )
    .prop_map(|preds| {
        let mut out = String::from("backlinks:\n");
        for (pred, paths) in preds {
            out.push_str("  ");
            out.push_str(&pred);
            out.push_str(":\n");
            for path in paths {
                out.push_str("    - ");
                out.push_str(&path);
                out.push('\n');
            }
        }
        out
    })
}

/// A complete YAML frontmatter document (delimiters + entries + body).
fn yaml_frontmatter() -> impl Strategy<Value = String> {
    (
        proptest::collection::vec((fm_key(), inline_text(12)), 0..4),
        proptest::option::of(yaml_backlinks_block()),
        inline_text(20),
    )
        .prop_map(|(kvs, backlinks, body)| {
            let mut out = String::from("---\n");
            for (k, v) in kvs {
                out.push_str(&k);
                out.push_str(": ");
                out.push_str(&v);
                out.push('\n');
            }
            if let Some(block) = backlinks {
                out.push_str(&block);
            }
            out.push_str("---\n");
            out.push_str(&body);
            out.push('\n');
            out
        })
}

/// A complete TOML frontmatter document (`+++` delimiters).
fn toml_frontmatter() -> impl Strategy<Value = String> {
    (
        proptest::collection::vec((fm_key(), inline_text(10)), 0..4),
        inline_text(20),
    )
        .prop_map(|(kvs, body)| {
            let mut out = String::from("+++\n");
            for (k, v) in kvs {
                out.push_str(&k);
                out.push_str(" = \"");
                out.push_str(&v.replace('"', ""));
                out.push_str("\"\n");
            }
            out.push_str("+++\n");
            out.push_str(&body);
            out.push('\n');
            out
        })
}

/// A complete JSON frontmatter document (`{ ... }`).
fn json_frontmatter() -> impl Strategy<Value = String> {
    (
        proptest::collection::vec((fm_key(), inline_text(8)), 0..4),
        inline_text(20),
    )
        .prop_map(|(kvs, body)| {
            let pairs: Vec<String> = kvs
                .into_iter()
                .map(|(k, v)| format!("\"{k}\": \"{}\"", v.replace(['"', '\\'], "")))
                .collect();
            format!("{{{}}}\n{body}\n", pairs.join(", "))
        })
}

/// Corrupt a generated string in one of several structural ways, to drive
/// the parsers' error-recovery paths.
fn corrupt(source: String, mode: u8, pos: usize) -> String {
    /// Round `idx` up to the next UTF-8 char boundary at or before `len`.
    fn boundary(s: &str, idx: usize) -> usize {
        let mut i = idx.min(s.len());
        while i < s.len() && !s.is_char_boundary(i) {
            i += 1;
        }
        i
    }

    match mode % 5 {
        0 => source,
        1 => {
            // Truncate at a char boundary.
            let cut = boundary(&source, pos % (source.len() + 1));
            source[..cut].to_string()
        }
        2 => format!("{source}\u{0}\u{fffd}garbage~~~"), // append garbage incl. NUL + replacement char
        3 => source
            .replacen("---", "-", 1)
            .replacen("+++", "+", 1)
            .replacen('{', "", 1), // break a delimiter
        _ => {
            // Insert garbage at a char boundary.
            let at = boundary(&source, pos % (source.len() + 1));
            let (head, tail) = source.split_at(at);
            format!("{head}≈GARBAGE≈{tail}")
        }
    }
}

/// Wrap a frontmatter generator with random corruption.
fn maybe_corrupt(base: impl Strategy<Value = String>) -> impl Strategy<Value = String> {
    (base, any::<u8>(), any::<usize>()).prop_map(|(s, mode, pos)| corrupt(s, mode, pos))
}

// ---------------------------------------------------------------------------
// Properties — block tree
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(config())]

    /// The full pipeline produces a well-formed tree for any UTF-8 input.
    #[test]
    fn tree_wellformed_on_random_utf8(source in arbitrary_string(400)) {
        assert_tree_wellformed(&parse_full(&source));
    }

    /// `parse_tree` alone (no frontmatter) is well-formed on any input.
    #[test]
    fn parse_tree_wellformed_on_random_utf8(source in arbitrary_string(300)) {
        assert_tree_wellformed(&block::parse_tree(&source, None));
    }

    /// Structured markdown documents parse to well-formed trees.
    #[test]
    fn tree_wellformed_on_structured_markdown(source in markdown_document()) {
        assert_tree_wellformed(&parse_full(&source));
    }

    /// Deeply nested structures don't break scope-stack invariants.
    #[test]
    fn tree_wellformed_on_nested(source in nested_document()) {
        assert_tree_wellformed(&parse_full(&source));
    }
}

// ---------------------------------------------------------------------------
// Properties — inline pass
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(config())]

    /// The inline pass never panics and preserves tree well-formedness on
    /// any paragraph/heading content.
    #[test]
    fn parse_inlines_wellformed(source in arbitrary_string(300)) {
        let mut tree = block::parse_tree(&source, None);
        inline::parse_inlines(&mut tree);
        assert_tree_wellformed(&tree);
    }

    /// Link-heavy documents keep every link node span sliceable.
    #[test]
    fn link_documents_wellformed(
        links in proptest::collection::vec(link_fragment(), 0..12)
    ) {
        assert_tree_wellformed(&parse_full(&links.concat()));
    }
}

// ---------------------------------------------------------------------------
// Properties — frontmatter
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(config())]

    /// YAML frontmatter parsing never panics and yields well-formed blocks.
    #[test]
    fn yaml_frontmatter_wellformed(source in maybe_corrupt(yaml_frontmatter())) {
        if let Some(block) = yaml::parse_frontmatter_block(&source) {
            assert_block_wellformed(&block, &source);
        }
    }

    /// TOML frontmatter parsing never panics and yields well-formed blocks.
    #[test]
    fn toml_frontmatter_wellformed(source in maybe_corrupt(toml_frontmatter())) {
        if let Some(block) = toml::parse_frontmatter_block(&source) {
            assert_block_wellformed(&block, &source);
        }
    }

    /// JSON frontmatter parsing never panics and yields well-formed blocks.
    #[test]
    fn json_frontmatter_wellformed(source in maybe_corrupt(json_frontmatter())) {
        if let Some(block) = json::parse_frontmatter_block(&source) {
            assert_block_wellformed(&block, &source);
        }
    }

    /// All three frontmatter parsers tolerate arbitrary UTF-8 without panic.
    #[test]
    fn frontmatter_parsers_no_panic_on_random(source in arbitrary_string(300)) {
        if let Some(block) = yaml::parse_frontmatter_block(&source) {
            assert_block_wellformed(&block, &source);
        }
        if let Some(block) = toml::parse_frontmatter_block(&source) {
            assert_block_wellformed(&block, &source);
        }
        if let Some(block) = json::parse_frontmatter_block(&source) {
            assert_block_wellformed(&block, &source);
        }
    }

    /// `extract_backlinks` never panics on any parsed frontmatter block.
    #[test]
    fn extract_backlinks_no_panic(
        source in prop_oneof![
            maybe_corrupt(yaml_frontmatter()),
            maybe_corrupt(toml_frontmatter()),
            maybe_corrupt(json_frontmatter()),
            arbitrary_string(200),
        ]
    ) {
        let block = yaml::parse_frontmatter_block(&source)
            .or_else(|| toml::parse_frontmatter_block(&source))
            .or_else(|| json::parse_frontmatter_block(&source));
        if let Some(block) = block {
            let _ = fm::extract_backlinks(&block, &source);
        }
    }
}

// ---------------------------------------------------------------------------
// Properties — HTML tokenizer
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(config())]

    /// `tokenize_tag` never panics and reports in-bounds spans and lengths.
    #[test]
    fn tokenize_tag_no_panic(
        text in prop_oneof![html_tag_string(), arbitrary_string(120)]
    ) {
        if let Some(tag) = html::tokenize_tag(&text, 0) {
            assert_html_tag_in_bounds(&tag, &text);
        }
    }

    /// Tokenizing at a non-zero base offset keeps spans relative to it.
    #[test]
    fn tokenize_tag_with_base_offset(text in html_tag_string(), base in 0usize..64) {
        // Spans are reported relative to `base`; subtract it to validate
        // against the tag text alone.
        if let Some(HtmlTag::Open { attrs, .. }) = html::tokenize_tag(&text, base) {
            for attr in attrs {
                prop_assert!(
                    attr.name_span.start >= base,
                    "attribute name span start {} should be at or after base {base}",
                    attr.name_span.start
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Regression-style deterministic checks
// ---------------------------------------------------------------------------

#[test]
fn wellformed_on_known_tricky_inputs() {
    let cases = [
        "",
        "\n\n\n",
        "# heading\n\nparagraph\n",
        "> lazy\ncontinuation line\n",
        "> > > deep\nlazy\n",
        "- a\n  - b\n    - c\n",
        "```\nunclosed code\n",
        "<div><div>\ntext\n</div>\n",
        "| a | b |\n| --- | --- |\n| c |\n",
        "日本語の見出し\n===\n",
        "---\ntitle: 値\nbacklinks:\n  referenced_by:\n    - ../x.md\n---\nbody 🎉\n",
        "+++\ntitle = \"t\"\n+++\nbody\n",
        "{\"backlinks\": {\"referenced_by\": [\"a.md\"]}}\nbody\n",
        "[text](./a.md \"references\") and 🎉 [ref][r]\n\n[r]: ./b.md\n",
        "\u{feff}# BOM heading\n",
    ];
    for case in cases {
        assert_tree_wellformed(&parse_full(case));
    }
}

#[test]
fn frontmatter_blocks_wellformed_on_known_inputs() {
    let yaml = "---\ntitle: x\n---\nbody\n";
    if let Some(block) = yaml::parse_frontmatter_block(yaml) {
        assert_block_wellformed(&block, yaml);
        let links = fm::extract_backlinks(&block, yaml);
        assert!(links.is_empty(), "no backlinks expected in {yaml:?}");
    }

    let with_backlinks = "---\nbacklinks:\n  referenced_by:\n    - a.md\n    - b.md\n---\n";
    let block =
        yaml::parse_frontmatter_block(with_backlinks).expect("frontmatter block should parse");
    assert_block_wellformed(&block, with_backlinks);
    let links = fm::extract_backlinks(&block, with_backlinks);
    assert_eq!(
        links.get("referenced_by").map(Vec::len),
        Some(2),
        "expected two referenced_by paths"
    );
}

#[test]
fn flow_collection_recovery_terminates() {
    // Regression: a flow-collection terminator the parser does not own
    // (a `]` inside `{...}`, a `}` inside `[...]`, a bare `:`) once left the
    // YAML/TOML/JSON flow parsers spinning a non-advancing loop, allocating
    // empty entries until the process ran out of memory. Each input below
    // must now terminate; reaching the assertions at all proves it.
    let yaml_cases = [
        "---\nx: {a]\n---\n",
        "---\nx: {]}\n---\n",
        "---\ntitle: { !    \ndate: swZ9U]JF\n---\nbody\n",
        "---\nx: [}]\n---\n",
    ];
    for case in yaml_cases {
        if let Some(block) = yaml::parse_frontmatter_block(case) {
            assert_block_wellformed(&block, case);
            let _ = fm::extract_backlinks(&block, case);
        }
    }

    let toml_cases = ["+++\nx = [}]\n+++\n", "+++\nx = [ } , 1]\n+++\n"];
    for case in toml_cases {
        if let Some(block) = toml::parse_frontmatter_block(case) {
            assert_block_wellformed(&block, case);
            let _ = fm::extract_backlinks(&block, case);
        }
    }

    let json_cases = [
        "{\"tags\": ]}\n",
        "{\"k\":[}]}\n",
        "{\"tags\":≈GARBAGE≈ \"]######\"}\nbody\n",
    ];
    for case in json_cases {
        if let Some(block) = json::parse_frontmatter_block(case) {
            assert_block_wellformed(&block, case);
            let _ = fm::extract_backlinks(&block, case);
        }
    }
}
