// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Span-aware YAML frontmatter parser.
//!
//! Parses the YAML subset used in markdown frontmatter, producing a tree of
//! [`YamlNode`] values where every node carries a [`Span`] back into the
//! original source text. This replaces `serde_yaml_ng` for frontmatter
//! parsing.
//!
//! The parser handles block mappings, block sequences, flow sequences,
//! flow mappings, all scalar types (plain, single-quoted, double-quoted,
//! block literal, block folded), and comments. It rejects anchors, aliases,
//! tags, directives, and complex keys with diagnostics.

use crate::span::Span;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Parsed frontmatter block with span information.
#[derive(Debug)]
pub struct FrontmatterBlock {
    /// Full range including `---` delimiters.
    pub span: Span,
    /// YAML content between delimiters.
    #[allow(dead_code, reason = "used by tree construction ticket 06a")]
    pub content_span: Span,
    /// Top-level YAML entries.
    pub entries: Vec<YamlNode>,
    /// Parse diagnostics (errors and warnings).
    pub diagnostics: Vec<YamlDiagnostic>,
}

/// A node in the YAML tree.
#[derive(Debug)]
pub enum YamlNode {
    /// A key-value mapping entry.
    Mapping {
        /// The mapping key.
        key: ScalarSpan,
        /// The mapping value.
        value: YamlValue,
        /// Span covering the full key-value pair.
        #[allow(dead_code, reason = "used by tree construction ticket 06a")]
        span: Span,
    },
    /// A sequence item (`- value`).
    SequenceItem {
        /// The item value.
        value: YamlValue,
        /// Span covering the `- value` line(s).
        #[allow(dead_code, reason = "used by tree construction ticket 06a")]
        span: Span,
    },
}

/// A YAML value.
#[derive(Debug)]
pub enum YamlValue {
    /// A scalar value (plain, quoted, or null).
    Scalar(ScalarSpan),
    /// A block sequence (list of `YamlNode::SequenceItem`).
    Sequence(Vec<YamlNode>),
    /// A block mapping (list of `YamlNode::Mapping`).
    Mapping(Vec<YamlNode>),
    /// An inline flow sequence (`[a, b, c]`).
    FlowSequence {
        /// Span of the entire flow sequence including brackets.
        #[allow(dead_code, reason = "used by tree construction ticket 06a")]
        span: Span,
        /// Scalar items.
        items: Vec<ScalarSpan>,
    },
    /// An inline flow mapping (`{a: b, c: d}`).
    FlowMapping {
        /// Span of the entire flow mapping including braces.
        #[allow(dead_code, reason = "used by tree construction ticket 06a")]
        span: Span,
        /// Key-value pairs.
        #[allow(dead_code, reason = "used by tree construction ticket 06a")]
        entries: Vec<(ScalarSpan, ScalarSpan)>,
    },
    /// A block scalar (literal `|` or folded `>`).
    BlockScalar {
        /// Span of the entire block scalar content.
        #[allow(dead_code, reason = "used by tree construction ticket 06a")]
        span: Span,
    },
}

/// A scalar with its source span and resolved text.
#[derive(Debug)]
pub struct ScalarSpan {
    /// Byte range in the original source.
    pub span: Span,
    /// Resolved text content.
    pub text: String,
}

/// Severity of a YAML diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YamlSeverity {
    /// A hard parse error.
    Error,
    /// A warning (e.g. unsupported feature that was skipped).
    #[allow(dead_code, reason = "used by structural diagnostics ticket 07")]
    Warning,
}

/// A diagnostic emitted during YAML parsing.
#[derive(Debug)]
pub struct YamlDiagnostic {
    /// Location in the source.
    pub span: Span,
    /// Severity level.
    pub severity: YamlSeverity,
    /// Human-readable message.
    pub message: String,
}

// ---------------------------------------------------------------------------
// BOM stripping
// ---------------------------------------------------------------------------

/// UTF-8 byte order mark.
const BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

/// Strip a UTF-8 BOM at byte 0, returning the remainder and the byte offset.
fn strip_bom(source: &str) -> (&str, usize) {
    if source.as_bytes().starts_with(BOM) {
        (&source[3..], 3)
    } else {
        (source, 0)
    }
}

// ---------------------------------------------------------------------------
// Delimiter detection
// ---------------------------------------------------------------------------

/// Find the opening `---` delimiter at byte 0 (after BOM).
///
/// Returns the byte length of the opening line (including the newline)
/// or `None` if no frontmatter opener is found.
fn find_opening(source: &str) -> Option<usize> {
    if source.starts_with("---\r\n") {
        Some(5)
    } else if source.starts_with("---\n") {
        Some(4)
    } else {
        None
    }
}

/// Find the closing `---` delimiter in the remaining text after the opener.
///
/// Returns the byte offset of the `---` within `rest`, or `None` if not
/// found.
fn find_closing(rest: &str) -> Option<usize> {
    let mut search_from = 0;
    loop {
        let candidate = rest[search_from..].find("---")?;
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
    /// Source bytes (the YAML content between delimiters).
    src: &'a [u8],
    /// Current byte position within `src`.
    pos: usize,
    /// Base offset to add to all spans (accounts for BOM + opening delimiter).
    base: usize,
    /// Collected diagnostics.
    diagnostics: Vec<YamlDiagnostic>,
}

impl<'a> Parser<'a> {
    fn new(content: &'a str, base: usize) -> Self {
        Self {
            src: content.as_bytes(),
            pos: 0,
            base,
            diagnostics: Vec::new(),
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

    /// Absolute byte position (for spans).
    fn abs(&self) -> usize {
        self.base + self.pos
    }

    fn emit(&mut self, span: Span, severity: YamlSeverity, message: String) {
        self.diagnostics.push(YamlDiagnostic {
            span,
            severity,
            message,
        });
    }

    /// Skip spaces and tabs (not newlines). Returns the number of spaces.
    /// Tabs are flagged.
    fn skip_inline_whitespace(&mut self) -> usize {
        let start = self.pos;
        while let Some(b) = self.peek() {
            match b {
                b' ' => {
                    self.pos += 1;
                }
                b'\t' => {
                    let span = Span::new(self.abs(), self.abs() + 1);
                    self.emit(
                        span,
                        YamlSeverity::Error,
                        "tab character in indentation is not allowed in YAML".into(),
                    );
                    self.pos += 1;
                }
                _ => break,
            }
        }
        self.pos - start
    }

    /// Skip a newline (LF or CRLF). Returns true if a newline was consumed.
    fn skip_newline(&mut self) -> bool {
        match self.peek() {
            Some(b'\n') => {
                self.pos += 1;
                true
            }
            Some(b'\r') if self.peek_at(1) == Some(b'\n') => {
                self.pos += 2;
                true
            }
            _ => false,
        }
    }

    /// Skip to end of line (excluding the newline itself).
    fn skip_to_eol(&mut self) {
        while let Some(b) = self.peek() {
            if b == b'\n' || b == b'\r' {
                break;
            }
            self.pos += 1;
        }
    }

    /// Skip an inline comment (`# ...` to end of line). Assumes current
    /// position is at `#`.
    fn skip_comment(&mut self) {
        self.skip_to_eol();
    }

    /// Measure the indentation of the current line (number of leading
    /// spaces). Does not advance the position.
    fn line_indent(&self) -> usize {
        let mut p = self.pos;
        let mut count = 0;
        while p < self.src.len() {
            match self.src[p] {
                b' ' => {
                    count += 1;
                    p += 1;
                }
                b'\t' => {
                    // Count as 1 for indent purposes; the tab error is
                    // emitted when we actually consume whitespace.
                    count += 1;
                    p += 1;
                }
                _ => break,
            }
        }
        count
    }

    /// Skip blank lines and comment-only lines.
    fn skip_blanks_and_comments(&mut self) {
        loop {
            let saved = self.pos;
            self.skip_inline_whitespace();

            match self.peek() {
                None => return,
                Some(b'\n' | b'\r') => {
                    self.skip_newline();
                }
                Some(b'#') => {
                    self.skip_comment();
                    self.skip_newline();
                }
                _ => {
                    // Non-blank, non-comment line — rewind.
                    self.pos = saved;
                    return;
                }
            }
        }
    }

    // -- Unsupported feature detection ------------------------------------

    /// Check for anchors (`&`), aliases (`*`), tags (`!`), and directives
    /// (`%`) at the current position. Returns true if one was detected and
    /// skipped.
    fn check_unsupported(&mut self) -> bool {
        match self.peek() {
            Some(b'&') => {
                let start = self.abs();
                self.skip_to_eol();
                self.emit(
                    Span::new(start, self.abs()),
                    YamlSeverity::Error,
                    "YAML anchors are not supported in frontmatter".into(),
                );
                true
            }
            Some(b'*') => {
                let start = self.abs();
                self.skip_to_eol();
                self.emit(
                    Span::new(start, self.abs()),
                    YamlSeverity::Error,
                    "YAML aliases are not supported in frontmatter".into(),
                );
                true
            }
            Some(b'!') => {
                let start = self.abs();
                self.skip_to_eol();
                self.emit(
                    Span::new(start, self.abs()),
                    YamlSeverity::Error,
                    "YAML tags are not supported in frontmatter".into(),
                );
                true
            }
            Some(b'%') => {
                let start = self.abs();
                self.skip_to_eol();
                self.emit(
                    Span::new(start, self.abs()),
                    YamlSeverity::Error,
                    "YAML directives are not supported in frontmatter".into(),
                );
                true
            }
            _ => false,
        }
    }

    // -- Scalar parsing ---------------------------------------------------

    /// Parse a plain (unquoted) scalar.
    ///
    /// A plain scalar is terminated by newline, EOF, or `: ` (in flow
    /// context we'd also stop at `,`, `]`, `}`). The `in_flow` parameter
    /// controls which terminators apply.
    fn parse_plain_scalar(&mut self, in_flow: bool) -> ScalarSpan {
        let start = self.pos;
        let abs_start = self.abs();

        loop {
            match self.peek() {
                None | Some(b'\n' | b'\r') => break,
                Some(b'#') if self.pos > start && self.src[self.pos - 1] == b' ' => break,
                Some(b',' | b']' | b'}') if in_flow => break,
                Some(b':')
                    if in_flow && matches!(self.peek_at(1), Some(b' ' | b',' | b'}') | None) =>
                {
                    break;
                }
                _ => {
                    self.pos += 1;
                }
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

    /// Parse a single-quoted scalar (`'...'`).
    fn parse_single_quoted(&mut self) -> ScalarSpan {
        let abs_start = self.abs();
        self.pos += 1; // skip opening '

        let mut text = String::new();
        loop {
            match self.advance() {
                None => {
                    self.emit(
                        Span::new(abs_start, self.abs()),
                        YamlSeverity::Error,
                        "unclosed single-quoted scalar".into(),
                    );
                    break;
                }
                Some(b'\'') => {
                    if self.peek() == Some(b'\'') {
                        // Escaped single quote.
                        text.push('\'');
                        self.pos += 1;
                    } else {
                        // End of scalar.
                        break;
                    }
                }
                Some(b'\r') if self.peek() == Some(b'\n') => {
                    self.pos += 1;
                    text.push('\n');
                }
                Some(b) => {
                    text.push(b as char);
                }
            }
        }

        ScalarSpan {
            span: Span::new(abs_start, self.abs()),
            text,
        }
    }

    /// Parse a double-quoted scalar (`"..."`).
    fn parse_double_quoted(&mut self) -> ScalarSpan {
        let abs_start = self.abs();
        self.pos += 1; // skip opening "

        let mut text = String::new();
        loop {
            match self.advance() {
                None => {
                    self.emit(
                        Span::new(abs_start, self.abs()),
                        YamlSeverity::Error,
                        "unclosed double-quoted scalar".into(),
                    );
                    break;
                }
                Some(b'"') => break,
                Some(b'\\') => {
                    match self.advance() {
                        None => break,
                        Some(b'n') => text.push('\n'),
                        Some(b't') => text.push('\t'),
                        Some(b'r') => text.push('\r'),
                        Some(b'\\') => text.push('\\'),
                        Some(b'"') => text.push('"'),
                        Some(b'/') => text.push('/'),
                        Some(b'0') => text.push('\0'),
                        Some(b' ') => text.push(' '),
                        Some(b'\n') => {
                            // Line continuation — skip leading whitespace on
                            // next line.
                            while self.peek() == Some(b' ') || self.peek() == Some(b'\t') {
                                self.pos += 1;
                            }
                        }
                        Some(b'\r') if self.peek() == Some(b'\n') => {
                            self.pos += 1;
                            while self.peek() == Some(b' ') || self.peek() == Some(b'\t') {
                                self.pos += 1;
                            }
                        }
                        Some(other) => {
                            // Unknown escape — pass through.
                            text.push('\\');
                            text.push(other as char);
                        }
                    }
                }
                Some(b'\r') if self.peek() == Some(b'\n') => {
                    self.pos += 1;
                    text.push('\n');
                }
                Some(b) => {
                    text.push(b as char);
                }
            }
        }

        ScalarSpan {
            span: Span::new(abs_start, self.abs()),
            text,
        }
    }

    /// Parse a block scalar (literal `|` or folded `>`).
    fn parse_block_scalar(&mut self) -> YamlValue {
        let abs_start = self.abs();
        self.pos += 1; // skip `|` or `>`

        // Parse optional chomping indicator and explicit indent.
        while let Some(b) = self.peek() {
            match b {
                b'+' | b'-' | b'0'..=b'9' => self.pos += 1,
                _ => break,
            }
        }

        // Skip inline comment / rest of indicator line.
        self.skip_inline_whitespace();
        if self.peek() == Some(b'#') {
            self.skip_comment();
        }
        self.skip_newline();

        // Determine content indentation from the first non-blank line.
        let content_indent = self.detect_block_scalar_indent();

        // Consume content lines.
        loop {
            if self.at_end() {
                break;
            }

            // Blank lines are always part of block scalar content.
            let saved = self.pos;
            let indent = self.line_indent();

            // Check if this is a blank line.
            let is_blank = matches!(self.src.get(saved + indent), Some(b'\n' | b'\r') | None);

            if is_blank {
                self.pos = saved;
                self.skip_to_eol();
                self.skip_newline();
                continue;
            }

            if indent < content_indent {
                // Dedented — end of block scalar.
                self.pos = saved;
                break;
            }

            self.pos = saved;
            self.skip_to_eol();
            self.skip_newline();
        }

        YamlValue::BlockScalar {
            span: Span::new(abs_start, self.abs()),
        }
    }

    /// Detect the indentation level for block scalar content by scanning
    /// ahead for the first non-blank line.
    fn detect_block_scalar_indent(&self) -> usize {
        let mut p = self.pos;
        loop {
            let mut indent = 0;
            while p < self.src.len() && (self.src[p] == b' ' || self.src[p] == b'\t') {
                indent += 1;
                p += 1;
            }

            match self.src.get(p) {
                Some(b'\n') => p += 1,
                Some(b'\r') if self.src.get(p + 1) == Some(&b'\n') => p += 2,
                _ => return indent,
            }
        }
    }

    // -- Flow collections -------------------------------------------------

    /// Parse a flow sequence (`[a, b, c]`).
    fn parse_flow_sequence(&mut self) -> YamlValue {
        let abs_start = self.abs();
        self.pos += 1; // skip '['

        let mut items = Vec::new();

        loop {
            self.skip_flow_whitespace();

            match self.peek() {
                None => {
                    self.emit(
                        Span::new(abs_start, self.abs()),
                        YamlSeverity::Error,
                        "unclosed flow sequence".into(),
                    );
                    break;
                }
                Some(b']') => {
                    self.pos += 1;
                    break;
                }
                Some(b',') => self.pos += 1,
                Some(b'\'') => items.push(self.parse_single_quoted()),
                Some(b'"') => items.push(self.parse_double_quoted()),
                _ => {
                    let scalar = self.parse_plain_scalar(true);
                    if !scalar.text.is_empty() {
                        items.push(scalar);
                    }
                }
            }
        }

        YamlValue::FlowSequence {
            span: Span::new(abs_start, self.abs()),
            items,
        }
    }

    /// Parse a flow mapping (`{a: b, c: d}`).
    fn parse_flow_mapping(&mut self) -> YamlValue {
        let abs_start = self.abs();
        self.pos += 1; // skip '{'

        let mut entries = Vec::new();

        loop {
            self.skip_flow_whitespace();

            match self.peek() {
                None => {
                    self.emit(
                        Span::new(abs_start, self.abs()),
                        YamlSeverity::Error,
                        "unclosed flow mapping".into(),
                    );
                    break;
                }
                Some(b'}') => {
                    self.pos += 1;
                    break;
                }
                Some(b',') => self.pos += 1,
                _ => {
                    let key = self.parse_flow_key();
                    self.skip_flow_whitespace();

                    if self.peek() == Some(b':') {
                        self.pos += 1;
                        self.skip_flow_whitespace();
                    }

                    let value = match self.peek() {
                        Some(b'\'') => self.parse_single_quoted(),
                        Some(b'"') => self.parse_double_quoted(),
                        Some(b',' | b'}') | None => ScalarSpan {
                            span: Span::new(self.abs(), self.abs()),
                            text: String::new(),
                        },
                        _ => self.parse_plain_scalar(true),
                    };

                    entries.push((key, value));
                }
            }
        }

        YamlValue::FlowMapping {
            span: Span::new(abs_start, self.abs()),
            entries,
        }
    }

    /// Parse a key inside a flow mapping.
    fn parse_flow_key(&mut self) -> ScalarSpan {
        match self.peek() {
            Some(b'\'') => self.parse_single_quoted(),
            Some(b'"') => self.parse_double_quoted(),
            _ => self.parse_plain_scalar(true),
        }
    }

    /// Skip whitespace inside flow collections (spaces, tabs, newlines).
    fn skip_flow_whitespace(&mut self) {
        while let Some(b) = self.peek() {
            match b {
                b' ' | b'\t' | b'\n' => self.pos += 1,
                b'\r' if self.peek_at(1) == Some(b'\n') => self.pos += 2,
                b'#' => self.skip_comment(),
                _ => break,
            }
        }
    }

    // -- Value parsing ----------------------------------------------------

    /// Parse a value that starts on the same line as the key (after `: `).
    fn parse_inline_value(&mut self, parent_indent: usize) -> YamlValue {
        // Check for unsupported features first.
        if self.check_unsupported() {
            self.skip_newline();
            return YamlValue::Scalar(ScalarSpan {
                span: Span::new(self.abs(), self.abs()),
                text: String::new(),
            });
        }

        match self.peek() {
            None | Some(b'\n' | b'\r') => {
                // Value on next line(s) — could be nested mapping, sequence,
                // or block scalar.
                self.skip_newline();
                self.parse_block_value(parent_indent)
            }
            Some(b'#') => {
                self.skip_comment();
                self.skip_newline();
                self.parse_block_value(parent_indent)
            }
            Some(b'[') => {
                let v = self.parse_flow_sequence();
                self.skip_trailing();
                v
            }
            Some(b'{') => {
                let v = self.parse_flow_mapping();
                self.skip_trailing();
                v
            }
            Some(b'|' | b'>') => self.parse_block_scalar(),
            Some(b'\'') => {
                let s = self.parse_single_quoted();
                self.skip_trailing();
                YamlValue::Scalar(s)
            }
            Some(b'"') => {
                let s = self.parse_double_quoted();
                self.skip_trailing();
                YamlValue::Scalar(s)
            }
            _ => {
                let s = self.parse_plain_scalar(false);
                self.skip_trailing();
                YamlValue::Scalar(s)
            }
        }
    }

    /// Parse a block value (nested mapping or sequence) that appears on
    /// subsequent lines after a key.
    fn parse_block_value(&mut self, parent_indent: usize) -> YamlValue {
        self.skip_blanks_and_comments();

        if self.at_end() {
            return YamlValue::Scalar(ScalarSpan {
                span: Span::new(self.abs(), self.abs()),
                text: String::new(),
            });
        }

        let child_indent = self.line_indent();
        if child_indent <= parent_indent {
            // No deeper content — this is a null/empty value.
            return YamlValue::Scalar(ScalarSpan {
                span: Span::new(self.abs(), self.abs()),
                text: String::new(),
            });
        }

        // Peek at the first non-whitespace character to determine structure.
        let first_content = self.src.get(self.pos + child_indent).copied();

        if first_content == Some(b'-')
            && matches!(
                self.src.get(self.pos + child_indent + 1),
                Some(b' ' | b'\n' | b'\r') | None
            )
        {
            // Block sequence.
            let items = self.parse_block_sequence(child_indent);
            YamlValue::Sequence(items)
        } else {
            // Block mapping.
            let entries = self.parse_entries(child_indent);
            YamlValue::Mapping(entries)
        }
    }

    /// Skip trailing whitespace and comment after a value, then consume
    /// the newline.
    fn skip_trailing(&mut self) {
        self.skip_inline_whitespace();
        if self.peek() == Some(b'#') {
            self.skip_comment();
        }
        self.skip_newline();
    }

    // -- Block structure --------------------------------------------------

    /// Parse block mapping entries at a given indentation level.
    fn parse_entries(&mut self, indent: usize) -> Vec<YamlNode> {
        let mut entries = Vec::new();

        loop {
            self.skip_blanks_and_comments();
            if self.at_end() {
                break;
            }

            let current_indent = self.line_indent();
            if current_indent < indent {
                break;
            }
            if current_indent > indent {
                // Unexpected deeper indentation — flag and skip line.
                let abs_start = self.abs();
                self.skip_inline_whitespace();
                self.skip_to_eol();
                self.emit(
                    Span::new(abs_start, self.abs()),
                    YamlSeverity::Error,
                    "unexpected indentation".into(),
                );
                self.skip_newline();
                continue;
            }

            // We're at exactly the expected indent level.
            let entry_start = self.abs();
            self.skip_inline_whitespace();

            // Check for unsupported features.
            if self.check_unsupported() {
                self.skip_newline();
                continue;
            }

            // Check if this is a sequence item at mapping level — error.
            if self.peek() == Some(b'-')
                && matches!(self.peek_at(1), Some(b' ' | b'\n' | b'\r') | None)
            {
                // Sequence items where we expected a mapping. Parse them
                // as a sequence anyway for recovery.
                let items = self.parse_block_sequence(indent);
                for item in items {
                    entries.push(item);
                }
                continue;
            }

            // Parse mapping key.
            let key = self.parse_mapping_key();
            let Some(key) = key else {
                // Could not parse a key — flag and skip this line.
                let err_start = self.abs();
                self.skip_to_eol();
                self.emit(
                    Span::new(err_start, self.abs()),
                    YamlSeverity::Error,
                    "expected mapping key".into(),
                );
                self.skip_newline();
                continue;
            };

            // Expect `:`.
            if self.peek() != Some(b':') {
                let span = Span::new(self.abs(), self.abs() + 1);
                self.emit(span, YamlSeverity::Error, "expected ':'".into());
                self.skip_to_eol();
                self.skip_newline();
                continue;
            }
            self.pos += 1; // skip ':'

            // Optional space after colon.
            self.skip_inline_whitespace();

            let value = self.parse_inline_value(indent);

            let entry_end = self.abs();
            entries.push(YamlNode::Mapping {
                key,
                value,
                span: Span::new(entry_start, entry_end),
            });
        }

        entries
    }

    /// Parse a mapping key (plain scalar up to `:`).
    fn parse_mapping_key(&mut self) -> Option<ScalarSpan> {
        match self.peek() {
            Some(b'\'') => Some(self.parse_single_quoted()),
            Some(b'"') => Some(self.parse_double_quoted()),
            Some(b'[' | b'{') => {
                let start = self.abs();
                self.emit(
                    Span::new(start, start + 1),
                    YamlSeverity::Error,
                    "complex keys are not supported in frontmatter".into(),
                );
                None
            }
            _ => {
                let abs_start = self.abs();
                let start = self.pos;
                // Scan for `:` that is followed by space, newline, or EOF.
                while let Some(b) = self.peek() {
                    if b == b':'
                        && matches!(self.peek_at(1), Some(b' ' | b'\t' | b'\n' | b'\r') | None)
                    {
                        break;
                    }
                    if b == b'\n' || b == b'\r' {
                        break;
                    }
                    self.pos += 1;
                }

                let raw = &self.src[start..self.pos];
                let text = String::from_utf8_lossy(raw).trim_end().to_string();
                if text.is_empty() {
                    return None;
                }
                let text_len = text.len();

                Some(ScalarSpan {
                    span: Span::new(abs_start, abs_start + text_len),
                    text,
                })
            }
        }
    }

    /// Parse a block sequence at a given indentation level.
    fn parse_block_sequence(&mut self, indent: usize) -> Vec<YamlNode> {
        let mut items = Vec::new();

        loop {
            self.skip_blanks_and_comments();
            if self.at_end() {
                break;
            }

            let current_indent = self.line_indent();
            if current_indent != indent {
                break;
            }

            let saved = self.pos;
            self.skip_inline_whitespace();

            if self.peek() != Some(b'-')
                || !matches!(self.peek_at(1), Some(b' ' | b'\n' | b'\r') | None)
            {
                // Not a sequence item — we're done.
                self.pos = saved;
                break;
            }

            let item_start = self.abs();
            self.pos += 1; // skip '-'

            // Skip the space after `-`.
            if self.peek() == Some(b' ') {
                self.pos += 1;
            }

            // The value after `- `.
            let value = if self.at_end() || self.peek() == Some(b'\n') || self.peek() == Some(b'\r')
            {
                // Value on next line(s).
                self.skip_newline();
                self.parse_block_value(indent)
            } else if self.peek() == Some(b'#') {
                self.skip_comment();
                self.skip_newline();
                YamlValue::Scalar(ScalarSpan {
                    span: Span::new(self.abs(), self.abs()),
                    text: String::new(),
                })
            } else {
                // Inline value.
                let item_indent = indent + 2; // `- ` is 2 chars
                self.parse_inline_value(item_indent)
            };

            let item_end = self.abs();
            items.push(YamlNode::SequenceItem {
                value,
                span: Span::new(item_start, item_end),
            });
        }

        items
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Parse YAML frontmatter from the start of a markdown document.
///
/// Returns `None` if the document does not start with `---` frontmatter
/// delimiters. Returns `Some(block)` with any parse diagnostics if
/// frontmatter is present.
///
/// UTF-8 BOM at byte 0 is stripped transparently.
pub fn parse_frontmatter_block(source: &str) -> Option<FrontmatterBlock> {
    let (stripped, bom_offset) = strip_bom(source);

    let opener_len = find_opening(stripped)?;
    let content_start = bom_offset + opener_len;

    let rest = &stripped[opener_len..];
    let closing_pos = find_closing(rest)?;

    let yaml_content = &rest[..closing_pos];
    let content_end = content_start + closing_pos;

    let closing_line_len = if rest[closing_pos..].starts_with("---\r\n") {
        5
    } else if rest[closing_pos..].starts_with("---\n") {
        4
    } else {
        3 // `---` at EOF
    };

    let block_end = content_end + closing_line_len;

    let mut parser = Parser::new(yaml_content, content_start);
    let entries = parser.parse_entries(0);
    let diagnostics = parser.diagnostics;

    Some(FrontmatterBlock {
        span: Span::new(bom_offset, block_end),
        content_span: Span::new(content_start, content_end),
        entries,
        diagnostics,
    })
}

// ---------------------------------------------------------------------------
// Backlink extraction helper
// ---------------------------------------------------------------------------

/// Extract backlinks from a parsed frontmatter block.
///
/// Walks the YAML tree looking for a top-level `backlinks` key whose value
/// is a mapping of predicate → list of paths. Returns the backlinks map
/// and any entries that don't match the expected shape.
pub fn extract_backlinks(
    block: &FrontmatterBlock,
    source: &str,
) -> std::collections::HashMap<String, Vec<String>> {
    let mut backlinks = std::collections::HashMap::new();

    for entry in &block.entries {
        if let YamlNode::Mapping { key, value, .. } = entry {
            if key.text != "backlinks" {
                continue;
            }

            let YamlValue::Mapping(predicates) = value else {
                break;
            };

            for pred_entry in predicates {
                let YamlNode::Mapping {
                    key: pred_key,
                    value: pred_value,
                    ..
                } = pred_entry
                else {
                    continue;
                };

                let mut paths = Vec::new();

                match pred_value {
                    YamlValue::Sequence(items) => {
                        for item in items {
                            if let YamlNode::SequenceItem {
                                value: YamlValue::Scalar(s),
                                ..
                            } = item
                            {
                                paths.push(s.text.clone());
                            }
                        }
                    }
                    YamlValue::FlowSequence { items, .. } => {
                        for item in items {
                            paths.push(item.text.clone());
                        }
                    }
                    _ => {}
                }

                backlinks.insert(pred_key.text.clone(), paths);
            }

            break;
        }
    }

    let _ = source; // reserved for future span-based extraction
    backlinks
}

/// Find the 1-based line number for a top-level key in the frontmatter.
///
/// Searches for the `backlinks` → predicate key and returns its line
/// number in the original source.
pub fn find_predicate_line(block: &FrontmatterBlock, predicate: &str, source: &str) -> usize {
    for entry in &block.entries {
        if let YamlNode::Mapping { key, value, .. } = entry {
            if key.text != "backlinks" {
                continue;
            }

            let YamlValue::Mapping(predicates) = value else {
                break;
            };

            for pred_entry in predicates {
                if let YamlNode::Mapping { key: pred_key, .. } = pred_entry
                    && pred_key.text == predicate
                {
                    return byte_offset_to_line(source, pred_key.span.start);
                }
            }
        }
    }

    // Fallback: line 1 (the opening `---`).
    1
}

/// Convert a byte offset to a 1-based line number.
fn byte_offset_to_line(source: &str, offset: usize) -> usize {
    source[..offset.min(source.len())]
        .bytes()
        .filter(|&b| b == b'\n')
        .count()
        + 1
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for clarity")]
#[allow(clippy::panic, reason = "tests use panic for unreachable match arms")]
mod tests {
    use super::*;

    // -- BOM stripping ----------------------------------------------------

    #[test]
    fn strip_bom_present() {
        let source = "\u{FEFF}---\ntitle: test\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse frontmatter with BOM");
        assert!(
            block.diagnostics.is_empty(),
            "BOM frontmatter should have no diagnostics"
        );
        assert_eq!(block.entries.len(), 1, "should have one top-level entry");
    }

    #[test]
    fn strip_bom_absent() {
        let source = "---\ntitle: test\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse frontmatter without BOM");
        assert_eq!(block.span.start, 0, "span should start at 0 without BOM");
    }

    // -- Delimiter detection ----------------------------------------------

    #[test]
    fn no_frontmatter() {
        let source = "# Just a heading\n";
        assert!(
            parse_frontmatter_block(source).is_none(),
            "no frontmatter should return None"
        );
    }

    #[test]
    fn empty_frontmatter() {
        let source = "---\n---\n# Heading\n";
        let block = parse_frontmatter_block(source).expect("should parse empty frontmatter");
        assert!(
            block.entries.is_empty(),
            "empty frontmatter should have no entries"
        );
        assert_eq!(
            block.span,
            Span::new(0, 8),
            "span should cover both delimiters"
        );
    }

    #[test]
    fn frontmatter_at_eof_no_trailing_newline() {
        let source = "---\ntitle: test\n---";
        let block = parse_frontmatter_block(source).expect("should parse frontmatter at EOF");
        assert_eq!(block.entries.len(), 1, "should have one entry");
    }

    #[test]
    fn dashes_not_at_start() {
        let source = "Some text\n---\ntitle: test\n---\n";
        assert!(
            parse_frontmatter_block(source).is_none(),
            "dashes not at file start should not be frontmatter"
        );
    }

    // -- Simple key-value pairs -------------------------------------------

    #[test]
    fn simple_key_value() {
        let source = "---\ntitle: My Document\nauthor: Test\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        assert_eq!(block.entries.len(), 2, "should have two entries");

        if let YamlNode::Mapping { key, value, .. } = &block.entries[0] {
            assert_eq!(key.text, "title", "first key should be title");
            if let YamlValue::Scalar(s) = value {
                assert_eq!(s.text, "My Document", "title value should match");
            } else {
                panic!("title value should be a scalar");
            }
        } else {
            panic!("entry should be a mapping");
        }
    }

    #[test]
    fn null_values() {
        let source = "---\nempty:\nnull_tilde: ~\nnull_word: null\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        assert_eq!(block.entries.len(), 3, "should have three entries");

        // First key has empty value (next line is another key at same indent).
        if let YamlNode::Mapping { key, value, .. } = &block.entries[0] {
            assert_eq!(key.text, "empty", "first key");
            if let YamlValue::Scalar(s) = value {
                assert!(s.text.is_empty(), "empty key should have empty value");
            } else {
                panic!("should be scalar");
            }
        } else {
            panic!("should be mapping");
        }
    }

    // -- Nested mappings --------------------------------------------------

    #[test]
    fn nested_mapping() {
        let source = "---\nbacklinks:\n  superseded_by:\n    - decisions/38.md\n  amended_by:\n    - decisions/38.md\n    - tickets/14h.md\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        assert_eq!(block.entries.len(), 1, "one top-level entry");

        if let YamlNode::Mapping { key, value, .. } = &block.entries[0] {
            assert_eq!(key.text, "backlinks", "top key");
            if let YamlValue::Mapping(preds) = value {
                assert_eq!(preds.len(), 2, "two predicates");
            } else {
                panic!("backlinks value should be a mapping");
            }
        } else {
            panic!("should be mapping");
        }
    }

    // -- Block sequences --------------------------------------------------

    #[test]
    fn block_sequence() {
        let source = "---\ntags:\n  - rust\n  - lsp\n  - markdown\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let YamlNode::Mapping { value, .. } = &block.entries[0] {
            if let YamlValue::Sequence(items) = value {
                assert_eq!(items.len(), 3, "should have three items");
                if let YamlNode::SequenceItem {
                    value: YamlValue::Scalar(s),
                    ..
                } = &items[0]
                {
                    assert_eq!(s.text, "rust", "first item");
                } else {
                    panic!("item should be scalar");
                }
            } else {
                panic!("value should be sequence");
            }
        } else {
            panic!("should be mapping");
        }
    }

    // -- Flow sequences ---------------------------------------------------

    #[test]
    fn flow_sequence() {
        let source = "---\ntags: [rust, lsp, markdown]\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let YamlNode::Mapping { value, .. } = &block.entries[0] {
            if let YamlValue::FlowSequence { items, .. } = value {
                assert_eq!(items.len(), 3, "should have three items");
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
    fn empty_flow_sequence() {
        let source = "---\ntags: []\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let YamlNode::Mapping { value, .. } = &block.entries[0] {
            if let YamlValue::FlowSequence { items, .. } = value {
                assert!(items.is_empty(), "should be empty");
            } else {
                panic!("value should be flow sequence");
            }
        } else {
            panic!("should be mapping");
        }
    }

    // -- Flow mappings ----------------------------------------------------

    #[test]
    fn flow_mapping() {
        let source = "---\nmeta: {a: b, c: d}\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let YamlNode::Mapping { value, .. } = &block.entries[0] {
            if let YamlValue::FlowMapping { entries, .. } = value {
                assert_eq!(entries.len(), 2, "should have two entries");
                assert_eq!(entries[0].0.text, "a", "first key");
                assert_eq!(entries[0].1.text, "b", "first value");
            } else {
                panic!("value should be flow mapping");
            }
        } else {
            panic!("should be mapping");
        }
    }

    // -- Quoted scalars ---------------------------------------------------

    #[test]
    fn single_quoted_scalar() {
        let source = "---\ntitle: 'Hello World'\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let YamlNode::Mapping { value, .. } = &block.entries[0] {
            if let YamlValue::Scalar(s) = value {
                assert_eq!(s.text, "Hello World", "single-quoted value");
            } else {
                panic!("should be scalar");
            }
        } else {
            panic!("should be mapping");
        }
    }

    #[test]
    fn single_quoted_escaped_quote() {
        let source = "---\ntitle: 'it''s a test'\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let YamlNode::Mapping { value, .. } = &block.entries[0] {
            if let YamlValue::Scalar(s) = value {
                assert_eq!(s.text, "it's a test", "escaped single quote");
            } else {
                panic!("should be scalar");
            }
        } else {
            panic!("should be mapping");
        }
    }

    #[test]
    fn double_quoted_scalar() {
        let source = "---\ntitle: \"Hello World\"\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let YamlNode::Mapping { value, .. } = &block.entries[0] {
            if let YamlValue::Scalar(s) = value {
                assert_eq!(s.text, "Hello World", "double-quoted value");
            } else {
                panic!("should be scalar");
            }
        } else {
            panic!("should be mapping");
        }
    }

    #[test]
    fn double_quoted_escapes() {
        let source = "---\npath: \"line1\\nline2\\ttab\"\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let YamlNode::Mapping { value, .. } = &block.entries[0] {
            if let YamlValue::Scalar(s) = value {
                assert_eq!(s.text, "line1\nline2\ttab", "escape sequences");
            } else {
                panic!("should be scalar");
            }
        } else {
            panic!("should be mapping");
        }
    }

    // -- Block scalars ----------------------------------------------------

    #[test]
    fn block_scalar_literal() {
        let source = "---\ndesc: |\n  line one\n  line two\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let YamlNode::Mapping { value, .. } = &block.entries[0] {
            assert!(
                matches!(value, YamlValue::BlockScalar { .. }),
                "should be block scalar"
            );
        } else {
            panic!("should be mapping");
        }
    }

    #[test]
    fn block_scalar_folded() {
        let source = "---\ndesc: >\n  line one\n  line two\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let YamlNode::Mapping { value, .. } = &block.entries[0] {
            assert!(
                matches!(value, YamlValue::BlockScalar { .. }),
                "should be block scalar"
            );
        } else {
            panic!("should be mapping");
        }
    }

    // -- Comments ---------------------------------------------------------

    #[test]
    fn inline_comments() {
        let source = "---\ntitle: test # this is a comment\nauthor: me\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        assert_eq!(block.entries.len(), 2, "should have two entries");

        if let YamlNode::Mapping { value, .. } = &block.entries[0] {
            if let YamlValue::Scalar(s) = value {
                assert_eq!(s.text, "test", "comment should be stripped");
            } else {
                panic!("should be scalar");
            }
        } else {
            panic!("should be mapping");
        }
    }

    #[test]
    fn comment_only_lines() {
        let source = "---\n# a comment\ntitle: test\n# another comment\nauthor: me\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        assert_eq!(block.entries.len(), 2, "comments should be skipped");
    }

    // -- CRLF line endings ------------------------------------------------

    #[test]
    fn crlf_line_endings() {
        let source = "---\r\nbacklinks:\r\n  superseded_by:\r\n    - a.md\r\n---\r\n";
        let block = parse_frontmatter_block(source).expect("should parse CRLF");
        assert_eq!(block.entries.len(), 1, "should parse with CRLF");

        let backlinks = extract_backlinks(&block, source);
        assert_eq!(
            backlinks.get("superseded_by"),
            Some(&vec!["a.md".to_string()]),
            "should extract backlinks with CRLF"
        );
    }

    // -- Backlink extraction ----------------------------------------------

    #[test]
    fn extract_backlinks_full() {
        let source = "---\nbacklinks:\n  superseded_by:\n    - decisions/38.md\n  amended_by:\n    - decisions/38.md\n    - tickets/14h.md\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        let bl = extract_backlinks(&block, source);

        assert_eq!(bl.len(), 2, "should have two predicates");
        assert_eq!(
            bl.get("superseded_by"),
            Some(&vec!["decisions/38.md".to_string()]),
            "superseded_by"
        );
        assert_eq!(
            bl.get("amended_by"),
            Some(&vec![
                "decisions/38.md".to_string(),
                "tickets/14h.md".to_string()
            ]),
            "amended_by"
        );
    }

    #[test]
    fn extract_backlinks_flow_sequence() {
        let source = "---\nbacklinks:\n  superseded_by: [decisions/38.md]\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        let bl = extract_backlinks(&block, source);

        assert_eq!(
            bl.get("superseded_by"),
            Some(&vec!["decisions/38.md".to_string()]),
            "flow sequence backlinks"
        );
    }

    #[test]
    fn extract_backlinks_empty_list() {
        let source = "---\nbacklinks:\n  superseded_by: []\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        let bl = extract_backlinks(&block, source);

        assert_eq!(
            bl.get("superseded_by"),
            Some(&vec![]),
            "empty flow sequence"
        );
    }

    #[test]
    fn no_backlinks_key() {
        let source = "---\ntitle: test\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        let bl = extract_backlinks(&block, source);
        assert!(bl.is_empty(), "no backlinks key should produce empty map");
    }

    // -- Span correctness -------------------------------------------------

    #[test]
    fn spans_are_correct() {
        let source = "---\ntitle: test\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        // "---\n" (4) + "title: test\n" (12) + "---\n" (4) = 20
        assert_eq!(block.span, Span::new(0, 20), "block span covers delimiters");
        assert_eq!(
            block.content_span,
            Span::new(4, 16),
            "content span is between delimiters"
        );

        if let YamlNode::Mapping { key, .. } = &block.entries[0] {
            assert_eq!(key.span, Span::new(4, 9), "key span");
            assert_eq!(
                &source[key.span.start..key.span.end],
                "title",
                "key text matches span"
            );
        } else {
            panic!("should be mapping");
        }
    }

    #[test]
    fn bom_spans_offset_correctly() {
        let source = "\u{FEFF}---\ntitle: test\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        // BOM is 3 bytes.
        assert_eq!(block.span.start, 3, "block span starts after BOM");

        if let YamlNode::Mapping { key, .. } = &block.entries[0] {
            assert_eq!(
                &source[key.span.start..key.span.end],
                "title",
                "key text matches span with BOM"
            );
        } else {
            panic!("should be mapping");
        }
    }

    // -- Predicate line finding -------------------------------------------

    #[test]
    fn find_predicate_line_correct() {
        let source =
            "---\nbacklinks:\n  superseded_by:\n    - a.md\n  amended_by:\n    - b.md\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        assert_eq!(
            find_predicate_line(&block, "superseded_by", source),
            3,
            "superseded_by on line 3"
        );
        assert_eq!(
            find_predicate_line(&block, "amended_by", source),
            5,
            "amended_by on line 5"
        );
        assert_eq!(
            find_predicate_line(&block, "nonexistent", source),
            1,
            "missing predicate falls back to line 1"
        );
    }

    // -- Error recovery ---------------------------------------------------

    #[test]
    fn tab_in_indentation() {
        let source = "---\ntitle: test\n\tindented: bad\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        assert!(
            block.diagnostics.iter().any(|d| d.message.contains("tab")),
            "should flag tab in indentation"
        );
    }

    #[test]
    fn unsupported_anchor() {
        let source = "---\ntitle: &anchor value\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        assert!(
            block
                .diagnostics
                .iter()
                .any(|d| d.message.contains("anchor")),
            "should flag anchor"
        );
    }

    #[test]
    fn unsupported_alias() {
        let source = "---\nref: *alias\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        assert!(
            block
                .diagnostics
                .iter()
                .any(|d| d.message.contains("alias")),
            "should flag alias"
        );
    }

    #[test]
    fn unsupported_tag() {
        let source = "---\ncount: !!int 42\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        assert!(
            block.diagnostics.iter().any(|d| d.message.contains("tag")),
            "should flag tag"
        );
    }

    #[test]
    fn unsupported_directive() {
        let source = "---\n%YAML 1.2\ntitle: test\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        assert!(
            block
                .diagnostics
                .iter()
                .any(|d| d.message.contains("directive")),
            "should flag directive"
        );
    }

    #[test]
    fn unclosed_single_quote() {
        let source = "---\ntitle: 'unclosed\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        assert!(
            block
                .diagnostics
                .iter()
                .any(|d| d.message.contains("unclosed single-quoted")),
            "should flag unclosed single quote"
        );
    }

    #[test]
    fn unclosed_double_quote() {
        let source = "---\ntitle: \"unclosed\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        assert!(
            block
                .diagnostics
                .iter()
                .any(|d| d.message.contains("unclosed double-quoted")),
            "should flag unclosed double quote"
        );
    }

    #[test]
    fn unclosed_flow_sequence_error() {
        let source = "---\ntags: [a, b\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        assert!(
            block
                .diagnostics
                .iter()
                .any(|d| d.message.contains("unclosed flow sequence")),
            "should flag unclosed flow sequence"
        );
    }

    #[test]
    fn unclosed_flow_mapping_error() {
        let source = "---\nmeta: {a: b\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        assert!(
            block
                .diagnostics
                .iter()
                .any(|d| d.message.contains("unclosed flow mapping")),
            "should flag unclosed flow mapping"
        );
    }

    // -- Multi-line double-quoted scalar ----------------------------------

    #[test]
    fn double_quoted_multiline() {
        let source = "---\ndesc: \"line one \\\n  continued\"\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");

        if let YamlNode::Mapping { value, .. } = &block.entries[0] {
            if let YamlValue::Scalar(s) = value {
                assert_eq!(s.text, "line one continued", "line continuation");
            } else {
                panic!("should be scalar");
            }
        } else {
            panic!("should be mapping");
        }
    }
}
