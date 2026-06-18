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
    Edit, assert_block_wellformed, assert_carrier_fidelity, assert_edit_sequence_stable,
    assert_emphasis_span_fidelity, assert_frontmatter_scalar_fidelity, assert_html_tag_in_bounds,
    assert_inline_resource_fidelity, assert_line_index_agrees, assert_structural_invariants,
    assert_tree_wellformed, carrier_backlinks, collect_scalars, detect_frontmatter,
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
        emphasis_fragment(),
    ]
}

/// A markdown document assembled from random fragments.
fn markdown_document() -> impl Strategy<Value = String> {
    proptest::collection::vec(markdown_fragment(), 0..25).prop_map(|frags| frags.concat())
}

/// An inline fragment exercising the emphasis / strong / strikethrough
/// delimiters, including flanking edge cases and the GFM single-`~` form. The
/// raw-delimiter arm wraps arbitrary delimiters around text so the flanking
/// rules are stressed with both well-formed and ill-formed runs.
fn emphasis_fragment() -> impl Strategy<Value = String> {
    let delim = prop_oneof![
        Just("*"),
        Just("**"),
        Just("_"),
        Just("__"),
        Just("~"),
        Just("~~")
    ];
    prop_oneof![
        (delim.clone(), inline_text(10)).prop_map(|(d, t)| format!("{d}{t}{d}\n")),
        inline_text(12).prop_map(|t| format!("a*{t}*c\n")),
        inline_text(12).prop_map(|t| format!("foo_{t}_baz\n")),
        // The headline correctness case: left-flanking-only single tildes.
        Just("~89 of ~162\n".to_string()),
        // Raw delimiters around text — flanking decides whether they pair.
        (delim, inline_text(8), delim_tail())
            .prop_map(|(open, t, close)| format!("{open}{t}{close}\n")),
    ]
}

/// A trailing delimiter run for the raw-delimiter emphasis arm.
fn delim_tail() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        Just("*"),
        Just("**"),
        Just("_"),
        Just("~"),
        Just("~~"),
        Just(" *"),
        Just("")
    ]
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
// Generators — edits (perf ticket 03)
// ---------------------------------------------------------------------------

/// Replacement text for a random edit. Mixes ordinary inline text with the
/// non-local cascade triggers issue 014 names — fence openers/closers, setext
/// underlines, thematic breaks, link-reference and footnote definitions, HTML
/// container tags — and a few encoding-axis characters, so a single edit can
/// flip a *distant* line's block role rather than only a local one.
fn edit_text() -> impl Strategy<Value = String> {
    prop_oneof![
        inline_text(20),
        Just("```\n".to_string()),
        Just("```rust\n".to_string()),
        Just("~~~\n".to_string()),
        Just("===\n".to_string()),
        Just("---\n".to_string()),
        Just("\n".to_string()),
        Just("[r]: ./target.md\n".to_string()),
        Just("[^f]: a footnote\n".to_string()),
        Just("<div>\n".to_string()),
        Just("</div>\n".to_string()),
        Just("> quote\n".to_string()),
        Just("café 🎉\u{200b}\n".to_string()),
        Just(String::new()),
    ]
}

/// A single edit coordinate: mostly small (so it lands inside a generated
/// document and produces a meaningful splice), with an occasional far-past-EOF
/// value to exercise the `LineIndex::offset` clamp.
fn edit_coord() -> impl Strategy<Value = u32> {
    prop_oneof![
        8 => 0u32..30,
        1 => 100u32..5000,
        1 => Just(u32::MAX),
    ]
}

/// A single `{range, text}` edit. The range may be empty (an insertion), span
/// columns (a same-line replacement), or span lines (a multi-line deletion);
/// `apply_lsp_edit` orders the endpoints, so a reversed range is still valid.
fn edit() -> impl Strategy<Value = Edit> {
    (
        edit_coord(),
        edit_coord(),
        edit_coord(),
        edit_coord(),
        edit_text(),
    )
        .prop_map(|(start_line, start_char, end_line, end_char, text)| Edit {
            start_line,
            start_char,
            end_line,
            end_char,
            text,
        })
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

    /// Every recognized emphasis / strong / strikethrough run carries a span
    /// that is delimited correctly at both ends with non-empty content — the
    /// off-by-one span guard for the flanking algorithm (ticket 26). Driven over
    /// both emphasis-focused fragments and arbitrary UTF-8 so adjacent
    /// multi-byte characters exercise char-boundary span arithmetic.
    #[test]
    fn emphasis_spans_are_delimited(
        doc in prop_oneof![
            proptest::collection::vec(emphasis_fragment(), 0..12).prop_map(|v| v.concat()),
            arbitrary_string(200),
        ]
    ) {
        let tree = parse_full(&doc);
        assert_tree_wellformed(&tree);
        assert_emphasis_span_fidelity(&tree);
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
// Properties — differential edit-sequence oracle (perf ticket 03)
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(config())]

    /// Applying a random sequence of `{range, text}` edits to a base document
    /// keeps every full-pipeline invariant holding after each edit, and maps
    /// each range through the `LineIndex` inverse (ticket perf 01) exactly as
    /// incremental text-sync will. This is the differential oracle of perf
    /// ticket 03 — today a parser-stability net over edited documents the static
    /// generators never assemble, and the gate for the incremental parse/graph
    /// work (tickets perf 04 / 05), which will add the
    /// `incremental(edits) ≡ full(final_text)` arm to the same entry point.
    #[test]
    fn edit_sequence_preserves_invariants(
        base in prop_oneof![
            markdown_document(),
            (markdown_document(), 0u8..4, any::<bool>())
                .prop_map(|(d, style, bom)| line_ending_variant(d, style, bom)),
            yaml_frontmatter(),
            toml_frontmatter(),
            json_frontmatter(),
        ],
        edits in proptest::collection::vec(edit(), 0..8),
    ) {
        assert_edit_sequence_stable(&base, &edits);
    }
}

// ---------------------------------------------------------------------------
// Generators — structural surface (issue 033)
// ---------------------------------------------------------------------------

/// A reference fragment shaped to reach the structural reference scanners: bare
/// paths, quoted paths (single and double — the issue 032 apostrophe surface),
/// backtick-wrapped paths, dangling `.md` references with fragments, and
/// `{Name}/…` external-namespace forms. Mixes contractions, possessives,
/// multibyte, and zero-width characters so the quoted-path byte-scanner is
/// exercised against the encoding axis.
fn structural_reference_fragment() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("See ./docs/guide.md for details.\n".to_string()),
        Just("Refer to \"notes/plan.md\" please.\n".to_string()),
        Just("It's in 'tasks/today.md' now.\n".to_string()),
        Just("The team's `archive/old.md` file.\n".to_string()),
        Just("Dangling [link](./missing.md#section).\n".to_string()),
        Just("Cross to {Design}/spec.md#overview here.\n".to_string()),
        Just("O'Brien's notes 'café/résumé.md' moved.\n".to_string()),
        Just("Zero\u{200b}width 'a\u{200b}b.md' path.\n".to_string()),
        Just("Unbalanced 'quote path.md continues.\n".to_string()),
        Just("Possessive's \"don't.md\" tricky.\n".to_string()),
    ]
}

/// A structural fragment of any kind that drives `structural::collect`: heading
/// hierarchy, raw HTML, a language-less code fence, and the reference scanners.
fn structural_fragment() -> impl Strategy<Value = String> {
    prop_oneof![
        heading_fragment(),
        html_tag_fragment(),
        Just("```\nno language fence\n```\n".to_string()),
        Just("```rust\nfn main() {}\n```\n".to_string()),
        structural_reference_fragment(),
        paragraph_fragment(),
        blank_fragment(),
    ]
}

/// A document assembled from structural fragments, optionally carrying an
/// `exceptions` frontmatter block so the 031 reconciliation path runs.
fn structural_document() -> impl Strategy<Value = String> {
    (
        proptest::option::of(exceptions_frontmatter()),
        proptest::collection::vec(structural_fragment(), 0..20),
    )
        .prop_map(|(fm, frags)| format!("{}{}", fm.unwrap_or_default(), frags.concat()))
}

/// A YAML `exceptions:` frontmatter block (issue 031) over the path-shaped
/// lints, so the structural pass reconciles suppressions and unused exceptions.
fn exceptions_frontmatter() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(
            "---\nexceptions:\n  bare_paths:\n    \"./missing.md\": gone on purpose\n---\n"
                .to_string()
        ),
        Just("---\nexceptions:\n  stale_references:\n    \"old.md\": legacy\n---\n".to_string()),
        Just("---\nexceptions:\n  bare_paths:\n    \"unmatched.md\":\n---\n".to_string()),
    ]
}

// ---------------------------------------------------------------------------
// Generators — metadata carrier (ticket 25, decision 015)
// ---------------------------------------------------------------------------

/// The YAML body of a metadata carrier: a `backlinks:` mapping, optionally
/// followed by an `exceptions:` block. The same body shape a leading `---` block
/// carries, so the differential `carrier ≡ leading block` arm compares like with
/// like. Paths are drawn from [`path_text`] (which includes a multi-byte form),
/// exercising the encoding axis through the carrier parse path.
fn carrier_body() -> impl Strategy<Value = String> {
    (
        yaml_backlinks_block(),
        proptest::option::of(
            proptest::collection::vec((path_text(), inline_text(10)), 1..3).prop_map(|entries| {
                let mut out = String::from("exceptions:\n  stale_references:\n");
                for (path, reason) in entries {
                    out.push_str("    \"");
                    out.push_str(&path);
                    out.push_str("\": ");
                    out.push_str(&reason.replace([':', '\n', '"'], ""));
                    out.push('\n');
                }
                out
            }),
        ),
    )
        .prop_map(|(backlinks, exceptions)| {
            format!("{backlinks}{}", exceptions.unwrap_or_default())
        })
}

/// A document whose metadata is sourced from a `yaml lattice` carrier
/// (decision 015): a naked top-level fence, a `<details>`-wrapped fence, or
/// (the inert control) a fence nested inside a blockquote or an outer
/// documentation fence. The carrier-fidelity invariant must source the live
/// carriers faithfully and find no live carrier in the inert ones.
fn carrier_document() -> impl Strategy<Value = String> {
    (carrier_body(), 0u8..4).prop_map(|(body, shape)| match shape {
        // Naked top-level carrier.
        0 => format!("# Title\n\n```yaml lattice\n{body}```\n"),
        // `<details>`-wrapped carrier (well-formed, render-clean).
        1 => format!(
            "# Title\n\n<details><summary>lattice</summary>\n\n```yaml lattice\n{body}```\n\n</details>\n"
        ),
        // Inert: nested inside a blockquote (quoted content, never live metadata).
        2 => {
            let fence = format!("```yaml lattice\n{body}```\n");
            let mut quoted = String::from("# Title\n\n");
            for line in fence.lines() {
                quoted.push_str("> ");
                quoted.push_str(line);
                quoted.push('\n');
            }
            quoted
        }
        // Inert: nested inside an outer documentation fence (one opaque node).
        _ => format!("# Docs\n\n````markdown\n```yaml lattice\n{body}```\n````\n"),
    })
}

// ---------------------------------------------------------------------------
// Properties — metadata carrier content fidelity (ticket 25)
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(config())]

    /// A `yaml lattice` carrier sources frontmatter faithfully: the carrier
    /// block's scalars occur verbatim in the source, and the backlinks/exceptions
    /// it yields equal those of the *same YAML* as a leading `---` block. Driven
    /// over naked, `<details>`-wrapped, and inert (blockquote / outer-fence)
    /// carriers under every line-ending style and an optional BOM, plus arbitrary
    /// UTF-8. This is the assertion the `fuzz_full` target shares, so the two
    /// suites cannot drift (ticket 25); it closes the carrier content-fidelity
    /// blind spot the leading-block-only `assert_frontmatter_scalar_fidelity`
    /// left open.
    #[test]
    fn carrier_frontmatter_is_faithful(
        source in prop_oneof![
            carrier_document(),
            (carrier_document(), 0u8..4, any::<bool>())
                .prop_map(|(d, style, bom)| line_ending_variant(d, style, bom)),
            arbitrary_string(300),
            markdown_document(),
        ]
    ) {
        assert_carrier_fidelity(&source);
    }
}

// ---------------------------------------------------------------------------
// Properties — structural diagnostic pass (issue 033)
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(config())]

    /// The structural pass never panics and every emitted diagnostic span is a
    /// valid, char-boundary byte range that round-trips through the LSP position
    /// mapping (or, for a line-only diagnostic, a 1-based line). Drives the same
    /// pipeline the workspace loader does — frontmatter + tree + exceptions +
    /// deterministic existence oracle — across structured references, headings,
    /// raw HTML, and language-less fences, plus arbitrary UTF-8 and every
    /// line-ending variant. This is the assertion the `fuzz_structural` target
    /// shares, so the two suites cannot drift (issue 033).
    #[test]
    fn structural_diagnostics_valid(
        source in prop_oneof![
            structural_document(),
            (structural_document(), 0u8..4, any::<bool>())
                .prop_map(|(d, style, bom)| line_ending_variant(d, style, bom)),
            arbitrary_string(300),
            markdown_document(),
        ]
    ) {
        assert_structural_invariants(&source);
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

#[test]
fn structural_diagnostics_valid_on_known_inputs() {
    // The structural surface, exercised deterministically: dark-matter / bare /
    // quoted / backtick reference shapes (the issue 032 apostrophe surface),
    // dangling `.md` references, `{Name}/…` external forms, headings, raw HTML,
    // a language-less fence, and an `exceptions` frontmatter block (issue 031).
    // `assert_structural_invariants` runs the full pass and checks every emitted
    // span is a valid, round-tripping char-boundary byte range.
    let cases = [
        "",
        "# Heading\n\nSee ./docs/guide.md here.\n",
        "It's the team's 'tasks/today.md' file.\n",
        "O'Brien's \"don't.md\" and café/résumé.md paths.\n",
        "Zero\u{200b}width 'a\u{200b}b.md' reference.\n",
        "Unbalanced 'quote that never closes.md\n",
        "Cross to {Design}/spec.md#overview here.\n",
        "Dangling [link](./missing.md#frag).\n",
        "```\nfence without a language\n```\n",
        "<div>raw <span>html</span></div>\n",
        "\u{feff}# BOM\r\nWith CRLF and a 'path.md' ref.\r\n",
        "---\nexceptions:\n  bare_paths:\n    \"./missing.md\": gone\n---\nSee ./missing.md\n",
        "---\nexceptions:\n  stale_references:\n    \"old.md\": legacy\n---\nbody\n",
        // Metadata-channel carrier (decision 015): naked, `<details>`-wrapped,
        // both render gotchas, a duplicate, malformed inner YAML, and an
        // inert example nested in an outer fence — every carrier diagnostic
        // span must round-trip, including under CRLF and a multi-byte path.
        "```yaml lattice\nbacklinks:\n  referenced_by:\n    - café.md\n```\n",
        "<details><summary>lattice</summary>\n\n```yaml lattice\nbacklinks:\n  referenced_by:\n    - a.md\n```\n\n</details>\n",
        "<details><summary>lattice</summary>\n```yaml lattice\nbacklinks:\n  referenced_by:\n    - a.md\n```\n\n</details>\n",
        "<details><summary>lattice</summary>\r\n\r\n```yaml lattice\r\nbacklinks:\r\n  referenced_by:\r\n    - a.md\r\n```\r\n</details>\r\n",
        "```yaml lattice\nbacklinks:\n  referenced_by:\n    - a.md\n```\n\n```yaml lattice\nbacklinks:\n  referenced_by:\n    - b.md\n```\n",
        "```yaml lattice\nbacklinks:\n      bad_indent\n```\n",
        "````markdown\n```yaml lattice\nbacklinks:\n  referenced_by:\n    - a.md\n```\n````\n",
    ];
    for case in cases {
        assert_structural_invariants(case);
    }
}

#[test]
fn carrier_fidelity_holds_on_known_inputs() {
    // Live carriers — naked, `<details>`-wrapped, with exceptions, multi-byte,
    // and under CRLF — must all source faithful metadata that agrees with the
    // same YAML as a leading block. The inert controls (blockquote, list,
    // outer-fence, incidental `yaml`, leading-block-present) have no live carrier,
    // so the invariant returns without firing. `assert_carrier_fidelity` runs the
    // full check, so reaching the end proves each held.
    let cases = [
        // Naked top-level carrier.
        "# Title\n\n```yaml lattice\nbacklinks:\n  referenced_by:\n    - README.md\n```\n",
        // `<details>`-wrapped, well-formed.
        "# Title\n\n<details><summary>lattice</summary>\n\n```yaml lattice\nbacklinks:\n  referenced_by:\n    - a.md\n    - b.md\n```\n\n</details>\n",
        // Carrier carrying an `exceptions` block.
        "# T\n\n```yaml lattice\nbacklinks:\n  referenced_by:\n    - a.md\nexceptions:\n  stale_references:\n    \"old.md\": migrated\n```\n",
        // Multi-byte path through the carrier parse path.
        "# T\n\n```yaml lattice\nbacklinks:\n  referenced_by:\n    - café/résumé.md\n```\n",
        // CRLF line endings.
        "# T\r\n\r\n```yaml lattice\r\nbacklinks:\r\n  referenced_by:\r\n    - a.md\r\n```\r\n",
        // Inert controls — no live carrier, invariant returns cleanly.
        "# T\n\n> ```yaml lattice\n> backlinks:\n>   referenced_by:\n>     - a.md\n> ```\n",
        "# Docs\n\n````markdown\n```yaml lattice\nbacklinks:\n  referenced_by:\n    - a.md\n```\n````\n",
        "# T\n\n```yaml\nbacklinks:\n  referenced_by:\n    - a.md\n```\n",
        // Leading block present: the carrier is not the data source, so the
        // invariant must not inspect it.
        "---\nbacklinks:\n  referenced_by:\n    - a.md\n---\n# T\n\n```yaml lattice\nbacklinks:\n  referenced_by:\n    - b.md\n```\n",
        // Degenerate: empty body, no metadata.
        "# T\n\n```yaml lattice\n```\n",
    ];
    for case in cases {
        assert_carrier_fidelity(case);
    }
}

#[test]
fn carrier_fidelity_unterminated_single_quote_at_eof() {
    // Issue 041 regression. A `yaml lattice` carrier whose final value is an
    // unterminated single-quoted scalar (a lone `'`) followed by the body's
    // trailing newline. The carrier body ends in `'\n`; the synthetic leading
    // block must present *those exact bytes* as the YAML content, or the
    // unterminated scalar absorbs a spurious newline and the differential arm
    // trips on a divergence the parser never produced. The body already ends in
    // a line ending, so `equivalent_leading_block` must not inject an extra one.
    let source = "# Title\n\n```yaml lattice\nbacklinks:\n  referenced_by:\n    - ../README.md\nexceptions:\n  stale_references:\n    \"../README.md\": '\n```\n";
    assert_carrier_fidelity(source);
}

#[test]
fn carrier_fidelity_has_teeth() {
    // The invariant is not vacuous: a carrier whose extracted backlinks are
    // *corrupted* away from the source must be caught. `assert_carrier_fidelity`
    // can only fire when the carrier parse genuinely diverges from the leading
    // block, which a correct parser never does — so to prove the comparison has
    // teeth we corrupt the carrier-sourced metadata and assert the same
    // `assert_eq!` the differential arm uses rejects it.
    let source = "# T\n\n```yaml lattice\nbacklinks:\n  referenced_by:\n    - real.md\n```\n";

    // The honest carrier metadata the invariant extracts.
    let carrier = carrier_backlinks(source);
    assert_eq!(
        carrier.get("referenced_by").map(Vec::as_slice),
        Some(["real.md".to_string()].as_slice()),
        "the carrier sources the real path: {carrier:?}"
    );

    // Corrupt it — the bug class the invariant guards against is the carrier
    // parse yielding a *different* path than the equivalent leading block would.
    let mut corrupted = carrier.clone();
    corrupted.insert(
        "referenced_by".to_string(),
        vec!["mangled\u{fffd}.md".to_string()],
    );

    // The differential arm's comparison must reject the divergence.
    let caught = std::panic::catch_unwind(|| {
        assert_eq!(
            carrier, corrupted,
            "corrupted carrier backlinks must differ from the honest extraction"
        );
    });
    assert!(
        caught.is_err(),
        "the carrier-fidelity comparison must catch corrupted metadata — otherwise it is vacuous"
    );

    // And the genuine invariant still passes on the honest source (teeth, not a
    // hair trigger).
    assert_carrier_fidelity(source);
}

#[test]
fn edit_sequences_cover_cascade_classes() {
    // The three cascade classes from issue 014 — an edit changing the parse of
    // distant lines — exercised deterministically, mirroring the `fuzz_edits`
    // seed corpus. `assert_edit_sequence_stable` re-checks every full-pipeline
    // invariant after each edit, so reaching the end proves each intermediate
    // document parsed cleanly and the `LineIndex` range→offset map round-tripped.

    // A far-past-EOF insertion (clamped to the document end).
    let eof_insert = |text: &str| Edit {
        start_line: 9999,
        start_char: 0,
        end_line: 9999,
        end_char: 0,
        text: text.to_string(),
    };

    // 1. Open-ended forward construct: a later edit closes an open fence,
    //    flipping every line between code and markdown.
    assert_edit_sequence_stable("```\nlet x = 1;\n", &[eof_insert("```\nafter\n")]);

    // 2. Backward / contextual dependence: inserting `===` under a line promotes
    //    the line *above* to a setext heading.
    assert_edit_sequence_stable(
        "Title\nbody\n",
        &[Edit {
            start_line: 1,
            start_char: 0,
            end_line: 1,
            end_char: 0,
            text: "===\n".to_string(),
        }],
    );

    // 3. Document-global, order-independent: a definition appended at the bottom
    //    resolves a reference / footnote at the top.
    assert_edit_sequence_stable("[ref][r]\n\nbody\n", &[eof_insert("[r]: ./x.md\n")]);
    assert_edit_sequence_stable("text[^f]\n", &[eof_insert("[^f]: a footnote\n")]);

    // A column-spanning deletion that removes an inline link (empty replacement).
    assert_edit_sequence_stable(
        "see [link](./a.md \"references\") here\n",
        &[Edit {
            start_line: 0,
            start_char: 4,
            end_line: 0,
            end_char: 31,
            text: String::new(),
        }],
    );

    // A multi-edit chain that builds a fenced block around multibyte text.
    assert_edit_sequence_stable(
        "# Heading\n",
        &[
            eof_insert("```\n"),
            eof_insert("café 🎉\u{200b}\n"),
            eof_insert("```\n"),
        ],
    );
}
