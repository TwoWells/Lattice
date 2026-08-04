// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Shared parse invariants.
//!
//! These assertions define what a *correct* parse looks like, independent of
//! any particular input: a well-formed tree, well-formed frontmatter blocks,
//! in-bounds HTML-tag spans, content fidelity (resolved text faithful to the
//! source bytes), LSP position round-tripping, and differential oracles that
//! recompute an internal result a simpler way and require the two to agree (the
//! bracket-match table, an edit sequence's reparse). They are the substance of
//! both hardening suites:
//!
//! - [`property_tests`](crate::property_tests) generates structured and random
//!   inputs and asserts these invariants hold.
//! - the `cargo-fuzz` targets under `fuzz/` feed coverage-guided mutations
//!   through the same assertions (via [`crate::fuzz_api`]).
//!
//! Keeping the checks here — rather than copied into each suite — is a
//! requirement of ticket 22: *the assertions are the product, the fuzzer is
//! just the input generator.* Ticket 21's mojibake and position bugs neither
//! panicked nor hung; only a content-fidelity / round-trip assertion catches
//! them. A single source means the two suites cannot drift.
//!
//! Every `assert_*` function panics with a descriptive message on violation.
//! Under `proptest` a panic is caught and shrunk to a counterexample; under
//! libFuzzer it is reported as a crash with the reproducing input.

#![allow(
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::too_many_lines,
    clippy::too_long_first_doc_paragraph,
    reason = "these are assertion helpers: panicking with a descriptive message on violation is their entire contract, the tree-wellformedness check is necessarily long, and each helper intentionally leads with a full explanatory paragraph describing the invariant it enforces"
)]

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use crate::block::{self, ElementKind, Syntax, Tree};
use crate::config::Config;
use crate::fm::{self, Exceptions, FmNode, FmValue, FrontmatterBlock, ScalarSpan};
use crate::html::HtmlTag;
use crate::line_index::LineIndex;
use crate::validation::Diagnostic;
use crate::workspace::{FileData, WorkspaceView, compute_structural, parse_content};
use crate::{inline, json, lsp, metadata, server, structural, toml, yaml};

// ---------------------------------------------------------------------------
// Full-pipeline helper
// ---------------------------------------------------------------------------

/// Detect frontmatter using the same precedence as the workspace loader:
/// YAML (`---`), then TOML (`+++`), then JSON (`{`). Returns the parsed block
/// (if any) and the syntax that matched (defaulting to `Yaml` when none does).
#[must_use]
pub fn detect_frontmatter(source: &str) -> (Option<FrontmatterBlock>, Syntax) {
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
// Tree well-formedness
// ---------------------------------------------------------------------------

/// Assert every universal structural invariant on a parsed [`Tree`]:
/// exactly one `Document` root at index 0, every span ordered, in bounds, and
/// on UTF-8 char boundaries, every child contained in its parent, every
/// ancestor chain acyclic and terminating at the root, and every diagnostic
/// span in bounds.
pub fn assert_tree_wellformed(tree: &Tree) {
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

// ---------------------------------------------------------------------------
// Frontmatter well-formedness and content fidelity
// ---------------------------------------------------------------------------

/// Assert structural invariants on a parsed frontmatter block: the block span
/// and content span are ordered, in bounds, and on UTF-8 char boundaries, and
/// every diagnostic span is in bounds.
pub fn assert_block_wellformed(block: &FrontmatterBlock, source: &str) {
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
    for diag in &block.diagnostics {
        assert!(
            diag.span.start <= diag.span.end && diag.span.end <= len,
            "frontmatter diagnostic span {:?} out of bounds for source length {len}",
            diag.span
        );
    }
}

/// Assert content fidelity for every resolved frontmatter scalar: a scalar
/// whose source slice is escape-free and single-line must contain its resolved
/// `text` verbatim. This catches byte-as-`char` decoding that mangles
/// multi-byte keys/values into Latin-1 mojibake (the ticket-21 bug class).
pub fn assert_frontmatter_scalar_fidelity(block: &FrontmatterBlock, source: &str) {
    for sc in collect_scalars(block) {
        assert!(
            sc.span.end <= source.len()
                && source.is_char_boundary(sc.span.start)
                && source.is_char_boundary(sc.span.end),
            "scalar span {:?} out of bounds / off a char boundary (len {})",
            sc.span,
            source.len()
        );
        let sliced = &source[sc.span.start..sc.span.end];
        // Backslash escapes (double-quoted YAML, TOML basic strings, JSON) and
        // folded multi-line scalars are decoded in too many ways to reconstruct
        // here; skip them.
        if sliced.contains('\\') || sliced.contains('\n') || sliced.contains('\r') {
            continue;
        }
        // A plain scalar is sliced verbatim, so its text occurs in the raw
        // slice. A YAML single-quoted scalar decodes `''` to one `'`, so its
        // text occurs in the slice with `''` collapsed. Accept either form —
        // the comparison stays *exact* (not skipped), so a mojibake'd multi-byte
        // char elsewhere in the scalar satisfies neither and is still caught.
        let occurs = sliced.contains(sc.text.as_str())
            || (sliced.contains("''") && sliced.replace("''", "'").contains(sc.text.as_str()));
        assert!(
            occurs,
            "resolved scalar text {:?} does not occur in its source slice {:?} \
             — encoding corruption",
            sc.text, sliced
        );
    }
}

/// Collect every scalar (mapping keys and scalar values, recursively) in a
/// parsed frontmatter block — the leaves whose resolved `text` must stay
/// faithful to the source bytes.
#[must_use]
pub fn collect_scalars(block: &FrontmatterBlock) -> Vec<&ScalarSpan> {
    let mut out = Vec::new();
    for entry in &block.entries {
        collect_node_scalars(entry, &mut out);
    }
    out
}

fn collect_node_scalars<'a>(node: &'a FmNode, out: &mut Vec<&'a ScalarSpan>) {
    match node {
        FmNode::Mapping { key, value, .. } => {
            out.push(key);
            collect_value_scalars(value, out);
        }
        FmNode::SequenceItem { value, .. } => collect_value_scalars(value, out),
    }
}

fn collect_value_scalars<'a>(value: &'a FmValue, out: &mut Vec<&'a ScalarSpan>) {
    match value {
        FmValue::Scalar(s) => out.push(s),
        FmValue::Sequence(items) | FmValue::Mapping(items) => {
            for item in items {
                collect_node_scalars(item, out);
            }
        }
        FmValue::FlowSequence { items, .. } => out.extend(items.iter()),
        FmValue::FlowMapping { entries, .. } => {
            for (k, v) in entries {
                out.push(k);
                out.push(v);
            }
        }
        FmValue::BlockScalar { .. } => {}
    }
}

// ---------------------------------------------------------------------------
// Inline resource fidelity
// ---------------------------------------------------------------------------

/// Assert content fidelity for every resolved inline resource field: each
/// Link/Image/Video/Audio `url` and `title` that is non-empty, escape-free,
/// and single-line must occur verbatim in the source. The parsers slice these
/// fields rather than decode them, so a byte-as-`char` regression anywhere in
/// the inline or HTML-attribute path would make the field absent.
pub fn assert_inline_resource_fidelity(tree: &Tree) {
    let source = tree.source();
    for node in tree.nodes() {
        let (ElementKind::Link { url, title }
        | ElementKind::Image { url, title }
        | ElementKind::Video { url, title }
        | ElementKind::Audio { url, title }) = &node.kind
        else {
            continue;
        };
        for field in [url, title] {
            // Empty, escaped, or multi-line fields legitimately differ from any
            // single source slice; skip them.
            if field.is_empty() || field.contains(['\\', '\n', '\r']) {
                continue;
            }
            // Email autolinks (`<user@host>`) synthesize a `mailto:` scheme that
            // is not present in the source; the address after it is sliced
            // verbatim. Strip the synthesized prefix before the check.
            let needle = field.strip_prefix("mailto:").unwrap_or(field);
            assert!(
                source.contains(needle),
                "resolved inline field {field:?} (as {needle:?}) does not occur in the source \
                 — encoding corruption"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Emphasis-run span fidelity (ticket 26)
// ---------------------------------------------------------------------------

/// Whether the leftmost delimiter of a slice's trailing delimiter run (of
/// length `raw_trail` characters) is backslash-escaped, and so is content
/// rather than a closing delimiter.
///
/// The inline scanner skips `\x` for any ASCII-punctuation `x`, so an escaped
/// tilde sitting immediately left of a strikethrough closer — the `\~` in a run
/// like `~a\~~` — is part of the content even though it is the same character as
/// the delimiter. A run of *k* backslashes escapes the character that follows it
/// iff *k* is odd (each `\\` pair is one escaped backslash), so the trailing
/// delimiter run is one shorter than its raw character count exactly when an odd
/// number of backslashes precedes it. Only the run's *leftmost* delimiter can be
/// escaped — every other delimiter in the run is preceded by a delimiter, not a
/// backslash — so the correction is at most one.
///
/// The delimiters (`*`, `_`, `~`) and the backslash are all ASCII, so a trailing
/// run measured in characters is the same length in bytes and starts on a char
/// boundary; counting backslash *bytes* before it cannot split a multi-byte
/// scalar.
fn trailing_delim_is_escaped(slice: &str, raw_trail: usize) -> bool {
    let bytes = slice.as_bytes();
    let run_start = bytes.len() - raw_trail;
    let backslashes = bytes[..run_start]
        .iter()
        .rev()
        .take_while(|&&b| b == b'\\')
        .count();
    backslashes % 2 == 1
}

/// Assert span fidelity for every emphasis / strong / strikethrough run.
///
/// Flanking is a classic source of off-by-one span bugs, and these runs carry
/// no resolved text field for [`assert_inline_resource_fidelity`] to check — the
/// span *is* the data. For each [`Strong`], [`Emphasis`], or [`Strikethrough`]
/// node this asserts the source slice is delimited by the expected marker at
/// both ends, with the correct opening/closing run lengths (`**`/`__` and `~~`
/// take two, `*`/`_` and single `~` take one), and that the inner content is
/// non-empty. A drifted span — one short of the closing delimiter, or one byte
/// into the content — fails the boundary check rather than slicing silently
/// wrong styling data.
///
/// [`Strong`]: ElementKind::Strong
/// [`Emphasis`]: ElementKind::Emphasis
/// [`Strikethrough`]: ElementKind::Strikethrough
pub fn assert_emphasis_span_fidelity(tree: &Tree) {
    let source = tree.source();
    for node in tree.nodes() {
        // The minimum number of delimiter characters the kind carries at each
        // edge. Strong takes two; emphasis takes one; a strikethrough run is one
        // or two tildes (its exact length is read from the slice and checked for
        // symmetry below, so its lower bound is one).
        let open_len = match node.kind {
            ElementKind::Strong => 2,
            ElementKind::Emphasis | ElementKind::Strikethrough => 1,
            _ => continue,
        };
        let slice = &source[node.span.start..node.span.end];
        // The first character is the delimiter that opens the run. `*` and `_`
        // are interchangeable across the emphasis family; `~` is strikethrough.
        let delim = slice.chars().next().unwrap_or(' ');
        let expected_family = matches!(node.kind, ElementKind::Strikethrough);
        let is_strike_delim = delim == '~';
        let is_emphasis_delim = delim == '*' || delim == '_';
        assert!(
            (expected_family && is_strike_delim) || (!expected_family && is_emphasis_delim),
            "emphasis run {slice:?} starts with {delim:?}, not a delimiter for {:?}",
            node.kind
        );
        // The leading and trailing delimiter runs. They must each carry *at
        // least* the kind's delimiter count: a nested run anchored at the same
        // source delimiter run (e.g. the outer `*` of `***foo***`) can place a
        // consumed inner delimiter immediately after the opener, so the edge
        // count is `>= open_len`, not exactly it. The matched delimiters
        // themselves are the outermost ones, so the closing edge mirrors it.
        let lead = slice.chars().take_while(|&c| c == delim).count();
        let trail = slice.chars().rev().take_while(|&c| c == delim).count();
        assert!(
            lead >= open_len && trail >= open_len,
            "emphasis run {slice:?} is not delimited by at least {open_len} {delim:?} at each \
             edge for {:?}",
            node.kind
        );
        // A strikethrough run pairs equal-length openers and closers (one or two
        // tildes), so the edges are symmetric and bounded. The closing edge is
        // measured with escapes honored: a backslash-escaped tilde immediately
        // left of the closing run is content, not a delimiter — the inline
        // scanner skips `\~` — so it must not inflate the closer count even
        // though it is the same character (the `~a\~~` class). The opener never
        // has this problem: a run's span starts at a real delimiter, never an
        // escaped one.
        if is_strike_delim {
            let close_run = trail - usize::from(trailing_delim_is_escaped(slice, trail));
            assert!(
                lead == close_run && (1..=2).contains(&lead),
                "strikethrough run {slice:?} edges {lead}/{close_run} (raw trail {trail}) are \
                 not a symmetric 1- or 2-tilde pair"
            );
        }
        // The matched delimiters bound non-empty content: stripping `open_len`
        // delimiters from each edge must leave at least one character (delimiters
        // are ASCII, so each is one byte).
        assert!(
            slice.len() > 2 * open_len,
            "emphasis run {slice:?} has no content between its delimiters for {:?}",
            node.kind
        );
    }
}

// ---------------------------------------------------------------------------
// Bracket-match differential oracle (issue 056)
// ---------------------------------------------------------------------------

/// The role a byte plays in the naive bracket-matching reference walk.
///
/// `Inert` covers everything that is not a live bracket: ordinary text, the
/// backslash of an escape and the character it escapes, and every byte of a span
/// the walk steps over whole — a closed backtick code span, an inline math span,
/// an autolink, a raw HTML tag — brackets inside them included.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BracketRole {
    /// A `[` the walk reached: an opener.
    Open,
    /// A `]` the walk reached: a closer.
    Close,
    /// Not a live bracket.
    Inert,
}

/// Length of the run of `byte` starting at `pos`.
///
/// The reference walk's own counter. Deliberately not
/// `crate::inline::count_char`: the whole point of the oracle is that the
/// reference shares no code with the table it checks, so a regression in the
/// production run-counting helper (the issue 017 class) shows up as a
/// disagreement rather than being cancelled out on both sides.
fn run_length(bytes: &[u8], pos: usize, byte: u8) -> usize {
    let mut len = 0;
    while pos + len < bytes.len() && bytes[pos + len] == byte {
        len += 1;
    }
    len
}

/// End (exclusive) of the code span opened by a run of `ticks` backticks at
/// `open`, or `None` when the run is never closed.
///
/// A code span closes on the next run of *exactly* `ticks` backticks; a run of a
/// different length is content and is skipped whole. A backslash inside a code
/// span is literal — it cannot escape the closing run — so this scan looks at
/// backticks only. Written from scratch for the same reason as [`run_length`].
fn reference_code_span_end(bytes: &[u8], open: usize, ticks: usize) -> Option<usize> {
    let mut i = open + ticks;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            let run = run_length(bytes, i, b'`');
            if run == ticks {
                return Some(i + run);
            }
            i += run;
        } else {
            i += 1;
        }
    }
    None
}

/// True for the four bytes the inline math rules treat as space.
///
/// The opening `$` may not be followed by one and the closing `$` may not be
/// preceded by one. ASCII form feed is deliberately absent: the rule names space,
/// tab, and the two line-ending bytes, which is narrower than "ASCII whitespace".
const fn is_math_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r')
}

/// End (exclusive) of the inline math span opened by the `$` at `open`, or `None`
/// when that `$` opens none.
///
/// GitHub's rules restated: the opening `$` must be followed by something that is
/// neither space nor a second `$`; the closing `$` must not be preceded by space;
/// and a backslash inside the span consumes the byte after it, so `\$` is content
/// and not the closer. A span with no closer swallows nothing. Written from
/// scratch for the same reason as [`run_length`].
fn reference_math_span_end(bytes: &[u8], open: usize) -> Option<usize> {
    match bytes.get(open + 1) {
        None => return None,
        Some(&next) if is_math_space(next) || next == b'$' => return None,
        Some(_) => {}
    }
    let mut i = open + 1;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            i += 2;
        } else if bytes[i] == b'$' && !is_math_space(bytes[i - 1]) {
            return Some(i + 1);
        } else {
            i += 1;
        }
    }
    None
}

/// End (exclusive) of the autolink opened by the `<` at `open`, or `None` when
/// that `<` opens none.
///
/// `CommonMark`'s two forms restated over the run up to the first `>`: the run may
/// hold no space, `<`, or line ending, and must be either `scheme:rest` — an ASCII
/// letter followed by ASCII alphanumerics, `+`, `.`, or `-` — or `local@domain`,
/// where the local part is ASCII alphanumeric or one of the address-literal
/// punctuation bytes and the domain is a dotted run of ASCII alphanumeric / `-`
/// labels, each non-empty and at most 63 bytes. Written from scratch for the same
/// reason as [`run_length`].
fn reference_autolink_end(bytes: &[u8], open: usize) -> Option<usize> {
    let close = open + 1 + bytes[open + 1..].iter().position(|&b| b == b'>')?;
    let inner = &bytes[open + 1..close];
    if inner
        .iter()
        .any(|&b| matches!(b, b' ' | b'<' | b'\n' | b'\r'))
    {
        return None;
    }
    let end = close + 1;

    if let Some(colon) = inner.iter().position(|&b| b == b':') {
        let scheme = &inner[..colon];
        if scheme.first().is_some_and(u8::is_ascii_alphabetic)
            && scheme
                .iter()
                .skip(1)
                .all(|&b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'.' | b'-'))
        {
            return Some(end);
        }
    }

    if let Some(at) = inner.iter().position(|&b| b == b'@') {
        let local = &inner[..at];
        let domain = &inner[at + 1..];
        let local_ok = !local.is_empty()
            && local
                .iter()
                .all(|&b| b.is_ascii_alphanumeric() || b".!#$%&'*+/=?^_`{|}~-".contains(&b));
        let domain_ok = !domain.is_empty()
            && domain.contains(&b'.')
            && domain.split(|&b| b == b'.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && label
                        .iter()
                        .all(|&b| b.is_ascii_alphanumeric() || b == b'-')
            });
        if local_ok && domain_ok {
            return Some(end);
        }
    }

    None
}

/// End (exclusive) of the raw HTML tag opened by the `<` at `open`, or `None`
/// when that `<` opens none.
///
/// The tag grammar restated, in the three shapes the inline scanner recognizes:
///
/// - `<!-- … -->`: everything through the first `-->` after the opener;
/// - `</name …>`: a name starting with an ASCII letter and continuing over ASCII
///   alphanumerics and `-`, then optional whitespace, then `>`;
/// - `<name …>` / `<name … />`: the same name rule, then attributes until `>` or
///   `/>`. An attribute is a name that stops at `=`, `>`, `/`, or whitespace —
///   empty is a parse failure — optionally followed by `=` and a value, which is
///   either quoted (running to the matching quote, `>` and line endings included)
///   or an unquoted run stopping at whitespace, `>`, or `/`.
///
/// Anything that runs off the end before its `>` is not a tag at all. Written
/// from scratch for the same reason as [`run_length`]: production's tokenizer is
/// the thing under test, so the reference may not call it.
fn reference_tag_end(bytes: &[u8], open: usize) -> Option<usize> {
    let rest = &bytes[open..];

    if rest.starts_with(b"<!--") {
        let body = &rest[4..];
        let close = body.windows(3).position(|w| w == b"-->")?;
        return Some(open + 4 + close + 3);
    }

    // The name of a close or open tag: an ASCII letter, then alphanumerics / `-`.
    let mut i = if rest.get(1) == Some(&b'/') { 2 } else { 1 };
    if !rest.get(i).is_some_and(u8::is_ascii_alphabetic) {
        return None;
    }
    let close_tag = i == 2;
    while i < rest.len() && (rest[i].is_ascii_alphanumeric() || rest[i] == b'-') {
        i += 1;
    }

    if close_tag {
        while i < rest.len() && rest[i].is_ascii_whitespace() {
            i += 1;
        }
        return (rest.get(i) == Some(&b'>')).then_some(open + i + 1);
    }

    loop {
        while i < rest.len() && rest[i].is_ascii_whitespace() {
            i += 1;
        }
        match rest.get(i) {
            None => return None,
            Some(&b'/') if rest.get(i + 1) == Some(&b'>') => return Some(open + i + 2),
            Some(&b'>') => return Some(open + i + 1),
            Some(_) => {}
        }

        // Attribute name.
        let name_start = i;
        while i < rest.len()
            && !matches!(rest[i], b'=' | b'>' | b'/')
            && !rest[i].is_ascii_whitespace()
        {
            i += 1;
        }
        if i == name_start {
            return None;
        }
        while i < rest.len() && rest[i].is_ascii_whitespace() {
            i += 1;
        }
        if rest.get(i) != Some(&b'=') {
            // Boolean attribute: no value follows.
            continue;
        }
        i += 1;
        while i < rest.len() && rest[i].is_ascii_whitespace() {
            i += 1;
        }

        // Attribute value.
        match rest.get(i) {
            None => return None,
            Some(&quote @ (b'"' | b'\'')) => {
                i += 1;
                while i < rest.len() && rest[i] != quote {
                    i += 1;
                }
                if i >= rest.len() {
                    return None;
                }
                i += 1;
            }
            Some(_) => {
                let value_start = i;
                while i < rest.len()
                    && !rest[i].is_ascii_whitespace()
                    && !matches!(rest[i], b'>' | b'/')
                {
                    i += 1;
                }
                if i == value_start {
                    return None;
                }
            }
        }
    }
}

/// Classify every byte of `bytes` as a live opener, a live closer, or inert.
///
/// A single left-to-right walk restating the skipping rules of the inline
/// parser's bracket pass in the simplest form that can express them:
///
/// - a backslash followed by any byte consumes both, so `\[` is literal text and
///   never an opener (a trailing lone backslash escapes nothing);
/// - a backtick run closed by a later run of the same length is a code span, and
///   every byte from the opener through the closer is inert — brackets included.
///   An unclosed run consumes only its own backticks, so brackets after it stay
///   live;
/// - the three spans the inline scanner treats as opaque are stepped over whole,
///   so a bracket inside one is inert: an inline math span
///   ([`reference_math_span_end`]), an autolink ([`reference_autolink_end`]), and
///   a raw HTML tag ([`reference_tag_end`]). A `<` is an autolink first and a tag
///   second, the order the scanner tries them in; a `<` that is neither is
///   ordinary text (issue 070).
///
/// The last three rules apply only within the first
/// [`MAX_INLINE_LINE_BYTES`](crate::limits::MAX_INLINE_LINE_BYTES) of a line,
/// because that is where the inline scanner stops recognizing constructs.
///
/// Every byte this walk inspects is ASCII, and no ASCII byte can appear inside a
/// multi-byte UTF-8 sequence, so classifying bytes rather than characters cannot
/// mistake a continuation byte for a bracket.
fn bracket_roles(bytes: &[u8]) -> Vec<BracketRole> {
    let mut roles = vec![BracketRole::Inert; bytes.len()];
    let mut i = 0;
    let mut line_start = 0;
    while i < bytes.len() {
        let recognize = i - line_start < crate::limits::MAX_INLINE_LINE_BYTES;
        match bytes[i] {
            b'\n' => {
                line_start = i + 1;
                i += 1;
            }
            b'\\' if i + 1 < bytes.len() => i += 2,
            b'`' => {
                let ticks = run_length(bytes, i, b'`');
                i = reference_code_span_end(bytes, i, ticks).unwrap_or(i + ticks);
            }
            b'$' if recognize => i = reference_math_span_end(bytes, i).unwrap_or(i + 1),
            b'<' if recognize => {
                i = reference_autolink_end(bytes, i)
                    .or_else(|| reference_tag_end(bytes, i))
                    .unwrap_or(i + 1);
            }
            b'[' => {
                roles[i] = BracketRole::Open;
                i += 1;
            }
            b']' => {
                roles[i] = BracketRole::Close;
                i += 1;
            }
            _ => i += 1,
        }
    }
    roles
}

/// The naive quadratic reference bracket-match table for `text`.
///
/// This is the loop the production precomputation replaced: for *every* live `[`,
/// rescan forward from scratch, counting nesting depth over the live brackets,
/// and take the `]` that brings the depth back to zero. Re-deriving each entry
/// independently is O(n²) — which is exactly why production stopped doing it —
/// but it is short enough to check by eye, and it shares no code with the
/// single-pass stack table it is compared against: which bytes are live is
/// [`bracket_roles`]'s own answer, from its own recognizers.
///
/// Exposed so the deterministic teeth test can build an honest table and corrupt
/// it, proving the comparison rejects a missing, spurious, or mismapped match.
#[must_use]
pub fn naive_bracket_matches(text: &str) -> Vec<Option<usize>> {
    let bytes = text.as_bytes();
    let roles = bracket_roles(bytes);
    let mut matches = vec![None; bytes.len()];
    for (open, role) in roles.iter().enumerate() {
        if *role != BracketRole::Open {
            continue;
        }
        let mut depth = 1usize;
        for (pos, later) in roles.iter().enumerate().skip(open + 1) {
            match later {
                BracketRole::Open => depth += 1,
                BracketRole::Close => {
                    depth -= 1;
                    if depth == 0 {
                        matches[open] = Some(pos);
                        break;
                    }
                }
                BracketRole::Inert => {}
            }
        }
    }
    matches
}

/// Assert two bracket-match tables over `text` are equal, entry by entry.
///
/// `table` is the implementation under test and `reference` the oracle's answer;
/// the message names the disagreeing byte position and both verdicts, so a
/// counterexample points straight at the bracket that moved.
///
/// Exposed so the deterministic teeth test can drive this exact comparison with a
/// deliberately corrupted table — otherwise a differential that never fires could
/// be vacuous and no one would know.
pub fn assert_bracket_tables_equal(
    text: &str,
    table: &[Option<usize>],
    reference: &[Option<usize>],
) {
    let bytes = text.as_bytes();
    assert_eq!(
        table.len(),
        reference.len(),
        "bracket-match table has {} entries for {} bytes; the reference has {} — the table must \
         carry one entry per byte\n  slice: {text:?}",
        table.len(),
        bytes.len(),
        reference.len()
    );
    for (pos, (got, want)) in table.iter().zip(reference.iter()).enumerate() {
        assert_eq!(
            got,
            want,
            "bracket-match table disagrees with the naive reference walk at byte {pos} \
             (byte {:?}): table says {got:?}, reference says {want:?} — a missing, spurious, or \
             mismapped bracket match\n  slice: {text:?}",
            bytes.get(pos).map(|&b| char::from(b))
        );
    }
}

/// The differential bracket-match oracle of issue 056, over one text slice.
///
/// The inline parser precomputes `[` → matching `]` for a whole inline host in one
/// skip-aware pass, so link parsing never rescans (a run like `[[[[...` used to be
/// quadratic). That table is internal, and the downstream fidelity invariants
/// cannot see it: they check the fields of the nodes that were emitted, so a
/// mismapped bracket that produces a *wrong* link is caught, but one that produces
/// a *missing* or *spurious* link is invisible — nothing asserts over a node that
/// was never created. A bracket mismapped across a code-span boundary by
/// miscounted backtick runs (the issue 017 undercount class) is exactly that:
/// silent wrong output.
///
/// This closes the gap by recomputing the table with [`naive_bracket_matches`] —
/// an independent quadratic walk that shares no code with the production pass —
/// and asserting entry-by-entry equality. Both directions are covered by
/// construction: a `Some` where the reference says `None` is a spurious match, a
/// `None` where the reference says `Some` is a missing one, and two different
/// `Some`s are a mismap.
///
/// The comparison covers the table's whole skip surface, so the invariant is
/// stated without exemptions: no skip list, no "unless", nothing to broaden. Every
/// rule the table applies — the `\` + any byte escape, backtick-run pairing, and
/// since issue 070 the three opaque spans the inline scanner steps over (math,
/// autolink, raw HTML tag) under the same per-line recognition cap — is re-derived
/// on the reference side by [`bracket_roles`] from its own recognizers, never by
/// calling production's. A regression in `try_parse_inline_math`,
/// `html::try_autolink`, or `html::tokenize_tag` therefore surfaces here as a
/// disagreement instead of cancelling out on both sides.
///
/// What this still does not pin is the handful of regions the scanner enters only
/// *after* recognizing a construct — the destination of a link it just matched,
/// the body of an `<a>` element up to its `</a>`, an `@path.md` import directive —
/// which the table does not model either, and so neither side of this comparison
/// does.
pub fn assert_bracket_table_agrees(text: &str) {
    let table = inline::precompute_bracket_matches(text);
    let reference = naive_bracket_matches(text);
    assert_bracket_tables_equal(text, &table, &reference);
}

/// Assert the bracket-match table agrees with the naive reference walk over every
/// byte slice the inline pass builds one for, plus the whole document.
///
/// The inline scanner builds a table per inline host — every `Paragraph`,
/// `Heading`, and `TableCell` span — so those slices are the inputs production
/// actually feeds it, and each is checked here on its own. The whole source is
/// checked too: the table function is pure over any byte slice, and the extra arm
/// keeps the oracle live for documents with no inline host at all (everything
/// inside a fence, say) where the per-host loop would otherwise check nothing.
///
/// Host spans are assumed sliceable — [`assert_tree_wellformed`] is the invariant
/// that guarantees it, and both suites assert it on the same tree first.
pub fn assert_bracket_table_fidelity(tree: &Tree) {
    let source = tree.source();
    for node in tree.nodes() {
        if matches!(
            node.kind,
            ElementKind::Paragraph | ElementKind::Heading { .. } | ElementKind::TableCell
        ) {
            assert_bracket_table_agrees(&source[node.span.start..node.span.end]);
        }
    }
    assert_bracket_table_agrees(source);
}

// ---------------------------------------------------------------------------
// HTML tag bounds
// ---------------------------------------------------------------------------

/// Assert a tokenized HTML tag reports lengths and spans within `text`.
pub fn assert_html_tag_in_bounds(tag: &HtmlTag, text: &str) {
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
// LSP position round-trip
// ---------------------------------------------------------------------------

/// Assert `byte → LSP position → byte` is the identity for every char-boundary
/// offset in `source`, excluding offsets strictly inside a `\r\n` pair (the one
/// degenerate point that is not a stable round-trip target). Exercises the same
/// line/column machinery the LSP server uses to map diagnostic spans, against
/// any line-ending style and multi-byte content.
pub fn assert_position_round_trip(source: &str) {
    let bytes = source.as_bytes();
    for off in 0..=source.len() {
        if !source.is_char_boundary(off) {
            continue;
        }
        // Skip the one degenerate case: an offset strictly inside a `\r\n`
        // pair, which is not a stable round-trip point.
        if off > 0 && bytes[off - 1] == b'\r' && bytes.get(off) == Some(&b'\n') {
            continue;
        }
        let pos = crate::server::byte_offset_to_lsp_position(source, off);
        let back = crate::server::lsp_position_to_byte_offset(source, pos);
        assert_eq!(
            back, off,
            "byte → LSP position → byte must round-trip at offset {off} \
             (position {pos:?} mapped back to {back})"
        );
    }
}

/// Assert the cached [`LineIndex`] is a byte-for-byte drop-in for the scalar
/// position conversions over `source`. For every char-boundary offset: the
/// index's forward conversion equals [`crate::server::byte_offset_to_lsp_position`]
/// (so routing diagnostic materialization through the index cannot move a
/// position), and `offset → position → offset` round-trips through the index
/// itself — excluding the one `\r\n`-interior point that is not a stable
/// round-trip target. Exercises the same line/column machinery the server uses,
/// across every line-ending style and multi-byte content; `index` must have been
/// built from `source`.
pub fn assert_line_index_agrees(source: &str, index: &LineIndex) {
    let bytes = source.as_bytes();
    for off in 0..=source.len() {
        if !source.is_char_boundary(off) {
            continue;
        }
        let scalar = crate::server::byte_offset_to_lsp_position(source, off);
        let indexed = index.position(source, off);
        assert_eq!(
            indexed, scalar,
            "LineIndex position {indexed:?} disagrees with the scalar conversion \
             {scalar:?} at offset {off}"
        );
        // Skip the degenerate offset strictly inside a `\r\n` pair: like the
        // scalar round-trip, it is not a stable round-trip target.
        if off > 0 && bytes[off - 1] == b'\r' && bytes.get(off) == Some(&b'\n') {
            continue;
        }
        let back = index.offset(source, indexed);
        assert_eq!(
            back, off,
            "LineIndex offset → position → offset must round-trip at {off} \
             (position {indexed:?} mapped back to {back})"
        );
    }
}

// ---------------------------------------------------------------------------
// Differential edit-sequence oracle (perf ticket 03)
// ---------------------------------------------------------------------------

/// One `{range, text}` content edit, in the shape of an LSP incremental
/// `textDocument/didChange` change: a half-open LSP range in the *current*
/// document's coordinates, and the text that replaces it. This is the exact unit
/// the incremental text-sync path of issue 014 / ticket perf 05 will consume.
///
/// The coordinates are stored as plain `u32`s rather than an `lsp::Range` so the
/// type carries no protocol internals; [`apply_lsp_edit`] assembles the positions
/// and maps them to byte offsets through the cached [`LineIndex`].
#[derive(Debug, Clone)]
pub struct Edit {
    /// 0-based start line.
    pub start_line: u32,
    /// 0-based start character (UTF-16 code units within the line).
    pub start_char: u32,
    /// 0-based end line.
    pub end_line: u32,
    /// 0-based end character (UTF-16 code units within the line).
    pub end_char: u32,
    /// Replacement text spliced in place of the range.
    pub text: String,
}

/// Apply one `{range, text}` edit to `source`, returning the edited document.
///
/// Both range endpoints are mapped to byte offsets through `index` — the same
/// [`LineIndex::offset`] primitive the incremental text-sync path will use to
/// turn an incoming range into byte offsets, so this exercises ticket perf 01's
/// inverse direction across arbitrary inputs. `LineIndex::offset` clamps each
/// position to an in-bounds char boundary, and the endpoints are ordered so
/// `lo <= hi`; the splice is therefore always in-bounds and on char boundaries
/// regardless of where the edit came from. `index` must have been built from
/// `source`.
#[must_use]
pub fn apply_lsp_edit(source: &str, index: &LineIndex, edit: &Edit) -> String {
    let a = index.offset(
        source,
        lsp::Position {
            line: edit.start_line,
            character: edit.start_char,
        },
    );
    let b = index.offset(
        source,
        lsp::Position {
            line: edit.end_line,
            character: edit.end_char,
        },
    );
    let lo = a.min(b);
    let hi = a.max(b);
    let mut edited = String::with_capacity(source.len() + edit.text.len());
    edited.push_str(&source[..lo]);
    edited.push_str(&edit.text);
    edited.push_str(&source[hi..]);
    edited
}

/// Assert every full-pipeline parse invariant on a single document.
///
/// Parses `source` exactly as the workspace loader does ([`parse_content`]) and
/// asserts the tree is well-formed, inline resources are faithful, the LSP
/// byte↔position round-trip holds, the cached [`LineIndex`] agrees byte-for-byte
/// with the scalar conversion, and — when frontmatter is present — the
/// frontmatter block is well-formed and its scalars are faithful. This is the
/// same bar [`crate::fuzz_api`]'s `fuzz_full` target asserts, bundled here so the
/// edit-sequence oracle re-checks an identical set after every edit.
pub fn assert_document_invariants(source: &str) {
    let file = parse_content(source, Path::new("oracle.md"), &Config::default());
    assert_tree_wellformed(&file.tree);
    assert_inline_resource_fidelity(&file.tree);
    assert_emphasis_span_fidelity(&file.tree);
    assert_position_round_trip(source);
    assert_line_index_agrees(source, &file.line_index);
    if let (Some(block), _) = detect_frontmatter(source) {
        assert_block_wellformed(&block, source);
        assert_frontmatter_scalar_fidelity(&block, source);
    }
}

/// The differential parse/diagnostic oracle of perf ticket 03.
///
/// Applies `edits` to `base` one at a time, re-parsing from scratch and asserting
/// [`assert_document_invariants`] after each step. Two things fall out of this:
/// a random edit sequence becomes a strong parser-stability net over documents
/// the static generators never assemble, and every step routes its range through
/// the [`LineIndex`] inverse exactly as incremental text-sync will — so ticket
/// perf 01's reverse lookup is exercised end-to-end before any incremental code
/// exists.
///
/// This is the **full-reparse arm**. The oracle issue 014 needs —
/// `incremental(edits) ≡ full(final_text)`, the same tree, spans, and diagnostics
/// — is the second arm: when an incremental parse/graph path lands (tickets perf
/// 04 / 05), drive the same `(base, edits)` through it and assert byte-for-byte
/// equality against the from-scratch reparse this function already pins. Both
/// arms share this entry point, so the `fuzz_edits` target and the property suite
/// that call it gain the equivalence check without changing shape, and the two
/// suites cannot drift (per `AGENTS.md`: the assertions are the product).
pub fn assert_edit_sequence_stable(base: &str, edits: &[Edit]) {
    assert_document_invariants(base);
    let mut text = base.to_string();
    for edit in edits {
        let index = LineIndex::new(&text);
        text = apply_lsp_edit(&text, &index, edit);
        assert_document_invariants(&text);
    }
}

// ---------------------------------------------------------------------------
// Metadata-carrier content fidelity (ticket 25, decision 015)
// ---------------------------------------------------------------------------

/// Whether a document carries a leading `---` / `+++` / `{` frontmatter block.
///
/// Mirrors the precedence in [`crate::workspace::parse_content`]: a leading
/// block is the primary carrier, and the `yaml lattice` carrier is consulted for
/// data *only* when no leading block matched. The carrier-fidelity invariant must
/// therefore look at the carrier exactly when production does — when this returns
/// `false`.
fn has_leading_frontmatter_block(source: &str) -> bool {
    yaml::parse_frontmatter_block(source).is_some()
        || toml::parse_frontmatter_block(source).is_some()
        || json::parse_frontmatter_block(source).is_some()
}

/// Build the parse tree the way [`crate::workspace::parse_content`] does for a
/// document with no leading frontmatter block: no frontmatter span, default
/// (`Yaml`) syntax, no pre-parsed entries. This is the exact tree the carrier
/// scanner runs against in production, so the carrier is reached top-level-only
/// per ticket 24.
fn carrier_tree(source: &str) -> Tree {
    block::parse_tree_with_entries(source, None, Syntax::Yaml, None)
}

/// Assert content fidelity for a document whose metadata comes from a
/// `yaml lattice` carrier (decision 015).
///
/// `fuzz_full`'s [`assert_frontmatter_scalar_fidelity`] only ever inspects the
/// *leading* `---` / `+++` / `{` block ([`detect_frontmatter`]) and skips the
/// carrier entirely — the content-fidelity blind spot ticket 25 closes. This
/// reaches the carrier the way [`crate::workspace::parse_content`] does
/// (top-level only, and only when no leading block is present), then asserts two
/// things about the metadata it sources:
///
/// 1. **Scalar fidelity.** The carrier's parsed [`FrontmatterBlock`] is
///    well-formed and every resolved scalar occurs verbatim in its document
///    source slice — the same bar a leading block must clear, now applied to the
///    carrier body. A byte-as-`char` regression in the carrier parse path would
///    mangle a multi-byte key/path here and is caught.
/// 2. **Differential `carrier ≡ leading block`.** Where feasible (see below), the
///    backlinks and exceptions extracted from the carrier equal those extracted
///    when the *same YAML body* is presented as a leading `---` block. This is the
///    strongest statement of the carrier-agnostic reconciliation validation 06
///    verified: it catches any carrier-specific parse drift — a divergence between
///    the `parse_yaml_body` path and the `parse_frontmatter_block` path that a
///    no-panic check is blind to.
///
/// The differential arm is **skipped** (the scalar-fidelity arm still runs) when
/// the carrier body cannot be losslessly re-expressed as a leading block — see
/// [`equivalent_leading_block`], which *verifies* the wrap round-trips instead of
/// predicting it. Skipping a genuinely non-equivalent transform keeps the
/// invariant from firing on a *correct* parse — it is never broadened to
/// no-panic.
pub fn assert_carrier_fidelity(source: &str) {
    // Production consults the carrier for data only when there is no leading
    // block; mirror that, so the invariant inspects the carrier exactly when the
    // workspace loader would source metadata from it.
    if has_leading_frontmatter_block(source) {
        return;
    }
    let tree = carrier_tree(source);
    let Some(carrier_block) = metadata::parse_carrier_block(&tree) else {
        return;
    };

    // Arm 1: the carrier block is well-formed and every scalar is faithful — the
    // carrier body's spans point into `source`, so the leading-block helpers
    // apply unchanged.
    assert_block_wellformed(&carrier_block, source);
    assert_frontmatter_scalar_fidelity(&carrier_block, source);

    let carrier_backlinks = fm::extract_backlinks(&carrier_block, source);
    let carrier_exceptions = fm::extract_exceptions(&carrier_block, source);

    // Arm 2: differential `carrier ≡ equivalent leading block`. The carrier body
    // is exactly `content_span` (see `yaml::parse_yaml_body`).
    let body = &source[carrier_block.content_span.start..carrier_block.content_span.end];
    let Some((leading, leading_block)) = equivalent_leading_block(body) else {
        // The body cannot be losslessly wrapped as a leading `---` block; the
        // scalar-fidelity arm above still guarantees content fidelity.
        return;
    };
    let leading_backlinks = fm::extract_backlinks(&leading_block, &leading);
    let leading_exceptions = fm::extract_exceptions(&leading_block, &leading);

    assert_eq!(
        carrier_backlinks, leading_backlinks,
        "backlinks from a `yaml lattice` carrier must equal those from the same YAML as a \
         leading `---` block — carrier-specific parse drift\n  carrier body: {body:?}"
    );
    assert_exceptions_equivalent(&carrier_exceptions, &leading_exceptions, body);
}

/// Wrap a carrier body as an equivalent leading `---` YAML block and parse it,
/// or `None` when the wrap would not be lossless.
///
/// The synthetic document feeds the *same* body bytes between `---` delimiters,
/// so [`yaml::parse_frontmatter_block`] sees the identical YAML the carrier's
/// [`yaml::parse_yaml_body`] did. Returns `None` when the transform is unsound:
///
/// - the body opens with a UTF-8 BOM — [`yaml::parse_frontmatter_block`] strips a
///   leading BOM transparently, which `parse_yaml_body` does not, so the two
///   would parse a different first key; conservatively declined;
/// - the body does not end in a line ending — the closing `---` must start its
///   own line, so any wrap would have to *insert* a separator after the body, and
///   that inserted byte is absorbed by an unterminated trailing scalar (e.g. a
///   lone `'` at EOF) differently than the carrier's EOF does, so the wrap would
///   not be byte-equivalent; conservatively declined (issue 041);
/// - the wrapped document does not parse as a leading block at all, or the block
///   it *does* parse covers only part of the body — a `---` inside the body closed
///   the synthetic block early, so the leading parser saw a prefix, not the whole
///   carrier (issue 083).
///
/// That last condition is **verified, not predicted**: the wrap is accepted only
/// when the parsed block's `content_span` reproduces `body` byte for byte. An
/// earlier version predicted it by scanning `body.lines()` for a line equal to
/// `---`, which silently disagreed with the parser it was modelling —
/// [`yaml::parse_frontmatter_block`]'s `find_closing` counts a bare `\r` as a line
/// ending on *both* sides of the delimiter, while [`str::lines`] splits only on
/// `\n` (stripping at most a trailing `\r`). A CR-delimited `---` (`---\r`, or one
/// preceded by a bare `\r`) therefore slipped the guard, truncated the synthetic
/// block, and made the differential arm compare the full carrier body against a
/// prefix of itself. Asking the production parser what it actually consumed cannot
/// drift from it.
///
/// Declining a non-equivalent transform is deliberate, and stays narrow: the
/// differential arm must compare like with like, so only a body that genuinely
/// cannot round-trip is left to the scalar-fidelity arm rather than producing a
/// false counterexample. Every body whose wrap *does* round-trip is still compared
/// in full.
pub(crate) fn equivalent_leading_block(body: &str) -> Option<(String, FrontmatterBlock)> {
    // A leading BOM is stripped by `parse_frontmatter_block` but not by
    // `parse_yaml_body`, so the wrapped block and the carrier would disagree on
    // the first key. Decline conservatively.
    if body.starts_with('\u{feff}') {
        return None;
    }
    // The carrier body is the bytes between the open and close fence, so it always
    // ends in the line ending that precedes the closing fence — that terminator is
    // what puts the synthetic `---` at the start of its own line. Append the
    // closing delimiter directly after the body (no extra separator) so the
    // wrapped block's YAML content is byte-identical to the carrier body and both
    // parsers see the same input. A body that does *not* end in a line ending
    // cannot be wrapped without inserting a separator the carrier never had — an
    // unterminated trailing scalar would absorb it (issue 041) — so decline.
    if !body.ends_with(['\n', '\r']) {
        return None;
    }
    let leading = format!("---\n{body}---\n");
    let block = yaml::parse_frontmatter_block(&leading)?;
    // The wrap is lossless exactly when the leading parser consumed the whole
    // body as its YAML content. Anything shorter means a `---` inside the body
    // terminated the block early, so the two sides would not be comparing the
    // same YAML.
    if leading[block.content_span.start..block.content_span.end] != *body {
        return None;
    }
    Some((leading, block))
}

/// Assert two [`Exceptions`] blocks carry the same reconciled metadata.
///
/// [`Exceptions`] is not `PartialEq`, and only the *extracted* content is
/// load-bearing for the carrier-agnostic guarantee (the per-key source spans
/// differ by construction — the carrier body and the synthetic leading block sit
/// at different offsets). This compares the reference/reason pairs and the
/// count-key shape per namespace, which is exactly what reconciliation consumes.
fn assert_exceptions_equivalent(carrier: &Exceptions, leading: &Exceptions, body: &str) {
    /// The `(reference, reason)` pairs of an entry list, in source order.
    fn pairs(entries: &[fm::ExceptionEntry]) -> Vec<(&str, &str)> {
        entries
            .iter()
            .map(|e| (e.reference.as_str(), e.reason.as_str()))
            .collect()
    }
    // The `(expected, reason, raw)` of a count-key, if any. A closure (not a
    // `fn`) so this thin `Option::map` does not trip the single-option-map lint a
    // mapping `fn` would.
    let count = |key: Option<&fm::CountKey>| -> Option<(usize, String, String)> {
        key.map(|c| (c.expected, c.reason.clone(), c.raw.clone()))
    };

    assert_eq!(
        pairs(&carrier.stale_references),
        pairs(&leading.stale_references),
        "carrier and leading-block `stale_references` exceptions must match — carrier parse \
         drift\n  carrier body: {body:?}"
    );
    assert_eq!(
        pairs(&carrier.bare_paths),
        pairs(&leading.bare_paths),
        "carrier and leading-block `bare_paths` exceptions must match — carrier parse drift\n  \
         carrier body: {body:?}"
    );
    assert_eq!(
        count(carrier.stale_references_count.as_ref()),
        count(leading.stale_references_count.as_ref()),
        "carrier and leading-block `stale_references` count-keys must match — carrier parse \
         drift\n  carrier body: {body:?}"
    );
    assert_eq!(
        count(carrier.bare_paths_count.as_ref()),
        count(leading.bare_paths_count.as_ref()),
        "carrier and leading-block `bare_paths` count-keys must match — carrier parse drift\n  \
         carrier body: {body:?}"
    );
}

/// Extract the backlinks a `yaml lattice` carrier sources for `source`, mirroring
/// [`crate::workspace::parse_content`]'s carrier path (no leading block, tree-based
/// recognition). Returns an empty map when there is no live carrier. Exposed for
/// the deterministic teeth test that corrupts the extracted metadata and asserts
/// the differential arm catches the divergence.
#[must_use]
pub fn carrier_backlinks(source: &str) -> HashMap<String, Vec<String>> {
    if has_leading_frontmatter_block(source) {
        return HashMap::new();
    }
    let tree = carrier_tree(source);
    metadata::parse_carrier_block(&tree)
        .map(|block| fm::extract_backlinks(&block, source))
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Structural diagnostic pass (issue 033)
// ---------------------------------------------------------------------------

/// Run the structural diagnostic pass over `source`, exactly as the workspace
/// loader does after a file is (re)parsed and inserted.
///
/// Mirrors [`crate::workspace::Workspace::recompute_structural`]: it parses the
/// content (so the `exceptions` frontmatter block, the 030 external-resolution
/// and 031 exception-reconciliation paths are all exercised, not just the
/// quoted-path scanner), then calls [`structural::collect`] with **deterministic**
/// existence oracles so a given input always yields the same diagnostics.
///
/// Both oracles answer existence from the path's own bytes via [`path_exists_oracle`]
/// rather than the filesystem or workspace membership, so a fuzzed reference can
/// land on either branch of every existence-gated check — the "make it a link"
/// hint when present and the dangling / stale path when absent — without any
/// I/O. The external oracle folds in the third verdict,
/// [`structural::ExternalExistence::Unknown`] (a failed `stat`, issue 050),
/// via [`external_exists_oracle`], so the cannot-verify arm is fuzzed too.
/// `rel_path` is fixed to `fuzz.md` so resolution is reproducible across runs.
#[must_use]
pub fn collect_structural(source: &str) -> Vec<Diagnostic> {
    let rel_path = Path::new("fuzz.md");
    let config = Config::default();
    let file = parse_content(source, rel_path, &config);
    let empty_exceptions = Exceptions::default();
    let exceptions = file
        .frontmatter
        .as_ref()
        .map_or(&empty_exceptions, |fm| &fm.exceptions);
    let file_exists = |target: &Path| path_exists_oracle(target);
    let external_exists = |target: &Path| external_exists_oracle(target);
    structural::collect(
        &file.tree,
        rel_path,
        &config,
        &file_exists,
        &external_exists,
        exceptions,
    )
}

/// Deterministic existence oracle for the structural harness.
///
/// Answers "does this path exist" purely from the path's bytes — no filesystem,
/// no workspace state — so a run is reproducible. The parity of the byte sum
/// splits the path space roughly evenly, so a fuzzed reference reaches both the
/// present branch (`true`) and the absent branch (`false`) of every
/// existence-gated structural check.
#[must_use]
fn path_exists_oracle(path: &Path) -> bool {
    let sum: u32 = path
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .map(|&b| u32::from(b))
        .sum();
    sum.is_multiple_of(2)
}

/// Deterministic tri-state oracle for the external-alias arm (issue 050).
///
/// Like [`path_exists_oracle`], the verdict is a pure function of the path's
/// bytes. `sum % 4 == 3` answers [`structural::ExternalExistence::Unknown`] —
/// the failed-`stat` verdict — so the cannot-verify branch of `{Name}/…`
/// resolution is reachable by the fuzzer; the remaining paths keep the
/// even/odd present/absent split.
#[must_use]
fn external_exists_oracle(path: &Path) -> structural::ExternalExistence {
    let sum: u32 = path
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .map(|&b| u32::from(b))
        .sum();
    if sum % 4 == 3 {
        structural::ExternalExistence::Unknown
    } else if sum.is_multiple_of(2) {
        structural::ExternalExistence::Present
    } else {
        structural::ExternalExistence::Absent
    }
}

/// Assert every structural diagnostic carries a location the LSP layer can
/// materialize without panicking or producing a corrupt range.
///
/// For a diagnostic with a byte `span` (the precise-underline path through
/// [`crate::server`]'s `span_to_lsp_range`): the span must be ordered, within
/// `[0, source.len()]`, and on UTF-8 char boundaries at both ends — and each
/// endpoint must round-trip `byte → LSP position → byte` through the same
/// position machinery [`assert_position_round_trip`] checks, excluding the one
/// `\r\n`-interior offset that is not a stable round-trip point. This is the
/// invariant that catches the byte-index class of bug the issue 032 single-quote
/// guard is exposed to: an off-by-one or non-boundary offset into the source
/// would either fail to slice or map to the wrong column.
///
/// For a line-only diagnostic (`span: None`, the whole-line fallback): only the
/// 1-based `line` anchor is load-bearing — the materializer clamps a past-EOF
/// line to an empty range at end-of-source — so the assertion requires `line >= 1`
/// and nothing more, matching exactly what the fallback consumes.
pub fn assert_structural_diagnostics_valid(source: &str, diagnostics: &[Diagnostic]) {
    let len = source.len();
    let bytes = source.as_bytes();
    for diag in diagnostics {
        let Some(span) = diag.span else {
            assert!(
                diag.line >= 1,
                "line-only structural diagnostic must carry a 1-based line, found {} ({:?})",
                diag.line,
                diag.message
            );
            continue;
        };
        assert!(
            span.start <= span.end && span.end <= len,
            "structural diagnostic span {span:?} out of bounds for source length {len} ({:?})",
            diag.message
        );
        assert!(
            source.is_char_boundary(span.start),
            "structural diagnostic span start {} is not a UTF-8 char boundary ({:?})",
            span.start,
            diag.message
        );
        assert!(
            source.is_char_boundary(span.end),
            "structural diagnostic span end {} is not a UTF-8 char boundary ({:?})",
            span.end,
            diag.message
        );
        for off in [span.start, span.end] {
            // Skip the one degenerate offset strictly inside a `\r\n` pair: like
            // the tree/line-index round-trips, it is not a stable round-trip
            // target, and a span endpoint there is still a valid char boundary.
            if off > 0 && bytes[off - 1] == b'\r' && bytes.get(off) == Some(&b'\n') {
                continue;
            }
            let pos = crate::server::byte_offset_to_lsp_position(source, off);
            let back = crate::server::lsp_position_to_byte_offset(source, pos);
            assert_eq!(
                back, off,
                "structural diagnostic span endpoint {off} must round-trip \
                 byte → LSP position → byte (position {pos:?} mapped back to {back}) ({:?})",
                diag.message
            );
        }
    }
}

/// Run the structural pass over `source` and assert it never panics and that
/// every emitted diagnostic span is a valid, char-boundary, round-tripping byte
/// range (or, for a line-only diagnostic, a 1-based line). Bundled so the
/// `fuzz_structural` target and the property suite share one entry point and
/// cannot drift (per `AGENTS.md`: the assertions are the product).
pub fn assert_structural_invariants(source: &str) {
    let diagnostics = collect_structural(source);
    assert_structural_diagnostics_valid(source, &diagnostics);
}

// ---------------------------------------------------------------------------
// Buffer locality (decision 024, issue 067)
// ---------------------------------------------------------------------------

/// The synthetic root the buffer-locality workspace is anchored at.
///
/// Absolute (so link classification behaves as it does in production) and
/// deliberately not on disk: every existence answer the differential depends on
/// comes from workspace membership, not from the filesystem.
const LOCALITY_ROOT: &str = "/lattice-buffer-locality";

/// Build one synthetic document's parsed data plus its structural cache,
/// against a membership oracle.
fn locality_file(rel: &Path, text: &str, config: &Config, members: &[PathBuf]) -> FileData {
    let mut data = parse_content(text, &Path::new(LOCALITY_ROOT).join(rel), config);
    let file_exists = |target: &Path| members.iter().any(|m| m == target);
    let (structural, suppressions) = compute_structural(&data, rel, config, &file_exists);
    data.structural = structural;
    data.suppressions = suppressions;
    data
}

/// Compute every document's published diagnostic rows for a synthetic workspace
/// under the **buffer-locality perspective merge** (decision 024 clause 8),
/// optionally with one document carrying a diverged buffer.
///
/// `docs` is the saved world: `(root-relative path, disk text)` pairs. `overlay`
/// is `(root-relative path, buffer text)` for the single document the client is
/// holding a diverged buffer for, or `None` for the no-overlay run.
///
/// The merge itself is [`crate::server::merge_perspectives`] — the same
/// function the LSP publish pass calls — so this oracle cannot drift from
/// production. What it reproduces around that call is the store's two tiers:
///
/// - every document's structural cache is computed against the **saved**
///   membership, exactly as [`crate::server`] recomputes them per tier;
/// - the overlay document's cache additionally counts itself, because a
///   document is always a member of its own perspective;
/// - the saved view holds only saved copies, and the perspective view is that
///   same map with the overlay swapped in for its one document.
#[must_use]
pub fn buffer_locality_rows(
    docs: &[(&str, &str)],
    overlay: Option<(&str, &str)>,
) -> BTreeMap<PathBuf, Vec<Diagnostic>> {
    let root = PathBuf::from(LOCALITY_ROOT);
    let config = Config::default();
    let members: Vec<PathBuf> = docs.iter().map(|(rel, _)| PathBuf::from(rel)).collect();

    let saved: Vec<(PathBuf, FileData)> = docs
        .iter()
        .map(|(rel, text)| {
            let rel = PathBuf::from(rel);
            let data = locality_file(&rel, text, &config, &members);
            (rel, data)
        })
        .collect();

    // The overlay copy sees the saved membership plus itself: a buffer-only
    // document lints itself and is invisible to every other document until the
    // first save (decision 024's notes).
    let overlaid = overlay.map(|(rel, text)| {
        let rel = PathBuf::from(rel);
        let mut members = members.clone();
        if !members.contains(&rel) {
            members.push(rel.clone());
        }
        let data = locality_file(&rel, text, &config, &members);
        (rel, data)
    });

    let saved_files: BTreeMap<PathBuf, &FileData> = saved
        .iter()
        .map(|(rel, data)| (rel.clone(), data))
        .collect();
    let saved_view =
        WorkspaceView::new(root.clone(), &config, true, saved_files.clone(), Vec::new());
    let saved_live = server::collect_all_diagnostics(&saved_view);

    let perspectives: Vec<(PathBuf, WorkspaceView<'_>)> = overlaid
        .iter()
        .map(|(rel, data)| {
            let mut files = saved_files.clone();
            files.insert(rel.clone(), data);
            (
                rel.clone(),
                WorkspaceView::new(root.clone(), &config, true, files, Vec::new()),
            )
        })
        .collect();

    server::merge_perspectives(saved_live, &perspectives)
}

/// **The buffer-locality differential** (decision 024's enforcement clause).
///
/// > For any workspace and any buffer text overlaid on any single document S,
/// > every file other than S produces a **byte-identical** diagnostic vector to
/// > the run with no overlay.
///
/// This is the purity property the whole two-tier store exists to deliver, and
/// it is the one place a regression would otherwise hide silently: every
/// individual collector would keep working, and only the *merge* would have
/// started letting one document's unsaved draft into another document's
/// verdict. A cross-file check reading the overlaid document — fragment
/// resolution against its headings, the reciprocal-link escape hatch reading
/// its links, backlink reconciliation reading its frontmatter — is exactly the
/// shape that leak takes, and every one of them is exercised here.
///
/// `focus` names the overlaid document; it need not be present in `docs` (a
/// buffer opened on a path absent from disk is a member of its own perspective
/// only).
///
/// # Panics
///
/// Panics with the offending file, and both diagnostic vectors, on violation.
pub fn assert_buffer_locality(docs: &[(&str, &str)], focus: &str, buffer: &str) {
    let base = buffer_locality_rows(docs, None);
    let overlaid = buffer_locality_rows(docs, Some((focus, buffer)));
    let focus_rel = PathBuf::from(focus);

    for (rel, _) in docs {
        let rel = PathBuf::from(rel);
        if rel == focus_rel {
            continue;
        }
        let before = base.get(&rel).map_or(&[][..], Vec::as_slice);
        let after = overlaid.get(&rel).map_or(&[][..], Vec::as_slice);
        assert_eq!(
            before,
            after,
            "buffer locality violated: overlaying a buffer on {} moved {}'s diagnostic vector.\n\
             A document's rows may read its own current text and everyone else's LAST SAVED \
             state, and nobody else's buffer (decision 024).\n  buffer: {buffer:?}",
            focus_rel.display(),
            rel.display(),
        );
    }

    // The complement: no document outside the workspace may acquire rows from
    // the merge either. Only the focus may appear beyond the saved membership.
    for rel in overlaid.keys() {
        assert!(
            rel == &focus_rel
                || docs.iter().any(|(d, _)| Path::new(d) == rel)
                || overlaid[rel].is_empty(),
            "the perspective merge invented rows for {}, which is neither the overlaid document \
             nor a member of the saved world",
            rel.display()
        );
    }
}
