// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Metadata-channel carrier recognition (decision 015).
//!
//! A document may carry its machine metadata — materialized backlinks
//! (decision 001) and reconciled exceptions (decisions 011/012) — in a fenced
//! `yaml lattice` code block instead of a leading `---` frontmatter block, for
//! a render that stays clean on github.com. The fence is optionally wrapped in
//! a `<details>` element for a collapsed disclosure render.
//!
//! Recognition is **tree-based, never regex**: the carrier is a real
//! [`CodeBlock`](crate::block::ElementKind::CodeBlock) node whose info string is
//! `yaml lattice`. A `yaml lattice` fence shown as a documentation example
//! inside an *outer* code fence is a single opaque code-block node — its own
//! info string is the outer language, not `yaml lattice`, so it is inert,
//! exactly as bare-path detection already treats fenced code.
//!
//! This module owns two consumers of one scan:
//!
//! - [`parse_carrier_block`] feeds the recognized fence body through the YAML
//!   parser into a [`FrontmatterBlock`], so `backlinks` and `exceptions`
//!   populate `FileData.frontmatter` exactly as a `---` block would.
//! - [`carrier_diagnostics`] emits the carrier's structural diagnostics: the
//!   `<details>` render gotchas (missing blank lines), malformed inner YAML,
//!   and the more-than-one-carrier warning. It is called from
//!   [`crate::structural::collect`].

use crate::block::{ElementKind, NodeId, Syntax, Tree};
use crate::fm::{FmSeverity, FrontmatterBlock};
use crate::span::Span;
use crate::validation::{Diagnostic, Severity};
use crate::yaml;
use std::path::Path;

/// The info string that marks a fenced code block as a metadata carrier.
///
/// `yaml` is the highlighted language (GitHub highlights on the first info
/// word); `lattice` is the discriminator against an incidental `yaml` example
/// (decision 015). The match is exact — these two whitespace-separated tokens
/// and nothing else.
const CARRIER_LANG: &str = "yaml";
/// The discriminator token following the language in a carrier info string.
const CARRIER_DISCRIMINATOR: &str = "lattice";

/// A recognized metadata carrier: the fence node and its YAML body.
#[derive(Debug, Clone, Copy)]
pub struct Carrier {
    /// Byte span of the whole fenced code-block node (open fence through close
    /// fence), for anchoring the duplicate-carrier diagnostic.
    pub node_span: Span,
    /// Byte span of the YAML body between the fence delimiters — the slice fed
    /// to the YAML parser, with offsets relative to the document.
    pub body_span: Span,
}

/// Whether a fence info string marks a metadata carrier (`yaml lattice`).
///
/// Exactly the language token `yaml` followed by the discriminator `lattice`,
/// with arbitrary surrounding/interior whitespace and nothing else. An
/// incidental `yaml` example (no discriminator) and any other language are not
/// carriers.
fn is_carrier_info(info: &str) -> bool {
    let mut tokens = info.split_whitespace();
    tokens.next() == Some(CARRIER_LANG)
        && tokens.next() == Some(CARRIER_DISCRIMINATOR)
        && tokens.next().is_none()
}

/// Whether a fence info string is a *near-miss* carrier: the language token
/// `yaml` followed by the discriminator `lattice`, but carrying one or more
/// extra tokens (`yaml lattice title="x"`, `yaml lattice lines`).
///
/// Such a block reads to an author as a metadata carrier, but the info string
/// is not exactly `yaml lattice`, so [`is_carrier_info`] rejects it and its
/// metadata is silently dropped. At a carrier site this earns a near-miss
/// warning (decision 015: hint when intent is high but the form is invalid);
/// nested/embedded near-misses stay inert and silent. Mutually exclusive with
/// [`is_carrier_info`] — an exact match has no extra token.
fn is_carrier_near_miss(info: &str) -> bool {
    let mut tokens = info.split_whitespace();
    tokens.next() == Some(CARRIER_LANG)
        && tokens.next() == Some(CARRIER_DISCRIMINATOR)
        && tokens.next().is_some()
}

/// Whether a node sits at a *carrier site* — the one position where a
/// `yaml lattice` fence is the document's own live metadata (decision 015).
///
/// A carrier site is a node whose parent is the document root
/// ([`ElementKind::Document`], always index 0), or whose parent is an
/// [`ElementKind::Details`] node that is itself a direct child of the document
/// root. Any other ancestor — `QuoteBlock`, `List` / `ListItem`,
/// `Table` / `TableRow` / `TableCell`, generic `Container`, `Admonition`, a
/// nested `<details>`, etc. — marks the fence as *quoted or embedded* content,
/// inert and silent, exactly like a documented example inside an outer fence.
///
/// The test is purely structural (parent / child links), never a text scan.
fn is_carrier_site(tree: &Tree, node_id: NodeId) -> bool {
    let Some(parent_id) = tree.node(node_id).parent else {
        // Only the `Document` root has no parent; it is never itself a fence.
        return false;
    };
    let parent = tree.node(parent_id);
    match parent.kind {
        // Directly at the document top level.
        ElementKind::Document => true,
        // Inside a `<details>` that is itself at the document top level. A
        // `<details>` nested deeper (e.g. in a blockquote) is not a carrier
        // site, so its parent must be the document root in turn.
        ElementKind::Details => parent.parent.is_some_and(|grandparent_id| {
            matches!(tree.node(grandparent_id).kind, ElementKind::Document)
        }),
        _ => false,
    }
}

/// Extract the info string of a fenced code block from its raw text, or `None`
/// if the block is not a backtick/tilde fence (e.g. an indented code block).
fn fence_info(raw: &str) -> Option<&str> {
    let first_line = raw.lines().next()?;
    let trimmed = first_line.trim_start_matches(' ');
    let fence_char = trimmed.as_bytes().first().copied()?;
    if fence_char != b'`' && fence_char != b'~' {
        return None;
    }
    let fence_len = trimmed.bytes().take_while(|&b| b == fence_char).count();
    if fence_len < 3 {
        return None;
    }
    Some(trimmed[fence_len..].trim())
}

/// The byte span of the YAML body inside a fenced code-block node.
///
/// The node span runs from the start of the open fence line through the end of
/// the close fence line (or EOF, for an unterminated fence). The body is every
/// line between: it starts after the open fence line's terminator and ends
/// before the close fence line (when one is present). An empty body — an open
/// fence immediately followed by its close — yields a zero-length span at the
/// body start, which the YAML parser reads as no entries.
fn fence_body_span(node_span: Span, source: &str) -> Span {
    let raw = &source[node_span.start..node_span.end];

    // Body starts after the first line ending. With no line ending, the fence
    // is a single line (degenerate) — the body is empty at the node end.
    let Some(open_nl) = raw.find('\n') else {
        return Span::new(node_span.end, node_span.end);
    };
    let body_start = node_span.start + open_nl + 1;

    // Determine the fence character/length from the open line, to find a real
    // closing fence (`fenced_code_close`-style) and exclude it from the body.
    let first_line = &raw[..open_nl];
    let open_trimmed = first_line.trim_start_matches(' ');
    let Some(fence_char) = open_trimmed.as_bytes().first().copied() else {
        return Span::new(body_start, node_span.end);
    };
    let open_len = open_trimmed
        .bytes()
        .take_while(|&b| b == fence_char)
        .count();

    // Walk the lines after the open fence, tracking each line's byte offset, and
    // stop the body at the first closing fence. Line endings are normalized by
    // `str::lines`, which strips a trailing `\r`, so we recompute byte offsets
    // from `body_start` over the original (un-split) remainder.
    let mut offset = body_start;
    let remainder = &source[body_start..node_span.end];
    for line in remainder.split_inclusive('\n') {
        let line_no_eol = line.trim_end_matches(['\n', '\r']);
        let trimmed = line_no_eol.trim_start_matches(' ');
        let indent = line_no_eol.len() - trimmed.len();
        let close_len = trimmed.bytes().take_while(|&b| b == fence_char).count();
        let is_close = indent <= 3
            && close_len >= open_len
            && close_len > 0
            && trimmed[close_len..].trim().is_empty();
        if is_close {
            // Body ends at the start of this closing fence line.
            return Span::new(body_start, offset);
        }
        offset += line.len();
    }

    // No closing fence (unterminated): the body runs to the node end.
    Span::new(body_start, node_span.end)
}

/// Whether the document carries a leading `---` / `+++` / `{` frontmatter block.
///
/// The tree builder emits a single [`Frontmatter`](ElementKind::Frontmatter)
/// node as the first child of the document exactly when a leading block was
/// detected (`parse_tree_with_entries` adds it only for a `Some` frontmatter
/// span). A `yaml lattice` carrier is never represented by this node — it is a
/// [`CodeBlock`](ElementKind::CodeBlock) — so the presence of a `Frontmatter`
/// node is an exact, tree-based signal that a leading block exists, the other
/// half of the one-carrier-per-document check (decision 015).
fn has_leading_frontmatter(tree: &Tree) -> bool {
    tree.nodes()
        .iter()
        .any(|node| matches!(node.kind, ElementKind::Frontmatter))
}

/// Scan the parse tree for every metadata-carrier fence (`yaml lattice`).
///
/// Returns the carriers in document order. A fence is a carrier only when it is
/// a real markdown [`CodeBlock`](ElementKind::CodeBlock) node at a *carrier
/// site* (top level, or directly inside a top-level `<details>`; see
/// [`is_carrier_site`]). An HTML-syntax code block, a `yaml lattice` example
/// nested inside an outer fence (one opaque code-block node whose own info
/// string is the outer language), or a `yaml lattice` fence embedded in a
/// blockquote / list / table / generic container is **not** recognized — it is
/// quoted or embedded content, not the document's own metadata.
fn scan_carriers(tree: &Tree) -> Vec<Carrier> {
    let source = tree.source();
    let mut carriers = Vec::new();
    for (node_id, node) in tree.nodes().iter().enumerate() {
        if !matches!(node.kind, ElementKind::CodeBlock) || node.syntax == Syntax::Html {
            continue;
        }
        if !is_carrier_site(tree, node_id) {
            continue;
        }
        let raw = &source[node.span.start..node.span.end];
        let Some(info) = fence_info(raw) else {
            continue;
        };
        if !is_carrier_info(info) {
            continue;
        }
        carriers.push(Carrier {
            node_span: node.span,
            body_span: fence_body_span(node.span, source),
        });
    }
    carriers
}

/// Parse the recognized metadata carrier's body into a [`FrontmatterBlock`].
///
/// Returns `None` when the document has no `yaml lattice` carrier. When more
/// than one carrier is present, the first (document order) is parsed — the
/// duplicate is a diagnostic (see [`carrier_diagnostics`]), not a parse choice.
/// The returned block's spans point *inside* the fence, so `backlinks` /
/// `exceptions` extraction and any diagnostic anchored at an offending entry
/// land in the document, exactly as a leading `---` block would.
#[must_use]
pub fn parse_carrier_block(tree: &Tree) -> Option<FrontmatterBlock> {
    let carrier = scan_carriers(tree).into_iter().next()?;
    let body = &tree.source()[carrier.body_span.start..carrier.body_span.end];
    Some(yaml::parse_yaml_body(body, carrier.body_span.start))
}

/// A line within a `<details>` element, classified for the render-gotcha check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetailsLine {
    /// A blank line (whitespace only).
    Blank,
    /// The `</summary>` end (or a one-line `<summary>…</summary>`).
    SummaryEnd,
    /// The open fence of a `yaml lattice` carrier.
    FenceOpen,
    /// A bare closing fence (all fence chars, no info string) that closes the
    /// carrier opened earlier in the same `<details>`.
    FenceClose,
    /// The `</details>` close tag.
    DetailsClose,
    /// Any other content line.
    Other,
}

/// Emit the metadata carrier's structural diagnostics for a single file.
///
/// Covers the render gotchas, the malformed-YAML errors, the
/// more-than-one-carrier warning, and the one-carrier-per-document warning
/// (a leading frontmatter block coexisting with a `yaml lattice` carrier) of
/// decision 015. Carrier *position* is never diagnosed — a carrier renders
/// correctly anywhere, so position fixes no defect (decisions 009/008). The
/// unterminated-`<details>` error is already emitted by the block parser (an
/// "unclosed `<details>` tag" diagnostic) and flows through the structural
/// layer's parser-diagnostic pass, so it is not duplicated here.
///
/// Every emitted span is a char-boundary byte range that round-trips through LSP
/// position mapping — the shared invariants (`invariants.rs`) the rest of the
/// structural layer upholds.
pub fn carrier_diagnostics(tree: &Tree, rel_path: &Path, out: &mut Vec<Diagnostic>) {
    let source = tree.source();
    let carriers = scan_carriers(tree);

    // One carrier per document — frontmatter XOR fenced block (decision 015). A
    // leading `---` / `+++` / `{` block coexisting with a `yaml lattice` carrier
    // is ambiguous authority: the two are not merged (the leading block wins for
    // metadata; the carrier is ignored for data, see `workspace::parse_content`),
    // so flag every carrier that shares the document with a leading block. The
    // warning anchors at the fenced carrier — the "second" carrier relative to
    // the leading block — pointing the author at the one to remove.
    if has_leading_frontmatter(tree) {
        for carrier in &carriers {
            out.push(Diagnostic {
                file: rel_path.to_path_buf(),
                line: crate::block::byte_offset_to_line(source, carrier.node_span.start),
                severity: Severity::Warning,
                message: "a `yaml lattice` metadata carrier coexists with leading frontmatter — \
                     document metadata has one home; keep the frontmatter or the carrier, not both"
                    .to_string(),
                span: Some(carrier.node_span),
            });
        }
    }

    // More than one carrier: document metadata is singular (decision 015). Flag
    // every carrier past the first, anchored at the offending fence node.
    for carrier in carriers.iter().skip(1) {
        out.push(Diagnostic {
            file: rel_path.to_path_buf(),
            line: crate::block::byte_offset_to_line(source, carrier.node_span.start),
            severity: Severity::Warning,
            message:
                "more than one `yaml lattice` metadata carrier — document metadata is singular; \
                 merge them into one carrier"
                    .to_string(),
            span: Some(carrier.node_span),
        });
    }

    // Malformed inner YAML is an error (decision 015). Surface every diagnostic
    // the YAML parser produced for the first carrier's body, mapped to the
    // structural diagnostic shape with its in-fence span.
    if let Some(block) = parse_carrier_block(tree) {
        for diag in &block.diagnostics {
            let severity = match diag.severity {
                FmSeverity::Error => Severity::Error,
                FmSeverity::Warning => Severity::Warning,
            };
            out.push(Diagnostic {
                file: rel_path.to_path_buf(),
                line: crate::block::byte_offset_to_line(source, diag.span.start),
                severity,
                message: format!("metadata carrier: {}", diag.message),
                span: Some(diag.span),
            });
        }
    }

    // Near-miss carrier (decision 015): a fence at a carrier site whose info
    // string is `yaml lattice` plus extra tokens (`yaml lattice title="x"`,
    // `yaml lattice lines`) reads as a metadata carrier but is not exactly
    // `yaml lattice`, so its metadata is silently dropped. Hint when intent is
    // high but the form is invalid — warn, anchored at the fence-open/info line.
    // This fires only at a carrier site: a near-miss nested in a blockquote or
    // list stays inert and silent (the structural-scoping rule wins over the
    // near-miss hint).
    for (node_id, node) in tree.nodes().iter().enumerate() {
        if !matches!(node.kind, ElementKind::CodeBlock) || node.syntax == Syntax::Html {
            continue;
        }
        if !is_carrier_site(tree, node_id) {
            continue;
        }
        let raw = &source[node.span.start..node.span.end];
        let Some(info) = fence_info(raw) else {
            continue;
        };
        if !is_carrier_near_miss(info) {
            continue;
        }
        let span = fence_open_line_span(source, node.span.start, node.span.end);
        out.push(Diagnostic {
            file: rel_path.to_path_buf(),
            line: crate::block::byte_offset_to_line(source, node.span.start),
            severity: Severity::Warning,
            message:
                "this looks like a `yaml lattice` metadata carrier but the info string is not \
                 exactly `yaml lattice`, so its metadata is not loaded — drop the extra tokens"
                    .to_string(),
            span: Some(span),
        });
    }

    // Render gotchas: a `<details>`-wrapped carrier whose fence is not separated
    // from the surrounding HTML by a blank line renders as a literal YAML wall on
    // GitHub (a genuine render defect, decision 009). Scan every `<details>`
    // element raw and check the blank-line invariants around any `yaml lattice`
    // fence inside it.
    for node in tree.nodes() {
        if !matches!(node.kind, ElementKind::Details) {
            continue;
        }
        emit_details_gotchas(node.span, source, rel_path, out);
    }
}

/// Check the blank-line render invariants around a `yaml lattice` fence inside a
/// `<details>` element, emitting a render-gotcha warning per violation.
///
/// GitHub stays in HTML-block mode unless a blank line separates the HTML from a
/// fenced block, so a fence directly after `</summary>` or directly before
/// `</details>` renders as literal text. Both are warnings: the data is still
/// readable, only the render is broken.
fn emit_details_gotchas(
    details_span: Span,
    source: &str,
    rel_path: &Path,
    out: &mut Vec<Diagnostic>,
) {
    let raw = &source[details_span.start..details_span.end];

    // Classify each line, recording its byte offset. A bare fence line is the
    // carrier's `FenceClose` only while a carrier fence is open (tracked across
    // the body lines), so a stray bare fence outside a carrier is left `Other`.
    let mut classes: Vec<(DetailsLine, usize)> = Vec::new();
    let mut offset = details_span.start;
    let mut in_carrier_fence = false;
    for line in raw.split_inclusive('\n') {
        let line_no_eol = line.trim_end_matches(['\n', '\r']);
        let trimmed = line_no_eol.trim();
        let info = fence_info(line_no_eol);
        let class = if in_carrier_fence {
            // Inside a carrier fence: a bare fence closes it; any other line is
            // opaque body.
            if info == Some("") {
                in_carrier_fence = false;
                DetailsLine::FenceClose
            } else {
                DetailsLine::Other
            }
        } else if info.is_some_and(is_carrier_info) {
            in_carrier_fence = true;
            DetailsLine::FenceOpen
        } else if trimmed.is_empty() {
            DetailsLine::Blank
        } else if trimmed.ends_with("</summary>") {
            DetailsLine::SummaryEnd
        } else if trimmed == "</details>" {
            DetailsLine::DetailsClose
        } else {
            DetailsLine::Other
        };
        classes.push((class, offset));
        offset += line.len();
    }

    for (idx, &(class, line_start)) in classes.iter().enumerate() {
        let span = fence_open_line_span(source, line_start, details_span.end);
        match class {
            // Gotcha 1: the line immediately before a carrier fence open is the
            // summary end (no blank line between `</summary>` and the fence).
            DetailsLine::FenceOpen if idx > 0 && classes[idx - 1].0 == DetailsLine::SummaryEnd => {
                out.push(Diagnostic {
                    file: rel_path.to_path_buf(),
                    line: crate::block::byte_offset_to_line(source, line_start),
                    severity: Severity::Warning,
                    message:
                        "metadata carrier: add a blank line after `</summary>` — without it GitHub \
                         renders the fence as literal text"
                            .to_string(),
                    span: Some(span),
                });
            }
            // Gotcha 2: the carrier fence close is immediately followed by
            // `</details>` (no blank line before `</details>`).
            DetailsLine::FenceClose
                if idx + 1 < classes.len() && classes[idx + 1].0 == DetailsLine::DetailsClose =>
            {
                out.push(Diagnostic {
                    file: rel_path.to_path_buf(),
                    line: crate::block::byte_offset_to_line(source, line_start),
                    severity: Severity::Warning,
                    message: "metadata carrier: add a blank line before `</details>` — without it \
                         GitHub renders the fence as literal text"
                        .to_string(),
                    span: Some(span),
                });
            }
            _ => {}
        }
    }
}

/// The byte span of a single line starting at `line_start`, clamped to `limit`.
///
/// The span covers the line's content excluding its terminator, so it anchors
/// a precise underline on the fence line. Both ends land on char boundaries
/// because the source is sliced on a line boundary and `find('\n')` lands after
/// a complete char.
fn fence_open_line_span(source: &str, line_start: usize, limit: usize) -> Span {
    let slice = &source[line_start..limit];
    let end = slice.find('\n').map_or(limit, |nl| line_start + nl);
    // Strip a trailing `\r` so a CRLF line ending is not included.
    let end = if end > line_start && source.as_bytes().get(end - 1) == Some(&b'\r') {
        end - 1
    } else {
        end
    };
    Span::new(line_start, end)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests use expect and panic for clarity"
)]
mod tests {
    use super::carrier_diagnostics;
    use crate::block;
    use crate::config::Config;
    use crate::invariants::assert_structural_diagnostics_valid;
    use crate::validation::{Diagnostic, Severity};
    use crate::workspace::parse_content;
    use std::path::Path;

    /// Build the parse tree for `content` the same way `parse_content` does
    /// (frontmatter detection, then full tree + inlines).
    fn tree_of(content: &str) -> block::Tree {
        let fm = crate::yaml::parse_frontmatter_block(content);
        let fm_span = fm.as_ref().map(|b| b.span);
        block::parse_tree(content, fm_span)
    }

    /// Collect only the carrier diagnostics for `content`, and assert every span
    /// round-trips through LSP position mapping (the shared invariant).
    fn carrier_diags(content: &str) -> Vec<Diagnostic> {
        let tree = tree_of(content);
        let mut out = Vec::new();
        carrier_diagnostics(&tree, Path::new("test.md"), &mut out);
        assert_structural_diagnostics_valid(content, &out);
        out
    }

    /// The `backlinks` map a full `parse_content` pass extracts for `content`.
    fn backlinks_of(content: &str) -> std::collections::HashMap<String, Vec<String>> {
        let file = parse_content(content, Path::new("test.md"), &Config::default());
        file.frontmatter.map(|fm| fm.backlinks).unwrap_or_default()
    }

    // -- Recognition + frontmatter population ----------------------------

    #[test]
    fn naked_carrier_populates_backlinks() {
        let content =
            "# Title\n\n```yaml lattice\nbacklinks:\n  referenced_by:\n    - README.md\n```\n";
        let backlinks = backlinks_of(content);
        assert_eq!(
            backlinks.get("referenced_by").map(Vec::as_slice),
            Some(["README.md".to_string()].as_slice()),
            "a naked carrier populates frontmatter backlinks: {backlinks:?}"
        );
    }

    #[test]
    fn details_wrapped_carrier_populates_backlinks() {
        let content = "# Title\n\n<details><summary>lattice</summary>\n\n```yaml lattice\nbacklinks:\n  referenced_by:\n    - README.md\n```\n\n</details>\n";
        let backlinks = backlinks_of(content);
        assert_eq!(
            backlinks.get("referenced_by").map(Vec::as_slice),
            Some(["README.md".to_string()].as_slice()),
            "a `<details>`-wrapped carrier populates backlinks identically: {backlinks:?}"
        );
    }

    #[test]
    fn carrier_matches_leading_frontmatter() {
        // A carrier and an equivalent `---` block populate the same backlinks.
        let carrier =
            "# T\n\n```yaml lattice\nbacklinks:\n  referenced_by:\n    - a.md\n    - b.md\n```\n";
        let leading = "---\nbacklinks:\n  referenced_by:\n    - a.md\n    - b.md\n---\n# T\n";
        assert_eq!(
            backlinks_of(carrier),
            backlinks_of(leading),
            "a carrier populates backlinks identically to a leading `---` block"
        );
    }

    #[test]
    fn carrier_populates_exceptions() {
        let content = "# T\n\n```yaml lattice\nexceptions:\n  stale_references:\n    \"old.md\": \"migrated\"\n```\n";
        let file = parse_content(content, Path::new("test.md"), &Config::default());
        let fm = file.frontmatter.expect("carrier populates frontmatter");
        assert_eq!(
            fm.exceptions.stale_references.len(),
            1,
            "a carrier populates the exceptions block: {:?}",
            fm.exceptions
        );
        assert_eq!(
            fm.exceptions.stale_references[0].reference, "old.md",
            "the exception reference is carried through: {:?}",
            fm.exceptions
        );
    }

    #[test]
    fn carrier_position_is_free_no_placement_diagnostic() {
        // Top, under the H1, and at the foot all populate frontmatter and emit
        // no placement diagnostic.
        for content in [
            "```yaml lattice\nbacklinks:\n  referenced_by:\n    - a.md\n```\n\n# Title\n",
            "# Title\n\n```yaml lattice\nbacklinks:\n  referenced_by:\n    - a.md\n```\n",
            "# Title\n\nbody\n\n```yaml lattice\nbacklinks:\n  referenced_by:\n    - a.md\n```\n",
        ] {
            let backlinks = backlinks_of(content);
            assert!(
                backlinks.contains_key("referenced_by"),
                "carrier populates regardless of position: {content:?}"
            );
            assert!(
                carrier_diags(content).is_empty(),
                "no placement diagnostic for any carrier position: {content:?}"
            );
        }
    }

    // -- Inertness (tree-based, never regex) -----------------------------

    #[test]
    fn carrier_inside_outer_fence_is_inert() {
        // A `yaml lattice` fence shown inside an outer ```` ```markdown ```` fence
        // is one opaque code-block node: no metadata, no diagnostic.
        let content = "# Docs\n\n````markdown\n```yaml lattice\nbacklinks:\n  referenced_by:\n    - a.md\n```\n````\n";
        assert!(
            backlinks_of(content).is_empty(),
            "a nested carrier extracts no metadata"
        );
        assert!(
            carrier_diags(content).is_empty(),
            "a nested carrier emits no diagnostic: {:?}",
            carrier_diags(content)
        );
    }

    #[test]
    fn incidental_yaml_block_is_not_a_carrier() {
        // A plain `yaml` example (no `lattice` discriminator) is not a carrier.
        let content = "# T\n\n```yaml\nbacklinks:\n  referenced_by:\n    - a.md\n```\n";
        assert!(
            backlinks_of(content).is_empty(),
            "an incidental `yaml` block is not recognized"
        );
        assert!(
            carrier_diags(content).is_empty(),
            "an incidental `yaml` block emits no carrier diagnostic"
        );
    }

    // -- Top-level scoping (the blockquote gotcha) -----------------------

    #[test]
    fn carrier_inside_blockquote_is_inert() {
        // A `yaml lattice` fence inside a blockquote is a real `CodeBlock`, but
        // quoted content — not the document's own metadata. Inert and silent.
        let content =
            "# T\n\n> ```yaml lattice\n> backlinks:\n>   referenced_by:\n>     - a.md\n> ```\n";
        assert!(
            backlinks_of(content).is_empty(),
            "a carrier inside a blockquote extracts no metadata: {:?}",
            backlinks_of(content)
        );
        assert!(
            carrier_diags(content).is_empty(),
            "a carrier inside a blockquote emits no diagnostic: {:?}",
            carrier_diags(content)
        );
    }

    #[test]
    fn carrier_inside_list_item_is_inert() {
        // A `yaml lattice` fence inside a list item is embedded content. Inert.
        let content =
            "# T\n\n- ```yaml lattice\n  backlinks:\n    referenced_by:\n      - a.md\n  ```\n";
        assert!(
            backlinks_of(content).is_empty(),
            "a carrier inside a list item extracts no metadata: {:?}",
            backlinks_of(content)
        );
        assert!(
            carrier_diags(content).is_empty(),
            "a carrier inside a list item emits no diagnostic: {:?}",
            carrier_diags(content)
        );
    }

    #[test]
    fn carrier_inside_generic_container_is_inert() {
        // A `yaml lattice` fence inside a generic HTML `<div>` container is a
        // real `CodeBlock` child of the `Container`, not the document's own
        // metadata — inert and silent. (Verified the fence parses to a
        // `CodeBlock` whose parent is `Container`, so the scoping path is
        // exercised, not bypassed.)
        let content = "# T\n\n<div>\n\n```yaml lattice\nbacklinks:\n  referenced_by:\n    - a.md\n```\n\n</div>\n";
        assert!(
            backlinks_of(content).is_empty(),
            "a carrier inside a generic container extracts no metadata: {:?}",
            backlinks_of(content)
        );
        assert!(
            carrier_diags(content).is_empty(),
            "a carrier inside a generic container emits no diagnostic: {:?}",
            carrier_diags(content)
        );
    }

    #[test]
    fn carrier_text_in_table_cell_is_inert() {
        // A GFM table cell parses inline content only — a fenced code block
        // cannot live inside one (the triple-backtick is an inline code span,
        // not a `CodeBlock` node). So a `yaml lattice` mention in a table cell
        // is never recognized: no metadata, no diagnostic.
        let content = "# T\n\n| a | b |\n| --- | --- |\n| ```yaml lattice``` | x |\n";
        assert!(
            backlinks_of(content).is_empty(),
            "a `yaml lattice` mention in a table cell extracts no metadata: {:?}",
            backlinks_of(content)
        );
        assert!(
            carrier_diags(content).is_empty(),
            "a `yaml lattice` mention in a table cell emits no diagnostic: {:?}",
            carrier_diags(content)
        );
    }

    #[test]
    fn carrier_inside_nested_details_is_inert() {
        // A `<details>` nested inside a blockquote is not a top-level carrier
        // site, so the fence inside it is inert — the `<details>` parent must be
        // the document root for the fence to be live metadata.
        let content = "# T\n\n> <details><summary>lattice</summary>\n>\n> ```yaml lattice\n> backlinks:\n>   referenced_by:\n>     - a.md\n> ```\n>\n> </details>\n";
        assert!(
            backlinks_of(content).is_empty(),
            "a carrier inside a nested `<details>` extracts no metadata: {:?}",
            backlinks_of(content)
        );
        assert!(
            carrier_diags(content).is_empty(),
            "a carrier inside a nested `<details>` emits no diagnostic: {:?}",
            carrier_diags(content)
        );
    }

    // -- Near-miss info strings (warning) --------------------------------

    #[test]
    fn near_miss_extra_attribute_warns_and_loads_nothing() {
        // `yaml lattice title="x"`: looks like a carrier, but the info string
        // is not exactly `yaml lattice`, so nothing is loaded — warn.
        let content =
            "# T\n\n```yaml lattice title=\"x\"\nbacklinks:\n  referenced_by:\n    - a.md\n```\n";
        let diags = carrier_diags(content);
        assert!(
            diags
                .iter()
                .any(|d| d.severity == Severity::Warning && d.message.contains("not loaded")),
            "a near-miss carrier earns a warning: {diags:?}"
        );
        assert!(
            backlinks_of(content).is_empty(),
            "a near-miss carrier loads no metadata: {:?}",
            backlinks_of(content)
        );
    }

    #[test]
    fn near_miss_extra_bare_token_warns() {
        // `yaml lattice lines`: a bare third token is still a near-miss.
        let content =
            "# T\n\n```yaml lattice lines\nbacklinks:\n  referenced_by:\n    - a.md\n```\n";
        let diags = carrier_diags(content);
        let warnings = diags
            .iter()
            .filter(|d| d.severity == Severity::Warning && d.message.contains("not loaded"))
            .count();
        assert_eq!(
            warnings, 1,
            "a near-miss carrier earns exactly one warning: {diags:?}"
        );
    }

    #[test]
    fn near_miss_warning_anchors_at_info_line() {
        // The warning span anchors at the fence-open / info line.
        let content =
            "# T\n\n```yaml lattice lines\nbacklinks:\n  referenced_by:\n    - a.md\n```\n";
        let diags = carrier_diags(content);
        let warning = diags
            .iter()
            .find(|d| d.message.contains("not loaded"))
            .expect("a near-miss carrier emits a warning");
        let span = warning.span.expect("near-miss warning carries a span");
        let info_line_start = content
            .find("```yaml lattice lines")
            .expect("fixture contains the near-miss fence");
        assert_eq!(
            span.start, info_line_start,
            "the near-miss span anchors at the fence-open/info line: {diags:?}"
        );
    }

    #[test]
    fn near_miss_inside_blockquote_is_silent() {
        // Nested wins: a near-miss inside a blockquote stays inert and silent —
        // the structural-scoping rule suppresses the near-miss hint.
        let content = "# T\n\n> ```yaml lattice title=\"x\"\n> backlinks:\n>   referenced_by:\n>     - a.md\n> ```\n";
        assert!(
            carrier_diags(content).is_empty(),
            "a near-miss inside a blockquote is silent (nested wins): {:?}",
            carrier_diags(content)
        );
        assert!(
            backlinks_of(content).is_empty(),
            "a near-miss inside a blockquote loads nothing: {:?}",
            backlinks_of(content)
        );
    }

    #[test]
    fn near_miss_under_top_level_details_warns() {
        // A near-miss directly under a top-level `<details>` is a carrier site,
        // so it earns the warning just like a naked top-level near-miss.
        let content = "# T\n\n<details><summary>lattice</summary>\n\n```yaml lattice title=\"x\"\nbacklinks:\n  referenced_by:\n    - a.md\n```\n\n</details>\n";
        let diags = carrier_diags(content);
        assert!(
            diags
                .iter()
                .any(|d| d.severity == Severity::Warning && d.message.contains("not loaded")),
            "a near-miss under a top-level `<details>` warns: {diags:?}"
        );
    }

    #[test]
    fn exact_carrier_is_not_a_near_miss() {
        // An exact `yaml lattice` carrier never trips the near-miss warning.
        let content = "# T\n\n```yaml lattice\nbacklinks:\n  referenced_by:\n    - a.md\n```\n";
        assert!(
            !carrier_diags(content)
                .iter()
                .any(|d| d.message.contains("not loaded")),
            "an exact carrier is not a near-miss: {:?}",
            carrier_diags(content)
        );
    }

    #[test]
    fn near_miss_span_round_trips_crlf() {
        // CRLF: the near-miss span must still round-trip (helper asserts it).
        let content = "# T\r\n\r\n```yaml lattice title=\"x\"\r\nbacklinks:\r\n  referenced_by:\r\n    - a.md\r\n```\r\n";
        let diags = carrier_diags(content);
        assert!(
            diags.iter().any(|d| d.message.contains("not loaded")),
            "the near-miss warning fires under CRLF: {diags:?}"
        );
    }

    // -- Render gotchas (warnings) ---------------------------------------

    #[test]
    fn missing_blank_after_summary_warns() {
        let content = "<details><summary>lattice</summary>\n```yaml lattice\nbacklinks:\n  referenced_by:\n    - a.md\n```\n\n</details>\n";
        let diags = carrier_diags(content);
        assert!(
            diags.iter().any(|d| d.severity == Severity::Warning
                && d.message.contains("blank line after `</summary>`")),
            "missing blank after `</summary>` is a render-gotcha warning: {diags:?}"
        );
    }

    #[test]
    fn missing_blank_before_details_close_warns() {
        let content = "<details><summary>lattice</summary>\n\n```yaml lattice\nbacklinks:\n  referenced_by:\n    - a.md\n```\n</details>\n";
        let diags = carrier_diags(content);
        assert!(
            diags.iter().any(|d| d.severity == Severity::Warning
                && d.message.contains("blank line before `</details>`")),
            "missing blank before `</details>` is a render-gotcha warning: {diags:?}"
        );
    }

    #[test]
    fn well_formed_details_has_no_gotcha() {
        let content = "<details><summary>lattice</summary>\n\n```yaml lattice\nbacklinks:\n  referenced_by:\n    - a.md\n```\n\n</details>\n";
        let diags = carrier_diags(content);
        assert!(
            diags.is_empty(),
            "a well-formed `<details>` carrier emits no gotcha: {diags:?}"
        );
    }

    // -- Errors ----------------------------------------------------------

    #[test]
    fn malformed_carrier_yaml_is_an_error() {
        // A deeper-indented line with no parent mapping is a YAML error.
        let content = "# T\n\n```yaml lattice\nbacklinks:\n      bad_indent\n```\n";
        let diags = carrier_diags(content);
        assert!(
            diags.iter().any(
                |d| d.severity == Severity::Error && d.message.starts_with("metadata carrier:")
            ),
            "malformed inner YAML is a carrier error: {diags:?}"
        );
    }

    #[test]
    fn unterminated_details_is_an_error() {
        // The block parser emits an "unclosed `<details>` tag" error that flows
        // through the structural parser-diagnostic pass; the carrier still
        // populates frontmatter.
        let content = "<details><summary>lattice</summary>\n\n```yaml lattice\nbacklinks:\n  referenced_by:\n    - a.md\n```\n";
        let tree = tree_of(content);
        assert!(
            tree.diagnostics()
                .iter()
                .any(|d| d.level == block::DiagnosticLevel::Error
                    && d.message.contains("unclosed `<details>`")),
            "an unterminated `<details>` is a parser error: {:?}",
            tree.diagnostics()
        );
        assert!(
            backlinks_of(content).contains_key("referenced_by"),
            "the carrier still populates backlinks despite the unclosed tag"
        );
    }

    // -- More than one carrier (warning) ---------------------------------

    #[test]
    fn second_carrier_warns() {
        let content = "# T\n\n```yaml lattice\nbacklinks:\n  referenced_by:\n    - a.md\n```\n\n```yaml lattice\nbacklinks:\n  referenced_by:\n    - b.md\n```\n";
        let diags = carrier_diags(content);
        let warnings = diags
            .iter()
            .filter(|d| d.severity == Severity::Warning && d.message.contains("more than one"))
            .count();
        assert_eq!(
            warnings, 1,
            "a second carrier yields exactly one warning: {diags:?}"
        );
    }

    #[test]
    fn single_carrier_no_duplicate_warning() {
        let content = "# T\n\n```yaml lattice\nbacklinks:\n  referenced_by:\n    - a.md\n```\n";
        assert!(
            !carrier_diags(content)
                .iter()
                .any(|d| d.message.contains("more than one")),
            "a single carrier yields no duplicate warning"
        );
    }

    // -- One carrier per document (warning) ------------------------------

    #[test]
    fn frontmatter_and_carrier_coexist_warns() {
        // Decision 015: frontmatter XOR fenced block. A leading `---` block plus
        // a `yaml lattice` carrier is ambiguous authority — exactly one warning,
        // anchored at the fenced carrier (the "second" carrier).
        let content = "---\nbacklinks:\n  referenced_by:\n    - a.md\n---\n# T\n\n```yaml lattice\nbacklinks:\n  referenced_by:\n    - b.md\n```\n";
        let diags = carrier_diags(content);
        let coexist: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Severity::Warning && d.message.contains("coexists"))
            .collect();
        assert_eq!(
            coexist.len(),
            1,
            "frontmatter + carrier yields exactly one coexistence warning: {diags:?}"
        );
        // The warning anchors at the fenced carrier, after the frontmatter block.
        let carrier_start = content
            .find("```yaml lattice")
            .expect("fixture contains a fenced carrier");
        assert_eq!(
            coexist[0]
                .span
                .expect("coexistence warning carries a span")
                .start,
            carrier_start,
            "the coexistence warning anchors at the fenced carrier, not the frontmatter: {diags:?}"
        );
    }

    #[test]
    fn carrier_only_no_coexistence_warning() {
        // A fenced carrier with no leading frontmatter is the supported single-
        // carrier case — no coexistence warning.
        let content = "# T\n\n```yaml lattice\nbacklinks:\n  referenced_by:\n    - a.md\n```\n";
        assert!(
            !carrier_diags(content)
                .iter()
                .any(|d| d.message.contains("coexists")),
            "a lone carrier yields no coexistence warning"
        );
    }

    #[test]
    fn frontmatter_only_no_coexistence_warning() {
        // A leading `---` block with no carrier is the zero-config default — no
        // coexistence warning (and no carrier diagnostics at all).
        let content = "---\nbacklinks:\n  referenced_by:\n    - a.md\n---\n# T\n";
        assert!(
            carrier_diags(content).is_empty(),
            "leading frontmatter with no carrier emits no carrier diagnostic"
        );
    }

    #[test]
    fn frontmatter_and_duplicate_carriers_warn_per_carrier() {
        // Two carriers alongside leading frontmatter: each carrier draws a
        // coexistence warning, and the second additionally draws the duplicate
        // warning — the two checks compose.
        let content = "---\nbacklinks:\n  referenced_by:\n    - a.md\n---\n# T\n\n```yaml lattice\nbacklinks:\n  referenced_by:\n    - b.md\n```\n\n```yaml lattice\nbacklinks:\n  referenced_by:\n    - c.md\n```\n";
        let diags = carrier_diags(content);
        let coexist = diags
            .iter()
            .filter(|d| d.message.contains("coexists"))
            .count();
        let duplicate = diags
            .iter()
            .filter(|d| d.message.contains("more than one"))
            .count();
        assert_eq!(
            coexist, 2,
            "each carrier coexisting with frontmatter is flagged: {diags:?}"
        );
        assert_eq!(
            duplicate, 1,
            "the second carrier still draws the duplicate warning: {diags:?}"
        );
    }

    #[test]
    fn coexistence_warning_spans_round_trip_crlf() {
        // CRLF line endings: the coexistence-warning span must still round-trip
        // (the `carrier_diags` helper asserts it via the shared invariant).
        let content = "---\r\nbacklinks:\r\n  referenced_by:\r\n    - a.md\r\n---\r\n# T\r\n\r\n```yaml lattice\r\nbacklinks:\r\n  referenced_by:\r\n    - b.md\r\n```\r\n";
        let diags = carrier_diags(content);
        assert!(
            diags.iter().any(|d| d.message.contains("coexists")),
            "the coexistence warning fires under CRLF: {diags:?}"
        );
    }

    // -- Span fidelity across encodings ----------------------------------

    #[test]
    fn carrier_diagnostic_spans_round_trip_crlf() {
        // CRLF line endings: every emitted span must still round-trip (the
        // helper asserts it). A duplicate carrier guarantees ≥1 span.
        let content = "# T\r\n\r\n```yaml lattice\r\nbacklinks:\r\n  referenced_by:\r\n    - a.md\r\n```\r\n\r\n```yaml lattice\r\nbacklinks:\r\n  referenced_by:\r\n    - b.md\r\n```\r\n";
        let diags = carrier_diags(content);
        assert!(
            !diags.is_empty(),
            "the duplicate carrier emits a diagnostic under CRLF"
        );
    }
}
