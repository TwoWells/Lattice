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

use crate::block::{self, Tree};
use crate::fm;
use crate::html::{self, HtmlTag};
use crate::invariants::{
    assert_block_wellformed, assert_frontmatter_scalar_fidelity, assert_html_tag_in_bounds,
    assert_inline_resource_fidelity, assert_line_index_agrees, assert_tree_wellformed,
    collect_scalars, detect_frontmatter,
};
use crate::line_index::LineIndex;
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
/// detect frontmatter (YAML, then TOML, then JSON), then build the block
/// tree. The inline pass runs inside `parse_tree_with_entries` (and is
/// idempotent), so there is no separate `parse_inlines` call here.
fn parse_full(source: &str) -> Tree {
    let (fm_block, fm_syntax) = detect_frontmatter(source);
    let fm_span = fm_block.as_ref().map(|b| b.span);
    let fm_entries = fm_block.as_ref().map(|b| b.entries.as_slice());
    block::parse_tree_with_entries(source, fm_span, fm_syntax, fm_entries)
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

/// A scalar of fidelity-safe characters: ASCII letters plus 2-, 3-, and
/// 4-byte samples, and never a quote, backslash, colon, or newline — so it is
/// a valid key/value in all three formats and its resolved text must equal the
/// raw source slice (no escape decoding involved).
fn fidelity_text() -> impl Strategy<Value = String> {
    proptest::collection::vec(
        prop_oneof![
            6 => (b'a'..=b'z').prop_map(char::from),
            1 => prop_oneof![Just('é'), Just('日'), Just('🎉')],
        ],
        1..12,
    )
    .prop_map(|cs| cs.into_iter().collect())
}

/// Frontmatter (YAML / TOML / JSON) carrying multi-byte characters in both the
/// key and the value, for the content-fidelity property. TOML and JSON quote
/// the key so a multi-byte key is legal.
fn multibyte_frontmatter() -> impl Strategy<Value = String> {
    (fidelity_text(), fidelity_text(), 0u8..4).prop_map(|(k, v, fmt)| match fmt {
        0 => format!("---\n{k}: {v}\n---\n"),          // YAML plain
        1 => format!("---\n{k}: \"{v}\"\n---\n"),      // YAML double-quoted
        2 => format!("+++\n\"{k}\" = \"{v}\"\n+++\n"), // TOML quoted key + basic string
        _ => format!("{{\"{k}\": \"{v}\"}}\n"),        // JSON
    })
}

/// Rewrite a generated (LF-only) document into a chosen line-ending style and
/// optionally prepend a UTF-8 BOM, so every structural shape can be tested
/// under `\n`, `\r\n`, bare `\r`, and a per-line mix.
///
/// The mixed style keeps the first line `\n`-terminated, so a frontmatter
/// opener stays recognizable while later lines exercise bare `\r` — the exact
/// combination that once hung the YAML/TOML scanners.
fn line_ending_variant(doc: String, style: u8, bom: bool) -> String {
    let body = match style {
        0 => doc,
        1 => doc.replace('\n', "\r\n"),
        2 => doc.replace('\n', "\r"),
        _ => {
            let mut out = String::new();
            for (i, line) in doc.split_inclusive('\n').enumerate() {
                if let Some(stripped) = line.strip_suffix('\n') {
                    out.push_str(stripped);
                    out.push_str(match i % 3 {
                        0 => "\n",
                        1 => "\r\n",
                        _ => "\r",
                    });
                } else {
                    out.push_str(line);
                }
            }
            out
        }
    };
    if bom { format!("\u{feff}{body}") } else { body }
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

    /// The inline pass is idempotent: `parse_tree` already runs it, so a
    /// second `parse_inlines` call must be a no-op (no duplicated nodes or
    /// diagnostics) and leave the tree well-formed.
    #[test]
    fn parse_inlines_is_idempotent(source in arbitrary_string(300)) {
        let mut tree = block::parse_tree(&source, None);
        let nodes_before = tree.len();
        let diags_before = tree.diagnostics().len();
        inline::parse_inlines(&mut tree);
        prop_assert_eq!(tree.len(), nodes_before, "re-running the inline pass added nodes");
        prop_assert_eq!(
            tree.diagnostics().len(),
            diags_before,
            "re-running the inline pass added diagnostics"
        );
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
// Properties — encoding (line endings, BOM, content fidelity)
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(config())]

    /// Every structural shape parses to a well-formed tree under any
    /// line-ending style (`\n`, `\r\n`, bare `\r`, mixed) and with or without
    /// a leading BOM. Catches lost lines, miscomputed spans, and — via the
    /// hang timeout — non-terminating scanners on bare `\r`.
    #[test]
    fn tree_wellformed_under_any_line_ending(
        doc in prop_oneof![
            markdown_document(),
            yaml_frontmatter(),
            toml_frontmatter(),
            json_frontmatter(),
        ],
        style in 0u8..4,
        bom in any::<bool>(),
    ) {
        let variant = line_ending_variant(doc, style, bom);
        assert_tree_wellformed(&parse_full(&variant));
    }

    /// Every resolved frontmatter scalar (key or value) stays faithful to its
    /// source bytes: its text occurs verbatim in its (escape-free, single-line)
    /// source slice. Catches byte-as-char decoding that mangles multi-byte
    /// keys/values into Latin-1 mojibake.
    #[test]
    fn frontmatter_scalar_text_occurs_in_source(
        doc in prop_oneof![
            multibyte_frontmatter(),
            yaml_frontmatter(),
            toml_frontmatter(),
            json_frontmatter(),
        ]
    ) {
        let (block, _) = detect_frontmatter(&doc);
        if let Some(block) = block {
            assert_frontmatter_scalar_fidelity(&block, &doc);
        }
    }

    /// Every resolved inline resource field (link/image/video/audio `url` and
    /// `title`) occurs verbatim in the source. The parsers slice these fields
    /// rather than decode them, so a byte-as-char regression anywhere in the
    /// inline or HTML-attribute path would make the field absent — the same
    /// fidelity guarantee as for frontmatter scalars, extended to inline nodes.
    #[test]
    fn inline_resource_text_occurs_in_source(
        doc in prop_oneof![
            markdown_document(),
            proptest::collection::vec(link_fragment(), 1..12).prop_map(|v| v.concat()),
        ]
    ) {
        assert_inline_resource_fidelity(&parse_full(&doc));
    }

    /// The cached [`LineIndex`] is a byte-for-byte drop-in for the scalar
    /// byte↔position conversions: its forward direction equals the server's
    /// `byte_offset_to_lsp_position`, and `offset → position → offset`
    /// round-trips through the index. Exercises the encoding axis — arbitrary
    /// UTF-8 plus structured documents under every line-ending style and an
    /// optional BOM — that diagnostic materialization now routes through the
    /// index (ticket perf 01).
    #[test]
    fn line_index_matches_scalar_conversion(
        doc in prop_oneof![
            arbitrary_string(60),
            (markdown_document(), 0u8..4, any::<bool>())
                .prop_map(|(d, style, bom)| line_ending_variant(d, style, bom)),
        ]
    ) {
        assert_line_index_agrees(&doc, &LineIndex::new(&doc));
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
fn multibyte_frontmatter_text_not_corrupted() {
    // Regression: the TOML/JSON quoted-string parsers once pushed each byte as
    // a `char`, mangling multi-byte keys and values into Latin-1 mojibake.
    // YAML used `from_utf8_lossy` and was correct; all three must agree now.
    let cases = [
        "+++\n\"日本語\" = \"café 🎉\"\n+++\n",
        "{\"日本語\": \"café 🎉\"}\n",
        "---\n日本語: café 🎉\n---\n",
    ];
    for case in cases {
        let (block, _) = detect_frontmatter(case);
        let block = block.expect("frontmatter should parse");
        for sc in collect_scalars(&block) {
            let sliced = &case[sc.span.start..sc.span.end];
            assert!(
                sliced.contains(sc.text.as_str()),
                "resolved scalar {:?} absent from source slice {:?} in {case:?}",
                sc.text,
                sliced
            );
        }
    }
}

#[test]
fn frontmatter_and_body_survive_mixed_line_endings() {
    // Regression: a bare `\r` inside otherwise-LF frontmatter once spun the
    // YAML and TOML `skip_blanks` scanners forever. Each ending style (incl.
    // the per-line mix that keeps a recognizable opener) must terminate with a
    // well-formed tree.
    let docs = [
        "---\ntitle: a\nx: b\n---\nbody\n",
        "+++\ntitle = \"a\"\nx = \"b\"\n+++\nbody\n",
        "{\"a\": \"1\", \"b\": \"2\"}\nbody\n",
        "# Heading\n\nA paragraph.\n\n- one\n- two\n",
    ];
    for doc in docs {
        for style in 0u8..4 {
            for bom in [false, true] {
                let variant = line_ending_variant((*doc).to_string(), style, bom);
                assert_tree_wellformed(&parse_full(&variant));
            }
        }
    }
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
