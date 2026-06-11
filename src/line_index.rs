// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Per-file line index: a precomputed map between byte offsets and LSP
//! `(line, UTF-16 character)` positions.
//!
//! Every diagnostic the server publishes converts a byte span into a UTF-16
//! line/character range. Done directly against the source, that walk is
//! `O(offset)` per conversion and is repeated for every diagnostic on every
//! materialization — the cost ticket perf 02 amortizes and ticket perf 01
//! removes at the source. A [`LineIndex`], built once per parse and cached on
//! [`crate::workspace::FileData`], turns each conversion into a binary search
//! over line starts plus a short within-line scan, reusable in both directions
//! (the inverse is the primitive the future incremental text-sync path needs to
//! map an incoming `{range}` back to byte offsets).
//!
//! The index carries no behaviour of its own: [`LineIndex::position`] is
//! byte-for-byte identical to [`crate::server::byte_offset_to_lsp_position`] and
//! [`LineIndex::offset`] to [`crate::server::lsp_position_to_byte_offset`],
//! including the degenerate `\r\n`-interior point. The shared
//! [`crate::invariants::assert_line_index_agrees`] invariant pins that
//! equivalence generatively, so the cached fast path cannot drift from the
//! scalar reference the rest of the crate already trusts.

use crate::lsp;

/// A precomputed map between byte offsets and LSP positions for one document.
///
/// Holds the byte offset at which each line begins. Line breaks are `\n`,
/// `\r\n`, and bare `\r` — the same accounting as
/// [`crate::fm::count_line_breaks`] — so a trailing terminator yields a final
/// empty line whose start is `source.len()`, matching the crate-wide line
/// counting. Rebuilt only when its file reparses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineIndex {
    /// Byte offset of the start of each line. Always begins with `0` and is
    /// strictly increasing; the last entry equals `source.len()` exactly when
    /// the source ends in a line terminator.
    line_starts: Vec<usize>,
}

impl Default for LineIndex {
    /// The index of the empty document: a single line starting at offset `0`.
    /// `Vec::new()` would be an *invalid* index (no line 0), so this is spelled
    /// out rather than derived — it is the fallback for the rare unindexed-file
    /// path in diagnostic materialization.
    fn default() -> Self {
        Self {
            line_starts: vec![0],
        }
    }
}

impl LineIndex {
    /// Build an index from `source` in a single pass over its bytes.
    #[must_use]
    pub fn new(source: &str) -> Self {
        let bytes = source.as_bytes();
        let mut line_starts = vec![0usize];
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'\n' => {
                    i += 1;
                    line_starts.push(i);
                }
                b'\r' => {
                    // A `\r\n` pair is one break; a bare `\r` is also one.
                    i += if bytes.get(i + 1) == Some(&b'\n') {
                        2
                    } else {
                        1
                    };
                    line_starts.push(i);
                }
                _ => i += 1,
            }
        }
        Self { line_starts }
    }

    /// Convert a byte offset to an LSP 0-based position.
    ///
    /// Mirrors [`crate::server::byte_offset_to_lsp_position`] exactly: an offset
    /// inside a multi-byte char is floored to that char's start so the UTF-16
    /// count cannot split a code point, and an offset wedged between the `\r` and
    /// `\n` of a CRLF reads as column 0 of the line the `\r` opens (the `\r`
    /// counts as a bare break when the `\n` is out of view) — the one degenerate
    /// point the round-trip excludes.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "line/column values in markdown files won't exceed u32::MAX"
    )]
    #[must_use]
    pub fn position(&self, source: &str, offset: usize) -> lsp::Position {
        let mut offset = offset.min(source.len());
        while offset > 0 && !source.is_char_boundary(offset) {
            offset -= 1;
        }
        let bytes = source.as_bytes();
        if offset > 0 && bytes[offset - 1] == b'\r' && bytes.get(offset) == Some(&b'\n') {
            // CRLF interior: the preceding `\r` reads as a line break, so this
            // offset is column 0 of the line it opens. `line_starts` folds the
            // pair into the next line, so count the breaks strictly before this
            // offset (those at or before the `\r`) instead.
            let line = self.line_starts.partition_point(|&s| s < offset) as u32;
            return lsp::Position { line, character: 0 };
        }
        // The 0-based line is the count of line starts at or before `offset`,
        // less the leading `0` — a binary search, not an O(offset) scan.
        let line = self.line_starts.partition_point(|&s| s <= offset) - 1;
        let line_start = self.line_starts[line];
        let character = source[line_start..offset]
            .chars()
            .map(char::len_utf16)
            .sum::<usize>() as u32;
        lsp::Position {
            line: line as u32,
            character,
        }
    }

    /// Convert an LSP 0-based position to a byte offset.
    ///
    /// Mirrors [`crate::server::lsp_position_to_byte_offset`]: `character` is a
    /// UTF-16 code-unit offset within the line, walked across the line's content
    /// and clamped to its length; a column landing inside a surrogate pair rounds
    /// down to the enclosing char's start; a line past the end of input maps to
    /// `source.len()`.
    #[must_use]
    #[allow(
        dead_code,
        reason = "the inverse lookup is added for the future incremental text-sync path (ticket perf 05 / issue 014); it has no production caller yet, but the round-trip invariant and unit tests exercise it under test/fuzzing"
    )]
    pub fn offset(&self, source: &str, pos: lsp::Position) -> usize {
        let Some(&start) = self.line_starts.get(pos.line as usize) else {
            return source.len();
        };
        let mut remaining = pos.character as usize;
        let mut byte = start;
        for ch in source[start..].chars() {
            // The line's content ends at its terminator; never walk past it.
            if ch == '\n' || ch == '\r' {
                break;
            }
            let units = ch.len_utf16();
            if remaining < units {
                break;
            }
            remaining -= units;
            byte += ch.len_utf8();
        }
        byte
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for clarity per project standards"
)]
mod tests {
    use super::LineIndex;
    use crate::invariants::assert_line_index_agrees;
    use crate::lsp;

    /// Build the index and assert it is a byte-for-byte drop-in for the scalar
    /// conversions over `source` (forward agreement plus index round-trip).
    fn check(source: &str) {
        assert_line_index_agrees(source, &LineIndex::new(source));
    }

    #[test]
    fn agrees_on_crlf_bare_cr_and_multibyte() {
        check("ab\r\ncd\r\nef");
        check("ab\rcd\ref");
        check("aé b\nx");
        check("# café 😀 header\r\nsecond λ line\n");
        check("");
        check("no trailing newline");
        check("trailing\n");
        check("\r\n\r\n");
    }

    #[test]
    fn crlf_interior_matches_scalar_quirk() {
        // Byte 2 sits between the `\r` (byte 1) and `\n` (byte 2): column 0 of
        // the line the CR opens, exactly as the scalar path reports it.
        let src = "a\r\nb";
        let index = LineIndex::new(src);
        let pos = index.position(src, 2);
        assert_eq!(
            (pos.line, pos.character),
            (1, 0),
            "CRLF-interior offset is line 1 column 0, matching the scalar conversion"
        );
    }

    #[test]
    fn position_past_eof_clamps_to_end() {
        let src = "abc\n";
        let index = LineIndex::new(src);
        let pos = index.position(src, 999);
        let back = index.offset(src, pos);
        assert_eq!(
            back,
            src.len(),
            "an offset past EOF clamps to source length and round-trips"
        );
    }

    #[test]
    fn offset_past_last_line_is_source_len() {
        let src = "one\ntwo";
        let index = LineIndex::new(src);
        let off = index.offset(
            src,
            lsp::Position {
                line: 99,
                character: 0,
            },
        );
        assert_eq!(
            off,
            src.len(),
            "a line past the end of input maps to source length"
        );
    }
}
