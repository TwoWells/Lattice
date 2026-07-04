// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Span-aware JSON frontmatter parser.
//!
//! Parses brace-delimited JSON frontmatter (Hugo convention): the document
//! starts with `{` at byte 0 (after BOM stripping), and the parser reads
//! until the matching `}`. Body parsing starts on the line after the
//! closing brace.
//!
//! Produces a tree of [`FmNode`] values where every node carries a [`Span`]
//! back into the original source text.

use crate::fm::{self, FmDiagnostic, FmNode, FmSeverity, FmValue, FrontmatterBlock, ScalarSpan};
use crate::span::Span;

// ---------------------------------------------------------------------------
// Brace-depth scanning
// ---------------------------------------------------------------------------

/// Find the byte offset of the matching `}` at brace depth 0 in `source`.
///
/// Tracks depth across `{` / `}` while skipping characters inside strings.
/// Returns `None` if no matching brace is found.
fn find_closing_brace(source: &str) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut depth: usize = 0;
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                // Skip string content (including escaped characters).
                // Break on unescaped newline (unclosed string recovery).
                i += 1;
                while i < bytes.len() {
                    match bytes[i] {
                        b'\\' => i += 2, // skip escaped char
                        b'"' => {
                            i += 1;
                            break;
                        }
                        b'\n' | b'\r' => {
                            // Unclosed string — break out to resume brace tracking.
                            break;
                        }
                        _ => i += 1,
                    }
                }
            }
            b'{' => {
                depth += 1;
                i += 1;
            }
            b'}' => {
                if depth == 1 {
                    return Some(i);
                }
                depth = depth.saturating_sub(1);
                i += 1;
            }
            _ => i += 1,
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// Internal parser state.
struct Parser<'a> {
    /// Source bytes (full JSON object including outer braces).
    src: &'a [u8],
    /// Current byte position within `src`.
    pos: usize,
    /// Base offset to add to all spans (accounts for BOM).
    base: usize,
    /// Collected diagnostics.
    diagnostics: Vec<FmDiagnostic>,
    /// Current object/array nesting depth (for the depth limit).
    depth: usize,
    /// Whether the nesting-depth diagnostic has already been emitted.
    depth_limit_hit: bool,
}

impl<'a> Parser<'a> {
    fn new(content: &'a str, base: usize) -> Self {
        Self {
            src: content.as_bytes(),
            pos: 0,
            base,
            diagnostics: Vec::new(),
            depth: 0,
            depth_limit_hit: false,
        }
    }

    // -- Helpers ----------------------------------------------------------

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn advance(&mut self) -> Option<u8> {
        let b = self.src.get(self.pos).copied()?;
        self.pos += 1;
        Some(b)
    }

    fn abs(&self) -> usize {
        self.base + self.pos
    }

    fn emit(&mut self, span: Span, severity: FmSeverity, message: String) {
        self.diagnostics.push(FmDiagnostic {
            span,
            severity,
            message,
        });
    }

    /// A span covering the single character at the current position, clamped
    /// to the source end so an at-EOF "expected X" diagnostic collapses to an
    /// empty span instead of pointing one byte past the input.
    ///
    /// The span covers the character's full UTF-8 encoding: a fixed one-byte
    /// span would end mid-character when the unexpected character is
    /// multi-byte, violating the char-boundary invariant every diagnostic
    /// span must satisfy (`fuzz_structural` soak finding). The source is valid
    /// UTF-8 and the parser stops at character starts, so the lead byte
    /// determines the length; a continuation byte (impossible at a character
    /// start) degrades to an empty span rather than risk a mid-character end.
    fn here_span(&self) -> Span {
        let len = self.src.get(self.pos).map_or(0, |&b| match b {
            0x00..=0x7F => 1,
            0x80..=0xBF => 0,
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xFF => 4,
        });
        let start = self.abs();
        Span::new(start, (start + len).min(self.base + self.src.len()))
    }

    /// Skip JSON whitespace (space, tab, CR, LF).
    fn skip_ws(&mut self) {
        while let Some(b) = self.peek() {
            if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    /// Skip to the next recovery point: `,`, `}`, `]`, or end of input.
    fn skip_to_recovery(&mut self) {
        while let Some(b) = self.peek() {
            match b {
                b',' | b'}' | b']' => return,
                _ => self.pos += 1,
            }
        }
    }

    // -- String parsing ---------------------------------------------------

    /// Parse a JSON string. The current position must be at the opening `"`.
    fn parse_string(&mut self) -> ScalarSpan {
        let abs_start = self.abs();
        self.pos += 1; // skip opening "

        let mut text = String::new();
        loop {
            match self.advance() {
                None | Some(b'\n' | b'\r') => {
                    self.emit(
                        Span::new(abs_start, self.abs()),
                        FmSeverity::Error,
                        "unclosed string".into(),
                    );
                    break;
                }
                Some(b'"') => break,
                Some(b'\\') => self.parse_escape(&mut text, abs_start),
                Some(_) => self.push_char(&mut text),
            }
        }

        ScalarSpan {
            span: Span::new(abs_start, self.abs()),
            text,
        }
    }

    /// Append the whole UTF-8 character whose lead byte `advance` just
    /// consumed (now at `self.pos - 1`), advancing past its continuation
    /// bytes so a multi-byte character is stored intact rather than as
    /// per-byte mojibake.
    fn push_char(&mut self, text: &mut String) {
        self.pos = fm::push_utf8_char(text, self.src, self.pos - 1);
    }

    /// Parse a JSON escape sequence. Position is right after the backslash.
    fn parse_escape(&mut self, text: &mut String, string_start: usize) {
        match self.advance() {
            None => {}
            Some(b'"') => text.push('"'),
            Some(b'\\') => text.push('\\'),
            Some(b'/') => text.push('/'),
            Some(b'b') => text.push('\u{0008}'),
            Some(b'f') => text.push('\u{000C}'),
            Some(b'n') => text.push('\n'),
            Some(b'r') => text.push('\r'),
            Some(b't') => text.push('\t'),
            Some(b'u') => {
                if let Some(code) = self.parse_hex4() {
                    // Check for surrogate pair (high surrogate D800-DBFF).
                    if (0xD800..=0xDBFF).contains(&code) {
                        // Expect \uXXXX low surrogate.
                        if self.peek() == Some(b'\\')
                            && self.src.get(self.pos + 1).copied() == Some(b'u')
                        {
                            self.pos += 2; // skip \u
                            if let Some(low) = self.parse_hex4() {
                                if (0xDC00..=0xDFFF).contains(&low) {
                                    let combined =
                                        0x1_0000 + ((code - 0xD800) << 10) + (low - 0xDC00);
                                    if let Some(ch) = char::from_u32(combined) {
                                        text.push(ch);
                                    } else {
                                        self.emit(
                                            Span::new(string_start, self.abs()),
                                            FmSeverity::Error,
                                            "invalid surrogate pair".into(),
                                        );
                                    }
                                } else {
                                    self.emit(
                                        Span::new(string_start, self.abs()),
                                        FmSeverity::Error,
                                        "expected low surrogate after high surrogate".into(),
                                    );
                                }
                            } else {
                                self.emit(
                                    Span::new(string_start, self.abs()),
                                    FmSeverity::Error,
                                    "invalid \\uXXXX escape in surrogate pair".into(),
                                );
                            }
                        } else {
                            self.emit(
                                Span::new(string_start, self.abs()),
                                FmSeverity::Error,
                                "expected low surrogate after high surrogate".into(),
                            );
                        }
                    } else if (0xDC00..=0xDFFF).contains(&code) {
                        self.emit(
                            Span::new(string_start, self.abs()),
                            FmSeverity::Error,
                            "unexpected low surrogate without high surrogate".into(),
                        );
                    } else if let Some(ch) = char::from_u32(code) {
                        text.push(ch);
                    } else {
                        self.emit(
                            Span::new(string_start, self.abs()),
                            FmSeverity::Error,
                            "invalid \\uXXXX escape".into(),
                        );
                    }
                } else {
                    self.emit(
                        Span::new(string_start, self.abs()),
                        FmSeverity::Error,
                        "invalid \\uXXXX escape".into(),
                    );
                }
            }
            Some(_) => {
                self.emit(
                    Span::new(string_start, self.abs()),
                    FmSeverity::Error,
                    "unknown escape sequence".into(),
                );
            }
        }
    }

    /// Parse four hex digits for a `\uXXXX` escape, returning the raw code point.
    ///
    /// Returns the raw `u32` value (may be a surrogate — caller handles pairing).
    fn parse_hex4(&mut self) -> Option<u32> {
        let mut hex = String::with_capacity(4);
        for _ in 0..4 {
            let b = self.advance()?;
            if b.is_ascii_hexdigit() {
                hex.push(b as char);
            } else {
                return None;
            }
        }
        u32::from_str_radix(&hex, 16).ok()
    }

    // -- Value parsing ----------------------------------------------------

    /// Parse a JSON value (string, number, boolean, null, object, or array).
    fn parse_value(&mut self) -> Option<FmValue> {
        self.skip_ws();
        match self.peek() {
            Some(b'"') => {
                let scalar = self.parse_string();
                Some(FmValue::Scalar(scalar))
            }
            Some(b'{') => Some(self.parse_nested(Self::parse_object)),
            Some(b'[') => Some(self.parse_nested(Self::parse_array)),
            Some(b't') => Some(self.parse_literal("true")),
            Some(b'f') => Some(self.parse_literal("false")),
            Some(b'n') => Some(self.parse_literal("null")),
            Some(b'-' | b'0'..=b'9') => Some(self.parse_number()),
            Some(_) => {
                let start = self.abs();
                self.skip_to_recovery();
                self.emit(
                    Span::new(start, self.abs()),
                    FmSeverity::Error,
                    "unexpected token".into(),
                );
                None
            }
            None => None,
        }
    }

    /// Run a nested object/array parser under the depth limit.
    ///
    /// Position is at the opening `{` or `[`. Below the limit, depth is
    /// incremented around `parse` so nested values are tracked. At the limit
    /// the collection is skipped as opaque (its bytes are consumed so the
    /// position stays synchronized) and a single diagnostic is emitted, so
    /// adversarial nesting can neither overflow the stack nor desync parsing.
    fn parse_nested(&mut self, parse: fn(&mut Self) -> FmValue) -> FmValue {
        if self.depth >= crate::limits::MAX_FRONTMATTER_NESTING {
            self.note_depth_limit();
            let start = self.abs();
            self.skip_balanced();
            return FmValue::Scalar(ScalarSpan {
                span: Span::new(start, self.abs()),
                text: String::new(),
            });
        }
        self.depth += 1;
        let value = parse(self);
        self.depth -= 1;
        value
    }

    /// Emit the nesting-depth diagnostic at most once.
    fn note_depth_limit(&mut self) {
        if !self.depth_limit_hit {
            self.depth_limit_hit = true;
            let pos = self.abs();
            self.emit(
                Span::new(pos, pos),
                FmSeverity::Warning,
                format!(
                    "JSON nesting exceeds the limit of {}; deeper structure is flattened",
                    crate::limits::MAX_FRONTMATTER_NESTING
                ),
            );
        }
    }

    /// Consume a brace/bracket-balanced region starting at the current `{`
    /// or `[`, skipping string contents. Used to discard over-deep structure
    /// without recursing. Always makes forward progress.
    fn skip_balanced(&mut self) {
        let mut depth = 0usize;
        while let Some(b) = self.peek() {
            match b {
                b'"' => self.skip_string_raw(),
                b'{' | b'[' => {
                    depth += 1;
                    self.pos += 1;
                }
                b'}' | b']' => {
                    self.pos += 1;
                    depth -= 1;
                    if depth == 0 {
                        return;
                    }
                }
                _ => self.pos += 1,
            }
        }
    }

    /// Skip a JSON string starting at the opening `"`, honoring `\` escapes.
    fn skip_string_raw(&mut self) {
        self.pos += 1; // opening quote
        while let Some(b) = self.peek() {
            match b {
                b'\\' => {
                    self.pos = (self.pos + 2).min(self.src.len());
                }
                b'"' => {
                    self.pos += 1;
                    return;
                }
                b'\n' | b'\r' => return,
                _ => self.pos += 1,
            }
        }
    }

    /// Parse a JSON literal (`true`, `false`, or `null`).
    fn parse_literal(&mut self, expected: &str) -> FmValue {
        let abs_start = self.abs();
        let start = self.pos;

        for &expected_byte in expected.as_bytes() {
            match self.advance() {
                Some(b) if b == expected_byte => {}
                _ => {
                    self.emit(
                        Span::new(abs_start, self.abs()),
                        FmSeverity::Error,
                        format!("expected `{expected}`"),
                    );
                    self.skip_to_recovery();
                    return FmValue::Scalar(ScalarSpan {
                        span: Span::new(abs_start, self.abs()),
                        text: String::from_utf8_lossy(&self.src[start..self.pos]).to_string(),
                    });
                }
            }
        }

        FmValue::Scalar(ScalarSpan {
            span: Span::new(abs_start, self.abs()),
            text: expected.to_string(),
        })
    }

    /// Parse a JSON number (integer or float).
    fn parse_number(&mut self) -> FmValue {
        let abs_start = self.abs();
        let start = self.pos;

        // Optional leading minus.
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }

        // Integer part.
        self.consume_digits();

        // Fractional part.
        if self.peek() == Some(b'.') {
            self.pos += 1;
            self.consume_digits();
        }

        // Exponent.
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            self.consume_digits();
        }

        let text = String::from_utf8_lossy(&self.src[start..self.pos]).to_string();
        FmValue::Scalar(ScalarSpan {
            span: Span::new(abs_start, self.abs()),
            text,
        })
    }

    /// Consume a run of ASCII digits.
    fn consume_digits(&mut self) {
        while let Some(b'0'..=b'9') = self.peek() {
            self.pos += 1;
        }
    }

    /// Parse a JSON object. Position must be at `{`.
    fn parse_object(&mut self) -> FmValue {
        let abs_start = self.abs();
        self.pos += 1; // skip '{'

        let mut entries = Vec::new();
        let mut expect_comma = false;

        loop {
            self.skip_ws();

            match self.peek() {
                None => {
                    self.emit(
                        Span::new(abs_start, self.abs()),
                        FmSeverity::Error,
                        "unclosed object".into(),
                    );
                    break;
                }
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                Some(b',') => {
                    self.pos += 1;
                    expect_comma = false;

                    // Check for trailing comma.
                    self.skip_ws();
                    if self.peek() == Some(b'}') {
                        self.emit(
                            Span::new(self.abs() - 1, self.abs()),
                            FmSeverity::Warning,
                            "trailing comma in object".into(),
                        );
                        self.pos += 1;
                        break;
                    }
                }
                _ => {
                    if expect_comma {
                        self.emit(
                            Span::new(self.abs(), self.abs() + 1),
                            FmSeverity::Warning,
                            "missing comma between entries".into(),
                        );
                    }

                    let arm_start = self.abs();
                    if let Some(entry) = self.parse_object_entry() {
                        entries.push(entry);
                    }
                    if self.abs() == arm_start {
                        // Forward-progress guard: a stray `]` leaves
                        // `skip_to_recovery` parked on a stop byte it won't
                        // consume, so no entry is produced and nothing
                        // advances. Skip one byte so the loop terminates.
                        self.pos += 1;
                    }
                    expect_comma = true;
                }
            }
        }

        FmValue::Mapping(entries)
    }

    /// Parse a single `"key": value` entry in a JSON object.
    fn parse_object_entry(&mut self) -> Option<FmNode> {
        let entry_start = self.abs();

        // Key must be a string.
        if self.peek() != Some(b'"') {
            self.emit(
                self.here_span(),
                FmSeverity::Error,
                "expected string key".into(),
            );
            self.skip_to_recovery();
            return None;
        }

        let key = self.parse_string();

        // Expect colon.
        self.skip_ws();
        if self.peek() == Some(b':') {
            self.pos += 1;
        } else {
            self.emit(
                self.here_span(),
                FmSeverity::Error,
                "expected ':' after key".into(),
            );
            self.skip_to_recovery();
            return None;
        }

        // Value.
        self.skip_ws();
        let value = self.parse_value().unwrap_or_else(|| {
            FmValue::Scalar(ScalarSpan {
                span: Span::new(self.abs(), self.abs()),
                text: String::new(),
            })
        });

        let entry_end = self.abs();

        Some(FmNode::Mapping {
            key,
            value,
            span: Span::new(entry_start, entry_end),
        })
    }

    /// Parse a JSON array. Position must be at `[`.
    fn parse_array(&mut self) -> FmValue {
        let abs_start = self.abs();
        self.pos += 1; // skip '['

        let mut items = Vec::new();
        let mut expect_comma = false;

        loop {
            self.skip_ws();

            match self.peek() {
                None => {
                    self.emit(
                        Span::new(abs_start, self.abs()),
                        FmSeverity::Error,
                        "unclosed array".into(),
                    );
                    break;
                }
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                Some(b',') => {
                    self.pos += 1;
                    expect_comma = false;

                    // Check for trailing comma.
                    self.skip_ws();
                    if self.peek() == Some(b']') {
                        self.emit(
                            Span::new(self.abs() - 1, self.abs()),
                            FmSeverity::Warning,
                            "trailing comma in array".into(),
                        );
                        self.pos += 1;
                        break;
                    }
                }
                _ => {
                    if expect_comma {
                        self.emit(
                            Span::new(self.abs(), self.abs() + 1),
                            FmSeverity::Warning,
                            "missing comma between elements".into(),
                        );
                    }

                    let item_start = self.abs();
                    if let Some(value) = self.parse_value() {
                        items.push(FmNode::SequenceItem {
                            value,
                            span: Span::new(item_start, self.abs()),
                        });
                    }
                    if self.abs() == item_start {
                        // Forward-progress guard: a stray `}` inside `[...]` is
                        // rejected by `parse_value` without consuming. Skip it
                        // so the loop cannot spin forever allocating.
                        self.pos += 1;
                    }
                    expect_comma = true;
                }
            }
        }

        FmValue::Sequence(items)
    }

    // -- Top-level --------------------------------------------------------

    /// Parse the top-level JSON object. Returns the entries.
    fn parse_top_level(&mut self) -> Vec<FmNode> {
        self.skip_ws();

        if self.peek() != Some(b'{') {
            return Vec::new();
        }

        match self.parse_object() {
            FmValue::Mapping(entries) => entries,
            _ => Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Parse JSON frontmatter from the start of a markdown document.
///
/// Returns `None` if the document does not start with `{` (after BOM
/// stripping). Returns `Some(block)` with any parse diagnostics if
/// JSON frontmatter is present.
///
/// The `{` must be at byte 0. The parser tracks brace depth to find the
/// matching `}`. The body starts on the line after the closing `}`.
#[must_use]
pub fn parse_frontmatter_block(source: &str) -> Option<FrontmatterBlock> {
    let (stripped, bom_offset) = fm::strip_bom(source);

    // Must start with `{`.
    if !stripped.starts_with('{') {
        return None;
    }

    // Find the matching `}` via brace-depth scanning.
    let closing_pos = find_closing_brace(stripped)?;

    // The JSON content is from byte 0 through the closing brace (inclusive).
    let json_content = &stripped[..=closing_pos];
    let content_start = bom_offset;
    let content_end = bom_offset + closing_pos + 1;

    // Block span covers from `{` through the line after `}`.
    // Find end of the closing brace's line.
    let after_brace = closing_pos + 1;
    let block_end = bom_offset
        + if stripped.as_bytes().get(after_brace) == Some(&b'\r')
            && stripped.as_bytes().get(after_brace + 1) == Some(&b'\n')
        {
            after_brace + 2
        } else if matches!(stripped.as_bytes().get(after_brace), Some(&b'\n' | &b'\r')) {
            // Bare `\n` or bare `\r` (legacy Mac) line ending.
            after_brace + 1
        } else {
            after_brace // `}` at EOF
        };

    // Size limit: an enormous block is treated as opaque and skipped, so the
    // parser never walks a multi-megabyte frontmatter region.
    if json_content.len() > crate::limits::MAX_FRONTMATTER_BYTES {
        return Some(FrontmatterBlock {
            span: Span::new(bom_offset, block_end),
            content_span: Span::new(content_start, content_end),
            entries: Vec::new(),
            diagnostics: vec![FmDiagnostic {
                span: Span::new(content_start, content_start),
                severity: FmSeverity::Warning,
                message: format!(
                    "frontmatter exceeds the {}-byte limit; skipped",
                    crate::limits::MAX_FRONTMATTER_BYTES
                ),
            }],
        });
    }

    // Parse.
    let mut parser = Parser::new(json_content, content_start);
    let entries = parser.parse_top_level();
    let diagnostics = parser.diagnostics;

    // If we found no valid entries and had errors, discard the block.
    if entries.is_empty() && diagnostics.iter().any(|d| d.severity == FmSeverity::Error) {
        return None;
    }

    Some(FrontmatterBlock {
        span: Span::new(bom_offset, block_end),
        content_span: Span::new(content_start, content_end),
        entries,
        diagnostics,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for clarity")]
#[allow(clippy::panic, reason = "tests use panic for unreachable match arms")]
mod tests {
    use super::*;
    use crate::fm::{extract_backlinks, find_predicate_line};

    // -- Regression: fuzz findings ---------------------------------------

    #[test]
    fn diagnostic_spans_stay_in_bounds_at_eof() {
        // Regression (fuzz_json, ticket 22): a malformed object whose parse
        // desyncs to EOF emitted "expected ':' after key" / "expected string
        // key" diagnostics built as `pos + 1` — one byte past the source. The
        // "expected X" sites now use `here_span`, which clamps the one-byte
        // span to the source end. The committed corpus seed is the byte-exact
        // reproducer.
        let bytes = include_bytes!("../fuzz/corpus/fuzz_json/eof_diag_overshoot.json");
        let source = std::str::from_utf8(bytes).expect("seed is valid UTF-8");
        let block = parse_frontmatter_block(source).expect("recognized as JSON frontmatter");
        crate::invariants::assert_block_wellformed(&block, source);

        // Targeted cases for the two emit sites: a key with no following colon,
        // and `{` with no key — both desyncing toward the closing brace.
        for src in ["{\"k\"\"v\"}", "{\"k\" }", "{ }", "{\"a\":1 \"b\"}"] {
            if let Some(block) = parse_frontmatter_block(src) {
                crate::invariants::assert_block_wellformed(&block, src);
            }
        }
    }

    // -- Detection --------------------------------------------------------

    #[test]
    fn no_json_frontmatter() {
        let source = "# Just a heading\n";
        assert!(
            parse_frontmatter_block(source).is_none(),
            "no JSON frontmatter should return None"
        );
    }

    #[test]
    fn yaml_delimiters_not_json() {
        let source = "---\ntitle: test\n---\n";
        assert!(
            parse_frontmatter_block(source).is_none(),
            "YAML delimiters should not parse as JSON"
        );
    }

    #[test]
    fn toml_delimiters_not_json() {
        let source = "+++\ntitle = \"test\"\n+++\n";
        assert!(
            parse_frontmatter_block(source).is_none(),
            "TOML delimiters should not parse as JSON"
        );
    }

    #[test]
    fn brace_at_byte_0() {
        let source = "{\n  \"title\": \"test\"\n}\n# Heading\n";
        let block = parse_frontmatter_block(source).expect("should parse JSON frontmatter");
        assert_eq!(block.entries.len(), 1, "should have one entry");
    }

    #[test]
    fn bom_before_brace() {
        let source = "\u{FEFF}{\n  \"title\": \"test\"\n}\n";
        let block = parse_frontmatter_block(source).expect("should parse JSON with BOM");
        assert!(
            block.diagnostics.is_empty(),
            "BOM JSON should have no error diagnostics: {:?}",
            block.diagnostics
        );
        assert_eq!(block.entries.len(), 1, "should have one entry");
        assert_eq!(block.span.start, 3, "span should start after BOM");
    }

    #[test]
    fn document_not_starting_with_brace() {
        let source = " { \"title\": \"test\" }\n";
        assert!(
            parse_frontmatter_block(source).is_none(),
            "leading space means no frontmatter"
        );
    }

    // -- Simple key-value pairs -------------------------------------------

    #[test]
    fn string_value() {
        let source = "{\n  \"title\": \"My Document\"\n}\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        assert_eq!(block.entries.len(), 1, "should have one entry");

        if let FmNode::Mapping { key, value, .. } = &block.entries[0] {
            assert_eq!(key.text, "title", "key text");
            if let FmValue::Scalar(s) = value {
                assert_eq!(s.text, "My Document", "value text");
            } else {
                panic!("value should be scalar");
            }
        } else {
            panic!("entry should be mapping");
        }
    }

    #[test]
    fn number_value() {
        let source = "{\n  \"count\": 42\n}\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let FmNode::Mapping { value, .. } = &block.entries[0] {
            if let FmValue::Scalar(s) = value {
                assert_eq!(s.text, "42", "integer stored as text");
            } else {
                panic!("should be scalar");
            }
        } else {
            panic!("should be mapping");
        }
    }

    #[test]
    fn float_value() {
        let source = "{\n  \"pi\": 3.14\n}\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let FmNode::Mapping { value, .. } = &block.entries[0] {
            if let FmValue::Scalar(s) = value {
                assert_eq!(s.text, "3.14", "float value");
            } else {
                panic!("should be scalar");
            }
        } else {
            panic!("should be mapping");
        }
    }

    #[test]
    fn boolean_values() {
        let source = "{\n  \"enabled\": true,\n  \"disabled\": false\n}\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        assert_eq!(block.entries.len(), 2, "two entries");

        if let FmNode::Mapping { value, .. } = &block.entries[0] {
            if let FmValue::Scalar(s) = value {
                assert_eq!(s.text, "true", "boolean true");
            } else {
                panic!("should be scalar");
            }
        } else {
            panic!("should be mapping");
        }
    }

    #[test]
    fn null_value() {
        let source = "{\n  \"empty\": null\n}\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let FmNode::Mapping { value, .. } = &block.entries[0] {
            if let FmValue::Scalar(s) = value {
                assert_eq!(s.text, "null", "null value");
            } else {
                panic!("should be scalar");
            }
        } else {
            panic!("should be mapping");
        }
    }

    // -- Nested objects ---------------------------------------------------

    #[test]
    fn nested_object() {
        let source = "{\n  \"meta\": {\n    \"author\": \"test\"\n  }\n}\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let FmNode::Mapping { key, value, .. } = &block.entries[0] {
            assert_eq!(key.text, "meta", "outer key");
            if let FmValue::Mapping(children) = value {
                assert_eq!(children.len(), 1, "one nested entry");
                if let FmNode::Mapping {
                    key: inner_key,
                    value: inner_value,
                    ..
                } = &children[0]
                {
                    assert_eq!(inner_key.text, "author", "inner key");
                    if let FmValue::Scalar(s) = inner_value {
                        assert_eq!(s.text, "test", "inner value");
                    } else {
                        panic!("inner value should be scalar");
                    }
                } else {
                    panic!("inner should be mapping");
                }
            } else {
                panic!("value should be mapping (nested object)");
            }
        } else {
            panic!("entry should be mapping");
        }
    }

    // -- Arrays -----------------------------------------------------------

    #[test]
    fn array_of_strings() {
        let source = "{\n  \"tags\": [\"rust\", \"lsp\"]\n}\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let FmNode::Mapping { value, .. } = &block.entries[0] {
            if let FmValue::Sequence(items) = value {
                assert_eq!(items.len(), 2, "two items");
                if let FmNode::SequenceItem {
                    value: FmValue::Scalar(s),
                    ..
                } = &items[0]
                {
                    assert_eq!(s.text, "rust", "first item");
                } else {
                    panic!("item should be scalar sequence item");
                }
            } else {
                panic!("value should be sequence");
            }
        } else {
            panic!("should be mapping");
        }
    }

    #[test]
    fn empty_array() {
        let source = "{\n  \"tags\": []\n}\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let FmNode::Mapping { value, .. } = &block.entries[0] {
            if let FmValue::Sequence(items) = value {
                assert!(items.is_empty(), "should be empty");
            } else {
                panic!("value should be sequence");
            }
        } else {
            panic!("should be mapping");
        }
    }

    #[test]
    fn deeply_nested() {
        let source = "{\n  \"a\": {\n    \"b\": {\n      \"c\": [1, 2, 3]\n    }\n  }\n}\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let FmNode::Mapping { value, .. } = &block.entries[0] {
            if let FmValue::Mapping(b) = value {
                if let FmNode::Mapping { value: bval, .. } = &b[0] {
                    if let FmValue::Mapping(c) = bval {
                        if let FmNode::Mapping { value: cval, .. } = &c[0] {
                            assert!(
                                matches!(cval, FmValue::Sequence(_)),
                                "deepest value should be array"
                            );
                        } else {
                            panic!("c should be mapping");
                        }
                    } else {
                        panic!("b value should be mapping");
                    }
                } else {
                    panic!("b should be mapping");
                }
            } else {
                panic!("a value should be mapping");
            }
        } else {
            panic!("should be mapping");
        }
    }

    // -- String escapes ---------------------------------------------------

    #[test]
    fn string_escapes() {
        let source = "{\n  \"val\": \"line1\\nline2\\ttab\"\n}\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let FmNode::Mapping { value, .. } = &block.entries[0] {
            if let FmValue::Scalar(s) = value {
                assert_eq!(s.text, "line1\nline2\ttab", "escape sequences");
            } else {
                panic!("should be scalar");
            }
        } else {
            panic!("should be mapping");
        }
    }

    #[test]
    fn unicode_escape_4() {
        let source = "{\n  \"val\": \"\\u0041\"\n}\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let FmNode::Mapping { value, .. } = &block.entries[0] {
            if let FmValue::Scalar(s) = value {
                assert_eq!(s.text, "A", "\\u0041 should be 'A'");
            } else {
                panic!("should be scalar");
            }
        } else {
            panic!("should be mapping");
        }
    }

    #[test]
    fn surrogate_pair() {
        // U+1F600 GRINNING FACE = \uD83D\uDE00
        let source = "{\n  \"val\": \"\\uD83D\\uDE00\"\n}\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let FmNode::Mapping { value, .. } = &block.entries[0] {
            if let FmValue::Scalar(s) = value {
                assert_eq!(
                    s.text, "\u{1F600}",
                    "surrogate pair should decode to grinning face"
                );
            } else {
                panic!("should be scalar");
            }
        } else {
            panic!("should be mapping");
        }
    }

    #[test]
    fn slash_escape() {
        let source = "{\n  \"val\": \"a\\/b\"\n}\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let FmNode::Mapping { value, .. } = &block.entries[0] {
            if let FmValue::Scalar(s) = value {
                assert_eq!(s.text, "a/b", "escaped slash");
            } else {
                panic!("should be scalar");
            }
        } else {
            panic!("should be mapping");
        }
    }

    // -- Backlinks --------------------------------------------------------

    #[test]
    fn backlinks_from_json() {
        let source = "{\n  \"backlinks\": {\n    \"superseded_by\": [\"decisions/26.md\"],\n    \"amended_by\": [\"decisions/26.md\", \"tickets/14h.md\"]\n  }\n}\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        let bl = extract_backlinks(&block, source);

        assert_eq!(bl.len(), 2, "two predicates");
        assert_eq!(
            bl.get("superseded_by"),
            Some(&vec!["decisions/26.md".to_string()]),
            "superseded_by"
        );
        assert_eq!(
            bl.get("amended_by"),
            Some(&vec![
                "decisions/26.md".to_string(),
                "tickets/14h.md".to_string()
            ]),
            "amended_by"
        );
    }

    #[test]
    fn predicate_line_from_json() {
        let source = "{\n  \"backlinks\": {\n    \"superseded_by\": [\"a.md\"],\n    \"amended_by\": [\"b.md\"]\n  }\n}\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        assert_eq!(
            find_predicate_line(&block, "superseded_by", source),
            3,
            "superseded_by on line 3"
        );
        assert_eq!(
            find_predicate_line(&block, "amended_by", source),
            4,
            "amended_by on line 4"
        );
    }

    // -- Empty object -----------------------------------------------------

    #[test]
    fn empty_object() {
        let source = "{}\n# Heading\n";
        let block = parse_frontmatter_block(source).expect("should parse empty JSON");
        assert!(
            block.entries.is_empty(),
            "empty JSON object should have no entries"
        );
    }

    // -- Trailing commas --------------------------------------------------

    #[test]
    fn trailing_comma_object() {
        let source = "{\n  \"title\": \"test\",\n}\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        assert_eq!(block.entries.len(), 1, "should parse the entry");
        assert!(
            block
                .diagnostics
                .iter()
                .any(|d| d.message.contains("trailing comma")),
            "should warn about trailing comma"
        );
    }

    #[test]
    fn trailing_comma_array() {
        let source = "{\n  \"tags\": [\"a\", \"b\",]\n}\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        assert!(
            block
                .diagnostics
                .iter()
                .any(|d| d.message.contains("trailing comma")),
            "should warn about trailing comma in array"
        );
    }

    // -- Error recovery ---------------------------------------------------

    #[test]
    fn unclosed_string() {
        let source = "{\n  \"title\": \"unclosed\n}\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        assert!(
            block
                .diagnostics
                .iter()
                .any(|d| d.message.contains("unclosed string")),
            "should flag unclosed string"
        );
    }

    #[test]
    fn missing_colon() {
        let source = "{\n  \"title\" \"test\",\n  \"other\": \"ok\"\n}\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        assert!(
            block
                .diagnostics
                .iter()
                .any(|d| d.message.contains("expected ':'")),
            "should flag missing colon"
        );
        // Should still parse the valid entry.
        assert_eq!(
            block.entries.len(),
            1,
            "should recover and parse valid entry"
        );
    }

    #[test]
    fn missing_comma() {
        let source = "{\n  \"a\": 1\n  \"b\": 2\n}\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        assert!(
            block
                .diagnostics
                .iter()
                .any(|d| d.message.contains("missing comma")),
            "should flag missing comma"
        );
        assert_eq!(block.entries.len(), 2, "should parse both entries");
    }

    // -- Body after closing brace -----------------------------------------

    #[test]
    fn body_starts_after_closing_brace() {
        let source = "{\n  \"title\": \"test\"\n}\n# Heading\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        // Span should end at the newline after `}` — the body starts there.
        let body_start = block.span.end;
        assert_eq!(
            &source[body_start..],
            "# Heading\n",
            "body should start after closing brace line"
        );
    }

    #[test]
    fn body_at_eof() {
        let source = "{\n  \"title\": \"test\"\n}";
        let block = parse_frontmatter_block(source).expect("should parse JSON at EOF");
        assert_eq!(block.span.end, source.len(), "span should extend to EOF");
    }

    // -- Symbol emission --------------------------------------------------

    #[test]
    fn symbol_label_json() {
        let source = "{\n  \"title\": \"test\"\n}\n# Heading\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        let tree = crate::block::parse_tree_with_entries(
            source,
            Some(block.span),
            crate::block::Syntax::Json,
            Some(&block.entries),
        );

        let fm_node = tree
            .children(0)
            .iter()
            .find(|&&id| matches!(tree.node(id).kind, crate::block::ElementKind::Frontmatter))
            .expect("should have Frontmatter node");
        assert_eq!(
            tree.node(*fm_node).syntax,
            crate::block::Syntax::Json,
            "Frontmatter node should have Json syntax"
        );
    }

    #[test]
    fn frontmatter_keys_as_field_children() {
        let source = "{\n  \"title\": \"test\",\n  \"author\": \"me\"\n}\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        let tree = crate::block::parse_tree_with_entries(
            source,
            Some(block.span),
            crate::block::Syntax::Json,
            Some(&block.entries),
        );

        let fm_node_id = *tree
            .children(0)
            .iter()
            .find(|&&id| matches!(tree.node(id).kind, crate::block::ElementKind::Frontmatter))
            .expect("should have Frontmatter node");

        let children = tree.children(fm_node_id);
        assert_eq!(children.len(), 2, "should have two Field children");
        for &child_id in children {
            assert!(
                matches!(
                    tree.node(child_id).kind,
                    crate::block::ElementKind::FrontmatterKey { .. }
                ),
                "child should be FrontmatterKey"
            );
        }
    }

    // -- Negative number --------------------------------------------------

    #[test]
    fn negative_number() {
        let source = "{\n  \"val\": -42\n}\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let FmNode::Mapping { value, .. } = &block.entries[0] {
            if let FmValue::Scalar(s) = value {
                assert_eq!(s.text, "-42", "negative integer");
            } else {
                panic!("should be scalar");
            }
        } else {
            panic!("should be mapping");
        }
    }

    #[test]
    fn scientific_notation() {
        let source = "{\n  \"val\": 1.5e10\n}\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let FmNode::Mapping { value, .. } = &block.entries[0] {
            if let FmValue::Scalar(s) = value {
                assert_eq!(s.text, "1.5e10", "scientific notation");
            } else {
                panic!("should be scalar");
            }
        } else {
            panic!("should be mapping");
        }
    }

    // -- CRLF line endings ------------------------------------------------

    #[test]
    fn crlf_line_endings() {
        let source = "{\r\n  \"title\": \"test\"\r\n}\r\n";
        let block = parse_frontmatter_block(source).expect("should parse CRLF");
        assert_eq!(block.entries.len(), 1, "should parse one entry");
    }

    // -- Multiple entries -------------------------------------------------

    #[test]
    fn multiple_entries() {
        let source = "{\n  \"title\": \"Test\",\n  \"count\": 5,\n  \"active\": true\n}\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        assert_eq!(block.entries.len(), 3, "should have three entries");
    }

    // -- Malformed discard ------------------------------------------------

    #[test]
    fn completely_malformed_discarded() {
        // No valid keys at all — should discard and return None.
        let source = "{ ??? }\n# Heading\n";
        assert!(
            parse_frontmatter_block(source).is_none(),
            "completely malformed JSON should be discarded"
        );
    }

    // -- Pathological input limits (ticket 20) ----------------------------

    #[test]
    fn deeply_nested_arrays_hit_limit() {
        // Nested arrays recurse through `parse_value` -> `parse_array`; the
        // depth cap prevents stack overflow on `[[[[...]]]]`.
        let source = format!("{{\"k\":{}{}}}\n", "[".repeat(2_000), "]".repeat(2_000));
        let block = parse_frontmatter_block(&source).expect("frontmatter should parse");
        assert!(
            block
                .diagnostics
                .iter()
                .any(|d| d.message.contains("JSON nesting exceeds")),
            "expected a JSON nesting diagnostic: {:?}",
            block.diagnostics
        );
    }

    #[test]
    fn deeply_nested_objects_hit_limit() {
        // Nested objects `{"a":{"a":{...}}}` recurse through `parse_object`.
        let source = format!("{}1{}\n", "{\"a\":".repeat(2_000), "}".repeat(2_000));
        let block = parse_frontmatter_block(&source).expect("frontmatter should parse");
        assert!(
            block
                .diagnostics
                .iter()
                .any(|d| d.message.contains("JSON nesting exceeds")),
            "expected a JSON nesting diagnostic: {:?}",
            block.diagnostics
        );
    }

    #[test]
    fn oversize_frontmatter_is_skipped() {
        let big = "a".repeat(crate::limits::MAX_FRONTMATTER_BYTES + 100);
        let source = format!("{{\"k\":\"{big}\"}}\n");
        let block = parse_frontmatter_block(&source).expect("frontmatter block returned");
        assert!(
            block.entries.is_empty(),
            "oversize frontmatter is not parsed"
        );
        assert!(
            block
                .diagnostics
                .iter()
                .any(|d| d.message.contains("exceeds the")),
            "expected an oversize diagnostic: {:?}",
            block.diagnostics
        );
    }
}
