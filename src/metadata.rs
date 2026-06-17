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

use crate::block::{ElementKind, Syntax, Tree};
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

/// Scan the parse tree for every metadata-carrier fence (`yaml lattice`).
///
/// Returns the carriers in document order. A fence is a carrier only when it is
/// a real markdown [`CodeBlock`](ElementKind::CodeBlock) node — an HTML-syntax
/// code block, or a `yaml lattice` example nested inside an outer fence (one
/// opaque code-block node whose own info string is the outer language), is not
/// recognized.
fn scan_carriers(tree: &Tree) -> Vec<Carrier> {
    let source = tree.source();
    let mut carriers = Vec::new();
    for node in tree.nodes() {
        if !matches!(node.kind, ElementKind::CodeBlock) || node.syntax == Syntax::Html {
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
/// Covers the render gotchas, the malformed-YAML errors, and the
/// more-than-one-carrier warning of decision 015. Carrier *position* is never
/// diagnosed — a carrier renders correctly anywhere, so position fixes no defect
/// (decisions 009/008). The unterminated-`<details>` error is already emitted by
/// the block parser (an "unclosed `<details>` tag" diagnostic) and flows through
/// the structural layer's parser-diagnostic pass, so it is not duplicated here.
///
/// Every emitted span is a char-boundary byte range that round-trips through LSP
/// position mapping — the shared invariants (`invariants.rs`) the rest of the
/// structural layer upholds.
pub fn carrier_diagnostics(tree: &Tree, rel_path: &Path, out: &mut Vec<Diagnostic>) {
    let source = tree.source();
    let carriers = scan_carriers(tree);

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
