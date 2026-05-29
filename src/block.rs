// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Block-level markdown parser.
//!
//! Reads source text line by line and classifies each line into a
//! block-level construct: headings, code fences, block quotes, thematic
//! breaks, HTML blocks, indented code, paragraphs, and blank lines.
//!
//! This module does **not** parse inline content (links, emphasis,
//! images). It produces a flat sequence of [`Block`] values, each
//! carrying a [`Span`] into the original source. Nested structures
//! (block quotes containing headings) are represented by recursive
//! parsing of the block quote content in a later ticket.

use crate::span::Span;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Classification of a block-level construct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockKind {
    /// ATX heading (`#` through `######`).
    AtxHeading {
        /// Heading level (1–6).
        level: u8,
        /// Span of the heading text content (after `#` markers and space,
        /// before optional closing `#` markers). Empty headings have a
        /// zero-length span at the position after the marker space.
        text_span: Span,
        /// Explicit `{#id}` attribute, if present.
        id: Option<AtxId>,
    },
    /// Setext heading (paragraph followed by `===` or `---` underline).
    SetextHeading {
        /// Heading level: 1 for `===`, 2 for `---`.
        level: u8,
        /// Span of the heading text (the paragraph lines above).
        text_span: Span,
    },
    /// Thematic break (`---`, `***`, `___` with variations).
    ThematicBreak,
    /// Fenced code block (`` ``` `` or `~~~`).
    FencedCodeBlock {
        /// Language tag from the info string, if any.
        info: Option<String>,
        /// Span of the content between opening and closing fences.
        content_span: Span,
    },
    /// Block math (`$$` delimiters).
    BlockMath {
        /// Span of the content between opening and closing `$$`.
        content_span: Span,
    },
    /// Indented code block (4+ spaces of indentation).
    IndentedCodeBlock,
    /// Block quote line(s) starting with `>`.
    BlockQuote {
        /// Span of the content after the `>` markers.
        content_span: Span,
    },
    /// HTML block (content is opaque at this stage).
    HtmlBlock,
    /// Paragraph text.
    Paragraph,
    /// Blank line.
    BlankLine,
}

/// An explicit `{#id}` attribute on an ATX heading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtxId {
    /// The ID text (without `{#` and `}`).
    pub id: String,
    /// Span of the ID text in the source.
    pub span: Span,
}

/// A classified block-level element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// What kind of block this is.
    pub kind: BlockKind,
    /// Byte range in the original source covering the entire block.
    pub span: Span,
}

/// An error emitted during block parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockDiagnostic {
    /// Location of the error in the source.
    pub span: Span,
    /// Human-readable message.
    pub message: String,
}

/// Result of block-level parsing.
#[derive(Debug)]
pub struct BlockParseResult {
    /// Classified blocks in document order.
    pub blocks: Vec<Block>,
    /// Diagnostics emitted during parsing.
    pub diagnostics: Vec<BlockDiagnostic>,
}

// ---------------------------------------------------------------------------
// Tab expansion
// ---------------------------------------------------------------------------

/// Expand tabs to spaces at the next tab stop (multiples of 4 columns).
///
/// Only expands tabs that appear in leading indentation — once a
/// non-whitespace character is seen, remaining tabs are preserved as-is
/// so that spans into the original source remain valid for content after
/// indentation.
fn expand_leading_tabs(line: &str) -> (String, Vec<TabMapping>) {
    let mut result = String::with_capacity(line.len());
    let mut mappings = Vec::new();
    let mut col = 0;
    let mut in_indent = true;

    for (byte_idx, ch) in line.char_indices() {
        if in_indent && ch == '\t' {
            let spaces = 4 - (col % 4);
            mappings.push(TabMapping {
                original_byte: byte_idx,
                expanded_col: col,
                num_spaces: spaces,
            });
            for _ in 0..spaces {
                result.push(' ');
            }
            col += spaces;
        } else {
            if ch != ' ' {
                in_indent = false;
            }
            result.push(ch);
            col += 1;
        }
    }

    (result, mappings)
}

/// Mapping from a tab character to its expansion.
#[derive(Debug)]
struct TabMapping {
    /// Byte offset of the tab in the original line.
    #[allow(dead_code, reason = "used for span remapping in later tickets")]
    original_byte: usize,
    /// Column position where expansion starts.
    #[allow(dead_code, reason = "used for span remapping in later tickets")]
    expanded_col: usize,
    /// Number of spaces this tab expanded to.
    #[allow(dead_code, reason = "used for span remapping in later tickets")]
    num_spaces: usize,
}

// ---------------------------------------------------------------------------
// Line classification helpers
// ---------------------------------------------------------------------------

/// Count leading spaces in a string (after tab expansion).
fn count_indent(line: &str) -> usize {
    line.bytes().take_while(|&b| b == b' ').count()
}

/// Strip a trailing `\n` or `\r\n` from a byte offset into source.
///
/// Returns the adjusted end offset with the line ending excluded.
fn strip_trailing_newline(source: &str, end: usize) -> usize {
    let bytes = source.as_bytes();
    if end > 0 && bytes.get(end - 1) == Some(&b'\n') {
        if end > 1 && bytes.get(end - 2) == Some(&b'\r') {
            end - 2
        } else {
            end - 1
        }
    } else {
        end
    }
}

/// Check if a line is an ATX heading opener. Returns `Some(level)` if so.
fn atx_heading_level(line: &str) -> Option<u8> {
    let trimmed = line.trim_start_matches(' ');
    // Must have at most 3 leading spaces
    if line.len() - trimmed.len() > 3 {
        return None;
    }

    let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }

    // After the hashes, must be space, tab, or EOL (including newline)
    let after = &trimmed[hashes..];
    if after.is_empty()
        || after.starts_with(' ')
        || after.starts_with('\t')
        || after.starts_with('\n')
        || after.starts_with('\r')
    {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "hashes is in 1..=6, always fits in u8"
        )]
        return Some(hashes as u8);
    }

    None
}

/// Extract the text span and optional `{#id}` from an ATX heading line.
///
/// `line_start` is the byte offset of this line in the original source.
/// `original_line` is the raw line from the source (not tab-expanded).
fn extract_atx_content(original_line: &str, line_start: usize) -> (Span, Option<AtxId>) {
    let trimmed = original_line.trim_start_matches(' ');
    let leading_spaces = original_line.len() - trimmed.len();
    let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();

    // Content starts after hashes + optional single space
    let content_start_in_line = leading_spaces + hashes;
    let after_hashes = &original_line[content_start_in_line..];
    let content_offset = if after_hashes.starts_with(' ') {
        content_start_in_line + 1
    } else {
        content_start_in_line
    };

    let content = &original_line[content_offset..];

    // Strip trailing whitespace, then trailing `#` markers, then trailing whitespace
    let content = content.trim_end();
    let stripped_trailing_hashes = content.trim_end_matches('#');
    let content = if stripped_trailing_hashes.is_empty()
        || stripped_trailing_hashes.ends_with(' ')
        || stripped_trailing_hashes.ends_with('\t')
    {
        stripped_trailing_hashes.trim_end()
    } else {
        // The `#` chars are part of the content if not preceded by space
        content
    };

    // Check for `{#id}` attribute at the end
    let (text_content, id) = match content.rfind("{#") {
        Some(attr_start) if content.ends_with('}') => {
            let id_text = &content[attr_start + 2..content.len() - 1];
            let text_before = content[..attr_start].trim_end();

            // Calculate the id span in the original source
            let text_before_end = content_offset + attr_start + 2;
            let id_end = content_offset + content.len() - 1;
            let id_span = Span::new(line_start + text_before_end, line_start + id_end);

            (
                text_before,
                Some(AtxId {
                    id: id_text.to_string(),
                    span: id_span,
                }),
            )
        }
        _ => (content, None),
    };

    // Calculate text span in original source
    let text_byte_start = if text_content.is_empty() {
        content_offset
    } else {
        // Find where text_content starts in original_line via pointer arithmetic
        text_content.as_ptr() as usize - original_line.as_ptr() as usize
    };
    let text_byte_end = text_byte_start + text_content.len();

    (
        Span::new(line_start + text_byte_start, line_start + text_byte_end),
        id,
    )
}

/// Check if a line is a thematic break.
///
/// Three or more `*`, `-`, or `_` characters (optionally with spaces
/// between them), with no other characters, and at most 3 leading spaces.
fn is_thematic_break(line: &str) -> bool {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return false;
    }

    let stripped: String = trimmed.chars().filter(|c| *c != ' ').collect();
    if stripped.len() < 3 {
        return false;
    }

    let first = stripped.as_bytes()[0];
    matches!(first, b'*' | b'-' | b'_') && stripped.bytes().all(|b| b == first)
}

/// Check if a line is a setext heading underline. Returns `Some(level)`.
fn setext_level(line: &str) -> Option<u8> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None;
    }

    let trimmed = trimmed.trim_end();
    if trimmed.is_empty() {
        return None;
    }

    let first = trimmed.as_bytes()[0];
    if first == b'=' && trimmed.bytes().all(|b| b == b'=') {
        Some(1)
    } else if first == b'-' && trimmed.bytes().all(|b| b == b'-') {
        Some(2)
    } else {
        None
    }
}

/// Check if a line opens a fenced code block. Returns the fence character,
/// fence length, and info string if so.
fn fenced_code_open(line: &str) -> Option<(u8, usize, Option<String>)> {
    let trimmed = line.trim_start_matches(' ');
    let indent = line.len() - trimmed.len();
    if indent > 3 {
        return None;
    }

    let fence_char = trimmed.as_bytes().first().copied()?;
    if fence_char != b'`' && fence_char != b'~' {
        return None;
    }

    let fence_len = trimmed.bytes().take_while(|&b| b == fence_char).count();
    if fence_len < 3 {
        return None;
    }

    // Backtick fences cannot have backticks in the info string
    let info_part = trimmed[fence_len..].trim();
    if fence_char == b'`' && info_part.contains('`') {
        return None;
    }

    let info = if info_part.is_empty() {
        None
    } else {
        Some(info_part.to_string())
    };

    Some((fence_char, fence_len, info))
}

/// Check if a line closes a fenced code block.
fn fenced_code_close(line: &str, fence_char: u8, open_len: usize) -> bool {
    let trimmed = line.trim_start_matches(' ');
    let indent = line.len() - trimmed.len();
    if indent > 3 {
        return false;
    }

    let close_len = trimmed.bytes().take_while(|&b| b == fence_char).count();
    if close_len < open_len {
        return false;
    }

    // Nothing after the fence except whitespace
    trimmed[close_len..].trim().is_empty()
}

/// Check if a line opens a block math span (`$$`).
fn block_math_open(line: &str) -> bool {
    let trimmed = line.trim_start_matches(' ');
    let indent = line.len() - trimmed.len();
    if indent > 3 {
        return false;
    }

    if !trimmed.starts_with("$$") {
        return false;
    }

    // After `$$`, must be whitespace, newline, or EOL
    let after = &trimmed[2..];
    after.is_empty()
        || after.starts_with(' ')
        || after.starts_with('\t')
        || after.starts_with('\n')
        || after.starts_with('\r')
}

/// Check if a line closes a block math span (`$$`).
fn block_math_close(line: &str) -> bool {
    line.trim() == "$$"
}

/// `CommonMark` HTML block types 1–7.
///
/// Returns the type number (1–7) if the line starts an HTML block, or
/// `None` otherwise.
fn html_block_start(line: &str) -> Option<u8> {
    let trimmed = line.trim_start_matches(' ');
    let indent = line.len() - trimmed.len();
    if indent > 3 {
        return None;
    }

    if !trimmed.starts_with('<') {
        return None;
    }

    let lower = trimmed.to_lowercase();

    // Type 1: <pre, <script, <style, <textarea (case-insensitive)
    for tag in &["<pre", "<script", "<style", "<textarea"] {
        if lower.strip_prefix(tag).is_some_and(|after| {
            after.is_empty()
                || after.starts_with(' ')
                || after.starts_with('\t')
                || after.starts_with('>')
                || after.starts_with('\n')
        }) {
            return Some(1);
        }
    }

    // Type 2: <!-- (HTML comment)
    if lower.starts_with("<!--") {
        return Some(2);
    }

    // Type 3: <? (processing instruction)
    if lower.starts_with("<?") {
        return Some(3);
    }

    // Type 4: <! followed by uppercase letter (declaration)
    if trimmed.len() >= 3
        && trimmed.as_bytes()[0] == b'<'
        && trimmed.as_bytes()[1] == b'!'
        && trimmed.as_bytes()[2].is_ascii_uppercase()
    {
        return Some(4);
    }

    // Type 5: <![CDATA[
    if lower.starts_with("<![cdata[") {
        return Some(5);
    }

    // Type 6: block-level HTML tags
    if extract_html_tag_name(trimmed).is_some_and(|name| is_block_html_tag(&name)) {
        return Some(6);
    }

    // Type 7: any other tag (open or closing), not starting a paragraph
    if is_html_tag_line(trimmed) {
        return Some(7);
    }

    None
}

/// Extract the tag name from an HTML-like line, lowercased.
fn extract_html_tag_name(line: &str) -> Option<String> {
    let rest = line.strip_prefix('<')?;
    let rest = rest.strip_prefix('/').unwrap_or(rest);

    let end = rest
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '-')
        .unwrap_or(rest.len());

    if end == 0 {
        return None;
    }

    Some(rest[..end].to_lowercase())
}

/// Check if a tag name is a block-level HTML tag per the `CommonMark` spec.
fn is_block_html_tag(name: &str) -> bool {
    matches!(
        name,
        "address"
            | "article"
            | "aside"
            | "base"
            | "basefont"
            | "blockquote"
            | "body"
            | "caption"
            | "center"
            | "col"
            | "colgroup"
            | "dd"
            | "details"
            | "dialog"
            | "dir"
            | "div"
            | "dl"
            | "dt"
            | "fieldset"
            | "figcaption"
            | "figure"
            | "footer"
            | "form"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "head"
            | "header"
            | "hr"
            | "html"
            | "iframe"
            | "legend"
            | "li"
            | "link"
            | "main"
            | "menu"
            | "menuitem"
            | "nav"
            | "noframes"
            | "ol"
            | "optgroup"
            | "option"
            | "p"
            | "param"
            | "search"
            | "section"
            | "summary"
            | "table"
            | "tbody"
            | "td"
            | "template"
            | "tfoot"
            | "th"
            | "thead"
            | "title"
            | "tr"
            | "track"
            | "ul"
    )
}

/// Check if a line looks like an HTML open or close tag (for type 7).
fn is_html_tag_line(line: &str) -> bool {
    if !line.starts_with('<') {
        return false;
    }

    let rest = &line[1..];
    let is_close = rest.starts_with('/');
    let rest = if is_close { &rest[1..] } else { rest };

    // Must start with an ASCII letter
    let first = rest.as_bytes().first().copied().unwrap_or(0);
    if !first.is_ascii_alphabetic() {
        return false;
    }

    // Tag name: letters, digits, hyphens
    let name_end = rest
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '-')
        .unwrap_or(rest.len());

    if name_end == 0 {
        return false;
    }

    let after_name = rest[name_end..].trim();

    // For close tags, must end with >
    if is_close {
        return after_name.is_empty() || after_name == ">";
    }

    // For open tags, the rest must be attributes and end with > or />
    after_name.is_empty()
        || after_name.ends_with('>')
        || after_name.ends_with("/>")
        || after_name.contains('>')
}

/// Check if a line ends an HTML block of the given type.
fn html_block_end(line: &str, html_type: u8) -> bool {
    let lower = line.to_lowercase();
    match html_type {
        1 => {
            lower.contains("</pre>")
                || lower.contains("</script>")
                || lower.contains("</style>")
                || lower.contains("</textarea>")
        }
        2 => lower.contains("-->"),
        3 => lower.contains("?>"),
        4 => lower.contains('>'),
        5 => lower.contains("]]>"),
        // Types 6 and 7 are terminated by a blank line, not by content
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Block quote helpers
// ---------------------------------------------------------------------------

/// Strip the leading `> ` or `>` from a block quote line.
///
/// Returns `Some((stripped_bytes, content))` where `stripped_bytes` is how
/// many bytes of the original line were consumed by the marker and
/// `content` is the remainder.
fn strip_blockquote_marker(line: &str) -> Option<(usize, &str)> {
    let trimmed = line.trim_start_matches(' ');
    let indent = line.len() - trimmed.len();
    if indent > 3 {
        return None;
    }

    let after_gt = trimmed.strip_prefix('>')?;
    Some(
        after_gt
            .strip_prefix(' ')
            .map_or((indent + 1, after_gt), |content| (indent + 2, content)),
    )
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// Parse the block structure of a markdown document.
///
/// `source` is the full document text. If frontmatter is present,
/// `body_offset` should be the byte offset where the document body
/// starts (after the closing `---\n`). If there is no frontmatter,
/// pass 0.
pub fn parse_blocks(source: &str, body_offset: usize) -> BlockParseResult {
    let body = &source[body_offset..];
    let mut parser = BlockParser {
        source,
        body_offset,
        blocks: Vec::new(),
        diagnostics: Vec::new(),
    };
    parser.parse(body);
    BlockParseResult {
        blocks: parser.blocks,
        diagnostics: parser.diagnostics,
    }
}

/// Internal parser state.
struct BlockParser<'a> {
    /// The full source text.
    #[allow(dead_code, reason = "used for span slicing in diagnostics")]
    source: &'a str,
    /// Byte offset where the body starts (after frontmatter).
    body_offset: usize,
    /// Accumulated blocks.
    blocks: Vec<Block>,
    /// Accumulated diagnostics.
    diagnostics: Vec<BlockDiagnostic>,
}

impl BlockParser<'_> {
    fn parse(&mut self, body: &str) {
        let lines: Vec<&str> = split_lines(body);
        let mut pos = 0;
        let mut line_idx = 0;

        while line_idx < lines.len() {
            let line = lines[line_idx];
            let line_start = self.body_offset + pos;
            let line_byte_len = line.len();

            let (expanded, _mappings) = expand_leading_tabs(line);
            let indent = count_indent(&expanded);

            if expanded.trim().is_empty() {
                self.blocks.push(Block {
                    kind: BlockKind::BlankLine,
                    span: Span::new(line_start, line_start + line_byte_len),
                });
                pos += line_byte_len;
                line_idx += 1;
            } else if let Some((fence_char, fence_len, info)) = fenced_code_open(&expanded) {
                pos += line_byte_len;
                line_idx += 1;
                self.parse_fenced_code(
                    &lines,
                    &mut pos,
                    &mut line_idx,
                    line_start,
                    line_byte_len,
                    fence_char,
                    fence_len,
                    info,
                );
            } else if block_math_open(&expanded) {
                pos += line_byte_len;
                line_idx += 1;
                self.parse_block_math(&lines, &mut pos, &mut line_idx, line_start, line_byte_len);
            } else if let Some(level) = atx_heading_level(&expanded) {
                let (text_span, id) = extract_atx_content(line, line_start);
                self.blocks.push(Block {
                    kind: BlockKind::AtxHeading {
                        level,
                        text_span,
                        id,
                    },
                    span: Span::new(line_start, line_start + line_byte_len),
                });
                pos += line_byte_len;
                line_idx += 1;
            } else if let Some(html_type) = html_block_start(&expanded) {
                self.parse_html_block(
                    &lines,
                    &mut pos,
                    &mut line_idx,
                    line_start,
                    line_byte_len,
                    line,
                    html_type,
                );
            } else if strip_blockquote_marker(&expanded).is_some() {
                self.parse_block_quote(
                    &lines,
                    &mut pos,
                    &mut line_idx,
                    line_start,
                    line_byte_len,
                    line,
                );
            } else if indent >= 4 && !self.prev_is_paragraph() {
                self.parse_indented_code(
                    &lines,
                    &mut pos,
                    &mut line_idx,
                    line_start,
                    line_byte_len,
                );
            } else {
                self.parse_paragraph(&lines, &mut pos, &mut line_idx, line_start, line_byte_len);
            }
        }
    }

    /// Parse a fenced code block (opening fence already consumed).
    #[allow(
        clippy::too_many_arguments,
        reason = "fence parameters are distinct concerns"
    )]
    fn parse_fenced_code(
        &mut self,
        lines: &[&str],
        pos: &mut usize,
        line_idx: &mut usize,
        open_start: usize,
        open_line_len: usize,
        fence_char: u8,
        fence_len: usize,
        info: Option<String>,
    ) {
        let content_start = self.body_offset + *pos;
        let mut info = Some(info);

        loop {
            if *line_idx >= lines.len() {
                let content_end = self.body_offset + *pos;
                self.blocks.push(Block {
                    kind: BlockKind::FencedCodeBlock {
                        info: info.take().unwrap_or_default(),
                        content_span: Span::new(content_start, content_end),
                    },
                    span: Span::new(open_start, self.body_offset + *pos),
                });
                self.diagnostics.push(BlockDiagnostic {
                    span: Span::new(open_start, open_start + open_line_len),
                    message: "unclosed fenced code block".to_string(),
                });
                break;
            }

            let inner_line = lines[*line_idx];
            let inner_byte_len = inner_line.len();
            let (inner_expanded, _) = expand_leading_tabs(inner_line);

            if fenced_code_close(&inner_expanded, fence_char, fence_len) {
                let content_end = self.body_offset + *pos;
                *pos += inner_byte_len;
                *line_idx += 1;

                self.blocks.push(Block {
                    kind: BlockKind::FencedCodeBlock {
                        info: info.take().unwrap_or_default(),
                        content_span: Span::new(content_start, content_end),
                    },
                    span: Span::new(open_start, self.body_offset + *pos),
                });
                break;
            }

            *pos += inner_byte_len;
            *line_idx += 1;
        }
    }

    /// Parse a block math span (opening `$$` already consumed).
    fn parse_block_math(
        &mut self,
        lines: &[&str],
        pos: &mut usize,
        line_idx: &mut usize,
        open_start: usize,
        open_line_len: usize,
    ) {
        let content_start = self.body_offset + *pos;
        let mut found_close = false;

        while *line_idx < lines.len() {
            let inner_line = lines[*line_idx];
            let inner_byte_len = inner_line.len();

            if block_math_close(inner_line) {
                let content_end = self.body_offset + *pos;
                *pos += inner_byte_len;
                *line_idx += 1;
                found_close = true;

                self.blocks.push(Block {
                    kind: BlockKind::BlockMath {
                        content_span: Span::new(content_start, content_end),
                    },
                    span: Span::new(open_start, self.body_offset + *pos),
                });
                break;
            }

            *pos += inner_byte_len;
            *line_idx += 1;
        }

        if !found_close {
            let content_end = self.body_offset + *pos;
            self.blocks.push(Block {
                kind: BlockKind::BlockMath {
                    content_span: Span::new(content_start, content_end),
                },
                span: Span::new(open_start, self.body_offset + *pos),
            });
            self.diagnostics.push(BlockDiagnostic {
                span: Span::new(open_start, open_start + open_line_len),
                message: "unclosed block math".to_string(),
            });
        }
    }

    /// Parse an HTML block.
    #[allow(
        clippy::too_many_arguments,
        reason = "HTML type and line info are distinct concerns"
    )]
    fn parse_html_block(
        &mut self,
        lines: &[&str],
        pos: &mut usize,
        line_idx: &mut usize,
        block_start: usize,
        first_line_len: usize,
        first_line: &str,
        html_type: u8,
    ) {
        if matches!(html_type, 6 | 7) {
            *pos += first_line_len;
            *line_idx += 1;

            while *line_idx < lines.len() {
                let inner_line = lines[*line_idx];
                if inner_line.trim().is_empty() {
                    break;
                }
                *pos += inner_line.len();
                *line_idx += 1;
            }
        } else {
            let end_on_first = html_block_end(first_line, html_type);
            *pos += first_line_len;
            *line_idx += 1;

            if !end_on_first {
                while *line_idx < lines.len() {
                    let inner_line = lines[*line_idx];
                    *pos += inner_line.len();
                    *line_idx += 1;

                    if html_block_end(inner_line, html_type) {
                        break;
                    }
                }
            }
        }

        self.blocks.push(Block {
            kind: BlockKind::HtmlBlock,
            span: Span::new(block_start, self.body_offset + *pos),
        });
    }

    /// Parse a block quote.
    fn parse_block_quote(
        &mut self,
        lines: &[&str],
        pos: &mut usize,
        line_idx: &mut usize,
        line_start: usize,
        first_line_len: usize,
        first_line: &str,
    ) {
        let mut content_parts: Vec<(usize, usize)> = Vec::new();

        if let Some((marker_len, _)) = strip_blockquote_marker(first_line) {
            content_parts.push((line_start + marker_len, line_start + first_line_len));
        }

        *pos += first_line_len;
        *line_idx += 1;

        while *line_idx < lines.len() {
            let inner_line = lines[*line_idx];
            let inner_start = self.body_offset + *pos;

            if inner_line.trim().is_empty() {
                break;
            }

            if let Some((ml, _)) = strip_blockquote_marker(inner_line) {
                content_parts.push((inner_start + ml, inner_start + inner_line.len()));
                *pos += inner_line.len();
                *line_idx += 1;
            } else if !is_thematic_break(inner_line)
                && atx_heading_level(inner_line).is_none()
                && fenced_code_open(inner_line).is_none()
                && html_block_start(inner_line).is_none()
            {
                content_parts.push((inner_start, inner_start + inner_line.len()));
                *pos += inner_line.len();
                *line_idx += 1;
            } else {
                break;
            }
        }

        let content_span = if content_parts.is_empty() {
            Span::new(line_start, line_start)
        } else {
            Span::new(
                content_parts[0].0,
                content_parts.last().map_or(line_start, |p| p.1),
            )
        };

        self.blocks.push(Block {
            kind: BlockKind::BlockQuote { content_span },
            span: Span::new(line_start, self.body_offset + *pos),
        });
    }

    /// Parse an indented code block.
    fn parse_indented_code(
        &mut self,
        lines: &[&str],
        pos: &mut usize,
        line_idx: &mut usize,
        block_start: usize,
        first_line_len: usize,
    ) {
        *pos += first_line_len;
        *line_idx += 1;

        while *line_idx < lines.len() {
            let inner_line = lines[*line_idx];
            let (inner_expanded, _) = expand_leading_tabs(inner_line);
            let inner_indent = count_indent(&inner_expanded);

            if inner_expanded.trim().is_empty() || inner_indent >= 4 {
                *pos += inner_line.len();
                *line_idx += 1;
            } else {
                break;
            }
        }

        self.blocks.push(Block {
            kind: BlockKind::IndentedCodeBlock,
            span: Span::new(block_start, self.body_offset + *pos),
        });
    }

    /// Parse a paragraph, detecting setext headings and thematic breaks.
    fn parse_paragraph(
        &mut self,
        lines: &[&str],
        pos: &mut usize,
        line_idx: &mut usize,
        para_start: usize,
        first_line_len: usize,
    ) {
        let mut para_text_end = para_start + first_line_len;
        *pos += first_line_len;
        *line_idx += 1;

        // Check if this single line is actually a standalone thematic break
        let first_line = &self.source[para_start..para_text_end];
        let first_trimmed = first_line.trim_end_matches('\n').trim_end_matches('\r');
        if is_thematic_break(first_trimmed) {
            self.blocks.push(Block {
                kind: BlockKind::ThematicBreak,
                span: Span::new(para_start, para_start + first_line_len),
            });
            return;
        }

        // Consume paragraph continuation lines
        loop {
            if *line_idx >= lines.len() {
                break;
            }

            let next_line = lines[*line_idx];
            let (next_expanded, _) = expand_leading_tabs(next_line);

            // Blank line ends paragraph
            if next_expanded.trim().is_empty() {
                break;
            }

            // Setext heading underline? (--- after paragraph is setext, not
            // thematic break)
            if let Some(level) = setext_level(&next_expanded) {
                *pos += next_line.len();
                *line_idx += 1;

                // Strip trailing line ending from text span so it contains
                // only the heading text, not syntax artifacts.
                let text_end = strip_trailing_newline(self.source, para_text_end);

                self.blocks.push(Block {
                    kind: BlockKind::SetextHeading {
                        level,
                        text_span: Span::new(para_start, text_end),
                    },
                    span: Span::new(para_start, self.body_offset + *pos),
                });
                return;
            }

            // Thematic break ends paragraph (only `***` and `___` reach
            // here — `---` was caught above as setext heading)
            if is_thematic_break(&next_expanded) {
                break;
            }

            // ATX heading ends paragraph
            if atx_heading_level(&next_expanded).is_some() {
                break;
            }

            // Fenced code block ends paragraph
            if fenced_code_open(&next_expanded).is_some() {
                break;
            }

            // Block quote ends paragraph
            if strip_blockquote_marker(&next_expanded).is_some() {
                break;
            }

            // HTML block start ends paragraph (types 1–6 only;
            // type 7 cannot interrupt a paragraph)
            if html_block_start(&next_expanded).is_some_and(|ht| ht <= 6) {
                break;
            }

            // Block math ends paragraph
            if block_math_open(&next_expanded) {
                break;
            }

            // Otherwise, continue the paragraph
            para_text_end = self.body_offset + *pos + next_line.len();
            *pos += next_line.len();
            *line_idx += 1;
        }

        self.blocks.push(Block {
            kind: BlockKind::Paragraph,
            span: Span::new(para_start, self.body_offset + *pos),
        });
    }

    /// Check if the previous block was a paragraph.
    fn prev_is_paragraph(&self) -> bool {
        self.blocks
            .last()
            .is_some_and(|b| matches!(b.kind, BlockKind::Paragraph))
    }
}

/// Split text into lines, preserving the line endings in each slice.
///
/// Each returned slice includes its trailing `\n` or `\r\n` if present.
fn split_lines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();

    while start < bytes.len() {
        if let Some(offset) = bytes[start..].iter().position(|&b| b == b'\n') {
            let end = start + offset + 1;
            lines.push(&text[start..end]);
            start = end;
        } else {
            lines.push(&text[start..]);
            start = bytes.len();
        }
    }

    lines
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    clippy::cast_possible_truncation,
    reason = "tests use expect, panic, and small casts for clarity"
)]
mod tests {
    use super::*;

    /// Helper: parse blocks with no frontmatter offset.
    fn parse(source: &str) -> BlockParseResult {
        parse_blocks(source, 0)
    }

    /// Helper: get the text of a span from source.
    fn span_text<'a>(source: &'a str, span: &Span) -> &'a str {
        &source[span.start..span.end]
    }

    // --- ATX headings ---

    #[test]
    fn atx_heading_levels() {
        let source = "# H1\n## H2\n### H3\n#### H4\n##### H5\n###### H6\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 6, "should find six headings");
        for (i, block) in result.blocks.iter().enumerate() {
            let expected_level = (i + 1) as u8;
            match &block.kind {
                BlockKind::AtxHeading { level, .. } => {
                    assert_eq!(
                        *level, expected_level,
                        "heading {i} should be level {expected_level}"
                    );
                }
                other => panic!("expected AtxHeading, got {other:?}"),
            }
        }
    }

    #[test]
    fn atx_heading_text_span() {
        let source = "## Hello World\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one heading");
        match &result.blocks[0].kind {
            BlockKind::AtxHeading { text_span, .. } => {
                assert_eq!(
                    span_text(source, text_span),
                    "Hello World",
                    "text span content"
                );
            }
            other => panic!("expected AtxHeading, got {other:?}"),
        }
    }

    #[test]
    fn atx_heading_with_explicit_id() {
        let source = "## My Heading {#custom-id}\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one heading");
        match &result.blocks[0].kind {
            BlockKind::AtxHeading {
                level,
                id,
                text_span,
            } => {
                assert_eq!(*level, 2, "heading level");
                assert_eq!(
                    span_text(source, text_span),
                    "My Heading",
                    "text span without id attribute"
                );
                let attr = id.as_ref().expect("should have id attribute");
                assert_eq!(attr.id, "custom-id", "id text");
            }
            other => panic!("expected AtxHeading, got {other:?}"),
        }
    }

    #[test]
    fn atx_heading_trailing_hashes() {
        let source = "## Heading ##\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one heading");
        match &result.blocks[0].kind {
            BlockKind::AtxHeading { text_span, .. } => {
                assert_eq!(
                    span_text(source, text_span),
                    "Heading",
                    "trailing hashes stripped"
                );
            }
            other => panic!("expected AtxHeading, got {other:?}"),
        }
    }

    #[test]
    fn atx_heading_empty() {
        let source = "#\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one heading");
        match &result.blocks[0].kind {
            BlockKind::AtxHeading {
                level, text_span, ..
            } => {
                assert_eq!(*level, 1, "heading level");
                assert!(text_span.is_empty(), "empty heading has empty text span");
            }
            other => panic!("expected AtxHeading, got {other:?}"),
        }
    }

    #[test]
    fn atx_heading_with_leading_spaces() {
        let source = "   ## Indented\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one heading");
        match &result.blocks[0].kind {
            BlockKind::AtxHeading {
                level, text_span, ..
            } => {
                assert_eq!(*level, 2, "heading level");
                assert_eq!(span_text(source, text_span), "Indented", "text content");
            }
            other => panic!("expected AtxHeading, got {other:?}"),
        }
    }

    #[test]
    fn four_leading_spaces_not_heading() {
        let source = "    ## Not a heading\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one block");
        assert!(
            !matches!(result.blocks[0].kind, BlockKind::AtxHeading { .. }),
            "4+ spaces should not be a heading"
        );
    }

    // --- Setext headings ---

    #[test]
    fn setext_heading_level_1() {
        let source = "Heading\n=======\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one block");
        match &result.blocks[0].kind {
            BlockKind::SetextHeading { level, text_span } => {
                assert_eq!(*level, 1, "setext level 1");
                assert_eq!(
                    span_text(source, text_span),
                    "Heading",
                    "text span is heading text without trailing newline"
                );
            }
            other => panic!("expected SetextHeading, got {other:?}"),
        }
    }

    #[test]
    fn setext_heading_level_2() {
        let source = "Heading\n-------\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one block");
        match &result.blocks[0].kind {
            BlockKind::SetextHeading { level, text_span } => {
                assert_eq!(*level, 2, "setext level 2");
                assert_eq!(
                    span_text(source, text_span),
                    "Heading",
                    "text span without trailing newline"
                );
            }
            other => panic!("expected SetextHeading, got {other:?}"),
        }
    }

    #[test]
    fn setext_heading_multiline() {
        let source = "Line one\nLine two\n=========\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one block");
        match &result.blocks[0].kind {
            BlockKind::SetextHeading { level, text_span } => {
                assert_eq!(*level, 1, "setext level 1");
                assert_eq!(
                    span_text(source, text_span),
                    "Line one\nLine two",
                    "multiline: internal newlines preserved, trailing stripped"
                );
            }
            other => panic!("expected SetextHeading, got {other:?}"),
        }
    }

    // --- Setext vs thematic break ---

    #[test]
    fn dashes_after_paragraph_is_setext() {
        let source = "Paragraph\n---\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one block");
        match &result.blocks[0].kind {
            BlockKind::SetextHeading { level, .. } => {
                assert_eq!(*level, 2, "--- after paragraph is setext heading");
            }
            other => panic!("expected SetextHeading, got {other:?}"),
        }
    }

    #[test]
    fn dashes_after_blank_is_thematic_break() {
        let source = "\n---\n";
        let result = parse(source);

        let non_blank: Vec<_> = result
            .blocks
            .iter()
            .filter(|b| !matches!(b.kind, BlockKind::BlankLine))
            .collect();
        assert_eq!(non_blank.len(), 1, "should find one non-blank block");
        assert!(
            matches!(non_blank[0].kind, BlockKind::ThematicBreak),
            "--- after blank line is thematic break"
        );
    }

    #[test]
    fn dashes_at_document_start_is_thematic_break() {
        let source = "---\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one block");
        assert!(
            matches!(result.blocks[0].kind, BlockKind::ThematicBreak),
            "--- at start is thematic break"
        );
    }

    // --- Thematic breaks ---

    #[test]
    fn thematic_break_stars() {
        let source = "***\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one block");
        assert!(
            matches!(result.blocks[0].kind, BlockKind::ThematicBreak),
            "*** is thematic break"
        );
    }

    #[test]
    fn thematic_break_underscores() {
        let source = "___\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one block");
        assert!(
            matches!(result.blocks[0].kind, BlockKind::ThematicBreak),
            "___ is thematic break"
        );
    }

    #[test]
    fn thematic_break_with_spaces() {
        let source = "* * * *\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one block");
        assert!(
            matches!(result.blocks[0].kind, BlockKind::ThematicBreak),
            "* * * * is thematic break"
        );
    }

    #[test]
    fn thematic_break_with_many_chars() {
        let source = "----------\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one block");
        assert!(
            matches!(result.blocks[0].kind, BlockKind::ThematicBreak),
            "many dashes is thematic break"
        );
    }

    // --- Fenced code blocks ---

    #[test]
    fn fenced_code_backticks() {
        let source = "```\ncode here\n```\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one block");
        match &result.blocks[0].kind {
            BlockKind::FencedCodeBlock { info, content_span } => {
                assert!(info.is_none(), "no info string");
                assert_eq!(
                    span_text(source, content_span),
                    "code here\n",
                    "content span"
                );
            }
            other => panic!("expected FencedCodeBlock, got {other:?}"),
        }
    }

    #[test]
    fn fenced_code_tildes() {
        let source = "~~~\ncode here\n~~~\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one block");
        assert!(
            matches!(result.blocks[0].kind, BlockKind::FencedCodeBlock { .. }),
            "tilde fence"
        );
    }

    #[test]
    fn fenced_code_with_info_string() {
        let source = "```rust\nfn main() {}\n```\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one block");
        match &result.blocks[0].kind {
            BlockKind::FencedCodeBlock { info, .. } => {
                assert_eq!(info.as_deref(), Some("rust"), "info string");
            }
            other => panic!("expected FencedCodeBlock, got {other:?}"),
        }
    }

    #[test]
    fn fenced_code_unclosed() {
        let source = "```\ncode here\nmore code\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one block");
        assert!(
            matches!(result.blocks[0].kind, BlockKind::FencedCodeBlock { .. }),
            "unclosed code block"
        );
        assert_eq!(result.diagnostics.len(), 1, "should emit one diagnostic");
        assert!(
            result.diagnostics[0].message.contains("unclosed"),
            "diagnostic mentions unclosed"
        );
    }

    #[test]
    fn fenced_code_longer_close() {
        let source = "```\ncode\n`````\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one block");
        match &result.blocks[0].kind {
            BlockKind::FencedCodeBlock { content_span, .. } => {
                assert_eq!(
                    span_text(source, content_span),
                    "code\n",
                    "longer close fence accepted"
                );
            }
            other => panic!("expected FencedCodeBlock, got {other:?}"),
        }
    }

    #[test]
    fn fenced_code_shorter_close_not_accepted() {
        let source = "````\ncode\n```\nmore\n````\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one block");
        match &result.blocks[0].kind {
            BlockKind::FencedCodeBlock { content_span, .. } => {
                assert_eq!(
                    span_text(source, content_span),
                    "code\n```\nmore\n",
                    "shorter fence is content"
                );
            }
            other => panic!("expected FencedCodeBlock, got {other:?}"),
        }
    }

    // --- Block math ---

    #[test]
    fn block_math_basic() {
        let source = "$$\nx + y = z\n$$\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one block");
        match &result.blocks[0].kind {
            BlockKind::BlockMath { content_span } => {
                assert_eq!(
                    span_text(source, content_span),
                    "x + y = z\n",
                    "math content"
                );
            }
            other => panic!("expected BlockMath, got {other:?}"),
        }
    }

    #[test]
    fn block_math_unclosed() {
        let source = "$$\nmath content\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one block");
        assert!(
            matches!(result.blocks[0].kind, BlockKind::BlockMath { .. }),
            "unclosed block math"
        );
        assert_eq!(result.diagnostics.len(), 1, "should emit one diagnostic");
        assert!(
            result.diagnostics[0].message.contains("unclosed"),
            "diagnostic mentions unclosed"
        );
    }

    // --- Indented code blocks ---

    #[test]
    fn indented_code_block() {
        let source = "    code line 1\n    code line 2\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one block");
        assert!(
            matches!(result.blocks[0].kind, BlockKind::IndentedCodeBlock),
            "indented code block"
        );
    }

    #[test]
    fn indented_code_not_after_paragraph() {
        // Indented text continuing a paragraph is paragraph continuation,
        // not an indented code block
        let source = "Paragraph\n    continuation\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one block");
        assert!(
            matches!(result.blocks[0].kind, BlockKind::Paragraph),
            "indented continuation is part of paragraph"
        );
    }

    // --- Block quotes ---

    #[test]
    fn block_quote_simple() {
        let source = "> quoted text\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one block");
        assert!(
            matches!(result.blocks[0].kind, BlockKind::BlockQuote { .. }),
            "block quote"
        );
    }

    #[test]
    fn block_quote_multiline() {
        let source = "> line one\n> line two\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one block");
        assert!(
            matches!(result.blocks[0].kind, BlockKind::BlockQuote { .. }),
            "multiline block quote"
        );
    }

    #[test]
    fn block_quote_lazy_continuation() {
        let source = "> first line\nlazy continuation\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one block");
        assert!(
            matches!(result.blocks[0].kind, BlockKind::BlockQuote { .. }),
            "lazy continuation in block quote"
        );
    }

    #[test]
    fn block_quote_nested() {
        let source = "> > nested\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one block");
        assert!(
            matches!(result.blocks[0].kind, BlockKind::BlockQuote { .. }),
            "nested block quote"
        );
    }

    // --- HTML blocks ---

    #[test]
    fn html_block_type1_pre() {
        let source = "<pre>\ncode\n</pre>\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one block");
        assert!(
            matches!(result.blocks[0].kind, BlockKind::HtmlBlock),
            "HTML block type 1 (pre)"
        );
    }

    #[test]
    fn html_block_type2_comment() {
        let source = "<!-- comment -->\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one block");
        assert!(
            matches!(result.blocks[0].kind, BlockKind::HtmlBlock),
            "HTML block type 2 (comment)"
        );
    }

    #[test]
    fn html_block_type6_div() {
        let source = "<div>\ncontent\n</div>\n\n";
        let result = parse(source);

        let non_blank: Vec<_> = result
            .blocks
            .iter()
            .filter(|b| !matches!(b.kind, BlockKind::BlankLine))
            .collect();
        assert_eq!(non_blank.len(), 1, "should find one non-blank block");
        assert!(
            matches!(non_blank[0].kind, BlockKind::HtmlBlock),
            "HTML block type 6 (div)"
        );
    }

    #[test]
    fn html_block_type7_cannot_interrupt_paragraph() {
        // Type 7 HTML block cannot interrupt a paragraph
        let source = "Paragraph\n<span>inline</span>\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one block");
        assert!(
            matches!(result.blocks[0].kind, BlockKind::Paragraph),
            "type 7 HTML cannot interrupt paragraph"
        );
    }

    // --- Blank lines ---

    #[test]
    fn blank_lines() {
        let source = "\n\n\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 3, "should find three blank lines");
        for block in &result.blocks {
            assert!(
                matches!(block.kind, BlockKind::BlankLine),
                "should be blank line"
            );
        }
    }

    // --- Paragraphs ---

    #[test]
    fn simple_paragraph() {
        let source = "Hello world.\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one block");
        assert!(
            matches!(result.blocks[0].kind, BlockKind::Paragraph),
            "simple paragraph"
        );
    }

    #[test]
    fn multiline_paragraph() {
        let source = "Line one.\nLine two.\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one block");
        assert!(
            matches!(result.blocks[0].kind, BlockKind::Paragraph),
            "multiline paragraph"
        );
    }

    // --- Mixed constructs ---

    #[test]
    fn mixed_blocks() {
        let source = "# Heading\n\nParagraph text.\n\n---\n\n```\ncode\n```\n";
        let result = parse(source);

        let kinds: Vec<_> = result.blocks.iter().map(|b| &b.kind).collect();
        assert!(
            matches!(kinds[0], BlockKind::AtxHeading { level: 1, .. }),
            "first block is ATX heading"
        );
        assert!(
            matches!(kinds[1], BlockKind::BlankLine),
            "second block is blank"
        );
        assert!(
            matches!(kinds[2], BlockKind::Paragraph),
            "third block is paragraph"
        );
        assert!(
            matches!(kinds[3], BlockKind::BlankLine),
            "fourth block is blank"
        );
        assert!(
            matches!(kinds[4], BlockKind::ThematicBreak),
            "fifth block is thematic break"
        );
        assert!(
            matches!(kinds[5], BlockKind::BlankLine),
            "sixth block is blank"
        );
        assert!(
            matches!(kinds[6], BlockKind::FencedCodeBlock { .. }),
            "seventh block is code block"
        );
    }

    // --- Tab expansion ---

    #[test]
    fn tab_expansion_basic() {
        let (expanded, _) = expand_leading_tabs("\tcode");
        assert_eq!(expanded, "    code", "tab at column 0 expands to 4 spaces");
    }

    #[test]
    fn tab_expansion_partial() {
        let (expanded, _) = expand_leading_tabs(" \tcode");
        assert_eq!(expanded, "    code", "tab at column 1 expands to 3 spaces");
    }

    #[test]
    fn tab_expansion_aligned() {
        let (expanded, _) = expand_leading_tabs("    \tcode");
        assert_eq!(
            expanded, "        code",
            "tab at column 4 expands to 4 spaces"
        );
    }

    #[test]
    fn tab_indented_code_block() {
        let source = "\tcode line\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one block");
        assert!(
            matches!(result.blocks[0].kind, BlockKind::IndentedCodeBlock),
            "tab-indented line is indented code block"
        );
    }

    #[test]
    fn tab_not_expanded_inside_content() {
        let (expanded, _) = expand_leading_tabs("text\there");
        assert_eq!(expanded, "text\there", "tab inside content is preserved");
    }

    // --- Frontmatter offset ---

    #[test]
    fn body_offset_shifts_spans() {
        let source = "---\ntitle: test\n---\n# Heading\n";
        let body_offset = source.find("# Heading").expect("should find heading");
        let result = parse_blocks(source, body_offset);

        assert_eq!(result.blocks.len(), 1, "should find one block");
        match &result.blocks[0].kind {
            BlockKind::AtxHeading { text_span, .. } => {
                assert_eq!(
                    span_text(source, text_span),
                    "Heading",
                    "text span in original source with offset"
                );
            }
            other => panic!("expected AtxHeading, got {other:?}"),
        }
    }

    // --- Span correctness ---

    #[test]
    fn spans_cover_original_source() {
        let source = "# Heading\n\nParagraph\n";
        let result = parse(source);

        for block in &result.blocks {
            let text = span_text(source, &block.span);
            assert!(
                !text.is_empty() || matches!(block.kind, BlockKind::BlankLine),
                "block span should reference source text: {block:?}"
            );
        }
    }

    #[test]
    fn no_text_copied() {
        // All spans should be valid substrings of the source
        let source = "## Title\n\n> Quote\n\n```\ncode\n```\n\n---\n";
        let result = parse(source);

        for block in &result.blocks {
            assert!(
                block.span.start <= block.span.end,
                "span start <= end: {block:?}"
            );
            assert!(
                block.span.end <= source.len(),
                "span end <= source length: {block:?}"
            );
        }
    }

    // --- HTML block types ---

    #[test]
    fn html_block_type3_processing_instruction() {
        let source = "<?xml version=\"1.0\"?>\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one block");
        assert!(
            matches!(result.blocks[0].kind, BlockKind::HtmlBlock),
            "HTML block type 3 (processing instruction)"
        );
    }

    #[test]
    fn html_block_type4_declaration() {
        let source = "<!DOCTYPE html>\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one block");
        assert!(
            matches!(result.blocks[0].kind, BlockKind::HtmlBlock),
            "HTML block type 4 (declaration)"
        );
    }

    #[test]
    fn html_block_type5_cdata() {
        let source = "<![CDATA[\nsome data\n]]>\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one block");
        assert!(
            matches!(result.blocks[0].kind, BlockKind::HtmlBlock),
            "HTML block type 5 (CDATA)"
        );
    }

    #[test]
    fn html_block_multiline_comment() {
        let source = "<!-- start\nmiddle\nend -->\n";
        let result = parse(source);

        assert_eq!(result.blocks.len(), 1, "should find one block");
        assert!(
            matches!(result.blocks[0].kind, BlockKind::HtmlBlock),
            "multiline HTML comment"
        );
    }
}
