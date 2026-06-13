// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Shared parse invariants.
//!
//! These assertions define what a *correct* parse looks like, independent of
//! any particular input: a well-formed tree, well-formed frontmatter blocks,
//! in-bounds HTML-tag spans, content fidelity (resolved text faithful to the
//! source bytes), and LSP position round-tripping. They are the substance of
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

use std::path::Path;

use crate::block::{ElementKind, Syntax, Tree};
use crate::config::Config;
use crate::fm::{Exceptions, FmNode, FmValue, FrontmatterBlock, ScalarSpan};
use crate::html::HtmlTag;
use crate::line_index::LineIndex;
use crate::validation::Diagnostic;
use crate::workspace::parse_content;
use crate::{json, lsp, structural, toml, yaml};

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
/// I/O. `rel_path` is fixed to `fuzz.md` so resolution is reproducible across runs.
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
    let external_exists = |target: &Path| path_exists_oracle(target);
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
