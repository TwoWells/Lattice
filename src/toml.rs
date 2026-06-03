// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Span-aware TOML frontmatter parser.
//!
//! Parses the TOML subset used in markdown frontmatter (delimited by `+++`),
//! producing a tree of [`FmNode`] values where every node carries a [`Span`]
//! back into the original source text.
//!
//! Supports bare keys, quoted keys, dotted keys, table headers (`[section]`),
//! arrays of tables (`[[section]]`), inline tables, inline arrays, all string
//! types (basic, literal, multi-line), integers, floats, booleans, datetimes,
//! and comments.

use std::collections::HashSet;

use crate::fm::{self, FmDiagnostic, FmNode, FmSeverity, FmValue, FrontmatterBlock, ScalarSpan};
use crate::span::Span;

// ---------------------------------------------------------------------------
// Delimiter detection
// ---------------------------------------------------------------------------

/// Find the opening `+++` delimiter at byte 0 (after BOM).
fn find_opening(source: &str) -> Option<usize> {
    if source.starts_with("+++\r\n") {
        Some(5)
    } else if source.starts_with("+++\n") {
        Some(4)
    } else {
        None
    }
}

/// Find the closing `+++` delimiter in the remaining text.
fn find_closing(rest: &str) -> Option<usize> {
    let mut search_from = 0;
    loop {
        let candidate = rest[search_from..].find("+++")?;
        let abs_pos = search_from + candidate;

        let at_line_start = abs_pos == 0 || rest.as_bytes().get(abs_pos - 1) == Some(&b'\n');
        if !at_line_start {
            search_from = abs_pos + 3;
            continue;
        }

        let after = abs_pos + 3;
        let valid_end = after >= rest.len()
            || rest.as_bytes().get(after) == Some(&b'\n')
            || rest.as_bytes().get(after) == Some(&b'\r');
        if !valid_end {
            search_from = after;
            continue;
        }

        return Some(abs_pos);
    }
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// Internal parser state.
struct Parser<'a> {
    /// Source bytes (TOML content between delimiters).
    src: &'a [u8],
    /// Current byte position within `src`.
    pos: usize,
    /// Base offset to add to all spans.
    base: usize,
    /// Collected diagnostics.
    diagnostics: Vec<FmDiagnostic>,
    /// Top-level entries (built up as we parse).
    root: Vec<FmNode>,
    /// Seen top-level keys for duplicate detection.
    seen_keys: HashSet<String>,
    /// Current inline array/table nesting depth (for the depth limit).
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
            root: Vec::new(),
            seen_keys: HashSet::new(),
            depth: 0,
            depth_limit_hit: false,
        }
    }

    // -- Helpers ----------------------------------------------------------

    fn at_end(&self) -> bool {
        self.pos >= self.src.len()
    }

    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<u8> {
        self.src.get(self.pos + offset).copied()
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

    /// A one-byte span at the current position, clamped to the source end so an
    /// at-EOF "expected X" diagnostic collapses to an empty span instead of
    /// pointing one byte past the input.
    fn here_span(&self) -> Span {
        let start = self.abs();
        Span::new(start, (start + 1).min(self.base + self.src.len()))
    }

    fn skip_whitespace(&mut self) {
        while let Some(b) = self.peek() {
            if b == b' ' || b == b'\t' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    /// Skip a newline (`\n`, `\r\n`, or bare `\r`). Returns true if one was
    /// consumed.
    ///
    /// Bare `\r` must advance the cursor: `skip_blanks` calls this in a loop
    /// on any `\n`/`\r`, so a bare `\r` that did not advance would spin
    /// forever.
    fn skip_newline(&mut self) -> bool {
        match self.peek() {
            Some(b'\n') => {
                self.pos += 1;
                true
            }
            Some(b'\r') => {
                self.pos += if self.peek_at(1) == Some(b'\n') { 2 } else { 1 };
                true
            }
            _ => false,
        }
    }

    /// Append the whole UTF-8 character whose lead byte `advance` just
    /// consumed (now at `self.pos - 1`), advancing past its continuation
    /// bytes so a multi-byte character is stored intact rather than as
    /// per-byte mojibake.
    fn push_char(&mut self, text: &mut String) {
        self.pos = fm::push_utf8_char(text, self.src, self.pos - 1);
    }

    fn skip_to_eol(&mut self) {
        while let Some(b) = self.peek() {
            if b == b'\n' || b == b'\r' {
                break;
            }
            self.pos += 1;
        }
    }

    fn skip_comment(&mut self) {
        if self.peek() == Some(b'#') {
            self.skip_to_eol();
        }
    }

    /// Skip blank lines, comment-only lines, and inline whitespace.
    fn skip_blanks(&mut self) {
        loop {
            self.skip_whitespace();
            match self.peek() {
                Some(b'\n' | b'\r') => {
                    self.skip_newline();
                }
                Some(b'#') => {
                    self.skip_comment();
                    self.skip_newline();
                }
                _ => return,
            }
        }
    }

    // -- Key parsing ------------------------------------------------------

    /// Parse a TOML key (bare, basic-quoted, or literal-quoted).
    fn parse_key(&mut self) -> Option<ScalarSpan> {
        self.skip_whitespace();
        match self.peek() {
            Some(b'"') => {
                if self.peek_at(1) == Some(b'"') && self.peek_at(2) == Some(b'"') {
                    // Multi-line basic strings are not valid as keys.
                    let start = self.abs();
                    self.emit(
                        Span::new(start, start + 3),
                        FmSeverity::Error,
                        "multi-line strings cannot be used as keys".into(),
                    );
                    self.skip_to_eol();
                    None
                } else {
                    Some(self.parse_basic_string())
                }
            }
            Some(b'\'') => {
                if self.peek_at(1) == Some(b'\'') && self.peek_at(2) == Some(b'\'') {
                    let start = self.abs();
                    self.emit(
                        Span::new(start, start + 3),
                        FmSeverity::Error,
                        "multi-line strings cannot be used as keys".into(),
                    );
                    self.skip_to_eol();
                    None
                } else {
                    Some(self.parse_literal_string())
                }
            }
            Some(b) if is_bare_key_char(b) => Some(self.parse_bare_key()),
            _ => None,
        }
    }

    /// Parse a bare key (alphanumeric, `-`, `_`).
    fn parse_bare_key(&mut self) -> ScalarSpan {
        let abs_start = self.abs();
        let start = self.pos;
        while let Some(b) = self.peek() {
            if is_bare_key_char(b) {
                self.pos += 1;
            } else {
                break;
            }
        }
        let text = String::from_utf8_lossy(&self.src[start..self.pos]).to_string();
        ScalarSpan {
            span: Span::new(abs_start, self.abs()),
            text,
        }
    }

    /// Parse a dotted key like `a.b.c`, returning the list of key parts.
    fn parse_dotted_key(&mut self) -> Vec<ScalarSpan> {
        let mut parts = Vec::new();
        if let Some(first) = self.parse_key() {
            parts.push(first);
        } else {
            return parts;
        }

        while self.peek() == Some(b'.') {
            self.pos += 1; // skip '.'
            self.skip_whitespace();
            if let Some(part) = self.parse_key() {
                parts.push(part);
            } else {
                let start = self.abs();
                self.emit(
                    Span::new(start, start),
                    FmSeverity::Error,
                    "expected key after '.'".into(),
                );
                break;
            }
            self.skip_whitespace();
        }

        parts
    }

    // -- String parsing ---------------------------------------------------

    /// Parse a basic string (`"..."`).
    fn parse_basic_string(&mut self) -> ScalarSpan {
        let abs_start = self.abs();
        self.pos += 1; // skip opening "

        let mut text = String::new();
        loop {
            match self.advance() {
                None => {
                    self.emit(
                        Span::new(abs_start, self.abs()),
                        FmSeverity::Error,
                        "unclosed basic string".into(),
                    );
                    break;
                }
                Some(b'"') => break,
                Some(b'\\') => self.parse_escape(&mut text, abs_start),
                Some(b'\n' | b'\r') => {
                    // Newlines are not allowed in basic strings.
                    self.emit(
                        Span::new(abs_start, self.abs()),
                        FmSeverity::Error,
                        "unclosed basic string".into(),
                    );
                    break;
                }
                Some(_) => self.push_char(&mut text),
            }
        }

        ScalarSpan {
            span: Span::new(abs_start, self.abs()),
            text,
        }
    }

    /// Parse a literal string (`'...'`).
    fn parse_literal_string(&mut self) -> ScalarSpan {
        let abs_start = self.abs();
        self.pos += 1; // skip opening '

        let mut text = String::new();
        loop {
            match self.advance() {
                None | Some(b'\n' | b'\r') => {
                    self.emit(
                        Span::new(abs_start, self.abs()),
                        FmSeverity::Error,
                        "unclosed literal string".into(),
                    );
                    break;
                }
                Some(b'\'') => break,
                Some(_) => self.push_char(&mut text),
            }
        }

        ScalarSpan {
            span: Span::new(abs_start, self.abs()),
            text,
        }
    }

    /// Parse a multi-line basic string (`"""..."""`).
    fn parse_ml_basic_string(&mut self) -> ScalarSpan {
        let abs_start = self.abs();
        self.pos += 3; // skip opening """

        // Skip immediate newline after opening delimiter.
        if self.peek() == Some(b'\n') {
            self.pos += 1;
        } else if self.peek() == Some(b'\r') && self.peek_at(1) == Some(b'\n') {
            self.pos += 2;
        }

        let mut text = String::new();
        loop {
            match self.advance() {
                None => {
                    self.emit(
                        Span::new(abs_start, self.abs()),
                        FmSeverity::Error,
                        "unclosed multi-line basic string".into(),
                    );
                    break;
                }
                Some(b'"') if self.peek() == Some(b'"') && self.peek_at(1) == Some(b'"') => {
                    self.pos += 2; // skip remaining ""
                    break;
                }
                Some(b'\\') => {
                    // Check for line-ending backslash (continuation).
                    let saved = self.pos;
                    let mut is_continuation = false;
                    let mut p = self.pos;
                    // Skip whitespace after backslash.
                    while p < self.src.len() && (self.src[p] == b' ' || self.src[p] == b'\t') {
                        p += 1;
                    }
                    if p < self.src.len()
                        && (self.src[p] == b'\n'
                            || (self.src[p] == b'\r'
                                && p + 1 < self.src.len()
                                && self.src[p + 1] == b'\n'))
                    {
                        is_continuation = true;
                    }

                    if is_continuation {
                        self.pos = p;
                        self.skip_newline();
                        // Skip leading whitespace on continuation lines.
                        loop {
                            self.skip_whitespace();
                            if self.peek() == Some(b'\n')
                                || (self.peek() == Some(b'\r') && self.peek_at(1) == Some(b'\n'))
                            {
                                self.skip_newline();
                            } else {
                                break;
                            }
                        }
                    } else {
                        self.pos = saved;
                        self.parse_escape(&mut text, abs_start);
                    }
                }
                Some(b'\r') if self.peek() == Some(b'\n') => {
                    self.pos += 1;
                    text.push('\n');
                }
                Some(_) => self.push_char(&mut text),
            }
        }

        ScalarSpan {
            span: Span::new(abs_start, self.abs()),
            text,
        }
    }

    /// Parse a multi-line literal string (`'''...'''`).
    fn parse_ml_literal_string(&mut self) -> ScalarSpan {
        let abs_start = self.abs();
        self.pos += 3; // skip opening '''

        // Skip immediate newline after opening delimiter.
        if self.peek() == Some(b'\n') {
            self.pos += 1;
        } else if self.peek() == Some(b'\r') && self.peek_at(1) == Some(b'\n') {
            self.pos += 2;
        }

        let mut text = String::new();
        loop {
            match self.advance() {
                None => {
                    self.emit(
                        Span::new(abs_start, self.abs()),
                        FmSeverity::Error,
                        "unclosed multi-line literal string".into(),
                    );
                    break;
                }
                Some(b'\'') if self.peek() == Some(b'\'') && self.peek_at(1) == Some(b'\'') => {
                    self.pos += 2; // skip remaining ''
                    break;
                }
                Some(b'\r') if self.peek() == Some(b'\n') => {
                    self.pos += 1;
                    text.push('\n');
                }
                Some(_) => self.push_char(&mut text),
            }
        }

        ScalarSpan {
            span: Span::new(abs_start, self.abs()),
            text,
        }
    }

    /// Parse a backslash escape sequence in a basic string.
    fn parse_escape(&mut self, text: &mut String, string_start: usize) {
        match self.advance() {
            None => {}
            Some(b'b') => text.push('\u{0008}'),
            Some(b't') => text.push('\t'),
            Some(b'n') => text.push('\n'),
            Some(b'f') => text.push('\u{000C}'),
            Some(b'r') => text.push('\r'),
            Some(b'"') => text.push('"'),
            Some(b'\\') => text.push('\\'),
            Some(b'u') => {
                if let Some(c) = self.parse_unicode_escape(4) {
                    text.push(c);
                } else {
                    let start = self.abs().saturating_sub(2);
                    self.emit(
                        Span::new(start, self.abs()),
                        FmSeverity::Error,
                        "invalid \\uXXXX escape".into(),
                    );
                }
            }
            Some(b'U') => {
                if let Some(c) = self.parse_unicode_escape(8) {
                    text.push(c);
                } else {
                    let start = self.abs().saturating_sub(2);
                    self.emit(
                        Span::new(start, self.abs()),
                        FmSeverity::Error,
                        "invalid \\UXXXXXXXX escape".into(),
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

    /// Parse a unicode escape of the given digit count.
    fn parse_unicode_escape(&mut self, digits: usize) -> Option<char> {
        let mut hex = String::with_capacity(digits);
        for _ in 0..digits {
            let b = self.advance()?;
            if b.is_ascii_hexdigit() {
                hex.push(b as char);
            } else {
                return None;
            }
        }
        let code = u32::from_str_radix(&hex, 16).ok()?;
        char::from_u32(code)
    }

    // -- Value parsing ----------------------------------------------------

    /// Parse a TOML value.
    fn parse_value(&mut self) -> ScalarSpan {
        self.skip_whitespace();
        match self.peek() {
            Some(b'"') => {
                if self.peek_at(1) == Some(b'"') && self.peek_at(2) == Some(b'"') {
                    self.parse_ml_basic_string()
                } else {
                    self.parse_basic_string()
                }
            }
            Some(b'\'') => {
                if self.peek_at(1) == Some(b'\'') && self.peek_at(2) == Some(b'\'') {
                    self.parse_ml_literal_string()
                } else {
                    self.parse_literal_string()
                }
            }
            _ => self.parse_unquoted_value(),
        }
    }

    /// Parse an unquoted value (number, boolean, datetime, or bare string).
    /// Stored as source text — type interpretation is a consumer concern.
    fn parse_unquoted_value(&mut self) -> ScalarSpan {
        let abs_start = self.abs();
        let start = self.pos;

        while let Some(b) = self.peek() {
            match b {
                b'\n' | b'\r' | b'#' | b',' | b']' | b'}' => break,
                _ => self.pos += 1,
            }
        }

        let raw = &self.src[start..self.pos];
        let text = String::from_utf8_lossy(raw).trim_end().to_string();
        let text_len = text.len();

        ScalarSpan {
            span: Span::new(abs_start, abs_start + text_len),
            text,
        }
    }

    /// Parse an inline array (`[1, 2, "three"]`).
    fn parse_inline_array(&mut self) -> FmValue {
        let abs_start = self.abs();

        // Depth limit. Inline arrays and tables nest recursively, so the
        // guard caps recursion and prevents stack overflow. Beyond the limit
        // the collection is skipped as opaque (its bytes are consumed) and a
        // single diagnostic is emitted.
        if self.depth >= crate::limits::MAX_FRONTMATTER_NESTING {
            self.note_depth_limit();
            self.skip_balanced();
            return FmValue::FlowSequence {
                span: Span::new(abs_start, self.abs()),
                items: Vec::new(),
            };
        }
        self.depth += 1;
        self.pos += 1; // skip '['

        let mut items = Vec::new();

        loop {
            self.skip_array_whitespace();

            match self.peek() {
                None => {
                    self.emit(
                        Span::new(abs_start, self.abs()),
                        FmSeverity::Error,
                        "unclosed inline array".into(),
                    );
                    break;
                }
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'[') => {
                    // Nested array — parse as value but skip for now (store empty).
                    let nested = self.parse_inline_array();
                    if let FmValue::FlowSequence {
                        items: nested_items,
                        span,
                    } = nested
                    {
                        for item in nested_items {
                            items.push(item);
                        }
                        let _ = span;
                    }
                }
                Some(b'{') => {
                    // Inline table inside array — skip.
                    self.parse_inline_table();
                }
                _ => {
                    let arm_start = self.pos;
                    let scalar = self.parse_value();
                    if self.pos == arm_start {
                        // Forward-progress guard: a stray `}` inside `[...]`
                        // is rejected by `parse_value` without consuming. Skip
                        // it so the loop cannot spin forever allocating.
                        self.pos += 1;
                    } else if !scalar.text.is_empty() {
                        items.push(scalar);
                    }
                }
            }
        }

        self.depth -= 1;
        FmValue::FlowSequence {
            span: Span::new(abs_start, self.abs()),
            items,
        }
    }

    /// Skip whitespace inside arrays (spaces, tabs, newlines, comments).
    fn skip_array_whitespace(&mut self) {
        loop {
            match self.peek() {
                Some(b' ' | b'\t' | b'\n') => self.pos += 1,
                Some(b'\r') if self.peek_at(1) == Some(b'\n') => self.pos += 2,
                Some(b'#') => {
                    self.skip_comment();
                }
                _ => break,
            }
        }
    }

    /// Parse an inline table (`{ key = "val", other = "val" }`).
    fn parse_inline_table(&mut self) -> FmValue {
        let abs_start = self.abs();

        // Depth limit (see `parse_inline_array`).
        if self.depth >= crate::limits::MAX_FRONTMATTER_NESTING {
            self.note_depth_limit();
            self.skip_balanced();
            return FmValue::Mapping(Vec::new());
        }
        self.depth += 1;
        self.pos += 1; // skip '{'

        let mut entries = Vec::new();

        loop {
            self.skip_whitespace();

            match self.peek() {
                None | Some(b'\n' | b'\r') => {
                    self.emit(
                        Span::new(abs_start, self.abs()),
                        FmSeverity::Error,
                        "unclosed inline table".into(),
                    );
                    break;
                }
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                Some(b',') => {
                    self.pos += 1;
                }
                _ => {
                    let entry_start = self.abs();

                    let parts = self.parse_dotted_key();
                    if parts.is_empty() {
                        self.skip_to_eol();
                        break;
                    }

                    self.skip_whitespace();
                    if self.peek() == Some(b'=') {
                        self.pos += 1;
                    }
                    self.skip_whitespace();

                    let value = self.parse_value_or_collection();
                    let entry_end = self.abs();

                    let node = build_dotted_entry(&parts, value, entry_start, entry_end);
                    entries.push(node);
                }
            }
        }

        self.depth -= 1;
        FmValue::Mapping(entries)
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
                    "TOML nesting exceeds the limit of {}; deeper structure is flattened",
                    crate::limits::MAX_FRONTMATTER_NESTING
                ),
            );
        }
    }

    /// Consume a bracket/brace-balanced region starting at the current `[`
    /// or `{`, skipping quoted strings. Used to discard over-deep structure
    /// without recursing. Always makes forward progress.
    fn skip_balanced(&mut self) {
        let mut depth = 0usize;
        while let Some(b) = self.peek() {
            match b {
                b'"' | b'\'' => self.skip_string_raw(b),
                b'[' | b'{' => {
                    depth += 1;
                    self.pos += 1;
                }
                b']' | b'}' => {
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

    /// Skip a quoted string starting at the opening quote `quote`. Basic
    /// strings honor `\` escapes; literal strings do not. Stops at a newline
    /// for recovery.
    fn skip_string_raw(&mut self, quote: u8) {
        self.pos += 1; // opening quote
        while let Some(b) = self.peek() {
            if b == quote {
                self.pos += 1;
                return;
            }
            if b == b'\n' || b == b'\r' {
                return;
            }
            if b == b'\\' && quote == b'"' {
                self.pos = (self.pos + 2).min(self.src.len());
            } else {
                self.pos += 1;
            }
        }
    }

    /// Parse a value that could be a scalar, inline array, or inline table.
    fn parse_value_or_collection(&mut self) -> FmValue {
        self.skip_whitespace();
        match self.peek() {
            Some(b'[') => self.parse_inline_array(),
            Some(b'{') => self.parse_inline_table(),
            _ => {
                let scalar = self.parse_value();
                FmValue::Scalar(scalar)
            }
        }
    }

    // -- Table headers ----------------------------------------------------

    /// Parse a table header (`[section]` or `[section.sub]`).
    /// Returns the list of key parts.
    fn parse_table_header(&mut self) -> Vec<ScalarSpan> {
        self.pos += 1; // skip '['
        self.skip_whitespace();

        let parts = self.parse_dotted_key();

        self.skip_whitespace();
        if self.peek() == Some(b']') {
            self.pos += 1;
        } else {
            let start = self.abs();
            self.emit(
                Span::new(start, start),
                FmSeverity::Error,
                "expected ']' to close table header".into(),
            );
        }

        self.skip_whitespace();
        self.skip_comment();
        self.skip_newline();

        parts
    }

    /// Parse an array-of-tables header (`[[section]]`).
    /// Returns the list of key parts.
    fn parse_array_table_header(&mut self) -> Vec<ScalarSpan> {
        self.pos += 2; // skip '[['
        self.skip_whitespace();

        let parts = self.parse_dotted_key();

        self.skip_whitespace();
        if self.peek() == Some(b']') && self.peek_at(1) == Some(b']') {
            self.pos += 2;
        } else {
            let start = self.abs();
            self.emit(
                Span::new(start, start),
                FmSeverity::Error,
                "expected ']]' to close array-of-tables header".into(),
            );
        }

        self.skip_whitespace();
        self.skip_comment();
        self.skip_newline();

        parts
    }

    /// Parse key-value pairs until the next table header or end of input.
    fn parse_table_body(&mut self) -> Vec<FmNode> {
        let mut entries = Vec::new();

        loop {
            self.skip_blanks();
            if self.at_end() {
                break;
            }

            // Stop at table headers.
            if self.peek() == Some(b'[') {
                break;
            }

            let entry_start = self.abs();

            let parts = self.parse_dotted_key();
            if parts.is_empty() {
                // Could not parse a key — skip line.
                let err_start = self.abs();
                self.skip_to_eol();
                if err_start != self.abs() {
                    self.emit(
                        Span::new(err_start, self.abs()),
                        FmSeverity::Error,
                        "expected key".into(),
                    );
                }
                self.skip_newline();
                continue;
            }

            self.skip_whitespace();

            if self.peek() != Some(b'=') {
                self.emit(self.here_span(), FmSeverity::Error, "expected '='".into());
                self.skip_to_eol();
                self.skip_newline();
                continue;
            }
            self.pos += 1; // skip '='
            self.skip_whitespace();

            let value = self.parse_value_or_collection();

            self.skip_whitespace();
            self.skip_comment();
            self.skip_newline();

            let entry_end = self.abs();

            let node = build_dotted_entry(&parts, value, entry_start, entry_end);
            entries.push(node);
        }

        entries
    }

    // -- Top-level parsing ------------------------------------------------

    /// Parse the entire TOML content.
    fn parse(&mut self) {
        // Parse top-level key-value pairs (before any table header).
        let top_entries = self.parse_table_body();
        for entry in top_entries {
            self.check_duplicate_and_push(entry);
        }

        // Parse table headers and their bodies.
        while !self.at_end() {
            if self.peek() != Some(b'[') {
                break;
            }

            let header_start = self.abs();

            if self.peek_at(1) == Some(b'[') {
                // Array of tables `[[section]]`.
                let parts = self.parse_array_table_header();
                let body = self.parse_table_body();

                if parts.is_empty() {
                    continue;
                }

                let header_end = self.abs();
                let table_node = build_array_table_entry(&parts, body, header_start, header_end);
                self.merge_array_table(table_node);
            } else {
                // Standard table `[section]`.
                let parts = self.parse_table_header();
                let body = self.parse_table_body();

                if parts.is_empty() {
                    continue;
                }

                let header_end = self.abs();
                let table_node = build_table_entry(&parts, body, header_start, header_end);
                self.check_duplicate_and_push(table_node);
            }
        }
    }

    /// Merge an array-of-tables entry into root. If an existing entry with
    /// the same key exists and is a sequence, append the new items.
    fn merge_array_table(&mut self, node: FmNode) {
        let FmNode::Mapping { key, value, span } = node else {
            return;
        };

        // Find existing entry with same key.
        for existing in &mut self.root {
            if let FmNode::Mapping {
                key: existing_key,
                value: existing_value,
                span: existing_span,
            } = existing
                && existing_key.text == key.text
            {
                // Merge: append sequence items.
                if let FmValue::Sequence(existing_items) = existing_value
                    && let FmValue::Sequence(new_items) = value
                {
                    existing_items.extend(new_items);
                    existing_span.end = span.end;
                    return;
                }
                // Key exists but is not a sequence — duplicate.
                self.emit(
                    key.span,
                    FmSeverity::Error,
                    format!("duplicate key: {}", key.text),
                );
                return;
            }
        }

        // No existing entry — add new.
        self.root.push(FmNode::Mapping { key, value, span });
    }

    /// Check for duplicate top-level key and push to root.
    fn check_duplicate_and_push(&mut self, node: FmNode) {
        if let FmNode::Mapping { ref key, .. } = node
            && !self.seen_keys.insert(key.text.clone())
        {
            self.emit(
                key.span,
                FmSeverity::Error,
                format!("duplicate key: {}", key.text),
            );
            return;
        }
        self.root.push(node);
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Check if a byte is a valid bare key character (alphanumeric, `-`, `_`).
fn is_bare_key_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

/// Build nested `FmNode::Mapping` entries for a dotted key like `a.b.c = val`.
fn build_dotted_entry(
    parts: &[ScalarSpan],
    value: FmValue,
    entry_start: usize,
    entry_end: usize,
) -> FmNode {
    if parts.len() == 1 {
        return FmNode::Mapping {
            key: ScalarSpan {
                span: parts[0].span,
                text: parts[0].text.clone(),
            },
            value,
            span: Span::new(entry_start, entry_end),
        };
    }

    // Build from right to left: innermost value wraps outward.
    let last = &parts[parts.len() - 1];
    let mut current = FmNode::Mapping {
        key: ScalarSpan {
            span: last.span,
            text: last.text.clone(),
        },
        value,
        span: Span::new(last.span.start, entry_end),
    };

    for part in parts[1..parts.len() - 1].iter().rev() {
        current = FmNode::Mapping {
            key: ScalarSpan {
                span: part.span,
                text: part.text.clone(),
            },
            value: FmValue::Mapping(vec![current]),
            span: Span::new(part.span.start, entry_end),
        };
    }

    FmNode::Mapping {
        key: ScalarSpan {
            span: parts[0].span,
            text: parts[0].text.clone(),
        },
        value: FmValue::Mapping(vec![current]),
        span: Span::new(entry_start, entry_end),
    }
}

/// Build a table entry from a header path and body entries.
fn build_table_entry(
    parts: &[ScalarSpan],
    body: Vec<FmNode>,
    entry_start: usize,
    entry_end: usize,
) -> FmNode {
    if parts.len() == 1 {
        return FmNode::Mapping {
            key: ScalarSpan {
                span: parts[0].span,
                text: parts[0].text.clone(),
            },
            value: FmValue::Mapping(body),
            span: Span::new(entry_start, entry_end),
        };
    }

    // Nest from right to left.
    let last = &parts[parts.len() - 1];
    let mut current = FmNode::Mapping {
        key: ScalarSpan {
            span: last.span,
            text: last.text.clone(),
        },
        value: FmValue::Mapping(body),
        span: Span::new(last.span.start, entry_end),
    };

    for part in parts[1..parts.len() - 1].iter().rev() {
        current = FmNode::Mapping {
            key: ScalarSpan {
                span: part.span,
                text: part.text.clone(),
            },
            value: FmValue::Mapping(vec![current]),
            span: Span::new(part.span.start, entry_end),
        };
    }

    FmNode::Mapping {
        key: ScalarSpan {
            span: parts[0].span,
            text: parts[0].text.clone(),
        },
        value: FmValue::Mapping(vec![current]),
        span: Span::new(entry_start, entry_end),
    }
}

/// Build an array-of-tables entry (one element of the sequence).
fn build_array_table_entry(
    parts: &[ScalarSpan],
    body: Vec<FmNode>,
    entry_start: usize,
    entry_end: usize,
) -> FmNode {
    // The innermost part is the array — its value is a sequence item
    // containing a mapping of the body entries.
    let item = FmNode::SequenceItem {
        value: FmValue::Mapping(body),
        span: Span::new(entry_start, entry_end),
    };

    if parts.len() == 1 {
        return FmNode::Mapping {
            key: ScalarSpan {
                span: parts[0].span,
                text: parts[0].text.clone(),
            },
            value: FmValue::Sequence(vec![item]),
            span: Span::new(entry_start, entry_end),
        };
    }

    let last = &parts[parts.len() - 1];
    let mut current = FmNode::Mapping {
        key: ScalarSpan {
            span: last.span,
            text: last.text.clone(),
        },
        value: FmValue::Sequence(vec![item]),
        span: Span::new(last.span.start, entry_end),
    };

    for part in parts[1..parts.len() - 1].iter().rev() {
        current = FmNode::Mapping {
            key: ScalarSpan {
                span: part.span,
                text: part.text.clone(),
            },
            value: FmValue::Mapping(vec![current]),
            span: Span::new(part.span.start, entry_end),
        };
    }

    FmNode::Mapping {
        key: ScalarSpan {
            span: parts[0].span,
            text: parts[0].text.clone(),
        },
        value: FmValue::Mapping(vec![current]),
        span: Span::new(entry_start, entry_end),
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Parse TOML frontmatter from the start of a markdown document.
///
/// Returns `None` if the document does not start with `+++` frontmatter
/// delimiters. Returns `Some(block)` with any parse diagnostics if
/// frontmatter is present.
///
/// UTF-8 BOM at byte 0 is stripped transparently.
#[must_use]
pub fn parse_frontmatter_block(source: &str) -> Option<FrontmatterBlock> {
    let (stripped, bom_offset) = fm::strip_bom(source);

    let opener_len = find_opening(stripped)?;
    let content_start = bom_offset + opener_len;

    let rest = &stripped[opener_len..];
    let closing_pos = find_closing(rest)?;

    let toml_content = &rest[..closing_pos];
    let content_end = content_start + closing_pos;

    let closing_line_len = if rest[closing_pos..].starts_with("+++\r\n") {
        5
    } else if rest[closing_pos..].starts_with("+++\n") {
        4
    } else {
        3 // `+++` at EOF
    };

    let block_end = content_end + closing_line_len;

    // Size limit: an enormous block is treated as opaque and skipped, so the
    // parser never walks a multi-megabyte frontmatter region.
    if toml_content.len() > crate::limits::MAX_FRONTMATTER_BYTES {
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

    let mut parser = Parser::new(toml_content, content_start);
    parser.parse();
    let entries = parser.root;
    let diagnostics = parser.diagnostics;

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

    // -- Delimiter detection ----------------------------------------------

    #[test]
    fn no_toml_frontmatter() {
        let source = "# Just a heading\n";
        assert!(
            parse_frontmatter_block(source).is_none(),
            "no TOML frontmatter should return None"
        );
    }

    #[test]
    fn yaml_delimiters_not_toml() {
        let source = "---\ntitle: test\n---\n";
        assert!(
            parse_frontmatter_block(source).is_none(),
            "YAML delimiters should not parse as TOML"
        );
    }

    #[test]
    fn empty_toml_frontmatter() {
        let source = "+++\n+++\n# Heading\n";
        let block = parse_frontmatter_block(source).expect("should parse empty TOML frontmatter");
        assert!(
            block.entries.is_empty(),
            "empty TOML frontmatter should have no entries"
        );
        assert_eq!(
            block.span,
            Span::new(0, 8),
            "span should cover both delimiters"
        );
    }

    #[test]
    fn toml_at_eof_no_trailing_newline() {
        let source = "+++\ntitle = \"test\"\n+++";
        let block = parse_frontmatter_block(source).expect("should parse TOML frontmatter at EOF");
        assert_eq!(block.entries.len(), 1, "should have one entry");
    }

    #[test]
    fn bom_before_toml() {
        let source = "\u{FEFF}+++\ntitle = \"test\"\n+++\n";
        let block = parse_frontmatter_block(source).expect("should parse TOML with BOM");
        assert!(
            block.diagnostics.is_empty(),
            "BOM TOML should have no diagnostics"
        );
        assert_eq!(block.entries.len(), 1, "should have one entry");
        assert_eq!(block.span.start, 3, "span should start after BOM");
    }

    // -- Simple key-value pairs -------------------------------------------

    #[test]
    fn bare_key_string_value() {
        let source = "+++\ntitle = \"My Document\"\nauthor = \"Test\"\n+++\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        assert_eq!(block.entries.len(), 2, "should have two entries");

        if let FmNode::Mapping { key, value, .. } = &block.entries[0] {
            assert_eq!(key.text, "title", "first key");
            if let FmValue::Scalar(s) = value {
                assert_eq!(s.text, "My Document", "title value");
            } else {
                panic!("title value should be scalar");
            }
        } else {
            panic!("entry should be mapping");
        }
    }

    #[test]
    fn integer_value() {
        let source = "+++\ncount = 42\n+++\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let FmNode::Mapping { value, .. } = &block.entries[0] {
            if let FmValue::Scalar(s) = value {
                assert_eq!(s.text, "42", "integer value stored as text");
            } else {
                panic!("should be scalar");
            }
        } else {
            panic!("should be mapping");
        }
    }

    #[test]
    fn float_value() {
        let source = "+++\npi = 3.14\n+++\n";
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
        let source = "+++\nenabled = true\ndisabled = false\n+++\n";
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

    // -- Quoted keys ------------------------------------------------------

    #[test]
    fn quoted_key() {
        let source = "+++\n\"dotted.key\" = \"value\"\n+++\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let FmNode::Mapping { key, .. } = &block.entries[0] {
            assert_eq!(key.text, "dotted.key", "quoted key preserved");
        } else {
            panic!("should be mapping");
        }
    }

    #[test]
    fn literal_quoted_key() {
        let source = "+++\n'literal.key' = \"value\"\n+++\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let FmNode::Mapping { key, .. } = &block.entries[0] {
            assert_eq!(key.text, "literal.key", "literal quoted key");
        } else {
            panic!("should be mapping");
        }
    }

    // -- Dotted keys ------------------------------------------------------

    #[test]
    fn dotted_key_expands() {
        let source = "+++\nbacklinks.superseded_by = [\"decisions/26.md\"]\n+++\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let FmNode::Mapping { key, value, .. } = &block.entries[0] {
            assert_eq!(key.text, "backlinks", "outer key");
            if let FmValue::Mapping(children) = value {
                assert_eq!(children.len(), 1, "one nested key");
                if let FmNode::Mapping {
                    key: inner_key,
                    value: inner_value,
                    ..
                } = &children[0]
                {
                    assert_eq!(inner_key.text, "superseded_by", "inner key");
                    assert!(
                        matches!(inner_value, FmValue::FlowSequence { .. }),
                        "inner value should be flow sequence"
                    );
                } else {
                    panic!("inner should be mapping");
                }
            } else {
                panic!("value should be mapping");
            }
        } else {
            panic!("should be mapping");
        }
    }

    // -- Table headers ----------------------------------------------------

    #[test]
    fn table_header() {
        let source = "+++\n[backlinks]\nsuperseded_by = [\"decisions/26.md\"]\n+++\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let FmNode::Mapping { key, value, .. } = &block.entries[0] {
            assert_eq!(key.text, "backlinks", "table key");
            if let FmValue::Mapping(children) = value {
                assert_eq!(children.len(), 1, "one child");
                if let FmNode::Mapping { key: child_key, .. } = &children[0] {
                    assert_eq!(child_key.text, "superseded_by", "child key");
                } else {
                    panic!("child should be mapping");
                }
            } else {
                panic!("value should be mapping");
            }
        } else {
            panic!("should be mapping");
        }
    }

    #[test]
    fn nested_table_header() {
        let source = "+++\n[backlinks.meta]\nkey = \"val\"\n+++\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let FmNode::Mapping { key, value, .. } = &block.entries[0] {
            assert_eq!(key.text, "backlinks", "outer table key");
            if let FmValue::Mapping(children) = value {
                if let FmNode::Mapping {
                    key: inner_key,
                    value: inner_val,
                    ..
                } = &children[0]
                {
                    assert_eq!(inner_key.text, "meta", "inner table key");
                    assert!(
                        matches!(inner_val, FmValue::Mapping(_)),
                        "inner value should be mapping"
                    );
                } else {
                    panic!("inner should be mapping");
                }
            } else {
                panic!("value should be mapping");
            }
        } else {
            panic!("should be mapping");
        }
    }

    // -- Arrays of tables -------------------------------------------------

    #[test]
    fn array_of_tables() {
        let source = "+++\n[[items]]\nname = \"a\"\n\n[[items]]\nname = \"b\"\n+++\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let FmNode::Mapping { key, value, .. } = &block.entries[0] {
            assert_eq!(key.text, "items", "array-of-tables key");
            if let FmValue::Sequence(items) = value {
                assert_eq!(items.len(), 2, "two array-of-table entries");
            } else {
                panic!("value should be sequence");
            }
        } else {
            panic!("should be mapping");
        }
    }

    // -- Inline tables ----------------------------------------------------

    #[test]
    fn inline_table() {
        let source = "+++\nmeta = { a = \"b\", c = \"d\" }\n+++\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let FmNode::Mapping { value, .. } = &block.entries[0] {
            if let FmValue::Mapping(children) = value {
                assert_eq!(children.len(), 2, "two inline table entries");
            } else {
                panic!("value should be mapping");
            }
        } else {
            panic!("should be mapping");
        }
    }

    // -- Inline arrays ----------------------------------------------------

    #[test]
    fn inline_array() {
        let source = "+++\ntags = [\"rust\", \"lsp\", \"markdown\"]\n+++\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let FmNode::Mapping { value, .. } = &block.entries[0] {
            if let FmValue::FlowSequence { items, .. } = value {
                assert_eq!(items.len(), 3, "three items");
                assert_eq!(items[0].text, "rust", "first item");
                assert_eq!(items[1].text, "lsp", "second item");
                assert_eq!(items[2].text, "markdown", "third item");
            } else {
                panic!("value should be flow sequence");
            }
        } else {
            panic!("should be mapping");
        }
    }

    #[test]
    fn empty_inline_array() {
        let source = "+++\ntags = []\n+++\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let FmNode::Mapping { value, .. } = &block.entries[0] {
            if let FmValue::FlowSequence { items, .. } = value {
                assert!(items.is_empty(), "should be empty");
            } else {
                panic!("value should be flow sequence");
            }
        } else {
            panic!("should be mapping");
        }
    }

    // -- String types -----------------------------------------------------

    #[test]
    fn basic_string_escapes() {
        let source = "+++\npath = \"line1\\nline2\\ttab\"\n+++\n";
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
    fn literal_string() {
        let source = "+++\npath = 'C:\\Users\\test'\n+++\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let FmNode::Mapping { value, .. } = &block.entries[0] {
            if let FmValue::Scalar(s) = value {
                assert_eq!(s.text, "C:\\Users\\test", "literal string no escapes");
            } else {
                panic!("should be scalar");
            }
        } else {
            panic!("should be mapping");
        }
    }

    #[test]
    fn ml_basic_string() {
        let source = "+++\ndesc = \"\"\"\nline one\nline two\"\"\"\n+++\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let FmNode::Mapping { value, .. } = &block.entries[0] {
            if let FmValue::Scalar(s) = value {
                assert_eq!(s.text, "line one\nline two", "multi-line basic string");
            } else {
                panic!("should be scalar");
            }
        } else {
            panic!("should be mapping");
        }
    }

    #[test]
    fn ml_basic_string_line_continuation() {
        let source = "+++\ndesc = \"\"\"\nline one \\\n  continued\"\"\"\n+++\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let FmNode::Mapping { value, .. } = &block.entries[0] {
            if let FmValue::Scalar(s) = value {
                assert_eq!(s.text, "line one continued", "line continuation");
            } else {
                panic!("should be scalar");
            }
        } else {
            panic!("should be mapping");
        }
    }

    #[test]
    fn ml_literal_string() {
        let source = "+++\ndesc = '''\nline one\nline two'''\n+++\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let FmNode::Mapping { value, .. } = &block.entries[0] {
            if let FmValue::Scalar(s) = value {
                assert_eq!(s.text, "line one\nline two", "multi-line literal string");
            } else {
                panic!("should be scalar");
            }
        } else {
            panic!("should be mapping");
        }
    }

    // -- Comments ---------------------------------------------------------

    #[test]
    fn comments_recognized() {
        let source = "+++\n# a comment\ntitle = \"test\" # inline comment\n# another\n+++\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        assert_eq!(block.entries.len(), 1, "comments should be skipped");
    }

    // -- Backlinks --------------------------------------------------------

    #[test]
    fn backlinks_from_toml() {
        let source = "+++\n[backlinks]\nsuperseded_by = [\"decisions/26.md\"]\namended_by = [\"decisions/26.md\", \"tickets/14h.md\"]\n+++\n";
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
    fn backlinks_dotted_key() {
        let source = "+++\nbacklinks.superseded_by = [\"decisions/26.md\"]\n+++\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        let bl = extract_backlinks(&block, source);

        assert_eq!(
            bl.get("superseded_by"),
            Some(&vec!["decisions/26.md".to_string()]),
            "dotted key backlinks"
        );
    }

    #[test]
    fn predicate_line_from_toml() {
        let source = "+++\n[backlinks]\nsuperseded_by = [\"a.md\"]\namended_by = [\"b.md\"]\n+++\n";
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

    // -- CRLF line endings ------------------------------------------------

    #[test]
    fn crlf_line_endings() {
        let source = "+++\r\n[backlinks]\r\nsuperseded_by = [\"a.md\"]\r\n+++\r\n";
        let block = parse_frontmatter_block(source).expect("should parse CRLF");
        let bl = extract_backlinks(&block, source);
        assert_eq!(
            bl.get("superseded_by"),
            Some(&vec!["a.md".to_string()]),
            "CRLF backlinks"
        );
    }

    // -- Error recovery ---------------------------------------------------

    #[test]
    fn missing_equals() {
        let source = "+++\ntitle \"test\"\nother = \"ok\"\n+++\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        assert!(
            block
                .diagnostics
                .iter()
                .any(|d| d.message.contains("expected '='")),
            "should flag missing ="
        );
        // Should still parse the valid entry.
        assert_eq!(
            block.entries.len(),
            1,
            "should recover and parse valid entry"
        );
    }

    #[test]
    fn unclosed_basic_string() {
        let source = "+++\ntitle = \"unclosed\n+++\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        assert!(
            block
                .diagnostics
                .iter()
                .any(|d| d.message.contains("unclosed basic string")),
            "should flag unclosed string"
        );
    }

    #[test]
    fn unclosed_inline_array() {
        let source = "+++\ntags = [\"a\", \"b\"\n+++\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        assert!(
            block
                .diagnostics
                .iter()
                .any(|d| d.message.contains("unclosed inline array")),
            "should flag unclosed array"
        );
    }

    #[test]
    fn unclosed_inline_table() {
        let source = "+++\nmeta = { a = \"b\"\n+++\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        assert!(
            block
                .diagnostics
                .iter()
                .any(|d| d.message.contains("unclosed inline table")),
            "should flag unclosed table"
        );
    }

    #[test]
    fn duplicate_key_flagged() {
        let source = "+++\ntitle = \"a\"\ntitle = \"b\"\n+++\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        assert!(
            block
                .diagnostics
                .iter()
                .any(|d| d.message.contains("duplicate key")),
            "should flag duplicate key"
        );
        // First entry is kept.
        assert_eq!(block.entries.len(), 1, "first entry kept");
    }

    // -- Unicode escapes --------------------------------------------------

    #[test]
    fn unicode_escape_4() {
        let source = "+++\nval = \"\\u0041\"\n+++\n";
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
    fn unicode_escape_8() {
        let source = "+++\nval = \"\\U0001F600\"\n+++\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let FmNode::Mapping { value, .. } = &block.entries[0] {
            if let FmValue::Scalar(s) = value {
                assert_eq!(s.text, "\u{1F600}", "\\U0001F600 should be grinning face");
            } else {
                panic!("should be scalar");
            }
        } else {
            panic!("should be mapping");
        }
    }

    // -- Symbol emission label --------------------------------------------

    #[test]
    fn symbol_label_toml() {
        // Integration test: verify that parsing TOML frontmatter with the
        // full workspace pipeline produces the expected tree syntax.
        let source = "+++\ntitle = \"test\"\n+++\n# Heading\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        let tree = crate::block::parse_tree_with_entries(
            source,
            Some(block.span),
            crate::block::Syntax::Toml,
            Some(&block.entries),
        );

        // The Frontmatter node should have Syntax::Toml.
        let fm_node = tree
            .children(0)
            .iter()
            .find(|&&id| matches!(tree.node(id).kind, crate::block::ElementKind::Frontmatter))
            .expect("should have Frontmatter node");
        assert_eq!(
            tree.node(*fm_node).syntax,
            crate::block::Syntax::Toml,
            "Frontmatter node should have Toml syntax"
        );
    }

    #[test]
    fn frontmatter_keys_as_field_children() {
        let source = "+++\ntitle = \"test\"\nauthor = \"me\"\n+++\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        let tree = crate::block::parse_tree_with_entries(
            source,
            Some(block.span),
            crate::block::Syntax::Toml,
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

    // -- Pathological input limits (ticket 20) ----------------------------

    #[test]
    fn deeply_nested_inline_arrays_hit_limit() {
        // Nested inline arrays recurse through `parse_inline_array`; the depth
        // cap prevents stack overflow on `[[[[...`.
        let source = format!("+++\nx = {}{}\n+++\n", "[".repeat(2_000), "]".repeat(2_000));
        let block = parse_frontmatter_block(&source).expect("frontmatter should parse");
        assert!(
            block
                .diagnostics
                .iter()
                .any(|d| d.message.contains("TOML nesting exceeds")),
            "expected a TOML nesting diagnostic: {:?}",
            block.diagnostics
        );
    }

    #[test]
    fn deeply_nested_inline_tables_hit_limit() {
        // Nested inline tables `{a={a={...}}}` recurse through
        // `parse_inline_table`.
        let source = format!(
            "+++\nx = {}1{}\n+++\n",
            "{ a = ".repeat(2_000),
            " }".repeat(2_000)
        );
        let block = parse_frontmatter_block(&source).expect("frontmatter should parse");
        assert!(
            block
                .diagnostics
                .iter()
                .any(|d| d.message.contains("TOML nesting exceeds")),
            "expected a TOML nesting diagnostic: {:?}",
            block.diagnostics
        );
    }

    #[test]
    fn oversize_frontmatter_is_skipped() {
        let big = "a = 1\n".repeat(crate::limits::MAX_FRONTMATTER_BYTES / 6 + 10);
        let source = format!("+++\n{big}+++\n");
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
