// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Helpers shared by more than one LSP surface.
//!
//! Four small families, all of them cross-cutting enough that no single
//! surface owns them:
//!
//! - **Position mapping.** The byte-offset/LSP-position conversions and the
//!   line scanner underneath them. `character` is a UTF-16 code-unit offset
//!   within its line, and [`crate::line_index`] documents itself as mirroring
//!   [`byte_offset_to_lsp_position`] and [`lsp_position_to_byte_offset`]
//!   exactly — the shared invariants assert the two agree.
//! - **Heading lookup.** Finding the heading on a cursor line, or the one
//!   enclosing it.
//! - **Hierarchy items.** The `HierarchyItem` builders both the type and the
//!   call hierarchy answer with.
//! - **Link labels.** Recovering a reference label from a span or an offset.

use std::path::Path;

use crate::block::{ElementKind, Heading, normalize_label};
use crate::line_index::LineIndex;
use crate::lsp;
use crate::span::Span;
use crate::uri::path_to_uri;

/// Find the heading whose line matches the cursor's 0-based line number.
pub fn heading_at_line(headings: &[Heading], lsp_line: u32) -> Option<&Heading> {
    heading_index_at_line(headings, lsp_line).and_then(|index| headings.get(index))
}

/// The *position* of the heading on the cursor's 0-based line number.
///
/// Fragment resolution answers in heading indices (a fragment names the first
/// heading that answers to it, in document order), so a surface that must
/// compare that answer against the cursor's heading needs its index, not a
/// borrow of it.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
pub fn heading_index_at_line(headings: &[Heading], lsp_line: u32) -> Option<usize> {
    headings
        .iter()
        .position(|h| h.line.saturating_sub(1) as u32 == lsp_line)
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Convert a heading to a hierarchy item (used for both type and call hierarchy).
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
pub fn heading_to_hierarchy_item(heading: &Heading, abs_path: &Path) -> lsp::HierarchyItem {
    let line = heading.line.saturating_sub(1) as u32;
    let range = lsp::Range {
        start: lsp::Position { line, character: 0 },
        end: lsp::Position { line, character: 0 },
    };

    lsp::HierarchyItem {
        name: heading.text.clone(),
        kind: lsp::symbol_kind::CLASS,
        uri: path_to_uri(abs_path),
        range,
        selection_range: range,
        detail: Some(format!("H{}", heading.level)),
        data: None,
    }
}

/// Create a file-level hierarchy item when a link has no enclosing heading.
pub fn file_hierarchy_item(abs_path: &Path, rel_path: &Path) -> lsp::HierarchyItem {
    let range = lsp::Range::default();
    lsp::HierarchyItem {
        name: rel_path.display().to_string(),
        kind: lsp::symbol_kind::FILE,
        uri: path_to_uri(abs_path),
        range,
        selection_range: range,
        detail: None,
        data: None,
    }
}

/// Find the heading that encloses a given 1-based line number.
pub fn enclosing_heading(headings: &[Heading], line: usize) -> Option<&Heading> {
    headings.iter().rev().find(|h| h.line < line)
}

/// Extract the heading level from a hierarchy item's detail field.
pub fn hierarchy_item_level(item: &lsp::HierarchyItem) -> u8 {
    item.detail
        .as_deref()
        .and_then(|d| d.strip_prefix('H'))
        .and_then(|n| n.parse::<u8>().ok())
        .unwrap_or(1)
}

/// Find the classified [`Link`] whose span matches a node span.
///
/// Bridges the gap between `find_link_at_offset` (which finds the tree node)
/// and the classified links from `Tree::links` (which resolve targets).
/// `abs_path` is the document's absolute path, so the classified target is
/// root-free (ticket server 11).
pub fn find_classified_link(
    tree: &crate::block::Tree,
    abs_path: &Path,
    node_span: Span,
) -> Option<crate::block::Link> {
    tree.links(abs_path)
        .into_iter()
        .find(|l| l.span == node_span)
}

/// Byte range `[start, content_end)` of 0-based `line` in `source`, excluding
/// the line's terminator. Recognizes `\n`, `\r\n`, and bare `\r`. A line past
/// the end of input yields an empty range at `source.len()`.
pub fn line_byte_range(source: &str, line: u32) -> (usize, usize) {
    let bytes = source.as_bytes();
    let mut idx = 0u32;
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let (is_break, next) = match bytes[i] {
            b'\n' => (true, i + 1),
            b'\r' => (
                true,
                if bytes.get(i + 1) == Some(&b'\n') {
                    i + 2
                } else {
                    i + 1
                },
            ),
            _ => (false, i + 1),
        };
        if is_break {
            if idx == line {
                return (start, i);
            }
            idx += 1;
            start = next;
        }
        i = next;
    }
    if idx == line {
        (start, bytes.len())
    } else {
        (bytes.len(), bytes.len())
    }
}

/// Convert an LSP 0-based position to a byte offset in `source`.
///
/// Recognizes `\n`, `\r\n`, and bare `\r`. `character` is a UTF-16 code-unit
/// offset within the line (the LSP default position encoding); it is walked
/// across the line's chars and clamped to the line's content length. A column
/// landing inside a surrogate pair rounds down to the enclosing char's start.
#[must_use]
pub fn lsp_position_to_byte_offset(source: &str, pos: lsp::Position) -> usize {
    let (start, end) = line_byte_range(source, pos.line);
    let mut remaining = pos.character as usize;
    let mut byte = start;
    for ch in source[start..end].chars() {
        let units = ch.len_utf16();
        if remaining < units {
            break;
        }
        remaining -= units;
        byte += ch.len_utf8();
    }
    byte
}

/// Convert a byte `Span` to an LSP `Range` through the file's cached
/// [`LineIndex`], so each endpoint is a binary search rather than an
/// `O(offset)` scan of `source`.
pub fn span_to_lsp_range(source: &str, index: &LineIndex, span: &Span) -> lsp::Range {
    let start = index.position(source, span.start);
    let end = index.position(source, span.end);
    lsp::Range { start, end }
}

/// Convert a byte offset to an LSP 0-based position.
///
/// Line counting recognizes `\n`, `\r\n`, and bare `\r`. The `character` field
/// is a UTF-16 code-unit offset within the line (the LSP default position
/// encoding), measured from the byte after the previous line break. A byte
/// offset that falls inside a multi-byte char is floored to that char's start
/// so the UTF-16 count cannot split a code point.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line/column values in markdown files won't exceed u32::MAX"
)]
#[must_use]
pub fn byte_offset_to_lsp_position(source: &str, offset: usize) -> lsp::Position {
    let mut offset = offset.min(source.len());
    while offset > 0 && !source.is_char_boundary(offset) {
        offset -= 1;
    }
    let line = (crate::block::byte_offset_to_line(source, offset) - 1) as u32;
    let line_start = source.as_bytes()[..offset]
        .iter()
        .rposition(|&b| b == b'\n' || b == b'\r')
        .map_or(0, |i| i + 1);
    let character = source[line_start..offset]
        .chars()
        .map(char::len_utf16)
        .sum::<usize>() as u32;
    lsp::Position { line, character }
}

/// Extract the normalized reference label from a link's source text,
/// if the link uses reference-style syntax.
///
/// Reference-style links look like `[text][label]`, `[text][]`, or `[text]`
/// (shortcut). Inline links contain `(` after the `]`.
///
/// Uses [`inline::find_matching_bracket`] for correct handling of nested
/// brackets, backslash escapes, and backtick spans.
pub fn link_ref_label(source: &str, span: &Span) -> Option<String> {
    let raw = &source[span.start..span.end];

    // Skip image prefix.
    let text = raw.strip_prefix('!').unwrap_or(raw);
    if !text.starts_with('[') {
        return None;
    }

    // Find the closing `]` for the link text.
    let text_close = crate::inline::find_matching_bracket(text.as_bytes(), 0)?;
    let after = &text[text_close + 1..];

    // Inline link: [text](url)
    if after.starts_with('(') {
        return None;
    }

    // Full reference: [text][label]
    if after.starts_with('[') {
        let label_start = 1;
        let label_end = after.find(']').unwrap_or(after.len());
        let label_text = &after[label_start..label_end];
        if label_text.is_empty() {
            // Collapsed reference [text][] — label is the link text
            let link_text = &text[1..text_close];
            return Some(normalize_label(link_text));
        }
        return Some(normalize_label(label_text));
    }

    // Shortcut reference: [text] — label is the link text
    let link_text = &text[1..text_close];
    Some(normalize_label(link_text))
}

/// Check if the byte offset falls on a `ReferenceDef` node, returning
/// its normalized label.
pub fn ref_def_label_at_offset(tree: &crate::block::Tree, offset: usize) -> Option<String> {
    for node in tree.nodes() {
        if let ElementKind::ReferenceDef { label, .. } = &node.kind
            && node.span.start <= offset
            && offset < node.span.end
        {
            return Some(label.clone());
        }
    }
    None
}

/// Get the text of a 0-based line in the source (recognizing `\n`, `\r\n`,
/// and bare `\r`), excluding the line terminator.
pub fn source_line_at(source: &str, lsp_line: u32) -> &str {
    let (start, end) = line_byte_range(source, lsp_line);
    &source[start..end]
}
