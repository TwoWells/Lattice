// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! `textDocument/semanticTokens` — emphasis runs, and the legend that names
//! them.
//!
//! Lattice emits a single token type, `markup`, and distinguishes bold,
//! italic and strikethrough through *modifiers*. That is what lets overlapping
//! runs — strong inside emphasis — compose into one token carrying both bits,
//! instead of two tokens the protocol forbids overlapping. The legend
//! constants live here with the encoder that indexes into them; `mod.rs`
//! imports them back to advertise the legend in the initialize handshake.

use crate::block::{ElementKind, Tree};
use crate::line_index::LineIndex;
use crate::lsp;
use crate::span::Span;

use super::helpers::{line_byte_range, span_to_lsp_range};
use super::workspaces::Workspaces;

// ---------------------------------------------------------------------------
// Semantic tokens legend (ticket integration 15)
// ---------------------------------------------------------------------------

/// The single semantic token type Lattice emits. All emphasis runs carry this
/// base type and distinguish themselves through modifiers, so overlapping runs
/// (strong inside emphasis) compose into one token with combined modifiers
/// rather than two illegal overlapping tokens.
pub const SEMANTIC_TOKEN_TYPE_MARKUP: &str = "markup";
/// Modifier name for strong (`**bold**`) runs.
pub const SEMANTIC_MODIFIER_BOLD: &str = "bold";
/// Modifier name for emphasis (`*italic*`) runs.
pub const SEMANTIC_MODIFIER_ITALIC: &str = "italic";
/// Modifier name for strikethrough (`~~struck~~`) runs.
pub const SEMANTIC_MODIFIER_STRIKETHROUGH: &str = "strikethrough";

/// Token-type index into the legend's `tokenTypes` array. Only `markup`
/// (index 0) exists.
pub const SEMANTIC_TOKEN_TYPE_MARKUP_INDEX: u32 = 0;
/// Modifier bit for `bold` — index 0 in the legend's `tokenModifiers` array.
pub const SEMANTIC_MODIFIER_BOLD_BIT: u32 = 1 << 0;
/// Modifier bit for `italic` — index 1 in the legend's `tokenModifiers` array.
pub const SEMANTIC_MODIFIER_ITALIC_BIT: u32 = 1 << 1;
/// Modifier bit for `strikethrough` — index 2 in the legend's `tokenModifiers`
/// array.
pub const SEMANTIC_MODIFIER_STRIKETHROUGH_BIT: u32 = 1 << 2;

// ---------------------------------------------------------------------------
// Semantic tokens (ticket integration 15)
// ---------------------------------------------------------------------------

/// A maximal disjoint byte region carrying the union of emphasis modifiers
/// active over it. Reconstructed from the parser's flat, *overlapping* sibling
/// emphasis spans so the emitted token stream can be non-overlapping (an LSP
/// hard requirement), while still styling the `foo` in `***foo***` as both
/// bold and italic.
#[derive(Debug, Clone, Copy)]
struct EmphasisRegion {
    /// Byte start (inclusive) in the source.
    start: usize,
    /// Byte end (exclusive) in the source.
    end: usize,
    /// OR of the `SEMANTIC_MODIFIER_*_BIT` flags active over `[start, end)`.
    modifiers: u32,
}

/// Map an emphasis [`ElementKind`] to its modifier bit, or `None` if the node
/// is not an emphasis run.
fn emphasis_modifier_bit(kind: &ElementKind) -> Option<u32> {
    match kind {
        ElementKind::Strong => Some(SEMANTIC_MODIFIER_BOLD_BIT),
        ElementKind::Emphasis => Some(SEMANTIC_MODIFIER_ITALIC_BIT),
        ElementKind::Strikethrough => Some(SEMANTIC_MODIFIER_STRIKETHROUGH_BIT),
        _ => None,
    }
}

/// Reconstruct the maximal disjoint regions from the parser's overlapping
/// emphasis spans, each tagged with the union of modifiers active over it.
///
/// Parser 26 emits emphasis as flat, *overlapping* sibling spans (e.g.
/// `***foo***` yields a `Strong` over `**foo**` and an `Emphasis` over the
/// whole `***foo***`), but the LSP semantic-tokens protocol requires a flat,
/// non-overlapping token list. We flatten by collecting every emphasis span's
/// endpoints as cut points, then, for each adjacent pair of cut points, OR the
/// modifiers of every span that fully covers that sub-segment. Segments with
/// no active modifier (the gaps between runs) are dropped. The result is sorted
/// by start and pairwise non-overlapping.
///
/// Emphasis runs never appear inside code spans or code blocks — the inline
/// parser excludes those before delimiter matching — so this naturally emits no
/// tokens in code.
fn collect_emphasis_regions(tree: &Tree) -> Vec<EmphasisRegion> {
    // (start, end, modifier_bit) for every emphasis run.
    let mut spans: Vec<(usize, usize, u32)> = Vec::new();
    for node in tree.nodes() {
        if let Some(bit) = emphasis_modifier_bit(&node.kind) {
            spans.push((node.span.start, node.span.end, bit));
        }
    }
    if spans.is_empty() {
        return Vec::new();
    }

    // Sorted, deduped boundary set: every distinct start/end is a cut point.
    let mut cuts: Vec<usize> = Vec::with_capacity(spans.len() * 2);
    for &(start, end, _) in &spans {
        cuts.push(start);
        cuts.push(end);
    }
    cuts.sort_unstable();
    cuts.dedup();

    // For each adjacent cut-point pair, the modifier mask is the OR of every
    // span that fully covers the segment.
    let mut regions: Vec<EmphasisRegion> = Vec::new();
    for window in cuts.windows(2) {
        let (seg_start, seg_end) = (window[0], window[1]);
        let mut modifiers = 0;
        for &(start, end, bit) in &spans {
            if start <= seg_start && seg_end <= end {
                modifiers |= bit;
            }
        }
        if modifiers != 0 {
            regions.push(EmphasisRegion {
                start: seg_start,
                end: seg_end,
                modifiers,
            });
        }
    }
    regions
}

/// Encode emphasis regions as the LSP delta-quintuple stream, restricted to
/// `byte_filter` (the whole document for `/full`, or a range's byte span for
/// `/range`).
///
/// A single LSP token may not span a line break, so each region is split at
/// line boundaries before encoding. Byte→UTF-16 conversion is delegated to the
/// file's cached [`LineIndex`] (`span_to_lsp_range`), the same UTF-16-aware
/// mapping diagnostics use, so multibyte and astral characters map correctly.
/// Tokens are delta-encoded against the previous token's position, as the
/// protocol requires.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line/column values in markdown files won't exceed u32::MAX"
)]
fn encode_semantic_tokens(
    source: &str,
    index: &LineIndex,
    regions: &[EmphasisRegion],
    byte_filter: std::ops::Range<usize>,
) -> lsp::SemanticTokens {
    let mut data: Vec<u32> = Vec::new();
    // Previous token's absolute (line, char) for delta encoding.
    let mut prev_line = 0u32;
    let mut prev_char = 0u32;

    for region in regions {
        let start = region.start.max(byte_filter.start);
        let end = region.end.min(byte_filter.end);
        if start >= end {
            continue;
        }
        let range = span_to_lsp_range(source, index, &Span::new(start, end));
        // Split into one token per line the region touches: an LSP token is
        // single-line, so a region crossing a `\n` becomes several tokens.
        for line in range.start.line..=range.end.line {
            let line_start_char = if line == range.start.line {
                range.start.character
            } else {
                0
            };
            // The line's content end in UTF-16 units, or the region end on the
            // final line.
            let line_end_char = if line == range.end.line {
                range.end.character
            } else {
                let (ls, le) = line_byte_range(source, line);
                source[ls..le].chars().map(char::len_utf16).sum::<usize>() as u32
            };
            let length = line_end_char.saturating_sub(line_start_char);
            if length == 0 {
                continue;
            }
            let delta_line = line - prev_line;
            let delta_start = if delta_line == 0 {
                line_start_char - prev_char
            } else {
                line_start_char
            };
            data.extend_from_slice(&[
                delta_line,
                delta_start,
                length,
                SEMANTIC_TOKEN_TYPE_MARKUP_INDEX,
                region.modifiers,
            ]);
            prev_line = line;
            prev_char = line_start_char;
        }
    }

    lsp::SemanticTokens { data }
}

/// Answer `textDocument/semanticTokens/full`: emphasis tokens over the whole
/// document.
///
/// Returns an empty token set for unknown documents. Styling only — never
/// emits a diagnostic.
///
/// # Perf seam
///
/// `full/delta` is intentionally not served: re-encoding only the emphasis runs
/// is cheap, and a delta handler should consume the perf workstream's reusable
/// "what changed since last parse" diff rather than recompute one — wire it
/// here once that lands (ticket integration 15, perf seam).
pub fn semantic_tokens_full(workspaces: &Workspaces, uri: &str) -> lsp::SemanticTokens {
    let Some((workspace, rel_path)) = workspaces.resolve_document(uri) else {
        return lsp::SemanticTokens::default();
    };
    let Some(file_data) = workspace.file(&rel_path) else {
        return lsp::SemanticTokens::default();
    };
    let source = file_data.tree.source();
    let regions = collect_emphasis_regions(&file_data.tree);
    encode_semantic_tokens(source, &file_data.line_index, &regions, 0..source.len())
}

/// Answer `textDocument/semanticTokens/range`: emphasis tokens restricted to
/// `range` (the byte span between its endpoints), for large documents.
///
/// Returns an empty token set for unknown documents.
pub fn semantic_tokens_range(
    workspaces: &Workspaces,
    uri: &str,
    range: &lsp::Range,
) -> lsp::SemanticTokens {
    let Some((workspace, rel_path)) = workspaces.resolve_document(uri) else {
        return lsp::SemanticTokens::default();
    };
    let Some(file_data) = workspace.file(&rel_path) else {
        return lsp::SemanticTokens::default();
    };
    let source = file_data.tree.source();
    let start = file_data.line_index.offset(source, range.start);
    let end = file_data.line_index.offset(source, range.end);
    let regions = collect_emphasis_regions(&file_data.tree);
    encode_semantic_tokens(source, &file_data.line_index, &regions, start..end)
}
