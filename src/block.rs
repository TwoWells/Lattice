// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Block-level markdown parser with tree output.
//!
//! Reads source text line by line and classifies each line into a
//! block-level construct, building a [`Tree`] of [`Node`] entries with
//! parent/children references and a scope stack. Block quotes are
//! container nodes whose children are parsed inline — no deferred
//! re-parsing.
//!
//! This module does **not** parse inline content (links, emphasis,
//! images). Inline parsing happens in a later ticket over completed
//! leaf nodes.

use crate::span::Span;

// ---------------------------------------------------------------------------
// Tree types
// ---------------------------------------------------------------------------

/// Index into `Tree::nodes`.
pub type NodeId = usize;

/// Classification of a structural element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElementKind {
    /// Root node — every tree has exactly one.
    Document,
    /// YAML frontmatter block (including `---` delimiters).
    Frontmatter,
    /// ATX or setext heading.
    Heading {
        /// Heading level (1–6).
        level: u8,
    },
    /// Thematic break (`---`, `***`, `___` with variations).
    Rules,
    /// Paragraph text.
    Paragraph,
    /// Fenced or indented code block.
    CodeBlock,
    /// Block math (`$$` delimiters).
    Math,
    /// Block quote container (`>`).
    QuoteBlock,
    /// HTML block (opaque at this stage).
    HtmlBlock,
    /// Link reference definition (`[label]: url "title"`).
    ReferenceDef {
        /// Normalized label (case-folded, whitespace-collapsed).
        label: String,
        /// Link destination URL.
        url: String,
        /// Link title (empty if none).
        title: String,
    },
    /// Footnote definition container (`[^label]: content`).
    FootnoteDef {
        /// Footnote label (without `^` prefix).
        label: String,
    },
    /// Inline or reference-style link.
    Link {
        /// Link destination URL.
        url: String,
        /// Link title / predicate (empty if none).
        title: String,
    },
    /// Inline or reference-style image.
    Image {
        /// Image source URL.
        url: String,
        /// Image title (empty if none).
        title: String,
    },
    /// Footnote reference call site (`[^label]`).
    FootnoteRef {
        /// Footnote label (without `^` prefix).
        label: String,
    },
    /// Inline code span (backtick-delimited, content skipped).
    InlineCode,
    /// Inline math span (`$...$`, content skipped).
    InlineMath,
    /// Import directive (`@path`).
    Import {
        /// The import path (without leading `@`).
        path: String,
    },
    /// List container (ordered or unordered).
    List {
        /// Whether this is an ordered list.
        ordered: bool,
        /// Start number (0 for unordered).
        start: u32,
        /// Whether the list is tight (no blank lines between items).
        tight: bool,
    },
    /// List item container.
    ListItem {
        /// Task state: `None` for regular items, `Some(false)` for
        /// unchecked, `Some(true)` for checked.
        task: Option<bool>,
    },
    /// GFM pipe table container.
    Table {
        /// Per-column alignment derived from the delimiter row.
        alignments: Vec<TableAlignment>,
    },
    /// A row in a GFM pipe table.
    TableRow {
        /// Whether this is the header row.
        header: bool,
    },
    /// A cell in a GFM pipe table row.
    TableCell,
}

/// Column alignment for a GFM pipe table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableAlignment {
    /// Left-aligned (default): `---` or `:---`.
    Left,
    /// Center-aligned: `:---:`.
    Center,
    /// Right-aligned: `---:`.
    Right,
}

/// Which syntax produced a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Syntax {
    /// YAML frontmatter.
    Yaml,
    /// Markdown structural syntax.
    Markdown,
    /// Raw HTML.
    #[allow(dead_code, reason = "used by HTML tag parser ticket 04")]
    Html,
}

/// A node in the parse tree.
#[derive(Debug)]
pub struct Node {
    /// What kind of element this is.
    pub kind: ElementKind,
    /// Which syntax produced this node.
    pub syntax: Syntax,
    /// Byte range in the original source covering this node.
    pub span: Span,
    /// Parent node, if any (`None` only for `Document`).
    pub parent: Option<NodeId>,
    /// Child nodes in document order.
    pub children: Vec<NodeId>,
}

/// A diagnostic emitted during parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// Location of the error in the source.
    pub span: Span,
    /// Human-readable message.
    pub message: String,
}

/// Parse tree over the source text.
///
/// The source text is the data. The tree is a structural view over
/// it — spans into the source, not extracted content.
#[derive(Debug)]
pub struct Tree {
    /// The full source text.
    source: String,
    /// All nodes in allocation order. Index 0 is always `Document`.
    nodes: Vec<Node>,
    /// Diagnostics emitted during parsing.
    diagnostics: Vec<Diagnostic>,
}

impl Tree {
    /// The full source text.
    #[must_use]
    pub fn source(&self) -> &str {
        &self.source
    }

    /// All nodes in the tree.
    #[must_use]
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    /// Get a node by its ID.
    #[must_use]
    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id]
    }

    /// The root `Document` node (always index 0).
    #[must_use]
    #[allow(
        clippy::unused_self,
        reason = "consistent accessor API; root may vary in later tickets"
    )]
    pub fn root(&self) -> NodeId {
        0
    }

    /// Diagnostics emitted during parsing.
    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// Slice the source text for a span.
    #[must_use]
    pub fn text(&self, span: &Span) -> &str {
        &self.source[span.start..span.end]
    }

    /// The number of nodes in the tree.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the tree is empty (it never is — always has `Document`).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Direct children of a node.
    #[must_use]
    pub fn children(&self, id: NodeId) -> &[NodeId] {
        &self.nodes[id].children
    }

    /// Add a child node to an existing node (used by the inline parser).
    pub fn add_child(
        &mut self,
        parent: NodeId,
        kind: ElementKind,
        syntax: Syntax,
        span: Span,
    ) -> NodeId {
        let id = self.nodes.len();
        self.nodes.push(Node {
            kind,
            syntax,
            span,
            parent: Some(parent),
            children: Vec::new(),
        });
        self.nodes[parent].children.push(id);
        id
    }

    /// Append a diagnostic (used by the inline parser).
    pub fn add_diagnostic(&mut self, diagnostic: Diagnostic) {
        self.diagnostics.push(diagnostic);
    }
}

/// An explicit `{#id}` attribute on an ATX heading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtxId {
    /// The ID text (without `{#` and `}`).
    pub id: String,
    /// Span of the ID text in the source.
    pub span: Span,
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
    original_byte: usize,
    /// Column position where expansion starts.
    expanded_col: usize,
    /// Number of spaces this tab expanded to.
    num_spaces: usize,
}

/// Map an offset in a tab-expanded string back to the corresponding byte
/// offset in the original (pre-expansion) string.
///
/// Uses the tab mappings produced by [`expand_leading_tabs`]. If there are
/// no tabs, the expanded and raw offsets are identical.
fn expanded_to_raw(expanded_offset: usize, raw_line: &str, mappings: &[TabMapping]) -> usize {
    if mappings.is_empty() {
        return expanded_offset.min(raw_line.len());
    }

    // Walk through tab mappings. Each tab occupies columns
    // [expanded_col .. expanded_col + num_spaces) in the expanded string
    // but only 1 byte in the raw string. We track the cumulative extra
    // bytes introduced by prior tab expansions.
    let mut delta: usize = 0;
    for m in mappings {
        if expanded_offset <= m.expanded_col {
            // Before this tab — delta is from prior tabs only.
            break;
        }
        let tab_expanded_end = m.expanded_col + m.num_spaces;
        if expanded_offset < tab_expanded_end {
            // Inside the expansion of this tab — map to just past the tab byte.
            return m.original_byte + 1;
        }
        // Past this tab: it added (num_spaces - 1) extra bytes.
        delta += m.num_spaces - 1;
    }

    expanded_offset.saturating_sub(delta).min(raw_line.len())
}

// ---------------------------------------------------------------------------
// Line classification helpers
// ---------------------------------------------------------------------------

/// Count leading spaces in a string (after tab expansion).
fn count_indent(line: &str) -> usize {
    line.bytes().take_while(|&b| b == b' ').count()
}

/// Strip up to `n` leading spaces from a string.
///
/// If the string has fewer than `n` leading spaces, strips all of them.
fn strip_n_spaces(line: &str, n: usize) -> &str {
    let count = line.bytes().take(n).take_while(|&b| b == b' ').count();
    &line[count..]
}

/// Strip a trailing `\n` or `\r\n` from a byte offset into source.
///
/// Returns the adjusted end offset with the line ending excluded.
#[allow(dead_code, reason = "used by consumer migration ticket 06")]
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

/// Normalize a reference label per `CommonMark` rules.
///
/// Case-fold (lowercase) and collapse consecutive whitespace to a single space.
pub fn normalize_label(label: &str) -> String {
    label
        .split_whitespace()
        .collect::<Vec<&str>>()
        .join(" ")
        .to_lowercase()
}

/// Try to parse a link reference definition from a line.
///
/// Returns `Some((label, url, title))` if the line is a reference definition.
/// Labels are normalized (case-folded, whitespace-collapsed).
fn parse_reference_def(line: &str) -> Option<(String, String, String)> {
    let trimmed = line.trim_start_matches(' ');
    let indent = line.len() - trimmed.len();
    if indent > 3 {
        return None;
    }

    let rest = trimmed.strip_prefix('[')?;

    // Must not start with `^` (that is a footnote definition)
    if rest.starts_with('^') {
        return None;
    }

    let bracket_end = rest.find("]:")?;
    let label_text = &rest[..bracket_end];

    if label_text.is_empty() || label_text.trim().is_empty() || label_text.len() > 999 {
        return None;
    }

    // No unescaped `[` in label
    if label_text.contains('[') {
        return None;
    }

    let after = rest[bracket_end + 2..].trim_start();

    if after.is_empty() || after.starts_with('\n') || after.starts_with('\r') {
        return None;
    }

    // Parse URL (optionally angle-bracketed)
    let (url, rest_after_url) = if let Some(inner) = after.strip_prefix('<') {
        let close = inner.find('>')?;
        (inner[..close].to_string(), inner[close + 1..].trim_start())
    } else {
        let end = after
            .find(|c: char| c.is_whitespace())
            .unwrap_or(after.len());
        if end == 0 {
            return None;
        }
        (after[..end].to_string(), after[end..].trim_start())
    };

    // Parse optional title
    let title = if rest_after_url.trim().is_empty() {
        String::new()
    } else if let Some(s) = rest_after_url.strip_prefix('"') {
        let end = s.find('"')?;
        if !s[end + 1..].trim().is_empty() {
            return None;
        }
        s[..end].to_string()
    } else if let Some(s) = rest_after_url.strip_prefix('\'') {
        let end = s.find('\'')?;
        if !s[end + 1..].trim().is_empty() {
            return None;
        }
        s[..end].to_string()
    } else if let Some(s) = rest_after_url.strip_prefix('(') {
        let end = s.find(')')?;
        if !s[end + 1..].trim().is_empty() {
            return None;
        }
        s[..end].to_string()
    } else {
        return None;
    };

    Some((normalize_label(label_text), url, title))
}

/// Try to parse a standalone title line (for multi-line reference definitions).
///
/// Matches `"title"`, `'title'`, or `(title)` optionally surrounded by
/// whitespace, with nothing else on the line.
fn parse_standalone_title(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if let Some(s) = trimmed.strip_prefix('"') {
        let end = s.find('"')?;
        if s[end + 1..].trim().is_empty() {
            return Some(s[..end].to_string());
        }
    } else if let Some(s) = trimmed.strip_prefix('\'') {
        let end = s.find('\'')?;
        if s[end + 1..].trim().is_empty() {
            return Some(s[..end].to_string());
        }
    } else if let Some(s) = trimmed.strip_prefix('(') {
        let end = s.find(')')?;
        if s[end + 1..].trim().is_empty() {
            return Some(s[..end].to_string());
        }
    }
    None
}

/// Try to parse the start of a footnote definition.
///
/// Returns `Some(label)` if the line starts with `[^label]:`.
fn parse_footnote_def_start(line: &str) -> Option<String> {
    let trimmed = line.trim_start_matches(' ');
    let indent = line.len() - trimmed.len();
    if indent > 3 {
        return None;
    }

    let rest = trimmed.strip_prefix("[^")?;
    let label_end = rest.find(']')?;
    let label = &rest[..label_end];

    if label.is_empty() || label.contains('[') || label.contains(']') {
        return None;
    }

    let after_bracket = &rest[label_end + 1..];
    if !after_bracket.starts_with(':') {
        return None;
    }

    Some(label.to_string())
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
pub fn extract_atx_content(original_line: &str, line_start: usize) -> (Span, Option<AtxId>) {
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
// List helpers
// ---------------------------------------------------------------------------

/// Information about a recognized list marker.
struct ListMarkerInfo {
    /// Whether this is an ordered list.
    ordered: bool,
    /// The marker character: bullet char (`-`, `*`, `+`) for unordered,
    /// or delimiter (`.`, `)`) for ordered.
    marker_char: u8,
    /// Start number for ordered lists, 0 for unordered.
    start: u32,
    /// Column where the marker starts (leading spaces).
    marker_indent: usize,
    /// Column where item content starts (after marker + spaces).
    content_column: usize,
    /// Byte offset into the line where content begins.
    content_offset: usize,
}

/// Recognize a list marker at the start of a (tab-expanded) line.
///
/// Returns `None` if the line doesn't start with a list marker, or if
/// the line is actually a thematic break.
fn recognize_list_marker(line: &str) -> Option<ListMarkerInfo> {
    let trimmed = line.trim_start_matches(' ');
    let indent = line.len() - trimmed.len();
    if indent > 3 || trimmed.is_empty() {
        return None;
    }

    // Reject thematic breaks — they take priority over list markers.
    let trimmed_end = trimmed.trim_end();
    if is_thematic_break(trimmed_end) {
        return None;
    }

    let first = trimmed.as_bytes()[0];

    if matches!(first, b'-' | b'*' | b'+') {
        // Unordered: marker char + at least one space.
        let after_marker = &trimmed[1..];
        if after_marker.is_empty() || !after_marker.starts_with(' ') {
            return None;
        }
        let spaces_after = after_marker.len() - after_marker.trim_start_matches(' ').len();
        let content_column = indent + 1 + spaces_after;
        let content_offset = content_column;
        // If rest is blank, content column = marker pos + 2
        let content_column = if after_marker.trim().is_empty() {
            indent + 2
        } else {
            content_column
        };
        let content_offset = if after_marker.trim().is_empty() {
            line.len()
        } else {
            content_offset
        };
        Some(ListMarkerInfo {
            ordered: false,
            marker_char: first,
            start: 0,
            marker_indent: indent,
            content_column,
            content_offset,
        })
    } else if first.is_ascii_digit() {
        // Ordered: digits + delimiter (. or )) + at least one space.
        let digit_count = trimmed.bytes().take_while(u8::is_ascii_digit).count();
        if digit_count == 0 || digit_count > 9 {
            return None;
        }
        let after_digits = &trimmed[digit_count..];
        if after_digits.is_empty() {
            return None;
        }
        let delimiter = after_digits.as_bytes()[0];
        if !matches!(delimiter, b'.' | b')') {
            return None;
        }
        let after_delim = &after_digits[1..];
        if after_delim.is_empty() || !after_delim.starts_with(' ') {
            return None;
        }
        let start: u32 = trimmed[..digit_count].parse().ok()?;
        let spaces_after = after_delim.len() - after_delim.trim_start_matches(' ').len();
        let marker_width = digit_count + 1; // digits + delimiter
        let content_column = indent + marker_width + spaces_after;
        let content_offset = content_column;
        let content_column = if after_delim.trim().is_empty() {
            indent + marker_width + 1
        } else {
            content_column
        };
        let content_offset = if after_delim.trim().is_empty() {
            line.len()
        } else {
            content_offset
        };
        Some(ListMarkerInfo {
            ordered: true,
            marker_char: delimiter,
            start,
            marker_indent: indent,
            content_column,
            content_offset,
        })
    } else {
        None
    }
}

/// Recognize a task list item checkbox at the start of item content.
///
/// Returns `Some(false)` for `[ ] `, `Some(true)` for `[x] ` or `[X] `.
fn recognize_task(content: &str) -> Option<bool> {
    if content.starts_with("[ ] ") {
        Some(false)
    } else if content.starts_with("[x] ") || content.starts_with("[X] ") {
        Some(true)
    } else {
        None
    }
}

/// Tracking state for an open list on the scope stack.
struct ListContext {
    /// The `List` node ID in the tree.
    list_node: NodeId,
    /// The current `ListItem` node ID.
    item_node: NodeId,
    /// Marker character: bullet for unordered, delimiter for ordered.
    marker_char: u8,
    /// Whether this is an ordered list.
    ordered: bool,
    /// Column where the marker starts.
    marker_indent: usize,
    /// Column where item content starts.
    content_column: usize,
    /// A blank line was seen in the current item.
    saw_blank: bool,
    /// Any blank line appeared between items (list is loose).
    loose: bool,
}

// ---------------------------------------------------------------------------
// Table helpers
// ---------------------------------------------------------------------------

/// Parse a GFM delimiter row and return per-column alignments.
///
/// A delimiter row consists of cells separated by pipes, where each cell
/// is optional `:`, at least one `-`, optional `:`, surrounded by optional
/// spaces. Returns `None` if the line is not a valid delimiter row or has
/// zero columns.
fn parse_delimiter_row(line: &str) -> Option<Vec<TableAlignment>> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Strip optional leading/trailing pipes.
    let inner = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let inner = inner.strip_suffix('|').unwrap_or(inner);

    if inner.trim().is_empty() {
        return None;
    }

    let mut alignments = Vec::new();
    for cell in inner.split('|') {
        let cell = cell.trim();
        if cell.is_empty() {
            return None;
        }
        let left = cell.starts_with(':');
        let right = cell.ends_with(':');
        let dashes = cell
            .trim_start_matches(':')
            .trim_end_matches(':')
            .trim_matches(' ');
        if dashes.is_empty() || !dashes.bytes().all(|b| b == b'-') {
            return None;
        }
        alignments.push(match (left, right) {
            (true, true) => TableAlignment::Center,
            (false, true) => TableAlignment::Right,
            _ => TableAlignment::Left,
        });
    }

    if alignments.is_empty() {
        None
    } else {
        Some(alignments)
    }
}

/// Split a table row into cell content spans, respecting backtick code spans.
///
/// Pipes inside backtick code spans do not split cells. Returns byte offsets
/// relative to `row_start` for each cell's trimmed content.
fn split_table_cells(line: &str, row_start: usize) -> Vec<Span> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    // Locate `trimmed` within `line`.
    let trim_offset = line.len() - line.trim_start().len();
    let inner_start_in_line = trim_offset;

    // Strip optional leading pipe.
    let (inner, inner_offset) = trimmed
        .strip_prefix('|')
        .map_or((trimmed, inner_start_in_line), |stripped| {
            (stripped, inner_start_in_line + 1)
        });

    // Strip optional trailing pipe.
    let inner = if inner.ends_with('|') && !inner.ends_with("\\|") {
        &inner[..inner.len() - 1]
    } else {
        inner
    };

    let bytes = inner.as_bytes();
    let mut cells = Vec::new();
    let mut cell_start = 0;
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'`' {
            // Skip backtick code span.
            let bt_count = bytes[i..].iter().take_while(|&&b| b == b'`').count();
            let after = i + bt_count;
            // Find matching closing backticks.
            if let Some(close_pos) = inner[after..].find(&inner[i..i + bt_count]) {
                i = after + close_pos + bt_count;
            } else {
                i = bytes.len();
            }
        } else if bytes[i] == b'\\' && i + 1 < bytes.len() && bytes[i + 1] == b'|' {
            // Escaped pipe — skip both characters.
            i += 2;
        } else if bytes[i] == b'|' {
            // Cell boundary.
            let raw = &inner[cell_start..i];
            let cell_trimmed = raw.trim();
            if cell_trimmed.is_empty() {
                cells.push(Span::new(
                    row_start + inner_offset + cell_start,
                    row_start + inner_offset + cell_start,
                ));
            } else {
                let leading = raw.len() - raw.trim_start().len();
                let s = cell_start + leading;
                let e = s + cell_trimmed.len();
                cells.push(Span::new(
                    row_start + inner_offset + s,
                    row_start + inner_offset + e,
                ));
            }
            cell_start = i + 1;
            i += 1;
        } else {
            i += 1;
        }
    }

    // Last cell after the final pipe.
    let raw = &inner[cell_start..];
    let cell_trimmed = raw.trim();
    if cell_trimmed.is_empty() {
        cells.push(Span::new(
            row_start + inner_offset + cell_start,
            row_start + inner_offset + cell_start,
        ));
    } else {
        let leading = raw.len() - raw.trim_start().len();
        let s = cell_start + leading;
        let e = s + cell_trimmed.len();
        cells.push(Span::new(
            row_start + inner_offset + s,
            row_start + inner_offset + e,
        ));
    }

    cells
}

/// Check if a line could be a table row (has at least one unescaped pipe
/// outside backtick code spans).
fn is_table_row(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            let bt_count = bytes[i..].iter().take_while(|&&b| b == b'`').count();
            let after = i + bt_count;
            if let Some(close_pos) = line[after..].find(&line[i..i + bt_count]) {
                i = after + close_pos + bt_count;
            } else {
                i = bytes.len();
            }
        } else if bytes[i] == b'\\' && i + 1 < bytes.len() && bytes[i + 1] == b'|' {
            i += 2;
        } else if bytes[i] == b'|' {
            return true;
        } else {
            i += 1;
        }
    }
    false
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

/// Parse a markdown document into a [`Tree`].
///
/// If frontmatter is present, pass its byte range as `frontmatter_span`
/// so a `Frontmatter` node is created as the first child of `Document`.
/// Body parsing starts after the frontmatter span.
pub fn parse_tree(source: &str, frontmatter_span: Option<Span>) -> Tree {
    let mut builder = TreeBuilder::new(source);

    // Create Document root.
    let doc_id = builder.add_node(
        ElementKind::Document,
        Syntax::Markdown,
        Span::new(0, source.len()),
        None,
    );
    builder.scope_stack.push(doc_id);

    // If frontmatter is present, add it as first child.
    let body_offset = frontmatter_span.map_or(0, |fm_span| {
        builder.add_node(
            ElementKind::Frontmatter,
            Syntax::Yaml,
            fm_span,
            Some(doc_id),
        );
        fm_span.end
    });

    // Parse the body.
    let body = &source[body_offset..];
    builder.parse_body(body, body_offset);

    // Close any remaining open lists (finalizes tight/loose).
    builder.close_all_lists(source.len());

    // Close any remaining open scopes (finalizes spans).
    while builder.scope_stack.len() > 1 {
        builder.pop_scope(source.len());
    }
    builder.quote_depth = 0;

    // Finalize the document span.
    builder.nodes[doc_id].span = Span::new(0, source.len());

    let mut tree = Tree {
        source: source.to_string(),
        nodes: builder.nodes,
        diagnostics: builder.diagnostics,
    };

    // Second pass: parse inline elements in Paragraph and Heading nodes.
    crate::inline::parse_inlines(&mut tree);

    tree
}

/// Internal tree builder with scope stack.
struct TreeBuilder<'a> {
    /// The full source text.
    source: &'a str,
    /// All nodes built so far.
    nodes: Vec<Node>,
    /// Stack of open container node IDs.
    scope_stack: Vec<NodeId>,
    /// Accumulated diagnostics.
    diagnostics: Vec<Diagnostic>,
    /// Current block quote nesting depth (open `QuoteBlock` scopes).
    quote_depth: usize,
    /// Stack of open list contexts.
    list_stack: Vec<ListContext>,
}

impl<'a> TreeBuilder<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            source,
            nodes: Vec::new(),
            scope_stack: Vec::new(),
            diagnostics: Vec::new(),
            quote_depth: 0,
            list_stack: Vec::new(),
        }
    }

    /// Add a node to the tree. If `parent` is `Some`, the node is added as
    /// a child of that parent.
    fn add_node(
        &mut self,
        kind: ElementKind,
        syntax: Syntax,
        span: Span,
        parent: Option<NodeId>,
    ) -> NodeId {
        let id = self.nodes.len();
        self.nodes.push(Node {
            kind,
            syntax,
            span,
            parent,
            children: Vec::new(),
        });
        if let Some(pid) = parent {
            self.nodes[pid].children.push(id);
        }
        id
    }

    /// Add a leaf node as a child of the current scope.
    fn add_leaf(&mut self, kind: ElementKind, syntax: Syntax, span: Span) -> NodeId {
        let parent = self.current_scope();
        self.add_node(kind, syntax, span, Some(parent))
    }

    /// Push a new container scope.
    fn push_scope(&mut self, kind: ElementKind, syntax: Syntax, span: Span) -> NodeId {
        let parent = self.current_scope();
        let id = self.add_node(kind, syntax, span, Some(parent));
        self.scope_stack.push(id);
        id
    }

    /// Pop the current scope, finalizing its span.
    fn pop_scope(&mut self, end: usize) {
        if self.scope_stack.len() > 1
            && let Some(id) = self.scope_stack.pop()
        {
            self.nodes[id].span.end = end;
        }
    }

    /// The node ID of the current (innermost) scope.
    fn current_scope(&self) -> NodeId {
        *self.scope_stack.last().unwrap_or(&0)
    }

    /// Check if the last child of the current scope is a paragraph.
    fn last_child_is_paragraph(&self) -> bool {
        let scope = self.current_scope();
        self.nodes[scope]
            .children
            .last()
            .is_some_and(|&id| matches!(self.nodes[id].kind, ElementKind::Paragraph))
    }

    // -- List scope management ------------------------------------------------

    /// Open a new list and its first item.
    ///
    /// `task` is the pre-computed checkbox state for the first item
    /// (caller resolves this from the raw content to avoid tab
    /// expansion offset mismatches).
    fn open_list(&mut self, marker: &ListMarkerInfo, span_start: usize, task: Option<bool>) {
        let list_node = self.push_scope(
            ElementKind::List {
                ordered: marker.ordered,
                start: marker.start,
                tight: true, // default, updated on close
            },
            Syntax::Markdown,
            Span::new(span_start, span_start),
        );
        let item_node = self.push_scope(
            ElementKind::ListItem { task },
            Syntax::Markdown,
            Span::new(span_start, span_start),
        );
        self.list_stack.push(ListContext {
            list_node,
            item_node,
            marker_char: marker.marker_char,
            ordered: marker.ordered,
            marker_indent: marker.marker_indent,
            content_column: marker.content_column,
            saw_blank: false,
            loose: false,
        });
    }

    /// Close the current list item (pop `ListItem` scope).
    fn close_list_item(&mut self, pos: usize) {
        self.pop_scope(pos);
    }

    /// Close the current list: finalize tight/loose, pop scopes.
    fn close_list(&mut self, pos: usize) {
        if let Some(ctx) = self.list_stack.pop() {
            // Update the List node's tight flag.
            if let ElementKind::List { ref mut tight, .. } = self.nodes[ctx.list_node].kind {
                *tight = !ctx.loose;
            }
            self.pop_scope(pos); // pop the List scope
        }
    }

    /// Close all open list levels.
    fn close_all_lists(&mut self, pos: usize) {
        while !self.list_stack.is_empty() {
            self.close_list_item(pos);
            self.close_list(pos);
        }
    }

    /// Record a blank line inside the current list.
    fn mark_list_blank(&mut self) {
        if let Some(ctx) = self.list_stack.last_mut() {
            ctx.saw_blank = true;
        }
    }

    /// Handle list continuation, new items, or list closure.
    ///
    /// Called on each non-blank line when inside a list. Returns the
    /// adjusted `(content, content_start)` after stripping list
    /// indentation or handling item transitions.
    fn handle_list_continuation<'b>(
        &mut self,
        line: &'b str,
        line_start: usize,
    ) -> (&'b str, usize) {
        if self.list_stack.is_empty() {
            return (line, line_start);
        }

        let (expanded, tab_mappings) = expand_leading_tabs(line);
        let indent = count_indent(&expanded);

        while let Some(ctx) = self.list_stack.last() {
            // Case 1: line continues the current item (sufficient indent).
            if indent >= ctx.content_column {
                let col = ctx.content_column;
                // A continuation line means any prior blank was within
                // the item, not between items — reset the flag.
                if let Some(ctx) = self.list_stack.last_mut() {
                    ctx.saw_blank = false;
                }
                // Strip content_column worth of leading spaces.
                let stripped = strip_n_spaces(line, col);
                let byte_offset = line.len() - stripped.len();
                return (stripped, line_start + byte_offset);
            }

            // Case 2: new item in the same list (matching marker at same indent).
            if let Some(marker) = recognize_list_marker(&expanded)
                && marker.marker_indent == ctx.marker_indent
                && marker.ordered == ctx.ordered
                && marker.marker_char == ctx.marker_char
            {
                // Blank between items → list is loose.
                let make_loose = ctx.saw_blank;
                self.close_list_item(line_start);
                if let Some(ctx) = self.list_stack.last_mut() {
                    if make_loose {
                        ctx.loose = true;
                    }
                    ctx.saw_blank = false;
                }

                // Open new item.
                let raw_offset = expanded_to_raw(marker.content_offset, line, &tab_mappings);
                let content_after = &line[raw_offset..];
                let task = if marker.ordered {
                    None
                } else {
                    recognize_task(content_after)
                };
                let item_node = self.push_scope(
                    ElementKind::ListItem { task },
                    Syntax::Markdown,
                    Span::new(line_start, line_start),
                );
                if let Some(ctx) = self.list_stack.last_mut() {
                    ctx.item_node = item_node;
                    ctx.content_column = marker.content_column;
                }

                return (&line[raw_offset..], line_start + raw_offset);
            }

            // Case 3: line breaks this list level.
            self.close_list_item(line_start);
            self.close_list(line_start);
        }

        (line, line_start)
    }

    /// Parse the body of a document (everything after frontmatter).
    ///
    /// Each line is processed through the scope stack: block quote markers
    /// are stripped and scopes opened/closed before classification. This
    /// means the main loop handles all block types in one place — there is
    /// no separate block quote parser.
    #[allow(
        clippy::too_many_lines,
        reason = "single-loop classifier over all block types"
    )]
    fn parse_body(&mut self, body: &str, body_offset: usize) {
        let lines: Vec<&str> = split_lines(body);
        let mut pos = 0;
        let mut line_idx = 0;

        while line_idx < lines.len() {
            let raw_line = lines[line_idx];
            let raw_start = body_offset + pos;
            let raw_len = raw_line.len();

            // Blank lines close block quotes but not lists.
            if raw_line.trim().is_empty() {
                self.close_block_quotes(raw_start);
                self.mark_list_blank();
                pos += raw_len;
                line_idx += 1;
                continue;
            }

            // Handle block quote continuation and new block quote opening.
            let (content, content_start) = self.handle_quote_markers(raw_line, raw_start);

            // Blank content after marker stripping (e.g. `> \n`).
            if content.trim().is_empty() {
                pos += raw_len;
                line_idx += 1;
                continue;
            }

            // Handle list continuation, new items, or list closure.
            let (content, content_start) = self.handle_list_continuation(content, content_start);

            // Classify the content.
            let (expanded, tab_mappings) = expand_leading_tabs(content);
            let indent = count_indent(&expanded);

            if let Some((fence_char, fence_len, info)) = fenced_code_open(&expanded) {
                pos += raw_len;
                line_idx += 1;
                self.parse_fenced_code(
                    &lines,
                    &mut pos,
                    &mut line_idx,
                    body_offset,
                    content_start,
                    raw_start + raw_len,
                    fence_char,
                    fence_len,
                    info.as_ref(),
                );
            } else if block_math_open(&expanded) {
                pos += raw_len;
                line_idx += 1;
                self.parse_block_math(
                    &lines,
                    &mut pos,
                    &mut line_idx,
                    body_offset,
                    content_start,
                    raw_start + raw_len,
                );
            } else if let Some(level) = atx_heading_level(&expanded) {
                self.add_leaf(
                    ElementKind::Heading { level },
                    Syntax::Markdown,
                    Span::new(content_start, raw_start + raw_len),
                );
                pos += raw_len;
                line_idx += 1;
            } else if let Some((label, url, title)) = parse_reference_def(content) {
                self.parse_reference_def_block(
                    &lines,
                    &mut pos,
                    &mut line_idx,
                    body_offset,
                    content_start,
                    raw_len,
                    label,
                    url,
                    title,
                );
            } else if let Some(label) = parse_footnote_def_start(content) {
                self.parse_footnote_def(
                    &lines,
                    &mut pos,
                    &mut line_idx,
                    body_offset,
                    content_start,
                    raw_len,
                    &label,
                    content,
                );
            } else if let Some(html_type) = html_block_start(&expanded) {
                self.parse_html_block(
                    &lines,
                    &mut pos,
                    &mut line_idx,
                    body_offset,
                    content_start,
                    raw_len,
                    content,
                    html_type,
                );
            } else if is_thematic_break(expanded.trim_end()) {
                self.add_leaf(
                    ElementKind::Rules,
                    Syntax::Markdown,
                    Span::new(content_start, raw_start + raw_len),
                );
                pos += raw_len;
                line_idx += 1;
            } else if let Some(marker) = recognize_list_marker(&expanded) {
                let raw_offset = expanded_to_raw(marker.content_offset, content, &tab_mappings);
                let after = &content[raw_offset..];
                let task = if marker.ordered {
                    None
                } else {
                    recognize_task(after)
                };
                self.open_list(&marker, content_start, task);
                let item_start = content_start + raw_offset;
                if after.trim().is_empty() {
                    pos += raw_len;
                    line_idx += 1;
                } else {
                    // Classify content after the marker through the
                    // block-level chain (it could be a fenced code
                    // opener, heading, etc. — not always a paragraph).
                    let (after_expanded, _) = expand_leading_tabs(after);
                    if let Some((fc, fl, fi)) = fenced_code_open(&after_expanded) {
                        pos += raw_len;
                        line_idx += 1;
                        self.parse_fenced_code(
                            &lines,
                            &mut pos,
                            &mut line_idx,
                            body_offset,
                            item_start,
                            raw_start + raw_len,
                            fc,
                            fl,
                            fi.as_ref(),
                        );
                    } else if block_math_open(&after_expanded) {
                        pos += raw_len;
                        line_idx += 1;
                        self.parse_block_math(
                            &lines,
                            &mut pos,
                            &mut line_idx,
                            body_offset,
                            item_start,
                            raw_start + raw_len,
                        );
                    } else if let Some(level) = atx_heading_level(&after_expanded) {
                        self.add_leaf(
                            ElementKind::Heading { level },
                            Syntax::Markdown,
                            Span::new(item_start, raw_start + raw_len),
                        );
                        pos += raw_len;
                        line_idx += 1;
                    } else {
                        self.parse_paragraph(
                            &lines,
                            &mut pos,
                            &mut line_idx,
                            body_offset,
                            item_start,
                            raw_len,
                        );
                    }
                }
            } else if indent >= 4 && !self.last_child_is_paragraph() {
                self.parse_indented_code(
                    &lines,
                    &mut pos,
                    &mut line_idx,
                    body_offset,
                    content_start,
                    raw_len,
                );
            } else {
                self.parse_paragraph(
                    &lines,
                    &mut pos,
                    &mut line_idx,
                    body_offset,
                    content_start,
                    raw_len,
                );
            }
        }
    }

    /// Close all open block quote scopes.
    fn close_block_quotes(&mut self, pos: usize) {
        while self.quote_depth > 0 {
            self.pop_scope(pos);
            self.quote_depth -= 1;
        }
    }

    /// Handle block quote continuation and new block quote opening.
    ///
    /// 1. Strips continuation markers for existing open block quotes.
    /// 2. Closes scopes for any unmatched levels.
    /// 3. Opens new `QuoteBlock` scopes for additional `>` markers.
    ///
    /// Returns `(content, content_start)` after all markers are stripped.
    fn handle_quote_markers<'b>(&mut self, line: &'b str, line_start: usize) -> (&'b str, usize) {
        // Step 1: Strip continuation markers for existing depth.
        let (matched, after_cont) = strip_n_quote_markers(line, self.quote_depth);
        for _ in matched..self.quote_depth {
            self.pop_scope(line_start);
        }
        self.quote_depth = matched;

        let marker_bytes = line.len() - after_cont.len();
        let mut content = after_cont;
        let mut content_start = line_start + marker_bytes;

        // Step 2: Open new block quote scopes for additional `>` markers.
        while let Some((ml, inner)) = strip_blockquote_marker(content) {
            self.push_scope(
                ElementKind::QuoteBlock,
                Syntax::Markdown,
                Span::new(content_start, content_start),
            );
            self.quote_depth += 1;
            content_start += ml;
            content = inner;
        }

        (content, content_start)
    }

    /// Strip continuation markers from a line inside a multi-line block.
    ///
    /// Strips block quote markers first, then list item indentation.
    /// Returns `Some((content, content_start))` if the current quote depth
    /// is fully matched and any list indentation is satisfied. Returns
    /// `None` if the line cannot continue the current context.
    fn strip_continuation<'b>(&self, line: &'b str, line_start: usize) -> Option<(&'b str, usize)> {
        // Strip block quote markers.
        let (content, content_start) = if self.quote_depth == 0 {
            (line, line_start)
        } else {
            let (matched, remaining) = strip_n_quote_markers(line, self.quote_depth);
            if matched == self.quote_depth {
                let marker_bytes = line.len() - remaining.len();
                (remaining, line_start + marker_bytes)
            } else {
                return None;
            }
        };

        // Strip list item indentation. If the line doesn't have enough
        // indent to continue the list item, return None so callers
        // break out of their continuation loops.
        if let Some(ctx) = self.list_stack.last() {
            let (expanded, _) = expand_leading_tabs(content);
            let indent = count_indent(&expanded);
            if indent < ctx.content_column && !content.trim().is_empty() {
                return None;
            }
            let stripped = strip_n_spaces(content, ctx.content_column);
            let byte_offset = content.len() - stripped.len();
            Some((stripped, content_start + byte_offset))
        } else {
            Some((content, content_start))
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
        body_offset: usize,
        open_start: usize,
        open_raw_end: usize,
        fence_char: u8,
        fence_len: usize,
        _info: Option<&String>,
    ) {
        loop {
            if *line_idx >= lines.len() {
                self.add_leaf(
                    ElementKind::CodeBlock,
                    Syntax::Markdown,
                    Span::new(open_start, body_offset + *pos),
                );
                self.diagnostics.push(Diagnostic {
                    span: Span::new(open_start, open_raw_end),
                    message: "unclosed fenced code block".to_string(),
                });
                break;
            }

            let inner_line = lines[*line_idx];
            let inner_start = body_offset + *pos;
            let inner_len = inner_line.len();

            // Strip continuation markers (quotes + list indent).
            let content = if let Some((c, _)) = self.strip_continuation(inner_line, inner_start) {
                c
            } else {
                // Context ended (quote or list). Check if the raw
                // line is a closing fence before giving up — a
                // fence at lower indentation closes the code block
                // and the enclosing container simultaneously.
                let (raw_expanded, _) = expand_leading_tabs(inner_line);
                if fenced_code_close(&raw_expanded, fence_char, fence_len) {
                    inner_line
                } else {
                    self.add_leaf(
                        ElementKind::CodeBlock,
                        Syntax::Markdown,
                        Span::new(open_start, body_offset + *pos),
                    );
                    self.diagnostics.push(Diagnostic {
                        span: Span::new(open_start, open_raw_end),
                        message: "unclosed fenced code block".to_string(),
                    });
                    break;
                }
            };

            let (inner_expanded, _) = expand_leading_tabs(content);

            if fenced_code_close(&inner_expanded, fence_char, fence_len) {
                *pos += inner_len;
                *line_idx += 1;

                self.add_leaf(
                    ElementKind::CodeBlock,
                    Syntax::Markdown,
                    Span::new(open_start, body_offset + *pos),
                );
                break;
            }

            *pos += inner_len;
            *line_idx += 1;
        }
    }

    /// Parse a block math span (opening `$$` already consumed).
    fn parse_block_math(
        &mut self,
        lines: &[&str],
        pos: &mut usize,
        line_idx: &mut usize,
        body_offset: usize,
        open_start: usize,
        open_raw_end: usize,
    ) {
        let mut found_close = false;

        while *line_idx < lines.len() {
            let inner_line = lines[*line_idx];
            let inner_start = body_offset + *pos;
            let inner_len = inner_line.len();

            let content = if let Some((c, _)) = self.strip_continuation(inner_line, inner_start) {
                c
            } else if block_math_close(inner_line) {
                // Context ended but raw line has closing delimiter.
                inner_line
            } else {
                break;
            };

            if block_math_close(content) {
                *pos += inner_len;
                *line_idx += 1;
                found_close = true;

                self.add_leaf(
                    ElementKind::Math,
                    Syntax::Markdown,
                    Span::new(open_start, body_offset + *pos),
                );
                break;
            }

            *pos += inner_len;
            *line_idx += 1;
        }

        if !found_close {
            self.add_leaf(
                ElementKind::Math,
                Syntax::Markdown,
                Span::new(open_start, body_offset + *pos),
            );
            self.diagnostics.push(Diagnostic {
                span: Span::new(open_start, open_raw_end),
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
        body_offset: usize,
        block_start: usize,
        first_line_raw_len: usize,
        first_content: &str,
        html_type: u8,
    ) {
        if matches!(html_type, 6 | 7) {
            *pos += first_line_raw_len;
            *line_idx += 1;

            while *line_idx < lines.len() {
                let inner_line = lines[*line_idx];
                let inner_start = body_offset + *pos;

                let Some((content, _)) = self.strip_continuation(inner_line, inner_start) else {
                    break;
                };

                if content.trim().is_empty() {
                    break;
                }
                *pos += inner_line.len();
                *line_idx += 1;
            }
        } else {
            let end_on_first = html_block_end(first_content, html_type);
            *pos += first_line_raw_len;
            *line_idx += 1;

            if !end_on_first {
                while *line_idx < lines.len() {
                    let inner_line = lines[*line_idx];
                    let inner_start = body_offset + *pos;

                    let Some((content, _)) = self.strip_continuation(inner_line, inner_start)
                    else {
                        break;
                    };

                    *pos += inner_line.len();
                    *line_idx += 1;

                    if html_block_end(content, html_type) {
                        break;
                    }
                }
            }
        }

        self.add_leaf(
            ElementKind::HtmlBlock,
            Syntax::Markdown,
            Span::new(block_start, body_offset + *pos),
        );
    }

    /// Parse a link reference definition, consuming a continuation title
    /// line if the first line has a URL but no title.
    #[allow(
        clippy::too_many_arguments,
        reason = "ref def parameters are distinct concerns"
    )]
    fn parse_reference_def_block(
        &mut self,
        lines: &[&str],
        pos: &mut usize,
        line_idx: &mut usize,
        body_offset: usize,
        def_start: usize,
        first_raw_len: usize,
        label: String,
        url: String,
        mut title: String,
    ) {
        *pos += first_raw_len;
        *line_idx += 1;
        let mut span_end = body_offset + *pos;

        // If no title on first line, check next line for continuation title
        if title.is_empty() && *line_idx < lines.len() {
            let next_line = lines[*line_idx];
            let next_start = body_offset + *pos;
            let next_len = next_line.len();

            if let Some((next_content, _)) = self.strip_continuation(next_line, next_start)
                && let Some(t) = parse_standalone_title(next_content)
            {
                title = t;
                *pos += next_len;
                *line_idx += 1;
                span_end = body_offset + *pos;
            }
        }

        self.add_leaf(
            ElementKind::ReferenceDef { label, url, title },
            Syntax::Markdown,
            Span::new(def_start, span_end),
        );
    }

    /// Parse a footnote definition container.
    ///
    /// Consumes the first line and any indented (4+ spaces) continuation
    /// lines. Inner content is added as `Paragraph` children.
    #[allow(
        clippy::too_many_arguments,
        reason = "footnote parameters are distinct concerns"
    )]
    fn parse_footnote_def(
        &mut self,
        lines: &[&str],
        pos: &mut usize,
        line_idx: &mut usize,
        body_offset: usize,
        def_start: usize,
        first_raw_len: usize,
        label: &str,
        first_line: &str,
    ) {
        self.push_scope(
            ElementKind::FootnoteDef {
                label: label.to_string(),
            },
            Syntax::Markdown,
            Span::new(def_start, def_start),
        );

        // Find content start: after `[^label]: `
        let marker = format!("[^{label}]:");
        let content_offset = first_line.find(&marker).map_or(first_line.len(), |p| {
            let after = p + marker.len();
            if first_line.get(after..after + 1) == Some(" ") {
                after + 1
            } else {
                after
            }
        });

        let first_text = &first_line[content_offset..];
        if !first_text.trim().is_empty() {
            self.add_leaf(
                ElementKind::Paragraph,
                Syntax::Markdown,
                Span::new(
                    def_start + content_offset,
                    body_offset + *pos + first_raw_len,
                ),
            );
        }

        *pos += first_raw_len;
        *line_idx += 1;

        while *line_idx < lines.len() {
            let inner_line = lines[*line_idx];
            let inner_start = body_offset + *pos;
            let inner_len = inner_line.len();

            let Some((inner_content, inner_content_start)) =
                self.strip_continuation(inner_line, inner_start)
            else {
                break;
            };

            if inner_content.trim().is_empty() {
                *pos += inner_len;
                *line_idx += 1;
                continue;
            }

            let (inner_expanded, _) = expand_leading_tabs(inner_content);
            let inner_indent = count_indent(&inner_expanded);

            if inner_indent < 4 {
                break;
            }

            self.add_leaf(
                ElementKind::Paragraph,
                Syntax::Markdown,
                Span::new(inner_content_start, inner_start + inner_len),
            );

            *pos += inner_len;
            *line_idx += 1;
        }

        self.pop_scope(body_offset + *pos);
    }

    /// Parse an indented code block.
    fn parse_indented_code(
        &mut self,
        lines: &[&str],
        pos: &mut usize,
        line_idx: &mut usize,
        body_offset: usize,
        block_start: usize,
        first_line_raw_len: usize,
    ) {
        *pos += first_line_raw_len;
        *line_idx += 1;

        while *line_idx < lines.len() {
            let inner_line = lines[*line_idx];
            let inner_start = body_offset + *pos;

            let Some((content, _)) = self.strip_continuation(inner_line, inner_start) else {
                break;
            };

            let (inner_expanded, _) = expand_leading_tabs(content);
            let inner_indent = count_indent(&inner_expanded);

            if inner_expanded.trim().is_empty() || inner_indent >= 4 {
                *pos += inner_line.len();
                *line_idx += 1;
            } else {
                break;
            }
        }

        self.add_leaf(
            ElementKind::CodeBlock,
            Syntax::Markdown,
            Span::new(block_start, body_offset + *pos),
        );
    }

    /// Parse a paragraph, detecting setext headings and GFM tables.
    ///
    /// Handles block quote continuation markers on each continuation line,
    /// with lazy continuation fallback (lines without `>` markers can
    /// continue a paragraph inside a block quote).
    fn parse_paragraph(
        &mut self,
        lines: &[&str],
        pos: &mut usize,
        line_idx: &mut usize,
        body_offset: usize,
        para_start: usize,
        first_line_raw_len: usize,
    ) {
        *pos += first_line_raw_len;
        *line_idx += 1;

        // Check for GFM table: header row with pipes followed by delimiter row.
        let header_end = self.source[para_start..]
            .find('\n')
            .map_or(self.source.len(), |p| para_start + p);
        let header_line = &self.source[para_start..header_end];

        if is_table_row(header_line) && *line_idx < lines.len() {
            let next_line = lines[*line_idx];
            let next_start = body_offset + *pos;
            if let Some((content, _)) = self.strip_continuation(next_line, next_start)
                && let Some(alignments) = parse_delimiter_row(content)
            {
                self.parse_table(lines, pos, line_idx, body_offset, para_start, alignments);
                return;
            }
        }

        // Consume paragraph continuation lines.
        loop {
            if *line_idx >= lines.len() {
                break;
            }

            let next_line = lines[*line_idx];
            let next_start = body_offset + *pos;
            let next_len = next_line.len();

            // Strip continuation markers, with lazy fallback.
            let content = if let Some((c, _)) = self.strip_continuation(next_line, next_start) {
                c
            } else {
                // Lazy continuation: line without proper markers/indent
                // that is not a block-starting construct can continue a
                // paragraph inside a block quote or list item.
                let (lazy_expanded, _) = expand_leading_tabs(next_line);
                if (self.quote_depth > 0 || !self.list_stack.is_empty())
                    && strip_blockquote_marker(next_line).is_none()
                    && !is_thematic_break(next_line)
                    && atx_heading_level(next_line).is_none()
                    && fenced_code_open(next_line).is_none()
                    && html_block_start(next_line).is_none()
                    && recognize_list_marker(&lazy_expanded).is_none()
                {
                    next_line
                } else {
                    break;
                }
            };

            let (next_expanded, _) = expand_leading_tabs(content);

            // Blank line ends paragraph
            if next_expanded.trim().is_empty() {
                break;
            }

            // Setext heading underline
            if let Some(level) = setext_level(&next_expanded) {
                *pos += next_len;
                *line_idx += 1;

                self.add_leaf(
                    ElementKind::Heading { level },
                    Syntax::Markdown,
                    Span::new(para_start, body_offset + *pos),
                );
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

            // List marker ends paragraph (ordered with start != 1 cannot
            // interrupt a paragraph per CommonMark)
            if let Some(marker) = recognize_list_marker(&next_expanded)
                && (!marker.ordered || marker.start == 1)
            {
                break;
            }

            // Otherwise, continue the paragraph
            *pos += next_len;
            *line_idx += 1;
        }

        self.add_leaf(
            ElementKind::Paragraph,
            Syntax::Markdown,
            Span::new(para_start, body_offset + *pos),
        );
    }

    /// Parse a GFM pipe table.
    ///
    /// Called after the header row has been consumed and a delimiter row
    /// has been detected at the current `line_idx`. Creates `Table`,
    /// `TableRow`, and `TableCell` nodes.
    #[allow(
        clippy::too_many_arguments,
        reason = "table parameters are distinct concerns"
    )]
    fn parse_table(
        &mut self,
        lines: &[&str],
        pos: &mut usize,
        line_idx: &mut usize,
        body_offset: usize,
        header_start: usize,
        alignments: Vec<TableAlignment>,
    ) {
        let col_count = alignments.len();

        // Open Table container.
        self.push_scope(
            ElementKind::Table { alignments },
            Syntax::Markdown,
            Span::new(header_start, header_start),
        );

        // Parse header row cells.
        let header_end = self.source[header_start..]
            .find('\n')
            .map_or(self.source.len(), |p| header_start + p);
        let header_line = &self.source[header_start..header_end];
        self.emit_table_row(header_line, header_start, header_end, col_count, true);

        // Consume the delimiter row (advance past it, no node emitted).
        let delim_len = lines[*line_idx].len();
        *pos += delim_len;
        *line_idx += 1;

        // Consume body rows.
        while *line_idx < lines.len() {
            let raw_line = lines[*line_idx];
            let raw_start = body_offset + *pos;
            let raw_len = raw_line.len();

            // Strip continuation markers.
            let Some((content, content_start)) = self.strip_continuation(raw_line, raw_start)
            else {
                break;
            };

            // Blank line or non-table-row line ends the table.
            if content.trim().is_empty() || !is_table_row(content) {
                break;
            }

            // Trim trailing newline from content for cell parsing.
            let content_trimmed = content.trim_end_matches('\n').trim_end_matches('\r');
            let content_end = content_start + content_trimmed.len();
            self.emit_table_row(
                content_trimmed,
                content_start,
                content_end,
                col_count,
                false,
            );

            *pos += raw_len;
            *line_idx += 1;
        }

        // Close the Table scope.
        self.pop_scope(body_offset + *pos);
    }

    /// Emit a single table row with cells, padding or truncating to `col_count`.
    fn emit_table_row(
        &mut self,
        line: &str,
        row_start: usize,
        row_end: usize,
        col_count: usize,
        header: bool,
    ) {
        self.push_scope(
            ElementKind::TableRow { header },
            Syntax::Markdown,
            Span::new(row_start, row_end),
        );

        let cell_spans = split_table_cells(line, row_start);
        let actual_count = cell_spans.len();

        // Emit cells up to col_count.
        for (i, span) in cell_spans.into_iter().enumerate() {
            if i >= col_count {
                break;
            }
            self.add_leaf(ElementKind::TableCell, Syntax::Markdown, span);
        }

        // Pad with empty cells if fewer than col_count.
        for _ in actual_count..col_count {
            self.add_leaf(
                ElementKind::TableCell,
                Syntax::Markdown,
                Span::new(row_end, row_end),
            );
        }

        // Record mismatch diagnostic.
        if actual_count != col_count {
            self.diagnostics.push(Diagnostic {
                span: Span::new(row_start, row_end),
                message: format!("table row has {actual_count} cells, expected {col_count}"),
            });
        }

        // Close the row scope.
        self.pop_scope(row_end);
    }
}

/// Strip exactly `n` levels of `>` markers from a line.
fn strip_n_quote_markers(line: &str, n: usize) -> (usize, &str) {
    let mut remaining = line;
    let mut stripped = 0;

    for _ in 0..n {
        match strip_blockquote_marker(remaining) {
            Some((_, content)) => {
                stripped += 1;
                remaining = content;
            }
            None => break,
        }
    }

    (stripped, remaining)
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

    /// Helper: parse a tree with no frontmatter.
    fn parse(source: &str) -> Tree {
        parse_tree(source, None)
    }

    /// Helper: get the text of a span from source.
    fn span_text<'a>(source: &'a str, span: &Span) -> &'a str {
        &source[span.start..span.end]
    }

    /// Helper: collect children of the root.
    fn root_children(tree: &Tree) -> Vec<NodeId> {
        tree.children(tree.root()).to_vec()
    }

    /// Helper: assert a node is a specific kind and return it.
    fn assert_kind<'a>(tree: &'a Tree, id: NodeId, expected: &ElementKind) -> &'a Node {
        let node = tree.node(id);
        assert_eq!(
            &node.kind, expected,
            "node {id} should be {expected:?}, got {:?}",
            node.kind
        );
        node
    }

    // --- Document root ---

    #[test]
    fn document_is_always_root() {
        let tree = parse("");
        assert_eq!(tree.root(), 0, "root is always node 0");
        assert_eq!(tree.node(0).kind, ElementKind::Document, "root is Document");
        assert!(tree.node(0).parent.is_none(), "root has no parent");
    }

    #[test]
    fn empty_document_has_no_children() {
        let tree = parse("");
        assert!(
            root_children(&tree).is_empty(),
            "empty document has no children"
        );
    }

    // --- ATX headings ---

    #[test]
    fn atx_heading_levels() {
        let source = "# H1\n## H2\n### H3\n#### H4\n##### H5\n###### H6\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 6, "should find six headings");
        for (i, &id) in children.iter().enumerate() {
            let expected_level = (i + 1) as u8;
            assert_kind(
                &tree,
                id,
                &ElementKind::Heading {
                    level: expected_level,
                },
            );
        }
    }

    #[test]
    fn atx_heading_text_span() {
        let source = "## Hello World\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one heading");
        let node = assert_kind(&tree, children[0], &ElementKind::Heading { level: 2 });
        let line = &source[node.span.start..node.span.end]
            .lines()
            .next()
            .expect("heading should have a line");
        let (text_span, _) = extract_atx_content(line, node.span.start);
        assert_eq!(
            span_text(source, &text_span),
            "Hello World",
            "text span content"
        );
    }

    #[test]
    fn atx_heading_with_explicit_id() {
        let source = "## My Heading {#custom-id}\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one heading");
        let node = assert_kind(&tree, children[0], &ElementKind::Heading { level: 2 });
        let line = &source[node.span.start..node.span.end]
            .lines()
            .next()
            .expect("should have a line");
        let (text_span, id) = extract_atx_content(line, node.span.start);
        assert_eq!(
            span_text(source, &text_span),
            "My Heading",
            "text span without id attribute"
        );
        let attr = id.expect("should have id attribute");
        assert_eq!(attr.id, "custom-id", "id text");
    }

    #[test]
    fn atx_heading_trailing_hashes() {
        let source = "## Heading ##\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one heading");
        let node = assert_kind(&tree, children[0], &ElementKind::Heading { level: 2 });
        let line = &source[node.span.start..node.span.end]
            .lines()
            .next()
            .expect("should have a line");
        let (text_span, _) = extract_atx_content(line, node.span.start);
        assert_eq!(
            span_text(source, &text_span),
            "Heading",
            "trailing hashes stripped"
        );
    }

    #[test]
    fn atx_heading_empty() {
        let source = "#\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one heading");
        let node = assert_kind(&tree, children[0], &ElementKind::Heading { level: 1 });
        let line = &source[node.span.start..node.span.end]
            .lines()
            .next()
            .expect("should have a line");
        let (text_span, _) = extract_atx_content(line, node.span.start);
        assert!(text_span.is_empty(), "empty heading has empty text span");
    }

    #[test]
    fn atx_heading_with_leading_spaces() {
        let source = "   ## Indented\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one heading");
        let node = assert_kind(&tree, children[0], &ElementKind::Heading { level: 2 });
        let line = &source[node.span.start..node.span.end]
            .lines()
            .next()
            .expect("should have a line");
        let (text_span, _) = extract_atx_content(line, node.span.start);
        assert_eq!(span_text(source, &text_span), "Indented", "text content");
    }

    #[test]
    fn four_leading_spaces_not_heading() {
        let source = "    ## Not a heading\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert!(
            !matches!(tree.node(children[0]).kind, ElementKind::Heading { .. }),
            "4+ spaces should not be a heading"
        );
    }

    // --- Setext headings ---

    #[test]
    fn setext_heading_level_1() {
        let source = "Heading\n=======\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert_kind(&tree, children[0], &ElementKind::Heading { level: 1 });
    }

    #[test]
    fn setext_heading_level_2() {
        let source = "Heading\n-------\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert_kind(&tree, children[0], &ElementKind::Heading { level: 2 });
    }

    #[test]
    fn setext_heading_multiline() {
        let source = "Line one\nLine two\n=========\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert_kind(&tree, children[0], &ElementKind::Heading { level: 1 });
        let node = tree.node(children[0]);
        assert_eq!(
            node.span,
            Span::new(0, source.len()),
            "setext heading span covers all lines"
        );
    }

    // --- Setext vs thematic break ---

    #[test]
    fn dashes_after_paragraph_is_setext() {
        let source = "Paragraph\n---\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert_kind(&tree, children[0], &ElementKind::Heading { level: 2 });
    }

    #[test]
    fn dashes_after_blank_is_thematic_break() {
        let source = "\n---\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one non-blank block");
        assert_kind(&tree, children[0], &ElementKind::Rules);
    }

    #[test]
    fn dashes_at_document_start_is_thematic_break() {
        let source = "---\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert_kind(&tree, children[0], &ElementKind::Rules);
    }

    // --- Thematic breaks ---

    #[test]
    fn thematic_break_stars() {
        let source = "***\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert_kind(&tree, children[0], &ElementKind::Rules);
    }

    #[test]
    fn thematic_break_underscores() {
        let source = "___\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert_kind(&tree, children[0], &ElementKind::Rules);
    }

    #[test]
    fn thematic_break_with_spaces() {
        let source = "* * * *\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert_kind(&tree, children[0], &ElementKind::Rules);
    }

    #[test]
    fn thematic_break_with_many_chars() {
        let source = "----------\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert_kind(&tree, children[0], &ElementKind::Rules);
    }

    // --- Fenced code blocks ---

    #[test]
    fn fenced_code_backticks() {
        let source = "```\ncode here\n```\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert_kind(&tree, children[0], &ElementKind::CodeBlock);
    }

    #[test]
    fn fenced_code_tildes() {
        let source = "~~~\ncode here\n~~~\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert_kind(&tree, children[0], &ElementKind::CodeBlock);
    }

    #[test]
    fn fenced_code_with_info_string() {
        let source = "```rust\nfn main() {}\n```\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert_kind(&tree, children[0], &ElementKind::CodeBlock);
    }

    #[test]
    fn fenced_code_unclosed() {
        let source = "```\ncode here\nmore code\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert_kind(&tree, children[0], &ElementKind::CodeBlock);
        assert_eq!(tree.diagnostics().len(), 1, "should emit one diagnostic");
        assert!(
            tree.diagnostics()[0].message.contains("unclosed"),
            "diagnostic mentions unclosed"
        );
    }

    #[test]
    fn fenced_code_longer_close() {
        let source = "```\ncode\n`````\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert_kind(&tree, children[0], &ElementKind::CodeBlock);
    }

    #[test]
    fn fenced_code_shorter_close_not_accepted() {
        let source = "````\ncode\n```\nmore\n````\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert_kind(&tree, children[0], &ElementKind::CodeBlock);
        let node = tree.node(children[0]);
        assert_eq!(
            node.span,
            Span::new(0, source.len()),
            "shorter fence is content, span covers entire block"
        );
    }

    // --- Block math ---

    #[test]
    fn block_math_basic() {
        let source = "$$\nx + y = z\n$$\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert_kind(&tree, children[0], &ElementKind::Math);
    }

    #[test]
    fn block_math_unclosed() {
        let source = "$$\nmath content\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert_kind(&tree, children[0], &ElementKind::Math);
        assert_eq!(tree.diagnostics().len(), 1, "should emit one diagnostic");
        assert!(
            tree.diagnostics()[0].message.contains("unclosed"),
            "diagnostic mentions unclosed"
        );
    }

    // --- Indented code blocks ---

    #[test]
    fn indented_code_block() {
        let source = "    code line 1\n    code line 2\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert_kind(&tree, children[0], &ElementKind::CodeBlock);
    }

    #[test]
    fn indented_code_not_after_paragraph() {
        let source = "Paragraph\n    continuation\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert_kind(&tree, children[0], &ElementKind::Paragraph);
    }

    // --- Block quotes ---

    #[test]
    fn block_quote_simple() {
        let source = "> quoted text\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        let node = assert_kind(&tree, children[0], &ElementKind::QuoteBlock);
        assert!(node.parent == Some(0), "block quote parent is Document");
        let quote_children = tree.children(children[0]);
        assert_eq!(quote_children.len(), 1, "block quote has one child");
        assert_kind(&tree, quote_children[0], &ElementKind::Paragraph);
    }

    #[test]
    fn block_quote_multiline() {
        let source = "> line one\n> line two\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert_kind(&tree, children[0], &ElementKind::QuoteBlock);
    }

    #[test]
    fn block_quote_lazy_continuation() {
        let source = "> first line\nlazy continuation\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert_kind(&tree, children[0], &ElementKind::QuoteBlock);
    }

    #[test]
    fn block_quote_nested() {
        let source = "> > nested\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one outer block quote");
        assert_kind(&tree, children[0], &ElementKind::QuoteBlock);

        let outer_children = tree.children(children[0]);
        assert_eq!(outer_children.len(), 1, "outer has one child");
        assert_kind(&tree, outer_children[0], &ElementKind::QuoteBlock);

        let inner_children = tree.children(outer_children[0]);
        assert_eq!(inner_children.len(), 1, "inner has one child");
        assert_kind(&tree, inner_children[0], &ElementKind::Paragraph);
    }

    #[test]
    fn block_quote_with_heading() {
        let source = "> # Heading\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert_kind(&tree, children[0], &ElementKind::QuoteBlock);

        let quote_children = tree.children(children[0]);
        assert_eq!(quote_children.len(), 1, "block quote has one child");
        assert_kind(&tree, quote_children[0], &ElementKind::Heading { level: 1 });
    }

    #[test]
    fn block_quote_with_code_block() {
        let source = "> ```\n> code\n> ```\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert_kind(&tree, children[0], &ElementKind::QuoteBlock);

        let quote_children = tree.children(children[0]);
        assert_eq!(quote_children.len(), 1, "block quote has one child");
        assert_kind(&tree, quote_children[0], &ElementKind::CodeBlock);
    }

    #[test]
    fn block_quote_with_thematic_break() {
        let source = "> ***\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert_kind(&tree, children[0], &ElementKind::QuoteBlock);

        let quote_children = tree.children(children[0]);
        assert_eq!(quote_children.len(), 1, "block quote has one child");
        assert_kind(&tree, quote_children[0], &ElementKind::Rules);
    }

    // --- HTML blocks ---

    #[test]
    fn html_block_type1_pre() {
        let source = "<pre>\ncode\n</pre>\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert_kind(&tree, children[0], &ElementKind::HtmlBlock);
    }

    #[test]
    fn html_block_type2_comment() {
        let source = "<!-- comment -->\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert_kind(&tree, children[0], &ElementKind::HtmlBlock);
    }

    #[test]
    fn html_block_type6_div() {
        let source = "<div>\ncontent\n</div>\n\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one non-blank block");
        assert_kind(&tree, children[0], &ElementKind::HtmlBlock);
    }

    #[test]
    fn html_block_type7_cannot_interrupt_paragraph() {
        let source = "Paragraph\n<span>inline</span>\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert_kind(&tree, children[0], &ElementKind::Paragraph);
    }

    // --- Paragraphs ---

    #[test]
    fn simple_paragraph() {
        let source = "Hello world.\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert_kind(&tree, children[0], &ElementKind::Paragraph);
    }

    #[test]
    fn multiline_paragraph() {
        let source = "Line one.\nLine two.\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert_kind(&tree, children[0], &ElementKind::Paragraph);
    }

    // --- Mixed constructs ---

    #[test]
    fn mixed_blocks() {
        let source = "# Heading\n\nParagraph text.\n\n---\n\n```\ncode\n```\n";
        let tree = parse(source);
        let children = root_children(&tree);

        // Blank lines are not nodes.
        assert_eq!(children.len(), 4, "should find four non-blank blocks");
        assert_kind(&tree, children[0], &ElementKind::Heading { level: 1 });
        assert_kind(&tree, children[1], &ElementKind::Paragraph);
        assert_kind(&tree, children[2], &ElementKind::Rules);
        assert_kind(&tree, children[3], &ElementKind::CodeBlock);
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
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert_kind(&tree, children[0], &ElementKind::CodeBlock);
    }

    #[test]
    fn expanded_to_raw_no_tabs() {
        let raw = "- item";
        let (_, mappings) = expand_leading_tabs(raw);
        assert_eq!(
            expanded_to_raw(2, raw, &mappings),
            2,
            "no tabs: offset unchanged"
        );
    }

    #[test]
    fn expanded_to_raw_single_tab() {
        // "\t- item" → "    - item" (tab at byte 0, 4 spaces)
        let raw = "\t- item";
        let (expanded, mappings) = expand_leading_tabs(raw);
        assert_eq!(expanded, "    - item", "expansion sanity check");
        // Offset 4 in expanded is `-`, which is byte 1 in raw
        assert_eq!(
            expanded_to_raw(4, raw, &mappings),
            1,
            "offset past tab maps to byte after tab"
        );
        // Offset 6 in expanded is `i`, which is byte 3 in raw
        assert_eq!(
            expanded_to_raw(6, raw, &mappings),
            3,
            "offset well past tab maps correctly"
        );
        // Offset 2 is inside the tab expansion → maps to byte 1 (past tab)
        assert_eq!(
            expanded_to_raw(2, raw, &mappings),
            1,
            "offset inside tab expansion maps past tab byte"
        );
    }

    #[test]
    fn expanded_to_raw_two_tabs() {
        // "\t\t- x" → "        - x" (8 spaces, then "- x")
        let raw = "\t\t- x";
        let (expanded, mappings) = expand_leading_tabs(raw);
        assert_eq!(expanded, "        - x", "expansion sanity check");
        // Offset 8 in expanded is `-`, which is byte 2 in raw
        assert_eq!(
            expanded_to_raw(8, raw, &mappings),
            2,
            "offset past both tabs"
        );
        // Offset 5 is inside second tab → maps to byte 2 (past second tab)
        assert_eq!(
            expanded_to_raw(5, raw, &mappings),
            2,
            "offset inside second tab expansion"
        );
        // Offset 0 is before any tab
        assert_eq!(expanded_to_raw(0, raw, &mappings), 0, "offset 0 stays at 0");
    }

    #[test]
    fn expanded_to_raw_partial_tab() {
        // " \t- item" → "    - item" (space + tab at col 1 → 3 spaces)
        let raw = " \t- item";
        let (expanded, mappings) = expand_leading_tabs(raw);
        assert_eq!(expanded, "    - item", "expansion sanity check");
        // Offset 4 is `-`, byte 2 in raw
        assert_eq!(
            expanded_to_raw(4, raw, &mappings),
            2,
            "offset past partial tab"
        );
        // Offset 1 is at expanded_col of the tab → inside expansion
        assert_eq!(
            expanded_to_raw(1, raw, &mappings),
            1,
            "offset at tab start maps to tab byte"
        );
    }

    #[test]
    fn expanded_to_raw_clamped_to_raw_len() {
        let raw = "ab";
        let (_, mappings) = expand_leading_tabs(raw);
        assert_eq!(
            expanded_to_raw(100, raw, &mappings),
            2,
            "offset beyond raw len is clamped"
        );
    }

    #[test]
    fn tab_not_expanded_inside_content() {
        let (expanded, _) = expand_leading_tabs("text\there");
        assert_eq!(expanded, "text\there", "tab inside content is preserved");
    }

    // --- Frontmatter ---

    #[test]
    fn frontmatter_is_first_child() {
        let source = "---\ntitle: test\n---\n# Heading\n";
        let fm_end = source.find("# Heading").expect("should find heading");
        let tree = parse_tree(source, Some(Span::new(0, fm_end)));
        let children = root_children(&tree);

        assert_eq!(children.len(), 2, "should find frontmatter + heading");
        assert_kind(&tree, children[0], &ElementKind::Frontmatter);
        assert_kind(&tree, children[1], &ElementKind::Heading { level: 1 });

        assert_eq!(
            tree.node(children[0]).syntax,
            Syntax::Yaml,
            "frontmatter has Yaml syntax"
        );
    }

    #[test]
    fn body_offset_shifts_spans() {
        let source = "---\ntitle: test\n---\n# Heading\n";
        let body_offset = source.find("# Heading").expect("should find heading");
        let tree = parse_tree(source, Some(Span::new(0, body_offset)));
        let children = root_children(&tree);

        let heading_id = children
            .iter()
            .find(|&&id| matches!(tree.node(id).kind, ElementKind::Heading { .. }))
            .expect("should find heading");
        let node = tree.node(*heading_id);
        let line = &source[node.span.start..node.span.end]
            .lines()
            .next()
            .expect("should have a line");
        let (text_span, _) = extract_atx_content(line, node.span.start);
        assert_eq!(
            span_text(source, &text_span),
            "Heading",
            "text span in original source with offset"
        );
    }

    // --- Span correctness ---

    #[test]
    fn spans_cover_original_source() {
        let source = "# Heading\n\nParagraph\n";
        let tree = parse(source);

        for node in tree.nodes() {
            let text = span_text(source, &node.span);
            assert!(
                !text.is_empty() || matches!(node.kind, ElementKind::Document),
                "node span should reference source text: {:?}",
                node.kind
            );
        }
    }

    #[test]
    fn no_text_copied() {
        let source = "## Title\n\n> Quote\n\n```\ncode\n```\n\n---\n";
        let tree = parse(source);

        for node in tree.nodes() {
            assert!(
                node.span.start <= node.span.end,
                "span start <= end: {:?}",
                node.kind
            );
            assert!(
                node.span.end <= source.len(),
                "span end <= source length: {:?}",
                node.kind
            );
        }
    }

    // --- Parent/children ---

    #[test]
    fn parent_children_consistency() {
        let source = "# Heading\n\nParagraph\n\n> Quote\n";
        let tree = parse(source);

        for (id, node) in tree.nodes().iter().enumerate() {
            for &child_id in &node.children {
                assert_eq!(
                    tree.node(child_id).parent,
                    Some(id),
                    "child {child_id} should have parent {id}"
                );
            }
            if let Some(pid) = node.parent {
                assert!(
                    tree.node(pid).children.contains(&id),
                    "node {id} should be in parent {pid}'s children"
                );
            }
        }
    }

    #[test]
    fn children_in_document_order() {
        let source = "# First\n\n## Second\n\nParagraph\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 3, "should have three children");
        for window in children.windows(2) {
            let a = tree.node(window[0]);
            let b = tree.node(window[1]);
            assert!(
                a.span.start < b.span.start,
                "children should be in document order: {:?} before {:?}",
                a.kind,
                b.kind
            );
        }
    }

    // --- HTML block types ---

    #[test]
    fn html_block_type3_processing_instruction() {
        let source = "<?xml version=\"1.0\"?>\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert_kind(&tree, children[0], &ElementKind::HtmlBlock);
    }

    #[test]
    fn html_block_type4_declaration() {
        let source = "<!DOCTYPE html>\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert_kind(&tree, children[0], &ElementKind::HtmlBlock);
    }

    #[test]
    fn html_block_type5_cdata() {
        let source = "<![CDATA[\nsome data\n]]>\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert_kind(&tree, children[0], &ElementKind::HtmlBlock);
    }

    #[test]
    fn html_block_multiline_comment() {
        let source = "<!-- start\nmiddle\nend -->\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert_kind(&tree, children[0], &ElementKind::HtmlBlock);
    }

    // --- Blank lines ---

    #[test]
    fn blank_lines_are_not_nodes() {
        let source = "\n\n\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert!(
            children.is_empty(),
            "blank lines should not produce child nodes"
        );
    }

    // --- Nested block quote tests ---

    #[test]
    fn nested_block_quotes_produce_nested_containers() {
        let source = "> > > deeply nested\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "one top-level quote");
        let l1 = children[0];
        assert_kind(&tree, l1, &ElementKind::QuoteBlock);

        let l1_children = tree.children(l1);
        assert_eq!(l1_children.len(), 1, "one child at level 1");
        let l2 = l1_children[0];
        assert_kind(&tree, l2, &ElementKind::QuoteBlock);

        let l2_children = tree.children(l2);
        assert_eq!(l2_children.len(), 1, "one child at level 2");
        let l3 = l2_children[0];
        assert_kind(&tree, l3, &ElementKind::QuoteBlock);

        let l3_children = tree.children(l3);
        assert_eq!(l3_children.len(), 1, "leaf content at level 3");
        assert_kind(&tree, l3_children[0], &ElementKind::Paragraph);
    }

    #[test]
    fn every_node_has_span() {
        let source = "# H\n\n> text\n\n```\ncode\n```\n";
        let tree = parse(source);

        for node in tree.nodes() {
            if matches!(node.kind, ElementKind::Document) {
                assert_eq!(node.span, Span::new(0, source.len()), "document span");
            } else {
                assert!(
                    node.span.start < node.span.end,
                    "non-document node should have non-empty span: {:?}",
                    node.kind
                );
            }
        }
    }

    #[test]
    fn block_quote_child_span_excludes_markers() {
        let source = "> # Heading\n";
        let tree = parse(source);
        let children = root_children(&tree);
        let quote_children = tree.children(children[0]);
        let heading = tree.node(quote_children[0]);

        // Heading span starts after "> ", not at the raw line start.
        assert_eq!(
            heading.span.start, 2,
            "heading span starts after quote marker"
        );
        assert_eq!(
            &source[heading.span.start..heading.span.end],
            "# Heading\n",
            "heading span content excludes marker"
        );
    }

    #[test]
    fn nested_quote_child_spans_exclude_all_markers() {
        let source = "> > text\n";
        let tree = parse(source);

        // Outer QuoteBlock starts at 0 (owns the first `>`).
        let outer = root_children(&tree)[0];
        assert_eq!(
            tree.node(outer).span.start,
            0,
            "outer quote starts at raw line start"
        );

        // Inner QuoteBlock starts at 2 (owns the second `>`).
        let inner = tree.children(outer)[0];
        assert_eq!(
            tree.node(inner).span.start,
            2,
            "inner quote starts after first marker"
        );

        // Paragraph starts at 4 (after both `> >`).
        let para = tree.children(inner)[0];
        assert_eq!(
            tree.node(para).span.start,
            4,
            "paragraph starts after all markers"
        );
        assert_eq!(
            &source[tree.node(para).span.start..tree.node(para).span.end],
            "text\n",
            "paragraph content excludes all markers"
        );
    }

    // --- Lists: basic ---

    #[test]
    fn single_unordered_item() {
        let source = "- item\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one list");
        let list_id = children[0];
        assert!(
            matches!(
                tree.node(list_id).kind,
                ElementKind::List {
                    ordered: false,
                    tight: true,
                    ..
                }
            ),
            "should be an unordered tight list"
        );

        let items = tree.children(list_id);
        assert_eq!(items.len(), 1, "list has one item");
        assert_kind(&tree, items[0], &ElementKind::ListItem { task: None });

        let item_children = tree.children(items[0]);
        assert_eq!(item_children.len(), 1, "item has one child");
        assert_kind(&tree, item_children[0], &ElementKind::Paragraph);
    }

    #[test]
    fn multi_item_unordered() {
        let source = "- a\n- b\n- c\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one list");
        let items = tree.children(children[0]);
        assert_eq!(items.len(), 3, "list has three items");
        for &item in items {
            assert!(
                matches!(tree.node(item).kind, ElementKind::ListItem { task: None }),
                "each item is a regular ListItem"
            );
        }
    }

    #[test]
    fn unordered_marker_star() {
        let source = "* item\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one list");
        assert!(
            matches!(
                tree.node(children[0]).kind,
                ElementKind::List { ordered: false, .. }
            ),
            "star marker produces unordered list"
        );
    }

    #[test]
    fn unordered_marker_plus() {
        let source = "+ item\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one list");
        assert!(
            matches!(
                tree.node(children[0]).kind,
                ElementKind::List { ordered: false, .. }
            ),
            "plus marker produces unordered list"
        );
    }

    #[test]
    fn ordered_list_dot() {
        let source = "1. first\n2. second\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one list");
        assert!(
            matches!(
                tree.node(children[0]).kind,
                ElementKind::List {
                    ordered: true,
                    start: 1,
                    ..
                }
            ),
            "ordered list with dot delimiter"
        );
        let items = tree.children(children[0]);
        assert_eq!(items.len(), 2, "list has two items");
    }

    #[test]
    fn ordered_list_paren() {
        let source = "1) first\n2) second\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one list");
        assert!(
            matches!(
                tree.node(children[0]).kind,
                ElementKind::List {
                    ordered: true,
                    start: 1,
                    ..
                }
            ),
            "ordered list with paren delimiter"
        );
    }

    #[test]
    fn ordered_list_start_number() {
        let source = "3. third\n4. fourth\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one list");
        assert!(
            matches!(
                tree.node(children[0]).kind,
                ElementKind::List {
                    ordered: true,
                    start: 3,
                    ..
                }
            ),
            "ordered list preserves start number"
        );
    }

    // --- Lists: structure ---

    #[test]
    fn list_items_are_children_of_list() {
        let source = "- a\n- b\n";
        let tree = parse(source);
        let list_id = root_children(&tree)[0];
        let items = tree.children(list_id);

        for &item_id in items {
            assert_eq!(
                tree.node(item_id).parent,
                Some(list_id),
                "item parent is the list"
            );
        }
    }

    #[test]
    fn list_span_covers_all_items() {
        let source = "- a\n- b\n- c\n";
        let tree = parse(source);
        let list = tree.node(root_children(&tree)[0]);

        assert_eq!(
            list.span,
            Span::new(0, source.len()),
            "list span covers entire content"
        );
    }

    // --- Lists: nested ---

    #[test]
    fn nested_list_two_levels() {
        let source = "- outer\n  - inner\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "one top-level list");
        let outer_items = tree.children(children[0]);
        assert_eq!(outer_items.len(), 1, "one outer item");

        // Outer item contains: paragraph + nested list
        let outer_item_children = tree.children(outer_items[0]);
        assert!(
            outer_item_children.len() >= 2,
            "outer item has paragraph + nested list, got {}",
            outer_item_children.len()
        );

        // Find the nested list
        let nested_list = outer_item_children
            .iter()
            .find(|&&id| matches!(tree.node(id).kind, ElementKind::List { .. }))
            .expect("should find nested list");
        let nested_items = tree.children(*nested_list);
        assert_eq!(nested_items.len(), 1, "nested list has one item");
    }

    #[test]
    fn nested_list_three_levels() {
        let source = "- a\n  - b\n    - c\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "one top-level list");
        let l1_items = tree.children(children[0]);
        let l1_item_children = tree.children(l1_items[0]);

        // Find level 2 list
        let l2_list = l1_item_children
            .iter()
            .find(|&&id| matches!(tree.node(id).kind, ElementKind::List { .. }))
            .expect("should find level 2 list");
        let l2_items = tree.children(*l2_list);
        let l2_item_children = tree.children(l2_items[0]);

        // Find level 3 list
        let l3_list = l2_item_children
            .iter()
            .find(|&&id| matches!(tree.node(id).kind, ElementKind::List { .. }))
            .expect("should find level 3 list");
        let l3_items = tree.children(*l3_list);
        assert_eq!(l3_items.len(), 1, "level 3 has one item");
    }

    // --- Lists: tight vs loose ---

    #[test]
    fn tight_list_no_blanks() {
        let source = "- a\n- b\n- c\n";
        let tree = parse(source);
        let list = tree.node(root_children(&tree)[0]);

        assert!(
            matches!(list.kind, ElementKind::List { tight: true, .. }),
            "no blank lines → tight"
        );
    }

    #[test]
    fn loose_list_blank_between_items() {
        let source = "- a\n\n- b\n";
        let tree = parse(source);
        let list = tree.node(root_children(&tree)[0]);

        assert!(
            matches!(list.kind, ElementKind::List { tight: false, .. }),
            "blank between items → loose"
        );
    }

    #[test]
    fn blank_within_item_not_loose() {
        let source = "- a\n\n  b\n- c\n";
        let tree = parse(source);
        let children = root_children(&tree);
        let list = tree.node(children[0]);

        // The blank is within the first item (between paragraphs),
        // but the next item follows without a blank → still tight
        // at the list level (blank between items makes it loose).
        // Actually per CommonMark, a blank within an item that has
        // a subsequent item with blank between them → loose.
        // Here: "- a\n\n  b\n- c\n" → blank after item 1's first
        // paragraph, continuation "  b" at content column, then
        // item 2 "- c". The blank is within item 1, not between
        // items, so the list is tight.
        assert!(
            matches!(list.kind, ElementKind::List { tight: true, .. }),
            "blank within item does not make list loose"
        );
    }

    // --- Lists: task items ---

    #[test]
    fn task_item_unchecked() {
        let source = "- [ ] todo\n";
        let tree = parse(source);
        let list_id = root_children(&tree)[0];
        let items = tree.children(list_id);

        assert_eq!(items.len(), 1, "one item");
        assert_kind(
            &tree,
            items[0],
            &ElementKind::ListItem { task: Some(false) },
        );
    }

    #[test]
    fn task_item_checked() {
        let source = "- [x] done\n";
        let tree = parse(source);
        let list_id = root_children(&tree)[0];
        let items = tree.children(list_id);

        assert_kind(&tree, items[0], &ElementKind::ListItem { task: Some(true) });
    }

    #[test]
    fn task_item_checked_uppercase() {
        let source = "- [X] done\n";
        let tree = parse(source);
        let list_id = root_children(&tree)[0];
        let items = tree.children(list_id);

        assert_kind(&tree, items[0], &ElementKind::ListItem { task: Some(true) });
    }

    #[test]
    fn mixed_task_and_regular() {
        let source = "- [ ] todo\n- regular\n- [x] done\n";
        let tree = parse(source);
        let list_id = root_children(&tree)[0];
        let items = tree.children(list_id);

        assert_eq!(items.len(), 3, "three items");
        assert_kind(
            &tree,
            items[0],
            &ElementKind::ListItem { task: Some(false) },
        );
        assert_kind(&tree, items[1], &ElementKind::ListItem { task: None });
        assert_kind(&tree, items[2], &ElementKind::ListItem { task: Some(true) });
    }

    // --- Lists: continuation ---

    #[test]
    fn multiline_item_continuation() {
        let source = "- line one\n  line two\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "one list");
        let items = tree.children(children[0]);
        assert_eq!(items.len(), 1, "one item");
        let item_children = tree.children(items[0]);
        assert_eq!(item_children.len(), 1, "item has one paragraph");
        assert_kind(&tree, item_children[0], &ElementKind::Paragraph);
    }

    #[test]
    fn lazy_continuation_no_indent() {
        let source = "- first\nlazy line\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "one list");
        let items = tree.children(children[0]);
        assert_eq!(items.len(), 1, "one item");
        let item_children = tree.children(items[0]);
        assert_eq!(item_children.len(), 1, "item has one paragraph");
        assert_kind(&tree, item_children[0], &ElementKind::Paragraph);
    }

    #[test]
    fn lazy_continuation_broken_by_blank() {
        let source = "- first\n\nnot in list\n";
        let tree = parse(source);
        let children = root_children(&tree);

        // Blank line + unindented line closes the list.
        assert!(children.len() >= 2, "list + paragraph");
        assert!(
            matches!(tree.node(children[0]).kind, ElementKind::List { .. }),
            "first child is list"
        );
        assert_kind(&tree, children[children.len() - 1], &ElementKind::Paragraph);
    }

    #[test]
    fn lazy_continuation_broken_by_list_marker() {
        let source = "- first\n+ second\n";
        let tree = parse(source);
        let children = root_children(&tree);

        // `+ second` is a different marker → new list, not lazy continuation.
        assert_eq!(children.len(), 2, "two lists");
    }

    // --- Lists: marker changes ---

    #[test]
    fn different_marker_starts_new_list() {
        let source = "* item a\n- item b\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 2, "two separate lists");
        assert!(
            matches!(tree.node(children[0]).kind, ElementKind::List { .. }),
            "first is a list"
        );
        assert!(
            matches!(tree.node(children[1]).kind, ElementKind::List { .. }),
            "second is a list"
        );
    }

    // --- Lists: items with block constructs ---

    #[test]
    fn item_containing_fenced_code() {
        let source = "- code:\n  ```\n  fn main() {}\n  ```\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "one list");
        let items = tree.children(children[0]);
        assert_eq!(items.len(), 1, "one item");
        let item_children = tree.children(items[0]);

        let has_code = item_children
            .iter()
            .any(|&id| matches!(tree.node(id).kind, ElementKind::CodeBlock));
        assert!(has_code, "item should contain a code block");
    }

    #[test]
    fn item_containing_block_quote() {
        let source = "- text\n  > quoted\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "one list");
        let items = tree.children(children[0]);
        let item_children = tree.children(items[0]);

        let has_quote = item_children
            .iter()
            .any(|&id| matches!(tree.node(id).kind, ElementKind::QuoteBlock));
        assert!(has_quote, "item should contain a block quote");
    }

    #[test]
    fn fence_at_list_boundary_closes_code_block() {
        // Closing fence at indent 0 while code block is inside a list
        // item (content_column=2). The fence should close the code block,
        // not produce an unclosed diagnostic.
        let source = "- ```\n  code\n```\n";
        let tree = parse(source);

        assert!(
            tree.diagnostics().is_empty(),
            "no unclosed diagnostic: {:?}",
            tree.diagnostics()
        );

        let children = root_children(&tree);
        assert_eq!(children.len(), 1, "one list");
        let items = tree.children(children[0]);
        let item_children = tree.children(items[0]);

        let has_code = item_children
            .iter()
            .any(|&id| matches!(tree.node(id).kind, ElementKind::CodeBlock));
        assert!(has_code, "item should contain a code block");
    }

    // --- Lists: interactions ---

    #[test]
    fn thematic_break_not_list_dashes() {
        let source = "---\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "one block");
        assert_kind(&tree, children[0], &ElementKind::Rules);
    }

    #[test]
    fn thematic_break_not_list_spaced_dashes() {
        let source = "- - -\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "one block");
        assert_kind(&tree, children[0], &ElementKind::Rules);
    }

    #[test]
    fn list_after_paragraph() {
        let source = "Paragraph\n- item\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 2, "paragraph + list");
        assert_kind(&tree, children[0], &ElementKind::Paragraph);
        assert!(
            matches!(tree.node(children[1]).kind, ElementKind::List { .. }),
            "second child is a list"
        );
    }

    #[test]
    fn ordered_start_not_1_cannot_interrupt_paragraph() {
        let source = "Paragraph\n3. item\n";
        let tree = parse(source);
        let children = root_children(&tree);

        // "3. item" cannot interrupt a paragraph, so it's part of the
        // paragraph continuation.
        assert_eq!(children.len(), 1, "single paragraph");
        assert_kind(&tree, children[0], &ElementKind::Paragraph);
    }

    // --- Tables: basic ---

    #[test]
    fn basic_table() {
        let source = "| A | B |\n| --- | --- |\n| 1 | 2 |\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "one table");
        let table = tree.node(children[0]);
        assert!(
            matches!(&table.kind, ElementKind::Table { alignments } if alignments.len() == 2),
            "table with 2 columns"
        );

        let rows = tree.children(children[0]);
        assert_eq!(rows.len(), 2, "header row + 1 body row");

        // Header row.
        assert_kind(&tree, rows[0], &ElementKind::TableRow { header: true });
        let header_cells = tree.children(rows[0]);
        assert_eq!(header_cells.len(), 2, "header has 2 cells");
        assert_kind(&tree, header_cells[0], &ElementKind::TableCell);
        assert_kind(&tree, header_cells[1], &ElementKind::TableCell);
        assert_eq!(
            tree.text(&tree.node(header_cells[0]).span),
            "A",
            "first header cell text"
        );
        assert_eq!(
            tree.text(&tree.node(header_cells[1]).span),
            "B",
            "second header cell text"
        );

        // Body row.
        assert_kind(&tree, rows[1], &ElementKind::TableRow { header: false });
        let body_cells = tree.children(rows[1]);
        assert_eq!(body_cells.len(), 2, "body has 2 cells");
        assert_eq!(
            tree.text(&tree.node(body_cells[0]).span),
            "1",
            "first body cell text"
        );
        assert_eq!(
            tree.text(&tree.node(body_cells[1]).span),
            "2",
            "second body cell text"
        );
    }

    #[test]
    fn table_multiple_body_rows() {
        let source = "| H |\n| --- |\n| a |\n| b |\n| c |\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "one table");
        let rows = tree.children(children[0]);
        assert_eq!(rows.len(), 4, "header + 3 body rows");
        assert_kind(&tree, rows[0], &ElementKind::TableRow { header: true });
        for &row_id in &rows[1..] {
            assert_kind(&tree, row_id, &ElementKind::TableRow { header: false });
        }
    }

    #[test]
    fn table_header_only() {
        let source = "| H1 | H2 |\n| --- | --- |\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "one table");
        let rows = tree.children(children[0]);
        assert_eq!(rows.len(), 1, "header row only");
    }

    // --- Tables: alignment ---

    #[test]
    fn table_alignment_left() {
        let source = "| A |\n| --- |\n| x |\n";
        let tree = parse(source);
        let table = tree.node(root_children(&tree)[0]);

        assert!(
            matches!(&table.kind, ElementKind::Table { alignments }
                if alignments == &[TableAlignment::Left]),
            "default left alignment"
        );
    }

    #[test]
    fn table_alignment_left_colon() {
        let source = "| A |\n| :--- |\n| x |\n";
        let tree = parse(source);
        let table = tree.node(root_children(&tree)[0]);

        assert!(
            matches!(&table.kind, ElementKind::Table { alignments }
                if alignments == &[TableAlignment::Left]),
            "explicit left alignment"
        );
    }

    #[test]
    fn table_alignment_center() {
        let source = "| A |\n| :---: |\n| x |\n";
        let tree = parse(source);
        let table = tree.node(root_children(&tree)[0]);

        assert!(
            matches!(&table.kind, ElementKind::Table { alignments }
                if alignments == &[TableAlignment::Center]),
            "center alignment"
        );
    }

    #[test]
    fn table_alignment_right() {
        let source = "| A |\n| ---: |\n| x |\n";
        let tree = parse(source);
        let table = tree.node(root_children(&tree)[0]);

        assert!(
            matches!(&table.kind, ElementKind::Table { alignments }
                if alignments == &[TableAlignment::Right]),
            "right alignment"
        );
    }

    #[test]
    fn table_mixed_alignment() {
        let source = "| L | C | R |\n| --- | :---: | ---: |\n| a | b | c |\n";
        let tree = parse(source);
        let table = tree.node(root_children(&tree)[0]);

        assert!(
            matches!(&table.kind, ElementKind::Table { alignments }
            if alignments == &[
                TableAlignment::Left,
                TableAlignment::Center,
                TableAlignment::Right,
            ]),
            "mixed alignment"
        );
    }

    // --- Tables: column count mismatches ---

    #[test]
    fn table_fewer_cells_padded() {
        let source = "| A | B | C |\n| --- | --- | --- |\n| 1 |\n";
        let tree = parse(source);
        let children = root_children(&tree);
        let rows = tree.children(children[0]);

        // Body row should be padded to 3 cells.
        let body_cells = tree.children(rows[1]);
        assert_eq!(body_cells.len(), 3, "padded to 3 cells");

        // First cell has content, rest are empty.
        assert_eq!(
            tree.text(&tree.node(body_cells[0]).span),
            "1",
            "first cell has content"
        );
        assert!(
            tree.node(body_cells[1]).span.is_empty(),
            "second cell is empty"
        );
        assert!(
            tree.node(body_cells[2]).span.is_empty(),
            "third cell is empty"
        );
    }

    #[test]
    fn table_excess_cells_ignored() {
        let source = "| A |\n| --- |\n| 1 | 2 | 3 |\n";
        let tree = parse(source);
        let children = root_children(&tree);
        let rows = tree.children(children[0]);

        // Body row should have only 1 cell (excess ignored).
        let body_cells = tree.children(rows[1]);
        assert_eq!(body_cells.len(), 1, "excess cells ignored");
    }

    #[test]
    fn table_mismatch_diagnostic() {
        let source = "| A | B |\n| --- | --- |\n| 1 |\n";
        let tree = parse(source);

        let mismatch_diags: Vec<_> = tree
            .diagnostics()
            .iter()
            .filter(|d| d.message.contains("cells"))
            .collect();
        assert_eq!(mismatch_diags.len(), 1, "one mismatch diagnostic");
        assert!(
            mismatch_diags[0].message.contains("1 cells, expected 2"),
            "diagnostic message: {}",
            mismatch_diags[0].message
        );
    }

    // --- Tables: pipes in inline code ---

    #[test]
    fn table_pipe_in_inline_code() {
        let source = "| A | B |\n| --- | --- |\n| `a|b` | c |\n";
        let tree = parse(source);
        let children = root_children(&tree);
        let rows = tree.children(children[0]);

        let body_cells = tree.children(rows[1]);
        assert_eq!(body_cells.len(), 2, "pipe in code does not split");
        assert_eq!(
            tree.text(&tree.node(body_cells[0]).span),
            "`a|b`",
            "code span preserved"
        );
    }

    #[test]
    fn table_pipe_in_double_backtick_code() {
        let source = "| A |\n| --- |\n| ``a | b`` |\n";
        let tree = parse(source);
        let children = root_children(&tree);
        let rows = tree.children(children[0]);

        let body_cells = tree.children(rows[1]);
        assert_eq!(
            body_cells.len(),
            1,
            "pipe in double-backtick code does not split"
        );
    }

    // --- Tables: links in cells ---

    #[test]
    fn table_with_links() {
        let source = "| Name |\n| --- |\n| [foo](bar.md) |\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "one table");
        let rows = tree.children(children[0]);
        let body_cells = tree.children(rows[1]);

        // The cell should have inline children from the inline parser.
        let cell_children = tree.children(body_cells[0]);
        let has_link = cell_children
            .iter()
            .any(|&id| matches!(tree.node(id).kind, ElementKind::Link { .. }));
        assert!(has_link, "cell should contain a link from inline parsing");
    }

    // --- Tables: edge cases ---

    #[test]
    fn table_single_column() {
        let source = "| A |\n| --- |\n| x |\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "one table");
        let table = tree.node(children[0]);
        assert!(
            matches!(&table.kind, ElementKind::Table { alignments } if alignments.len() == 1),
            "single column table"
        );
    }

    #[test]
    fn table_no_leading_trailing_pipes() {
        let source = "A | B\n--- | ---\n1 | 2\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "one table");
        let rows = tree.children(children[0]);
        assert_eq!(rows.len(), 2, "header + body");

        let body_cells = tree.children(rows[1]);
        assert_eq!(
            body_cells.len(),
            2,
            "2 cells without leading/trailing pipes"
        );
        assert_eq!(tree.text(&tree.node(body_cells[0]).span), "1", "first cell");
        assert_eq!(
            tree.text(&tree.node(body_cells[1]).span),
            "2",
            "second cell"
        );
    }

    #[test]
    fn table_empty_cells() {
        let source = "| A | B |\n| --- | --- |\n| | |\n";
        let tree = parse(source);
        let children = root_children(&tree);
        let rows = tree.children(children[0]);

        let body_cells = tree.children(rows[1]);
        assert_eq!(body_cells.len(), 2, "two empty cells");
        assert!(tree.node(body_cells[0]).span.is_empty(), "first cell empty");
        assert!(
            tree.node(body_cells[1]).span.is_empty(),
            "second cell empty"
        );
    }

    #[test]
    fn table_ends_at_blank_line() {
        let source = "| A |\n| --- |\n| x |\n\nParagraph\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 2, "table + paragraph");
        assert!(
            matches!(&tree.node(children[0]).kind, ElementKind::Table { .. }),
            "first is table"
        );
        assert_kind(&tree, children[1], &ElementKind::Paragraph);
    }

    #[test]
    fn table_ends_at_non_row_line() {
        let source = "| A |\n| --- |\n| x |\n# Heading\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 2, "table + heading");
        assert!(
            matches!(&tree.node(children[0]).kind, ElementKind::Table { .. }),
            "first is table"
        );
        assert_kind(&tree, children[1], &ElementKind::Heading { level: 1 });
    }

    #[test]
    fn dashes_after_paragraph_is_setext_not_table() {
        // `---` after a paragraph line is a setext heading, not a table
        // delimiter, because the first line has no pipes.
        let source = "Heading\n---\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "one heading");
        assert_kind(&tree, children[0], &ElementKind::Heading { level: 2 });
    }

    #[test]
    fn not_a_table_without_delimiter() {
        let source = "| A | B |\n| C | D |\n";
        let tree = parse(source);
        let children = root_children(&tree);

        // Second line is not a delimiter row, so this is a paragraph.
        assert_eq!(children.len(), 1, "one paragraph");
        assert_kind(&tree, children[0], &ElementKind::Paragraph);
    }

    #[test]
    fn table_in_list_item() {
        let source = "- | A |\n  | --- |\n  | x |\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "one list");
        let items = tree.children(children[0]);
        assert_eq!(items.len(), 1, "one item");

        let item_children = tree.children(items[0]);
        assert!(
            item_children
                .iter()
                .any(|&id| matches!(&tree.node(id).kind, ElementKind::Table { .. })),
            "list item contains table"
        );
    }

    #[test]
    fn table_in_block_quote() {
        let source = "> | A |\n> | --- |\n> | x |\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "one block quote");
        assert_kind(&tree, children[0], &ElementKind::QuoteBlock);

        let quote_children = tree.children(children[0]);
        assert!(
            quote_children
                .iter()
                .any(|&id| matches!(&tree.node(id).kind, ElementKind::Table { .. })),
            "block quote contains table"
        );
    }

    #[test]
    fn table_tree_structure() {
        // Verify parent-child relationships throughout.
        let source = "| A | B |\n| --- | --- |\n| 1 | 2 |\n";
        let tree = parse(source);
        let table_id = root_children(&tree)[0];
        let rows = tree.children(table_id);

        for &row_id in rows {
            assert_eq!(
                tree.node(row_id).parent,
                Some(table_id),
                "row parent is table"
            );
            for &cell_id in tree.children(row_id) {
                assert_eq!(
                    tree.node(cell_id).parent,
                    Some(row_id),
                    "cell parent is row"
                );
            }
        }
    }

    #[test]
    fn table_span_covers_all_content() {
        let source = "| A |\n| --- |\n| x |\n";
        let tree = parse(source);
        let table = tree.node(root_children(&tree)[0]);

        assert_eq!(
            tree.text(&table.span),
            source,
            "table span covers all rows including delimiter"
        );
    }

    // --- Tables: delimiter row validation ---

    #[test]
    fn delimiter_row_requires_dashes() {
        // Spaces-only cells are not valid delimiter rows.
        let source = "| A |\n|   |\n| x |\n";
        let tree = parse(source);
        let children = root_children(&tree);

        // Should be a paragraph (no valid delimiter row).
        assert_kind(&tree, children[0], &ElementKind::Paragraph);
    }

    #[test]
    fn delimiter_row_minimum_one_dash() {
        let source = "| A |\n| - |\n| x |\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "one table");
        assert!(
            matches!(&tree.node(children[0]).kind, ElementKind::Table { .. }),
            "single dash is valid delimiter"
        );
    }
}
