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

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use crate::html::{self, HtmlTag};
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
    /// A scalar key-value pair in frontmatter (e.g. `title: "Doc"`).
    FrontmatterKey {
        /// The key name.
        key: String,
        /// Number of leaf values (sequence items). Zero for scalar values.
        leaf_count: usize,
    },
    /// A mapping value in frontmatter (e.g. `backlinks:` with nested keys).
    FrontmatterMap {
        /// The key name.
        key: String,
    },
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
    /// GFM admonition (`> [!TYPE]`) or styled container (`<div class="warning">`).
    Admonition {
        /// Admonition type (e.g. `NOTE`, `WARNING`, `TIP`).
        kind: String,
    },
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
    /// Inline or reference-style image (or `<img>` / `<iframe>`).
    Image {
        /// Image source URL.
        url: String,
        /// Image title (empty if none).
        title: String,
    },
    /// Video embed (`<video>` or `![](*.mp4)`).
    Video {
        /// Video source URL.
        url: String,
        /// Video title (empty if none).
        title: String,
    },
    /// Audio embed (`<audio>` or `![](*.mp3)`).
    Audio {
        /// Audio source URL.
        url: String,
        /// Audio title (empty if none).
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
    /// Generic HTML container (`<div>`, `<section>`, `<article>`, etc.).
    Container,
    /// `<details>` disclosure container.
    Details,
    /// `<summary>` inside a `<details>`.
    DetailsSummary,
    /// HTML form control (`<input>`, `<select>`, `<textarea>`).
    FormControl,
    /// Definition list container (`<dl>` or Pandoc/PHP Extra syntax).
    DefinitionList,
    /// Term in a definition list (`<dt>` or plain text before `: `).
    DefinitionTerm,
    /// Description in a definition list (`<dd>` or `: ` content).
    DefinitionDesc,
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
    /// TOML frontmatter.
    Toml,
    /// JSON frontmatter.
    Json,
    /// Markdown structural syntax.
    Markdown,
    /// Raw HTML.
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
    #[allow(dead_code, reason = "structural field used by navigation ticket 08")]
    pub parent: Option<NodeId>,
    /// Child nodes in document order.
    pub children: Vec<NodeId>,
}

/// Severity level for parser diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticLevel {
    /// Fatal issue.
    Error,
    /// Non-fatal issue.
    Warning,
}

/// A diagnostic emitted during parsing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// Location of the error in the source.
    pub span: Span,
    /// Severity level.
    pub level: DiagnosticLevel,
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
    /// Whether the node-count limit diagnostic has been emitted. Carried
    /// from the builder so the inline pass does not duplicate it.
    node_limit_emitted: bool,
    /// Whether the inline pass has already run. The pass is not re-entrant —
    /// re-running it would duplicate every inline child node and diagnostic —
    /// so [`crate::inline::parse_inlines`] checks this and no-ops on a second
    /// call, making the pass idempotent.
    inlines_parsed: bool,
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
        dead_code,
        clippy::unused_self,
        reason = "public API used by tests in other modules"
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
    #[allow(dead_code, reason = "public API for structural diagnostics ticket 07")]
    pub fn text(&self, span: &Span) -> &str {
        &self.source[span.start..span.end]
    }

    /// The number of nodes in the tree.
    #[must_use]
    #[allow(dead_code, reason = "public API for future consumers")]
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the tree is empty (it never is — always has `Document`).
    #[must_use]
    #[allow(dead_code, reason = "public API for future consumers")]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Direct children of a node.
    #[must_use]
    #[allow(dead_code, reason = "public API used by tests in other modules")]
    pub fn children(&self, id: NodeId) -> &[NodeId] {
        &self.nodes[id].children
    }

    /// Find the first `ReferenceDef` node matching a normalized label.
    #[must_use]
    pub fn find_ref_def(&self, label: &str) -> Option<(NodeId, &Node)> {
        self.nodes.iter().enumerate().find(|(_, node)| {
            matches!(
                &node.kind,
                ElementKind::ReferenceDef { label: l, .. } if l == label
            )
        })
    }

    /// Find a `Link`, `Image`, `Video`, or `Audio` node whose span contains
    /// the given byte offset.
    #[must_use]
    pub fn find_link_at_offset(&self, offset: usize) -> Option<(NodeId, &Node)> {
        self.nodes.iter().enumerate().find(|(_, node)| {
            matches!(
                node.kind,
                ElementKind::Link { .. }
                    | ElementKind::Image { .. }
                    | ElementKind::Video { .. }
                    | ElementKind::Audio { .. }
            ) && node.span.start <= offset
                && offset < node.span.end
        })
    }

    /// Add a child node to an existing node (used by the inline parser).
    ///
    /// Honors the tree node-count limit: once reached, no node is created and
    /// the parent id is returned. The first call to hit the limit during the
    /// inline pass emits the (single) node-limit diagnostic.
    pub fn add_child(
        &mut self,
        parent: NodeId,
        kind: ElementKind,
        syntax: Syntax,
        span: Span,
    ) -> NodeId {
        if self.nodes.len() >= crate::limits::MAX_NODES {
            if !self.node_limit_emitted {
                self.node_limit_emitted = true;
                self.diagnostics.push(Diagnostic {
                    level: DiagnosticLevel::Warning,
                    span,
                    message: format!(
                        "document exceeds the {}-node limit; remaining structure is not indexed",
                        crate::limits::MAX_NODES
                    ),
                });
            }
            return parent;
        }
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

    /// Whether the inline pass has already run on this tree.
    #[must_use]
    pub const fn inlines_parsed(&self) -> bool {
        self.inlines_parsed
    }

    /// Mark the inline pass as having run, so a later call is a no-op.
    pub const fn mark_inlines_parsed(&mut self) {
        self.inlines_parsed = true;
    }
}

// ---------------------------------------------------------------------------
// Consumer types
// ---------------------------------------------------------------------------

/// A link extracted from the parse tree.
#[derive(Debug)]
pub struct Link {
    /// 1-based line number in the source.
    pub line: usize,
    /// Byte span of the link in the source.
    pub span: Span,
    /// Classification and resolved details.
    pub kind: LinkKind,
}

/// Classification of a link.
#[derive(Debug)]
pub enum LinkKind {
    /// External URL (`http://`, `https://`, `mailto:`).
    External {
        /// The raw URL.
        #[allow(dead_code, reason = "stored for LSP diagnostics")]
        url: String,
    },
    /// Intra-document fragment-only link (`#section`).
    IntraDocument {
        /// Fragment without the leading `#`.
        fragment: String,
    },
    /// Link to a non-markdown file in the project.
    NonMarkdown {
        /// Resolved path to the target.
        target: PathBuf,
    },
    /// Intra-project link to a markdown file.
    IntraProject {
        /// Resolved path to the target `.md` file.
        target: PathBuf,
        /// Fragment (heading anchor), if any.
        fragment: Option<String>,
        /// Predicate from title text, or `"references"` if absent.
        predicate: String,
        /// Whether the predicate was explicitly set via title text.
        explicit_predicate: bool,
    },
}

/// A heading extracted from the parse tree.
#[derive(Debug)]
pub struct Heading {
    /// 1-based line number in the source.
    pub line: usize,
    /// Heading level (1–6).
    pub level: u8,
    /// Raw text content of the heading.
    pub text: String,
    /// Heading anchor ID.
    pub id: HeadingId,
    /// Byte span of the heading text in the source (for rename support).
    pub text_span: Span,
    /// Which syntax produced this heading.
    #[allow(dead_code, reason = "structural field for future syntax-aware rename")]
    pub syntax: Syntax,
}

/// How a heading's anchor ID was determined.
#[derive(Debug)]
pub enum HeadingId {
    /// Explicit `{#id}` attribute on the heading.
    Explicit(String),
    /// Computed slugs from the heading text.
    Computed {
        /// GitHub slug.
        github: String,
        /// GitLab slug.
        gitlab: String,
        /// VS Code slug.
        vscode: String,
    },
}

/// A bare file path found in document text.
#[derive(Debug)]
pub struct BarePath {
    /// 1-based line number in the source.
    pub line: usize,
    /// The detected path text.
    pub path: String,
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

/// Expand ALL tabs to spaces (not just leading ones).
///
/// Used for list marker recognition where tabs may appear after the
/// marker character (e.g. `-\t\tfoo`).
fn expand_all_tabs(line: &str) -> (String, Vec<TabMapping>) {
    let mut result = String::with_capacity(line.len());
    let mut mappings = Vec::new();
    let mut col = 0;

    for (byte_idx, ch) in line.char_indices() {
        if ch == '\t' {
            let spaces = 4 - (col % 4);
            mappings.push(TabMapping {
                original_byte: byte_idx,
                num_spaces: spaces,
            });
            for _ in 0..spaces {
                result.push(' ');
            }
            col += spaces;
        } else {
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
    /// Number of spaces this tab expanded to.
    num_spaces: usize,
}

/// Map a column offset in a tab-expanded string back to the corresponding byte
/// offset in the original (pre-expansion) string.
///
/// Walks the raw line accumulating expanded columns until it reaches
/// `expanded_offset`. Because each character advances the byte index by its
/// UTF-8 width, the returned offset always lands on a char boundary even when
/// the indentation region contains multi-byte characters — e.g. a U+00A0
/// non-breaking space, which `str::trim` counts as whitespace, so an
/// all-whitespace continuation line can reach the slice path. A tab recorded in
/// `mappings` occupies its expanded `num_spaces` columns; every other character
/// (including a non-leading tab absent from `mappings`) is one column. With no
/// tabs and ASCII content this reduces to the identity `min(len)`.
fn expanded_to_raw(expanded_offset: usize, raw_line: &str, mappings: &[TabMapping]) -> usize {
    let mut col = 0;
    let mut mi = 0;
    for (byte_idx, ch) in raw_line.char_indices() {
        if col >= expanded_offset {
            return byte_idx;
        }
        // Mappings are in increasing byte order; skip any already passed.
        while mi < mappings.len() && mappings[mi].original_byte < byte_idx {
            mi += 1;
        }
        if ch == '\t' && mi < mappings.len() && mappings[mi].original_byte == byte_idx {
            col += mappings[mi].num_spaces;
            mi += 1;
        } else {
            col += 1;
        }
    }
    raw_line.len()
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

/// Skip ASCII spaces and tabs (not line endings) from `i`.
const fn skip_inline_ws(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    i
}

/// Consume a single line ending (`\n`, `\r\n`, or `\r`) at `i`, returning the
/// index just past it. If no line ending is present, returns `i` unchanged.
const fn consume_line_ending(bytes: &[u8], mut i: usize) -> usize {
    if i < bytes.len() && bytes[i] == b'\r' {
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b'\n' {
        i += 1;
    }
    i
}

/// Scan a link destination starting at `start`.
///
/// Either an angle-bracketed destination (`<...>`, single line) or a bare
/// sequence of non-whitespace, non-control characters. Backslash escapes are
/// skipped when locating the boundary. Returns the raw inner text and the
/// index just past the destination, or `None` if no destination is present.
fn scan_destination(s: &str, start: usize) -> Option<(String, usize)> {
    let bytes = s.as_bytes();
    let len = bytes.len();
    if bytes[start] == b'<' {
        let mut i = start + 1;
        while i < len {
            match bytes[i] {
                b'\\' if i + 1 < len && bytes[i + 1] < 0x80 => i += 2,
                b'>' => return Some((s[start + 1..i].to_string(), i + 1)),
                b'\n' | b'\r' | b'<' => return None,
                _ => i += 1,
            }
        }
        None
    } else {
        let mut i = start;
        while i < len {
            let b = bytes[i];
            if b == b'\\' && i + 1 < len && bytes[i + 1] < 0x80 {
                i += 2;
                continue;
            }
            if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' || b < 0x20 {
                break;
            }
            i += 1;
        }
        if i == start {
            None
        } else {
            Some((s[start..i].to_string(), i))
        }
    }
}

/// Scan a link title starting at its opening delimiter (`"`, `'`, or `(`).
///
/// Titles may span multiple lines (the caller never passes a buffer that
/// crosses a blank line, so an unterminated title correctly fails). Backslash
/// escapes are skipped. Returns the raw inner text and the index just past the
/// closing delimiter, or `None` if the title is not closed.
fn scan_title(s: &str, start: usize) -> Option<(String, usize)> {
    let bytes = s.as_bytes();
    let len = bytes.len();
    let close = match bytes[start] {
        b'"' => b'"',
        b'\'' => b'\'',
        b'(' => b')',
        _ => return None,
    };
    let open = bytes[start];
    let mut i = start + 1;
    while i < len {
        let b = bytes[i];
        if b == b'\\' && i + 1 < len && bytes[i + 1] < 0x80 {
            i += 2;
            continue;
        }
        if b == close {
            return Some((s[start + 1..i].to_string(), i + 1));
        }
        // An unescaped opening paren inside a `(...)` title is invalid.
        if open == b'(' && b == b'(' {
            return None;
        }
        i += 1;
    }
    None
}

/// Recognize the opener of a reference-definition label: up to three spaces of
/// indentation, then a `[` that is not a footnote marker (`[^`). Returns the
/// byte index just past the `[`, or `None`. Shared by the cheap gate and the
/// full label scan — only their label-body handling differs.
fn refdef_label_open(bytes: &[u8]) -> Option<usize> {
    let len = bytes.len();
    let mut i = 0;
    while i < len && bytes[i] == b' ' {
        i += 1;
    }
    if i > 3 || i >= len || bytes[i] != b'[' {
        return None;
    }
    i += 1;
    // Footnote definitions (`[^...]`) are not reference definitions.
    if i < len && bytes[i] == b'^' {
        return None;
    }
    Some(i)
}

/// Cheap, allocation-free gate: could the first line begin a reference
/// definition? Examines only `line` (the candidate's first line).
///
/// Returns `true` when the line opens a label that either closes with `]:`
/// here, or stays open at the line end (a label may continue on the next
/// line). Returns `false` for ordinary bracketed text such as `[text][ref]`,
/// `[link](url)`, and shortcut references, so they never trigger run
/// collection. Being a fast pre-filter, it tolerates false positives (e.g.
/// `[]:`); the full scan rejects those.
fn first_line_opens_refdef(line: &str) -> bool {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let Some(mut i) = refdef_label_open(bytes) else {
        return false;
    };

    loop {
        if i >= len {
            return true; // label still open at end of line — may continue
        }
        match bytes[i] {
            b'\\' if i + 1 < len && bytes[i + 1] < 0x80 => i += 2,
            b'\n' | b'\r' => return true, // label may continue on the next line
            b']' => return bytes.get(i + 1) == Some(&b':'),
            b'[' => return false, // unescaped `[` — not a label
            _ => i += 1,
        }
    }
}

/// Recognize a reference-definition label at the start of `s`.
///
/// The label runs to the first unescaped `]` and may span line endings (the
/// caller's buffer never crosses a blank line, so a label that would need one
/// fails to close); it must contain at least one non-whitespace character and
/// be at most 999 bytes. Returns the byte index just past the `:` and the raw
/// label text, or `None`.
fn scan_refdef_label(s: &str) -> Option<(usize, &str)> {
    let bytes = s.as_bytes();
    let len = bytes.len();

    // Label: up to the first unescaped `]`; no unescaped `[`; may span lines.
    let label_start = refdef_label_open(bytes)?;
    let mut i = label_start;
    loop {
        if i >= len {
            return None;
        }
        match bytes[i] {
            b'\\' if i + 1 < len && bytes[i + 1] < 0x80 => i += 2,
            b']' => break,
            b'[' => return None,
            _ => i += 1,
        }
    }
    let label = &s[label_start..i];
    if label.trim().is_empty() || label.len() > 999 {
        return None;
    }
    i += 1; // consume `]`
    if i >= len || bytes[i] != b':' {
        return None;
    }
    Some((i + 1, label))
}

/// Scan a single link reference definition from the start of `s`.
///
/// `s` is the joined content of consecutive non-blank lines (each retaining its
/// line ending). Implements the `CommonMark` grammar with multi-line
/// destinations and titles and backslash escapes, including the
/// through-destination fallback: when a title is started but cannot be
/// completed, a definition valid up through the destination still matches.
///
/// Returns `(consumed_bytes, label, url, title)` for the first definition, or
/// `None` if `s` does not begin with one. `consumed_bytes` always lands on a
/// line boundary (or the end of `s`). The label is normalized.
fn scan_one_refdef(s: &str) -> Option<(usize, String, String, String)> {
    let bytes = s.as_bytes();
    let len = bytes.len();

    let (mut i, label) = scan_refdef_label(s)?;

    // Whitespace (including up to one line ending) before the destination.
    i = skip_inline_ws(bytes, i);
    if i < len && (bytes[i] == b'\n' || bytes[i] == b'\r') {
        i = consume_line_ending(bytes, i);
        i = skip_inline_ws(bytes, i);
    }
    // A second line ending (blank line) means there is no destination.
    if i >= len || bytes[i] == b'\n' || bytes[i] == b'\r' {
        return None;
    }

    // Destination.
    let (url, dest_end) = scan_destination(s, i)?;

    // Through-destination checkpoint: spaces/tabs then a line ending (or EOF)
    // after the destination make the definition valid without a title.
    let after_dest_ws = skip_inline_ws(bytes, dest_end);
    let had_trailing_ws = after_dest_ws > dest_end;
    let ckpt_dest = if after_dest_ws >= len {
        Some(len)
    } else if bytes[after_dest_ws] == b'\n' || bytes[after_dest_ws] == b'\r' {
        Some(consume_line_ending(bytes, after_dest_ws))
    } else {
        None
    };

    // Locate a possible title: on the same line, or on the next line when the
    // destination is followed only by whitespace (one line ending).
    let mut title_pos = after_dest_ws;
    let mut title_sep_ok = had_trailing_ws;
    if ckpt_dest.is_some()
        && after_dest_ws < len
        && (bytes[after_dest_ws] == b'\n' || bytes[after_dest_ws] == b'\r')
    {
        let nl_end = consume_line_ending(bytes, after_dest_ws);
        let next = skip_inline_ws(bytes, nl_end);
        if next < len && bytes[next] != b'\n' && bytes[next] != b'\r' {
            title_pos = next;
            title_sep_ok = true;
        }
    }

    if title_sep_ok
        && title_pos < len
        && matches!(bytes[title_pos], b'"' | b'\'' | b'(')
        && let Some((title, title_end)) = scan_title(s, title_pos)
    {
        let after_title_ws = skip_inline_ws(bytes, title_end);
        let ckpt_title = if after_title_ws >= len {
            Some(len)
        } else if bytes[after_title_ws] == b'\n' || bytes[after_title_ws] == b'\r' {
            Some(consume_line_ending(bytes, after_title_ws))
        } else {
            None
        };
        if let Some(end) = ckpt_title {
            return Some((end, normalize_label(label), url, title));
        }
    }

    // Fall back to a definition through the destination only.
    ckpt_dest.map(|end| (end, normalize_label(label), url, String::new()))
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
/// Three or more matching `*`, `-`, or `_` characters, each optionally
/// separated by spaces or tabs, with no other characters, and at most 3
/// leading spaces. A trailing line ending does not affect the result, so
/// callers may pass raw lines (including the `\n`) directly.
fn is_thematic_break(line: &str) -> bool {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return false;
    }

    // Spaces and tabs between markers, and any trailing line ending, are
    // not part of the break sequence.
    let stripped: String = trimmed
        .chars()
        .filter(|c| !matches!(c, ' ' | '\t' | '\n' | '\r'))
        .collect();
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
pub fn html_block_start(line: &str) -> Option<u8> {
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
                || after.starts_with('\r')
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
pub fn extract_html_tag_name(line: &str) -> Option<String> {
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

/// Whether the closing tag for `tag_name` appears on the same line after
/// the opening tag. `open_len` is the byte length of the opening tag
/// (from [`HtmlTag::Open::len`]).
fn has_close_on_same_line(line: &str, tag_name: &str, open_len: usize) -> bool {
    let mut rest = &line[open_len..];
    while let Some(idx) = rest.find("</") {
        if let Some(HtmlTag::Close { ref name, .. }) = html::tokenize_tag(&rest[idx..], 0)
            && name == tag_name
        {
            return true;
        }
        rest = &rest[idx + 2..];
    }
    false
}

/// Check if a line opens a `<pre><code>` block (case-insensitive).
fn is_pre_code_open(line: &str) -> bool {
    let lower = line.trim().to_lowercase();
    if let Some(after) = lower.strip_prefix("<pre>") {
        return after.trim_start().starts_with("<code");
    }
    // <pre followed by whitespace then > (e.g. <pre >) is also type 1,
    // but the <code> must follow the closing >.
    false
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
        let after_marker = &trimmed[1..];
        // Bare marker (nothing or only whitespace/newline after).
        if after_marker.is_empty() || after_marker.trim_end().is_empty() {
            return Some(ListMarkerInfo {
                ordered: false,
                marker_char: first,
                start: 0,
                marker_indent: indent,
                content_column: indent + 2,
                content_offset: line.len(),
            });
        }
        // Normal case: marker char + at least one space + content.
        if !after_marker.starts_with(' ') {
            return None;
        }
        let spaces_after = after_marker.len() - after_marker.trim_start_matches(' ').len();
        // If rest is blank, content column = marker pos + 2.
        // If > 4 spaces after marker with content, cap to marker + 1
        // (excess spaces become indented code within the item).
        let (content_column, content_offset) = if after_marker.trim().is_empty() {
            (indent + 2, line.len())
        } else if spaces_after > 4 {
            (indent + 2, indent + 2)
        } else {
            let cc = indent + 1 + spaces_after;
            (cc, cc)
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
        let start: u32 = trimmed[..digit_count].parse().ok()?;
        let marker_width = digit_count + 1; // digits + delimiter
        // Bare ordered marker (nothing or only whitespace/newline after delimiter).
        if after_delim.is_empty() || after_delim.trim_end().is_empty() {
            return Some(ListMarkerInfo {
                ordered: true,
                marker_char: delimiter,
                start,
                marker_indent: indent,
                content_column: indent + marker_width + 1,
                content_offset: line.len(),
            });
        }
        // Normal case: delimiter + at least one space + content.
        if !after_delim.starts_with(' ') {
            return None;
        }
        let spaces_after = after_delim.len() - after_delim.trim_start_matches(' ').len();
        // If rest is blank, content column = marker + 1.
        // If > 4 spaces after delimiter with content, cap to marker + 1
        // (excess spaces become indented code within the item).
        let (content_column, content_offset) = if after_delim.trim().is_empty() {
            (indent + marker_width + 1, line.len())
        } else if spaces_after > 4 {
            let cc = indent + marker_width + 1;
            (cc, cc)
        } else {
            let cc = indent + marker_width + spaces_after;
            (cc, cc)
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
    /// Column where item content starts (in stripped coordinates).
    content_column: usize,
    /// Cumulative indent from parent lists / blockquotes.
    /// The real marker indent in raw coordinates is `base_indent + marker_indent`.
    base_indent: usize,
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
            // Skip a backtick code span. Per CommonMark a span opened by a run
            // of N backticks closes only on the next run of *exactly* N; a
            // longer inner run (e.g. ``` inside a `` span) is literal content
            // and must not be mistaken for the close. A plain substring search
            // for N backticks would match the first N of a longer run, desync,
            // and swallow `|` delimiters past the real close.
            let bt_count = crate::inline::count_char(bytes, i, b'`');
            let after = i + bt_count;
            i = crate::inline::find_closing_backticks(bytes, after, bt_count)
                .unwrap_or(bytes.len());
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
            // Same exact-length backtick-run close as `split_table_cells`; a
            // longer inner run must not be mistaken for the closing run.
            let bt_count = crate::inline::count_char(bytes, i, b'`');
            let after = i + bt_count;
            i = crate::inline::find_closing_backticks(bytes, after, bt_count)
                .unwrap_or(bytes.len());
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

/// Detect a GFM admonition marker in blockquote content.
///
/// Returns the admonition type (e.g. `NOTE`, `WARNING`) if the content
/// starts with `[!TYPE]`, or `None` otherwise.
fn detect_admonition(content: &str) -> Option<String> {
    let trimmed = content.trim();
    let after = trimmed.strip_prefix("[!")?;
    let end = after.find(']')?;
    let kind = &after[..end];
    if kind.is_empty() || !kind.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    // Must be the only content on the line (possibly followed by whitespace).
    let rest = after[end + 1..].trim();
    if rest.is_empty() {
        Some(kind.to_uppercase())
    } else {
        None
    }
}

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
// Frontmatter tree expansion
// ---------------------------------------------------------------------------

/// Expand frontmatter entries into `FrontmatterKey` and `FrontmatterMap` child nodes.
fn expand_frontmatter_entries(
    builder: &mut TreeBuilder<'_>,
    parent_id: NodeId,
    syntax: Syntax,
    entries: &[crate::fm::FmNode],
) {
    for entry in entries {
        let crate::fm::FmNode::Mapping { key, value, span } = entry else {
            continue;
        };

        match value {
            crate::fm::FmValue::Mapping(children) => {
                let map_id = builder.add_node(
                    ElementKind::FrontmatterMap {
                        key: key.text.clone(),
                    },
                    syntax,
                    *span,
                    Some(parent_id),
                );
                expand_frontmatter_entries(builder, map_id, syntax, children);
            }
            _ => {
                builder.add_node(
                    ElementKind::FrontmatterKey {
                        key: key.text.clone(),
                        leaf_count: fm_leaf_count(value),
                    },
                    syntax,
                    *span,
                    Some(parent_id),
                );
            }
        }
    }
}

/// Count the number of leaf items in a frontmatter value.
///
/// Block sequences and flow sequences return their item count.
/// Scalars and other values return 0 (no list structure).
fn fm_leaf_count(value: &crate::fm::FmValue) -> usize {
    match value {
        crate::fm::FmValue::Sequence(items) => items.len(),
        crate::fm::FmValue::FlowSequence { items, .. } => items.len(),
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

/// Parse a markdown document into a [`Tree`].
///
/// If frontmatter is present, pass its byte range as `frontmatter_span`
/// so a `Frontmatter` node is created as the first child of `Document`.
/// Body parsing starts after the frontmatter span.
///
/// When `frontmatter_entries` is provided, child nodes are emitted for
/// each top-level key (and nested maps) so that symbol emission can
/// expose frontmatter structure.
#[allow(dead_code, reason = "public API used by tests in other modules")]
pub fn parse_tree(source: &str, frontmatter_span: Option<Span>) -> Tree {
    parse_tree_with_entries(source, frontmatter_span, Syntax::Yaml, None)
}

/// Extended variant of [`parse_tree`] that accepts parsed frontmatter entries
/// for child expansion.
pub fn parse_tree_with_entries(
    source: &str,
    frontmatter_span: Option<Span>,
    frontmatter_syntax: Syntax,
    frontmatter_entries: Option<&[crate::fm::FmNode]>,
) -> Tree {
    let mut builder = TreeBuilder::new(source);

    // Create Document root.
    let doc_id = builder.add_node(
        ElementKind::Document,
        Syntax::Markdown,
        Span::new(0, source.len()),
        None,
    );
    builder.scope_stack.push(doc_id);

    // If frontmatter is present, add it as first child. The frontmatter span
    // already starts after any leading BOM (the format parsers account for
    // it). With no frontmatter, a UTF-8 BOM at byte 0 is skipped here so the
    // first body block is still recognized; the BOM bytes fall under the
    // Document span only, and all block spans stay aligned to the original
    // source.
    let body_offset = frontmatter_span.map_or_else(
        || {
            if source.as_bytes().starts_with(crate::fm::BOM) {
                crate::fm::BOM.len()
            } else {
                0
            }
        },
        |fm_span| {
            let fm_id = builder.add_node(
                ElementKind::Frontmatter,
                frontmatter_syntax,
                fm_span,
                Some(doc_id),
            );

            // Expand frontmatter entries into child nodes.
            if let Some(entries) = frontmatter_entries {
                expand_frontmatter_entries(&mut builder, fm_id, frontmatter_syntax, entries);
            }

            fm_span.end
        },
    );

    // Parse the body.
    let body = &source[body_offset..];
    builder.parse_body(body, body_offset);

    // Close any remaining open lists (finalizes tight/loose).
    builder.close_all_lists(source.len());

    // Close any remaining open HTML scopes (emits unclosed diagnostics).
    builder.close_all_html_scopes(source.len());

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
        node_limit_emitted: builder.limits_hit.nodes,
        inlines_parsed: false,
    };

    // Second pass: parse inline elements in Paragraph and Heading nodes.
    crate::inline::parse_inlines(&mut tree);

    tree
}

/// An open HTML container on the html stack.
struct HtmlScope {
    /// Lowercased tag name (for matching close tags).
    tag: String,
    /// Node ID of the container in the tree.
    node_id: NodeId,
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
    /// Stack of open HTML container tags (for close-tag matching).
    html_stack: Vec<HtmlScope>,
    /// A blank line preceded the current line (for indented code detection).
    blank_before: bool,
    /// Whether each resource limit has already emitted its one diagnostic.
    /// Limits degrade silently after the first hit so a pathological
    /// document does not produce thousands of identical diagnostics.
    limits_hit: LimitFlags,
}

/// Tracks which resource limits have emitted their (single) diagnostic.
#[derive(Default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent one-shot latches, one per resource limit; not a state machine"
)]
struct LimitFlags {
    quote: bool,
    list: bool,
    html: bool,
    scope: bool,
    nodes: bool,
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
            html_stack: Vec::new(),
            blank_before: false,
            limits_hit: LimitFlags::default(),
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
        // Tree node count limit. Once reached, stop creating nodes so an
        // adversarial document cannot exhaust memory. Reuse the parent (or
        // the Document root) as the returned id so callers that record it
        // still reference a live node; the structure below this point is
        // simply not indexed.
        if self.nodes.len() >= crate::limits::MAX_NODES {
            if !self.limits_hit.nodes {
                self.limits_hit.nodes = true;
                self.diagnostics.push(Diagnostic {
                    level: DiagnosticLevel::Warning,
                    span,
                    message: format!(
                        "document exceeds the {}-node limit; remaining structure is not indexed",
                        crate::limits::MAX_NODES
                    ),
                });
            }
            return parent.unwrap_or(0);
        }
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
    ///
    /// The scope stack is hard-limited: once [`crate::limits::MAX_SCOPE_DEPTH`]
    /// open scopes are reached the node is still created (as a child of the
    /// current scope) but is not pushed, flattening any deeper nesting. This
    /// is the cross-container backstop behind the per-structure depth caps.
    fn push_scope(&mut self, kind: ElementKind, syntax: Syntax, span: Span) -> NodeId {
        let parent = self.current_scope();
        let id = self.add_node(kind, syntax, span, Some(parent));
        if self.scope_stack.len() < crate::limits::MAX_SCOPE_DEPTH {
            self.scope_stack.push(id);
        } else if !self.limits_hit.scope {
            self.limits_hit.scope = true;
            self.diagnostics.push(Diagnostic {
                level: DiagnosticLevel::Warning,
                span,
                message: format!(
                    "nesting exceeds the maximum scope depth of {}; deeper structure is flattened",
                    crate::limits::MAX_SCOPE_DEPTH
                ),
            });
        }
        id
    }

    /// Attempt to open a new block quote scope, respecting the nesting cap.
    ///
    /// Returns `true` when a `QuoteBlock` scope was opened. At the cap the
    /// `>` marker is left for the caller to treat as text and a single
    /// diagnostic is emitted.
    fn try_open_quote(&mut self, span_start: usize) -> bool {
        if self.quote_depth >= crate::limits::MAX_QUOTE_NESTING {
            self.note_quote_limit(span_start);
            return false;
        }
        self.push_scope(
            ElementKind::QuoteBlock,
            Syntax::Markdown,
            Span::new(span_start, span_start),
        );
        self.quote_depth += 1;
        true
    }

    /// Emit the block-quote nesting diagnostic at most once.
    fn note_quote_limit(&mut self, span_start: usize) {
        if !self.limits_hit.quote {
            self.limits_hit.quote = true;
            self.diagnostics.push(Diagnostic {
                level: DiagnosticLevel::Warning,
                span: Span::new(span_start, span_start),
                message: format!(
                    "block quote nesting exceeds the limit of {}; deeper `>` markers are treated as text",
                    crate::limits::MAX_QUOTE_NESTING
                ),
            });
        }
    }

    /// Emit the list nesting diagnostic at most once.
    fn note_list_limit(&mut self, span_start: usize) {
        if !self.limits_hit.list {
            self.limits_hit.list = true;
            self.diagnostics.push(Diagnostic {
                level: DiagnosticLevel::Warning,
                span: Span::new(span_start, span_start),
                message: format!(
                    "list nesting exceeds the limit of {}; deeper markers are treated as text",
                    crate::limits::MAX_LIST_NESTING
                ),
            });
        }
    }

    /// Emit the HTML container nesting diagnostic at most once.
    fn note_html_limit(&mut self, span_start: usize) {
        if !self.limits_hit.html {
            self.limits_hit.html = true;
            self.diagnostics.push(Diagnostic {
                level: DiagnosticLevel::Warning,
                span: Span::new(span_start, span_start),
                message: format!(
                    "HTML container nesting exceeds the limit of {}; deeper tags are not opened as scopes",
                    crate::limits::MAX_HTML_NESTING
                ),
            });
        }
    }

    /// Whether opening another list level would exceed the nesting cap.
    fn list_nesting_full(&self) -> bool {
        self.list_stack.len() >= crate::limits::MAX_LIST_NESTING
    }

    /// Pop the current scope, finalizing its span.
    ///
    /// Returns `true` if a scope was popped, `false` when refusing to pop the
    /// root `Document`. "Pop until" drain loops rely on this signal to
    /// terminate even if their target scope was already removed.
    fn pop_scope(&mut self, end: usize) -> bool {
        if self.scope_stack.len() > 1
            && let Some(id) = self.scope_stack.pop()
        {
            self.nodes[id].span.end = end;
            return true;
        }
        false
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
        // Base indent: sum of parent lists' content columns (so we can
        // compare marker indents in raw coordinates).
        let base_indent = self
            .list_stack
            .last()
            .map_or(0, |ctx| ctx.base_indent + ctx.content_column);
        self.list_stack.push(ListContext {
            list_node,
            item_node,
            marker_char: marker.marker_char,
            ordered: marker.ordered,
            content_column: marker.content_column,
            base_indent,
            saw_blank: false,
            loose: false,
        });
    }

    /// Classify content after a list marker on the same line.
    ///
    /// Handles fenced code, block math, ATX headings, nested list markers,
    /// blockquote markers, indented code, and paragraphs. Nested list and
    /// blockquote markers are detected recursively.
    #[allow(
        clippy::too_many_arguments,
        reason = "line context parameters are distinct concerns"
    )]
    fn classify_item_content(
        &mut self,
        lines: &[&str],
        pos: &mut usize,
        line_idx: &mut usize,
        body_offset: usize,
        item_start: usize,
        raw_start: usize,
        raw_len: usize,
        after: &str,
    ) {
        let (after_expanded, after_tab_maps) = expand_all_tabs(after);
        if let Some((fc, fl, fi)) = fenced_code_open(&after_expanded) {
            *pos += raw_len;
            *line_idx += 1;
            self.parse_fenced_code(
                lines,
                pos,
                line_idx,
                body_offset,
                item_start,
                raw_start + raw_len,
                fc,
                fl,
                fi.as_ref(),
            );
        } else if block_math_open(&after_expanded) {
            *pos += raw_len;
            *line_idx += 1;
            self.parse_block_math(
                lines,
                pos,
                line_idx,
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
            *pos += raw_len;
            *line_idx += 1;
        } else if let Some(inner_marker) = recognize_list_marker(&after_expanded) {
            // Nested list marker on the same line — recurse. Recursion depth
            // equals list nesting depth, so cap it to avoid stack overflow on
            // pathological `- - - - ...` input.
            if self.list_nesting_full() {
                self.note_list_limit(item_start);
                self.parse_paragraph(lines, pos, line_idx, body_offset, item_start, raw_len);
                return;
            }
            let inner_offset = expanded_to_raw(inner_marker.content_offset, after, &after_tab_maps);
            let inner_after = &after[inner_offset..];
            let inner_task = if inner_marker.ordered {
                None
            } else {
                recognize_task(inner_after)
            };
            self.open_list(&inner_marker, item_start, inner_task);
            let inner_start = item_start + inner_offset;
            if inner_after.trim().is_empty() {
                *pos += raw_len;
                *line_idx += 1;
            } else {
                self.classify_item_content(
                    lines,
                    pos,
                    line_idx,
                    body_offset,
                    inner_start,
                    raw_start,
                    raw_len,
                    inner_after,
                );
            }
        } else if let Some((ml, _)) = strip_blockquote_marker(&after_expanded) {
            // Blockquote inside the list item.
            if !self.try_open_quote(item_start) {
                // At the nesting cap — treat the `>` content as a paragraph.
                self.parse_paragraph(lines, pos, line_idx, body_offset, item_start, raw_len);
                return;
            }
            let bq_offset = expanded_to_raw(ml, after, &after_tab_maps);
            let bq_content = &after[bq_offset..];
            let bq_start = item_start + bq_offset;
            if bq_content.trim().is_empty() {
                *pos += raw_len;
                *line_idx += 1;
            } else {
                self.parse_paragraph(lines, pos, line_idx, body_offset, bq_start, raw_len);
            }
        } else if count_indent(&after_expanded) >= 4 {
            self.parse_indented_code(lines, pos, line_idx, body_offset, item_start, raw_len);
        } else {
            self.parse_paragraph(lines, pos, line_idx, body_offset, item_start, raw_len);
        }
    }

    /// Close the current list item, popping any intervening scopes
    /// (blockquotes, HTML containers) that were opened inside the item.
    fn close_list_item(&mut self, pos: usize) {
        if let Some(ctx) = self.list_stack.last() {
            let target = ctx.item_node;
            // Pop scopes above the list item. The `pop_scope` progress check
            // bounds the loop even if `target` is no longer on the stack.
            while self.scope_stack.last().is_some_and(|&top| top != target) {
                let top = *self.scope_stack.last().unwrap_or(&0);
                if matches!(
                    self.nodes[top].kind,
                    ElementKind::QuoteBlock | ElementKind::Admonition { .. }
                ) {
                    self.quote_depth = self.quote_depth.saturating_sub(1);
                }
                if self.html_stack.last().is_some_and(|hs| hs.node_id == top) {
                    self.html_stack.pop();
                }
                if !self.pop_scope(pos) {
                    break;
                }
            }
            // Pop the list item itself.
            self.pop_scope(pos);
        }
    }

    /// Close the current list: finalize tight/loose, pop scopes.
    fn close_list(&mut self, pos: usize) {
        if let Some(ctx) = self.list_stack.pop() {
            // Update the List node's tight flag.
            if let ElementKind::List { ref mut tight, .. } = self.nodes[ctx.list_node].kind {
                *tight = !ctx.loose;
            }
            // Pop any scopes between the current top and the List node. The
            // `pop_scope` progress check bounds the loop even if `list_node`
            // is no longer on the stack.
            while self
                .scope_stack
                .last()
                .is_some_and(|&top| top != ctx.list_node)
            {
                if !self.pop_scope(pos) {
                    break;
                }
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

    // -- HTML scope management -----------------------------------------------

    /// Push an HTML container scope onto both the scope stack and html stack.
    fn push_html_scope(&mut self, tag: &str, kind: ElementKind, span: Span) -> NodeId {
        let id = self.push_scope(kind, Syntax::Html, span);
        self.html_stack.push(HtmlScope {
            tag: tag.to_string(),
            node_id: id,
        });
        id
    }

    /// Handle an HTML closing tag. Returns `true` if the tag matched an
    /// open scope (including error recovery for mismatched nesting).
    fn handle_html_close_tag(&mut self, tag: &str, span_end: usize) -> bool {
        // Find the matching open tag in the html stack.
        let pos = self.html_stack.iter().rposition(|s| s.tag == tag);

        let Some(idx) = pos else {
            // No match — unexpected close tag.
            self.diagnostics.push(Diagnostic {
                level: DiagnosticLevel::Error,
                span: Span::new(span_end.saturating_sub(tag.len() + 3), span_end),
                message: format!("unexpected closing tag `</{tag}>`"),
            });
            return false;
        };

        // Close everything above the match (implicit close, flagged).
        let above = self.html_stack.split_off(idx + 1);
        for scope in above.iter().rev() {
            self.diagnostics.push(Diagnostic {
                level: DiagnosticLevel::Error,
                span: self.nodes[scope.node_id].span,
                message: format!("unclosed `<{}>` tag", scope.tag),
            });
            self.pop_scope(span_end);
        }

        // Pop the matched scope.
        self.html_stack.pop();
        self.pop_scope(span_end);
        true
    }

    /// Close all remaining HTML scopes (at end of document).
    fn close_all_html_scopes(&mut self, pos: usize) {
        while let Some(scope) = self.html_stack.pop() {
            self.diagnostics.push(Diagnostic {
                level: DiagnosticLevel::Error,
                span: self.nodes[scope.node_id].span,
                message: format!("unclosed `<{}>` tag", scope.tag),
            });
            self.pop_scope(pos);
        }
    }

    /// Try to handle a line as an HTML closing tag. Returns `true` if
    /// the line was consumed (matched or emitted a diagnostic).
    fn try_html_close_tag(&mut self, content: &str, content_start: usize, line_end: usize) -> bool {
        let trimmed = content.trim();
        if let Some(HtmlTag::Close { ref name, .. }) = html::tokenize_tag(trimmed, content_start) {
            // handle_html_close_tag returns true on match, false on
            // unexpected close (but still emits a diagnostic).
            if self.html_stack.is_empty() {
                self.diagnostics.push(Diagnostic {
                    level: DiagnosticLevel::Error,
                    span: Span::new(content_start, line_end),
                    message: format!("unexpected closing tag `</{name}>`"),
                });
                return true;
            }
            self.handle_html_close_tag(name, line_end);
            return true;
        }
        false
    }

    /// Handle an HTML opening tag on a type 6/7 block line.
    ///
    /// Returns `true` if the tag was handled as a mapped HTML element
    /// (container scope pushed or leaf added). Returns `false` if the
    /// tag has no structural mapping and should fall through to the
    /// opaque `HtmlBlock` path.
    #[allow(
        clippy::too_many_arguments,
        reason = "line context parameters are distinct concerns"
    )]
    fn handle_html_open(
        &mut self,
        lines: &[&str],
        pos: &mut usize,
        line_idx: &mut usize,
        body_offset: usize,
        content: &str,
        content_start: usize,
        first_raw_len: usize,
    ) -> bool {
        let trimmed = content.trim();

        // Try autolink first — not a structural tag.
        if html::try_autolink(trimmed).is_some() {
            return false;
        }

        let Some(tag) = html::tokenize_tag(trimmed, content_start) else {
            return false;
        };

        match tag {
            HtmlTag::Open {
                ref name,
                ref attrs,
                self_closing,
                len: tag_len,
            } => {
                let line_end = body_offset + *pos + first_raw_len;
                let span = Span::new(content_start, line_end);

                // <a> tags produce Link nodes (not structural containers).
                if name == "a" {
                    let (href, title) = html::extract_link_attrs(attrs);
                    *pos += first_raw_len;
                    *line_idx += 1;
                    if !has_close_on_same_line(trimmed, name, tag_len) {
                        self.consume_html_leaf(lines, pos, line_idx, name);
                    }
                    let full_span = Span::new(content_start, body_offset + *pos);
                    self.add_leaf(
                        ElementKind::Link { url: href, title },
                        Syntax::Html,
                        full_span,
                    );
                    return true;
                }

                let Some(mut kind) = html::tag_to_element_kind(name) else {
                    return false;
                };

                // Promote Container to Admonition if class matches.
                if matches!(kind, ElementKind::Container)
                    && let Some(adm) = html::extract_admonition_class(attrs)
                {
                    kind = ElementKind::Admonition { kind: adm };
                }

                // Void elements and self-closing: always leaf.
                if self_closing || html::VOID_ELEMENTS.contains(name.as_str()) {
                    // Special handling for <img> to extract src/title.
                    let leaf_kind = if name == "img" {
                        let (url, title) = html::extract_image_attrs(attrs);
                        ElementKind::Image { url, title }
                    } else {
                        kind
                    };
                    self.add_leaf(leaf_kind, Syntax::Html, span);
                    *pos += first_raw_len;
                    *line_idx += 1;
                    return true;
                }

                // Non-container leaf elements: <p>, <h1>-<h6>, media, <dt>, <dd>.
                if !html::is_html_container(name) {
                    *pos += first_raw_len;
                    *line_idx += 1;
                    if !has_close_on_same_line(trimmed, name, tag_len) {
                        self.consume_html_leaf(lines, pos, line_idx, name);
                    }
                    let full_span = Span::new(content_start, body_offset + *pos);
                    let leaf_kind = match kind {
                        ElementKind::Image { .. }
                        | ElementKind::Video { .. }
                        | ElementKind::Audio { .. } => {
                            let (url, title) = html::extract_image_attrs(attrs);
                            // Preserve the variant from tag_to_element_kind.
                            match &kind {
                                ElementKind::Video { .. } => ElementKind::Video { url, title },
                                ElementKind::Audio { .. } => ElementKind::Audio { url, title },
                                _ => ElementKind::Image { url, title },
                            }
                        }
                        _ => kind,
                    };
                    self.add_leaf(leaf_kind, Syntax::Html, full_span);
                    return true;
                }

                // Container element: push scope.
                *pos += first_raw_len;
                *line_idx += 1;

                // HTML container nesting cap. Nested containers are parsed
                // recursively (`consume_html_raw` -> `handle_html_open`), so
                // the cap bounds recursion depth and prevents stack overflow.
                // Beyond it, the tag is recorded as a flat leaf and its
                // content is not entered as a scope.
                if self.html_stack.len() >= crate::limits::MAX_HTML_NESTING {
                    self.note_html_limit(content_start);
                    self.add_leaf(kind, Syntax::Html, span);
                    return true;
                }

                self.push_html_scope(name, kind, span);

                // If the close tag is on the same line as the open tag
                // (e.g. `<summary>Title</summary>`), close immediately.
                if has_close_on_same_line(trimmed, name, tag_len) {
                    self.handle_html_close_tag(name, body_offset + *pos);
                    return true;
                }

                // When the next line is non-blank, process content in
                // HTML mode — dispatching nested HTML tags while treating
                // non-HTML lines as opaque content.
                let next_is_nonblank = *line_idx < lines.len() && {
                    let next = lines[*line_idx];
                    let content = self
                        .strip_continuation(next, body_offset + *pos)
                        .map_or(next, |(c, _)| c);
                    !content.trim().is_empty()
                };
                if next_is_nonblank {
                    self.consume_html_raw(lines, pos, line_idx, body_offset, name);
                }

                true
            }
            HtmlTag::Close { .. } | HtmlTag::Comment { .. } => false,
        }
    }

    /// Consume lines inside an HTML container scope.
    ///
    /// Dispatches nested HTML open/close tags while treating non-HTML
    /// lines as opaque content. Stops at a blank line (switching to
    /// markdown mode) or the matching close tag.
    fn consume_html_raw(
        &mut self,
        lines: &[&str],
        pos: &mut usize,
        line_idx: &mut usize,
        body_offset: usize,
        tag: &str,
    ) {
        while *line_idx < lines.len() {
            let line = lines[*line_idx];
            let inner_start = body_offset + *pos;

            // Strip continuation markers (quotes + list indent).
            let (content, content_start) = self
                .strip_continuation(line, inner_start)
                .unwrap_or((line, inner_start));

            if content.trim().is_empty() {
                // Blank line: switch to markdown mode (stop consuming raw).
                break;
            }

            let trimmed = content.trim();

            // 1. Check for the container's own close tag.
            if let Some(HtmlTag::Close { ref name, .. }) = html::tokenize_tag(trimmed, 0) {
                if name == tag {
                    *pos += line.len();
                    *line_idx += 1;
                    self.handle_html_close_tag(tag, body_offset + *pos);
                    return;
                }
                // Nested close tag — dispatch to close handler.
                *pos += line.len();
                *line_idx += 1;
                self.handle_html_close_tag(name, body_offset + *pos);
                continue;
            }

            // 2. Check for nested HTML open tags.
            let raw_len = line.len();
            if html_block_start(&content.trim_start().to_lowercase())
                .is_some_and(|ht| ht == 6 || ht == 7)
                && self.handle_html_open(
                    lines,
                    pos,
                    line_idx,
                    body_offset,
                    content,
                    content_start,
                    raw_len,
                )
            {
                continue;
            }

            // 3. Opaque content — skip line.
            *pos += line.len();
            *line_idx += 1;
        }
    }

    /// Consume lines until matching close tag for a leaf-level HTML element.
    fn consume_html_leaf(&self, lines: &[&str], pos: &mut usize, line_idx: &mut usize, tag: &str) {
        while *line_idx < lines.len() {
            let line = lines[*line_idx];
            let inner_start = *pos; // offset doesn't matter, only content

            // Strip continuation markers (quotes + list indent).
            let content = self
                .strip_continuation(line, inner_start)
                .map_or(line, |(c, _)| c);
            let trimmed = content.trim();

            *pos += line.len();
            *line_idx += 1;

            if let Some(HtmlTag::Close { ref name, .. }) = html::tokenize_tag(trimmed, 0)
                && name == tag
            {
                return;
            }

            if trimmed.is_empty() {
                return;
            }
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

        let (expanded, tab_mappings) = expand_all_tabs(line);
        let indent = count_indent(&expanded);

        while let Some(ctx) = self.list_stack.last() {
            // Raw content column: the absolute column in the original line
            // where this list item's content starts.
            let raw_cc = ctx.base_indent + ctx.content_column;

            // Case 1: line continues the current item (sufficient indent).
            if indent >= raw_cc {
                // Empty items followed by a blank line cannot continue —
                // a list item can begin with at most one blank line.
                let item_empty = self.nodes[ctx.item_node].children.is_empty();
                if ctx.saw_blank && item_empty {
                    self.close_list_item(line_start);
                    self.close_list(line_start);
                    continue;
                }
                // A blank within an item makes the list loose.
                if let Some(ctx) = self.list_stack.last_mut() {
                    if ctx.saw_blank {
                        ctx.loose = true;
                    }
                    ctx.saw_blank = false;
                }
                // Strip raw_cc worth of indent (tab-aware).
                let raw_offset = expanded_to_raw(raw_cc, line, &tab_mappings);
                let stripped = &line[raw_offset..];
                return (stripped, line_start + raw_offset);
            }

            // Case 2: new item in the same list.
            // A marker matches if it has the same type/character and its
            // raw indent falls within the list's marker level (base_indent + 0..=3).
            if let Some(marker) = recognize_list_marker(&expanded)
                && marker.ordered == ctx.ordered
                && marker.marker_char == ctx.marker_char
                && marker.marker_indent >= ctx.base_indent
                && marker.marker_indent <= ctx.base_indent + 3
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
            // Propagate blank flag to parent list so blank lines between
            // nested structures and continuation content are detected.
            let child_saw_blank = ctx.saw_blank;
            self.close_list_item(line_start);
            self.close_list(line_start);
            if child_saw_blank && let Some(parent) = self.list_stack.last_mut() {
                parent.saw_blank = true;
            }
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
                self.blank_before = true;
                pos += raw_len;
                line_idx += 1;
                continue;
            }

            // Handle block quote continuation and new block quote opening.
            let (content, content_start, new_quotes) =
                self.handle_quote_markers(raw_line, raw_start);

            // Detect GFM admonition on the first line of a new blockquote.
            if new_quotes > 0
                && let Some(kind) = detect_admonition(content)
            {
                let scope_id = self.current_scope();
                self.nodes[scope_id].kind = ElementKind::Admonition { kind };
            }

            // Blank content after marker stripping (e.g. `> \n`).
            if content.trim().is_empty() {
                // Mark list blank only when the list is inside the quotes
                // (the blank is at the list level). When a blockquote is
                // inside a list item, a blank at the blockquote level
                // should not affect the list's tight/loose state.
                let list_inside_quotes = self.list_stack.last().is_some_and(|ctx| {
                    self.scope_stack
                        .iter()
                        .position(|&id| id == ctx.item_node)
                        .is_some_and(|ip| {
                            self.scope_stack[..ip].iter().any(|&id| {
                                matches!(
                                    self.nodes[id].kind,
                                    ElementKind::QuoteBlock | ElementKind::Admonition { .. }
                                )
                            })
                        })
                });
                if list_inside_quotes || self.quote_depth == 0 {
                    self.mark_list_blank();
                    self.blank_before = true;
                }
                pos += raw_len;
                line_idx += 1;
                continue;
            }

            // Handle list continuation, new items, or list closure.
            // Skip when new blockquote scopes were opened — the blockquote
            // is inside the list item and its content is not at the list level.
            let (content, content_start) = if new_quotes > 0 {
                (content, content_start)
            } else {
                self.handle_list_continuation(content, content_start)
            };

            // A bare list marker (new item with no content) leaves nothing
            // to classify — just advance past the line.
            if content.trim().is_empty() && !self.list_stack.is_empty() {
                pos += raw_len;
                line_idx += 1;
                continue;
            }

            // Detect blockquote markers revealed after list indent stripping.
            // This handles blockquotes nested inside list items where the `>`
            // was hidden behind the list's content-column indentation.
            let (content, content_start) = {
                let mut c = content;
                let mut cs = content_start;
                while let Some((ml, inner)) = strip_blockquote_marker(c) {
                    if !self.try_open_quote(cs) {
                        // At the nesting cap — leave the remaining `>` as text.
                        break;
                    }
                    // Check for admonition on the first line of the new blockquote.
                    if let Some(kind) = detect_admonition(inner) {
                        let scope_id = self.current_scope();
                        self.nodes[scope_id].kind = ElementKind::Admonition { kind };
                    }
                    cs += ml;
                    c = inner;
                }
                (c, cs)
            };

            // Blank content after all stripping.
            if content.trim().is_empty() {
                pos += raw_len;
                line_idx += 1;
                continue;
            }

            // Classify the content. Use full tab expansion so list markers
            // with tabs after them (e.g. `-\t\tfoo`) are recognized.
            let (expanded, tab_mappings) = expand_all_tabs(content);
            let indent = count_indent(&expanded);
            let blank_before = self.blank_before;
            self.blank_before = false;

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
            } else if self.try_reference_defs(
                &lines,
                &mut pos,
                &mut line_idx,
                body_offset,
                content,
                content_start,
                raw_len,
            ) {
                // One or more reference definitions were consumed.
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
            } else if self.try_html_close_tag(content, content_start, raw_start + raw_len) {
                pos += raw_len;
                line_idx += 1;
            } else if let Some(html_type) = html_block_start(&expanded) {
                if matches!(html_type, 6 | 7)
                    && self.handle_html_open(
                        &lines,
                        &mut pos,
                        &mut line_idx,
                        body_offset,
                        content,
                        content_start,
                        raw_len,
                    )
                {
                    // Handled by HTML tag integration.
                } else if html_type == 1 && is_pre_code_open(content) {
                    self.parse_pre_code_block(
                        &lines,
                        &mut pos,
                        &mut line_idx,
                        body_offset,
                        content_start,
                        raw_len,
                        content,
                    );
                } else if html_type == 1
                    && content.trim_start().to_lowercase().starts_with("<textarea")
                {
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
                    // Upgrade the HtmlBlock that parse_html_block just added.
                    if let Some(&last_id) = self.nodes[self.current_scope()].children.last() {
                        self.nodes[last_id].kind = ElementKind::FormControl;
                        self.nodes[last_id].syntax = Syntax::Html;
                    }
                } else {
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
                }
            } else if is_thematic_break(expanded.trim_end()) {
                self.add_leaf(
                    ElementKind::Rules,
                    Syntax::Markdown,
                    Span::new(content_start, raw_start + raw_len),
                );
                pos += raw_len;
                line_idx += 1;
            } else if let Some(marker) = recognize_list_marker(&expanded)
                && !self.list_nesting_full()
            {
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
                    self.classify_item_content(
                        &lines,
                        &mut pos,
                        &mut line_idx,
                        body_offset,
                        item_start,
                        raw_start,
                        raw_len,
                        after,
                    );
                }
            } else if recognize_list_marker(&expanded).is_some() {
                // List marker present but the nesting cap is reached — emit a
                // single diagnostic and fall back to paragraph handling.
                self.note_list_limit(content_start);
                self.parse_paragraph(
                    &lines,
                    &mut pos,
                    &mut line_idx,
                    body_offset,
                    content_start,
                    raw_len,
                );
            } else if indent >= 4 && (!self.last_child_is_paragraph() || blank_before) {
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
        self.close_quote_levels(0, pos);
    }

    /// Close block quote scopes until `quote_depth` reaches `target_depth`.
    ///
    /// Each unmatched `QuoteBlock`/`Admonition` is closed along with every
    /// scope nested inside it — lists, list items, and HTML containers —
    /// keeping `list_stack`, `html_stack`, and `quote_depth` in sync with
    /// `scope_stack`. Unclosed HTML containers emit a diagnostic, matching
    /// the end-of-document cleanup.
    fn close_quote_levels(&mut self, target_depth: usize, pos: usize) {
        while self.quote_depth > target_depth {
            // Pop scopes from the top until (and including) the next
            // QuoteBlock. Scopes nested inside the quote — list items,
            // lists, HTML containers — are closed first. At the root Document
            // none of the bookkeeping below fires (it is neither quote, HTML,
            // nor list), so the `pop_scope` progress check is the sole, shared
            // termination guard.
            loop {
                let top = self.current_scope();
                let is_quote = matches!(
                    self.nodes[top].kind,
                    ElementKind::QuoteBlock | ElementKind::Admonition { .. }
                );
                if self.html_stack.last().is_some_and(|hs| hs.node_id == top) {
                    if let Some(scope) = self.html_stack.pop() {
                        self.diagnostics.push(Diagnostic {
                            level: DiagnosticLevel::Error,
                            span: self.nodes[scope.node_id].span,
                            message: format!("unclosed `<{}>` tag", scope.tag),
                        });
                    }
                } else if self
                    .list_stack
                    .last()
                    .is_some_and(|ctx| ctx.list_node == top)
                {
                    self.list_stack.pop();
                }
                if !self.pop_scope(pos) {
                    return; // reached the root Document
                }
                if is_quote {
                    self.quote_depth -= 1;
                    break;
                }
            }
        }
    }

    /// Handle block quote continuation and new block quote opening.
    ///
    /// 1. Strips continuation markers for existing open block quotes.
    /// 2. Closes scopes for any unmatched levels.
    /// 3. Opens new `QuoteBlock` scopes for additional `>` markers.
    ///
    /// Returns `(content, content_start, new_quotes)` after all markers
    /// are stripped, where `new_quotes` is the number of newly opened
    /// block quote scopes.
    fn handle_quote_markers<'b>(
        &mut self,
        line: &'b str,
        line_start: usize,
    ) -> (&'b str, usize, usize) {
        // Step 1: Strip continuation markers for existing depth, closing any
        // unmatched levels (and the lists/HTML nested inside them).
        let (matched, after_cont) = strip_n_quote_markers(line, self.quote_depth);
        self.close_quote_levels(matched, line_start);

        let marker_bytes = line.len() - after_cont.len();
        let mut content = after_cont;
        let mut content_start = line_start + marker_bytes;

        // Step 2: Open new block quote scopes for additional `>` markers.
        let mut new_quotes = 0;
        while let Some((ml, inner)) = strip_blockquote_marker(content) {
            if !self.try_open_quote(content_start) {
                // At the nesting cap — leave the remaining `>` as text.
                break;
            }
            new_quotes += 1;
            content_start += ml;
            content = inner;
        }

        (content, content_start, new_quotes)
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
            let (expanded, tab_mappings) = expand_leading_tabs(content);
            let indent = count_indent(&expanded);
            // After quote stripping, the remaining indent must reach the
            // list item's content column. For nested lists the stored
            // content_column is relative, so add base_indent for lines
            // that still carry parent indentation. However, if quotes
            // were stripped, the parent list indent was consumed with
            // them, so use content_column directly.
            let effective_cc = if self.quote_depth > 0 {
                ctx.content_column
            } else {
                ctx.base_indent + ctx.content_column
            };
            if indent < effective_cc && !content.trim().is_empty() {
                return None;
            }
            let raw_offset = expanded_to_raw(effective_cc, content, &tab_mappings);
            let stripped = &content[raw_offset..];
            Some((stripped, content_start + raw_offset))
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
                    level: DiagnosticLevel::Error,
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
                        level: DiagnosticLevel::Error,
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
                level: DiagnosticLevel::Error,
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

    /// Parse a `<pre><code>` block as a `CodeBlock` with `Syntax::Html`.
    ///
    /// Consumes lines until `</pre>` (same end condition as type 1).
    #[allow(
        clippy::too_many_arguments,
        reason = "line context parameters are distinct concerns"
    )]
    fn parse_pre_code_block(
        &mut self,
        lines: &[&str],
        pos: &mut usize,
        line_idx: &mut usize,
        body_offset: usize,
        block_start: usize,
        first_line_raw_len: usize,
        first_content: &str,
    ) {
        let end_on_first = html_block_end(first_content, 1);
        *pos += first_line_raw_len;
        *line_idx += 1;

        if !end_on_first {
            while *line_idx < lines.len() {
                let inner_line = lines[*line_idx];
                let inner_start = body_offset + *pos;

                let Some((content, _)) = self.strip_continuation(inner_line, inner_start) else {
                    break;
                };

                *pos += inner_line.len();
                *line_idx += 1;

                if html_block_end(content, 1) {
                    break;
                }
            }
        }

        self.add_leaf(
            ElementKind::CodeBlock,
            Syntax::Html,
            Span::new(block_start, body_offset + *pos),
        );
    }

    /// Try to parse one link reference definition starting at the current
    /// line, consuming continuation lines for a multi-line destination or
    /// title. Returns `true` if a definition was emitted (advancing `pos` and
    /// `line_idx` past it), `false` otherwise.
    ///
    /// Reference definitions are recognized only at the start of a block, so
    /// the contiguous run of non-blank continuation lines is the candidate
    /// "paragraph block". A definition is parsed off the front; any remaining
    /// lines are left for the main loop (they become a paragraph or another
    /// construct). Only one definition is consumed per call — stacked
    /// definitions are handled by re-entering the main loop.
    #[allow(
        clippy::too_many_arguments,
        reason = "ref def parameters are distinct concerns"
    )]
    fn try_reference_defs(
        &mut self,
        lines: &[&str],
        pos: &mut usize,
        line_idx: &mut usize,
        body_offset: usize,
        first_content: &str,
        first_content_start: usize,
        first_raw_len: usize,
    ) -> bool {
        // The look-ahead is capped: a reference definition spans only a few
        // lines (label, destination, and title, each of which may sit on its
        // own line). Without a cap, a long contiguous block of stacked
        // definitions — each parsed one line at a time — would re-collect the
        // whole tail on every line, which is quadratic. Only the first
        // definition is ever consumed; the extra lines are look-ahead to spot
        // a destination or title on a following line.
        const REFDEF_MAX_PROBE_LINES: usize = 32;

        // Cheap gate: bail before any allocation unless the first line could
        // open a reference-definition label. This filters ordinary bracketed
        // text (`[text][ref]`, `[link](url)`, shortcut refs) while still
        // admitting labels that continue onto a later line.
        if !first_line_opens_refdef(first_content) {
            return false;
        }

        // Collect the contiguous run of non-blank continuation lines, joining
        // their stripped content. Each entry is `(content_len, raw_len,
        // content_start)`.
        let mut run: Vec<(usize, usize, usize)> =
            vec![(first_content.len(), first_raw_len, first_content_start)];
        let mut text = String::from(first_content);
        let mut probe_pos = *pos + first_raw_len;
        let mut probe_idx = *line_idx + 1;
        while probe_idx < lines.len() && run.len() < REFDEF_MAX_PROBE_LINES {
            let raw = lines[probe_idx];
            let raw_start = body_offset + probe_pos;
            let Some((content, content_start)) = self.strip_continuation(raw, raw_start) else {
                break;
            };
            if content.trim().is_empty() {
                break;
            }
            text.push_str(content);
            run.push((content.len(), raw.len(), content_start));
            probe_pos += raw.len();
            probe_idx += 1;
        }

        let Some((consumed, label, url, title)) = scan_one_refdef(&text) else {
            return false;
        };

        // Map the consumed byte count to a whole number of run lines.
        let mut acc = 0usize;
        let mut consumed_lines = 0usize;
        while consumed_lines < run.len() && acc < consumed {
            acc += run[consumed_lines].0;
            consumed_lines += 1;
        }

        let span_start = run[0].2;
        let last = run[consumed_lines - 1];
        let span_end = last.2 + last.0;
        self.add_leaf(
            ElementKind::ReferenceDef { label, url, title },
            Syntax::Markdown,
            Span::new(span_start, span_end),
        );

        for &(_, raw_len, _) in &run[..consumed_lines] {
            *pos += raw_len;
        }
        *line_idx += consumed_lines;
        true
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
    #[allow(
        clippy::too_many_lines,
        reason = "continuation logic with lazy fallback and multiple break conditions"
    )]
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
        let header_end = line_content_end(self.source, para_start);
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

            // Strip continuation markers, with lazy fallback. `lazy` marks
            // a line that continues the paragraph without proper markers or
            // indentation (inside a block quote or list item) — such lines
            // cannot form a setext heading underline.
            let (content, lazy) =
                if let Some((c, _)) = self.strip_continuation(next_line, next_start) {
                    (c, false)
                } else {
                    // Lazy continuation: line without proper markers/indent
                    // that is not a block-starting construct can continue a
                    // paragraph inside a block quote or list item.
                    //
                    // Two paths: (1) the line has no markers at all — direct
                    // lazy continuation, (2) the line has partial quote
                    // markers (outer but not inner) — lazy continuation
                    // through partial stripping.
                    let (lazy_expanded, _) = expand_leading_tabs(next_line);
                    if (self.quote_depth > 0 || !self.list_stack.is_empty())
                        && strip_blockquote_marker(next_line).is_none()
                        && !is_thematic_break(next_line)
                        && atx_heading_level(next_line).is_none()
                        && fenced_code_open(next_line).is_none()
                        && html_block_start(next_line).is_none()
                        && recognize_list_marker(&lazy_expanded).is_none()
                    {
                        (next_line, true)
                    } else if self.quote_depth > 0 {
                        // Partial quote match: strip as many outer quote
                        // markers as possible and check if the remaining
                        // content can lazily continue.
                        let (matched, partial) = strip_n_quote_markers(next_line, self.quote_depth);
                        let (pe, _) = expand_leading_tabs(partial);
                        if matched > 0
                            && !partial.trim().is_empty()
                            && strip_blockquote_marker(partial).is_none()
                            && !is_thematic_break(partial)
                            && atx_heading_level(partial).is_none()
                            && fenced_code_open(partial).is_none()
                            && html_block_start(partial).is_none()
                            && recognize_list_marker(&pe).is_none()
                        {
                            (partial, true)
                        } else {
                            break;
                        }
                    } else {
                        break;
                    }
                };

            let (next_expanded, _) = expand_leading_tabs(content);

            // Blank line ends paragraph
            if next_expanded.trim().is_empty() {
                break;
            }

            // Setext heading underline. A lazy continuation line cannot be
            // a setext underline: CommonMark requires the underline to
            // belong to the same block as the paragraph it underlines, so a
            // `===`/`---` line lazily continuing a block quote or list
            // paragraph stays paragraph text (or, for `---`, falls through
            // to the thematic break check below).
            if !lazy && let Some(level) = setext_level(&next_expanded) {
                *pos += next_len;
                *line_idx += 1;

                self.add_leaf(
                    ElementKind::Heading { level },
                    Syntax::Markdown,
                    Span::new(para_start, body_offset + *pos),
                );
                return;
            }

            // Thematic break ends paragraph. For non-lazy lines only `***`
            // and `___` reach here (`---` was caught above as a setext
            // heading); on a lazy line `---`/`-----` reaches here too and
            // correctly terminates the paragraph.
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
            // interrupt, and empty list items cannot interrupt, per CommonMark)
            if let Some(marker) = recognize_list_marker(&next_expanded)
                && (!marker.ordered || marker.start == 1)
                && marker.content_offset < next_expanded.len()
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
        let header_end = line_content_end(self.source, header_start);
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
                level: DiagnosticLevel::Error,
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
/// Recognizes all three line-ending styles — `\n` (Unix), `\r\n` (Windows),
/// and bare `\r` (legacy Mac). Each returned slice includes its own trailing
/// line ending, so the concatenation of all slices reproduces `text` exactly
/// and byte offsets accumulated from slice lengths stay aligned with the
/// original source.
fn split_lines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();

    while start < bytes.len() {
        if let Some(offset) = bytes[start..]
            .iter()
            .position(|&b| b == b'\n' || b == b'\r')
        {
            let nl = start + offset;
            // Include the line ending: `\r\n` is two bytes, `\n` and bare
            // `\r` are one.
            let end = if bytes[nl] == b'\r' && bytes.get(nl + 1) == Some(&b'\n') {
                nl + 2
            } else {
                nl + 1
            };
            lines.push(&text[start..end]);
            start = end;
        } else {
            lines.push(&text[start..]);
            start = bytes.len();
        }
    }

    lines
}

/// Find the byte offset where the line beginning at `start` ends — the
/// position of the next line-ending byte (`\n` or `\r`), or `source.len()`
/// if the line runs to the end of input. Robust to all three line-ending
/// styles (the `\r` of a `\r\n` pair is reported, which is the line's true
/// content boundary).
fn line_content_end(source: &str, start: usize) -> usize {
    source[start..]
        .find(['\n', '\r'])
        .map_or(source.len(), |p| start + p)
}

/// The first line of `source`, with no trailing line ending.
///
/// Equivalent to `source.lines().next().unwrap_or("")` except it also breaks
/// on a bare `\r` (legacy-Mac line ending), which [`str::lines`] leaves
/// embedded in the line. Returns `""` for empty input.
#[must_use]
pub fn first_line(source: &str) -> &str {
    &source[..line_content_end(source, 0)]
}

/// Iterator over the content of each line in `source`, with the trailing line
/// ending removed. See [`content_lines`].
struct ContentLines<'a> {
    source: &'a str,
    pos: usize,
}

impl<'a> Iterator for ContentLines<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        if self.pos >= self.source.len() {
            return None;
        }
        let bytes = self.source.as_bytes();
        let end = line_content_end(self.source, self.pos);
        let content = &self.source[self.pos..end];
        self.pos = if end >= self.source.len() {
            end
        } else if bytes[end] == b'\r' && bytes.get(end + 1) == Some(&b'\n') {
            end + 2
        } else {
            end + 1
        };
        Some(content)
    }
}

/// Iterate the content of each line in `source` (no trailing line ending).
///
/// Like [`str::lines`] — a trailing line ending does not yield a final empty
/// line, and `""` yields nothing — but also splits on a bare `\r` (legacy-Mac
/// line ending). Line boundaries match the parser's own line counting, so an
/// index into this iterator aligns with a 0-based parser line number.
///
/// The returned iterator is `#[must_use]` via the `Iterator` trait.
pub fn content_lines(source: &str) -> impl Iterator<Item = &str> {
    ContentLines { source, pos: 0 }
}

// ---------------------------------------------------------------------------
// Consumer helpers
// ---------------------------------------------------------------------------

/// Normalize a path by resolving `.` and `..` components without touching
/// the filesystem.
pub fn normalize_path(path: &Path) -> PathBuf {
    let mut parts: Vec<Component<'_>> = Vec::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(parts.last(), Some(Component::Normal(_))) {
                    parts.pop();
                } else {
                    parts.push(c);
                }
            }
            _ => parts.push(c),
        }
    }
    parts.iter().collect()
}

/// Check whether a URL is external (http/https/mailto).
fn is_external(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://") || url.starts_with("mailto:")
}

/// Split a URL into path and optional fragment.
fn split_url_fragment(url: &str) -> (&str, Option<String>) {
    match url.split_once('#') {
        Some((path, frag)) => (path, Some(frag.to_string())),
        None => (url, None),
    }
}

/// Check whether a path has a `.md` extension.
fn is_markdown_ext(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "md")
}

/// Video file extensions.
static VIDEO_EXTENSIONS: phf::Set<&str> = phf::phf_set! {
    "mp4", "webm", "ogv", "mov", "avi", "mkv",
};

/// Audio file extensions.
static AUDIO_EXTENSIONS: phf::Set<&str> = phf::phf_set! {
    "mp3", "wav", "ogg", "flac", "aac", "m4a", "opus",
};

/// Classify an image URL into `Image`, `Video`, or `Audio` based on
/// file extension. Falls back to `Image` for unknown extensions.
pub fn classify_media(url: String, title: String) -> ElementKind {
    let path = url.split(['?', '#']).next().unwrap_or(&url);
    if let Some(ext) = path.rsplit('.').next() {
        let ext_lower = ext.to_lowercase();
        if VIDEO_EXTENSIONS.contains(ext_lower.as_str()) {
            return ElementKind::Video { url, title };
        }
        if AUDIO_EXTENSIONS.contains(ext_lower.as_str()) {
            return ElementKind::Audio { url, title };
        }
    }
    ElementKind::Image { url, title }
}

/// Classify a raw link URL and title into a [`Link`].
fn classify_link(
    url: &str,
    title: &str,
    file_path: &Path,
    line: usize,
    span: Span,
) -> Option<Link> {
    if url.is_empty() {
        return None;
    }

    let kind = if is_external(url) {
        LinkKind::External {
            url: url.to_string(),
        }
    } else if let Some(fragment) = url.strip_prefix('#') {
        LinkKind::IntraDocument {
            fragment: fragment.to_string(),
        }
    } else {
        let (path_str, fragment) = split_url_fragment(url);
        let parent = file_path.parent().unwrap_or_else(|| Path::new(""));
        let target = normalize_path(&parent.join(path_str));

        if is_markdown_ext(&target) {
            let explicit_predicate = !title.is_empty();
            let predicate = if explicit_predicate {
                title.to_string()
            } else {
                "references".to_string()
            };
            LinkKind::IntraProject {
                target,
                fragment,
                predicate,
                explicit_predicate,
            }
        } else {
            LinkKind::NonMarkdown { target }
        }
    };

    Some(Link { line, span, kind })
}

/// Classify an import directive path into a [`Link`].
fn classify_import(path: &str, file_path: &Path, line: usize, span: Span) -> Link {
    let parent = file_path.parent().unwrap_or_else(|| Path::new(""));
    let target = normalize_path(&parent.join(path));
    let kind = if is_markdown_ext(&target) {
        LinkKind::IntraProject {
            target,
            fragment: None,
            predicate: "imports".to_string(),
            explicit_predicate: true,
        }
    } else {
        LinkKind::NonMarkdown { target }
    };
    Link { line, span, kind }
}

// --- Slug algorithms ---

/// GitHub heading slug ([github-slugger] compatible).
fn github_slug(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-' || *c == ' ')
        .map(|c| if c == ' ' { '-' } else { c })
        .collect()
}

/// GitLab heading slug.
fn gitlab_slug(text: &str) -> String {
    let raw: String = text
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-' || *c == ' ')
        .map(|c| if c == ' ' { '-' } else { c })
        .collect();

    collapse_hyphens(&raw).trim_matches('-').to_string()
}

/// VS Code heading slug.
fn vscode_slug(text: &str) -> String {
    let raw: String = text
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_whitespace() { '-' } else { c })
        .filter(|c| !is_vscode_punctuation(*c))
        .collect();

    raw.trim_matches('-').to_string()
}

fn collapse_hyphens(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut prev_hyphen = false;
    for c in s.chars() {
        if c == '-' {
            if !prev_hyphen {
                result.push(c);
            }
            prev_hyphen = true;
        } else {
            result.push(c);
            prev_hyphen = false;
        }
    }
    result
}

const fn is_vscode_punctuation(c: char) -> bool {
    matches!(
        c,
        '[' | ']'
            | '!'
            | '"'
            | '#'
            | '$'
            | '%'
            | '&'
            | '\''
            | '('
            | ')'
            | '*'
            | '+'
            | ','
            | '.'
            | '/'
            | ':'
            | ';'
            | '<'
            | '='
            | '>'
            | '?'
            | '@'
            | '\\'
            | '^'
            | '{'
            | '|'
            | '}'
            | '~'
            | '`'
    )
}

/// Tracks slug occurrences across a document for deduplication.
struct SlugCounts {
    github: HashMap<String, usize>,
    gitlab: HashMap<String, usize>,
    vscode: HashMap<String, usize>,
}

impl SlugCounts {
    fn new() -> Self {
        Self {
            github: HashMap::new(),
            gitlab: HashMap::new(),
            vscode: HashMap::new(),
        }
    }

    fn next_github(&mut self, text: &str) -> String {
        deduplicate(github_slug(text), &mut self.github)
    }

    fn next_gitlab(&mut self, text: &str) -> String {
        deduplicate(gitlab_slug(text), &mut self.gitlab)
    }

    fn next_vscode(&mut self, text: &str) -> String {
        deduplicate(vscode_slug(text), &mut self.vscode)
    }
}

/// Deduplicate a slug by appending `-1`, `-2`, etc. on collision.
fn deduplicate(base: String, slugs: &mut HashMap<String, usize>) -> String {
    let original = base.clone();
    let mut slug = base;
    while slugs.contains_key(&slug) {
        let count = slugs.entry(original.clone()).or_insert(0);
        *count += 1;
        slug = format!("{original}-{count}");
    }
    slugs.insert(slug.clone(), 0);
    slug
}

// --- Bare path detection ---

const BARE_PATH_EXTENSIONS: &[&str] = &[".md", ".png", ".jpg", ".svg", ".pdf"];

/// File extensions recognized in `@path` import directives.
const IMPORT_EXTENSIONS: &[&str] = &[".json", ".md", ".toml", ".txt", ".xml", ".yaml", ".yml"];

/// Check whether a string looks like a bare file path.
fn is_bare_path(s: &str) -> bool {
    !is_import_directive(s)
        && s.contains('/')
        && BARE_PATH_EXTENSIONS.iter().any(|ext| s.ends_with(ext))
}

/// Check whether a string is an `@path` import directive.
fn is_import_directive(s: &str) -> bool {
    let Some(path) = s.strip_prefix('@') else {
        return false;
    };
    is_import_path(path)
}

/// Check whether a path (after stripping `@`) looks like a relative import.
fn is_import_path(path: &str) -> bool {
    if path.starts_with('/') || path.starts_with('~') || path.is_empty() {
        return false;
    }
    IMPORT_EXTENSIONS.iter().any(|ext| path.ends_with(ext))
}

/// Scan a text segment for bare file paths.
fn scan_bare_paths_in_text(text: &str, base_line: usize, out: &mut Vec<BarePath>) {
    for (line_idx, line_text) in text.split('\n').enumerate() {
        for word in line_text.split_whitespace() {
            let cleaned = word
                .trim_start_matches(['(', '[', '"', '\''])
                .trim_end_matches([',', '.', ';', ':', '!', '?', ')', ']', '"', '\'']);

            if is_bare_path(cleaned) {
                out.push(BarePath {
                    line: base_line + line_idx,
                    path: cleaned.to_string(),
                });
            }
        }
    }
}

// --- Text helpers ---

/// Convert a byte offset to a 1-based line number.
///
/// Recognizes `\n`, `\r\n`, and bare `\r` line endings (delegates to the
/// crate-wide counter in [`crate::fm`]).
pub fn byte_offset_to_line(content: &str, offset: usize) -> usize {
    crate::fm::byte_offset_to_line(content, offset)
}

/// Strip backtick-delimited code spans from text, keeping inner content.
fn strip_code_spans(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut result = String::with_capacity(text.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'`' {
            let tick_count = bytes[i..].iter().take_while(|&&b| b == b'`').count();
            if let Some(end) = find_code_span_close(bytes, i + tick_count, tick_count) {
                let inner = &text[i + tick_count..end];
                // CommonMark: strip one leading and one trailing space if both present
                // and content is not all spaces.
                let stripped = if inner.len() >= 2
                    && inner.starts_with(' ')
                    && inner.ends_with(' ')
                    && inner.trim().len() < inner.len()
                {
                    &inner[1..inner.len() - 1]
                } else {
                    inner
                };
                result.push_str(stripped);
                i = end + tick_count;
            } else {
                for _ in 0..tick_count {
                    result.push('`');
                }
                i += tick_count;
            }
        } else {
            let ch = text[i..].chars().next().unwrap_or(' ');
            result.push(ch);
            i += ch.len_utf8();
        }
    }

    result
}

/// Find closing backticks of exactly `count` length.
fn find_code_span_close(bytes: &[u8], start: usize, count: usize) -> Option<usize> {
    let mut i = start;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            let n = bytes[i..].iter().take_while(|&&b| b == b'`').count();
            if n == count {
                return Some(i);
            }
            i += n;
        } else {
            i += 1;
        }
    }
    None
}

/// Compute the byte span of the text content inside an HTML heading tag.
///
/// Given `<h1>text</h1>` and its `base` offset in the source, returns the
/// span covering `text`.
fn html_heading_text_span(raw: &str, base: usize) -> Span {
    let start = raw.find('>').map_or(0, |i| i + 1);
    let end = raw.rfind("</").unwrap_or(raw.len());
    Span::new(base + start, base + end)
}

/// Extract display text from an HTML heading like `<h1>text</h1>`.
pub fn extract_html_heading_text(source: &str) -> String {
    // Strip the opening tag
    let after_open = source.find('>').map_or(source, |i| &source[i + 1..]);
    // Strip the closing tag
    let before_close = after_open
        .rfind("</")
        .map_or(after_open, |i| &after_open[..i]);
    // Join lines and trim
    before_close
        .lines()
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

// ---------------------------------------------------------------------------
// Tree accessors
// ---------------------------------------------------------------------------

impl Tree {
    /// Extract links from the tree, classified relative to `file_path`.
    ///
    /// `file_path` is the workspace-relative path of the file, used to
    /// resolve relative link targets.
    #[must_use]
    pub fn links(&self, file_path: &Path) -> Vec<Link> {
        let mut links = Vec::new();

        for node in &self.nodes {
            match &node.kind {
                ElementKind::Link { url, title } => {
                    let line = byte_offset_to_line(&self.source, node.span.start);
                    if let Some(link) = classify_link(url, title, file_path, line, node.span) {
                        links.push(link);
                    }
                }
                ElementKind::Import { path } => {
                    let line = byte_offset_to_line(&self.source, node.span.start);
                    links.push(classify_import(path, file_path, line, node.span));
                }
                _ => {}
            }
        }

        links
    }

    /// Extract headings with computed slugs.
    #[must_use]
    pub fn headings(&self) -> Vec<Heading> {
        let mut slugs = SlugCounts::new();
        let mut headings = Vec::new();

        for (id, node) in self.nodes.iter().enumerate() {
            let ElementKind::Heading { level } = &node.kind else {
                continue;
            };

            let line = byte_offset_to_line(&self.source, node.span.start);
            let (text, explicit_id, text_span) = self.heading_content(id);
            let level = *level;
            let syntax = node.syntax;

            let heading_id = explicit_id.map_or_else(
                || HeadingId::Computed {
                    github: slugs.next_github(&text),
                    gitlab: slugs.next_gitlab(&text),
                    vscode: slugs.next_vscode(&text),
                },
                HeadingId::Explicit,
            );

            headings.push(Heading {
                line,
                level,
                text,
                id: heading_id,
                text_span,
                syntax,
            });
        }

        headings
    }

    /// Scan paragraphs for bare file paths.
    #[must_use]
    pub fn bare_paths(&self) -> Vec<BarePath> {
        let mut bare_paths = Vec::new();

        for (id, node) in self.nodes.iter().enumerate() {
            if !matches!(node.kind, ElementKind::Paragraph) {
                continue;
            }
            self.scan_bare_paths_in_node(id, &mut bare_paths);
        }

        bare_paths
    }

    /// Extract heading display text, optional explicit ID, and text byte span.
    pub fn heading_content(&self, node_id: NodeId) -> (String, Option<String>, Span) {
        let node = &self.nodes[node_id];
        let raw = &self.source[node.span.start..node.span.end];

        if node.syntax == Syntax::Html {
            let text = extract_html_heading_text(raw);
            let clean = strip_code_spans(&text);
            let text_span = html_heading_text_span(raw, node.span.start);
            return (clean, None, text_span);
        }

        // Check if ATX (starts with '#') or setext
        let trimmed = raw.trim_start();
        if trimmed.starts_with('#') {
            let first_line = raw.lines().next().unwrap_or("");
            let (content_span, atx_id) = extract_atx_content(first_line, node.span.start);
            let content = &self.source[content_span.start..content_span.end];
            let clean = strip_code_spans(content);
            (clean.trim().to_string(), atx_id.map(|a| a.id), content_span)
        } else {
            // Setext: text is all lines except the last (underline).
            // Find the underline line by trimming trailing whitespace and
            // splitting at the last newline.
            let trimmed_raw = raw.trim_end();
            let underline_start = trimmed_raw.rfind('\n').map_or(0, |i| i + 1);
            let text_raw = &trimmed_raw[..underline_start].trim_end_matches('\n');
            let leading = raw.len() - raw.trim_start().len();
            let text_end = leading + text_raw.trim_start().len();
            let text_span = Span::new(node.span.start + leading, node.span.start + text_end);
            let lines: Vec<&str> = raw.lines().collect();
            let joined = lines[..lines.len().saturating_sub(1).max(1)].join(" ");
            let clean = strip_code_spans(&joined);
            (clean.trim().to_string(), None, text_span)
        }
    }

    /// Scan a paragraph node for bare paths, excluding inline children.
    fn scan_bare_paths_in_node(&self, node_id: NodeId, out: &mut Vec<BarePath>) {
        let node = &self.nodes[node_id];

        // Collect child spans (inline elements to exclude)
        let mut excluded: Vec<Span> = node
            .children
            .iter()
            .map(|&child| self.nodes[child].span)
            .collect();
        excluded.sort_by_key(|s| s.start);

        let mut pos = node.span.start;

        for exclude in &excluded {
            if pos < exclude.start {
                let segment = &self.source[pos..exclude.start];
                let base_line = byte_offset_to_line(&self.source, pos);
                scan_bare_paths_in_text(segment, base_line, out);
            }
            pos = exclude.end;
        }

        // Text after last child
        if pos < node.span.end {
            let segment = &self.source[pos..node.span.end];
            let base_line = byte_offset_to_line(&self.source, pos);
            scan_bare_paths_in_text(segment, base_line, out);
        }
    }
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

    #[test]
    fn list_continuation_multibyte_whitespace_wellformed() {
        // Regression (fuzz_parse_tree / fuzz_inlines, ticket 22): an
        // all-whitespace list-continuation line containing a multi-byte
        // whitespace char (e.g. U+00A0 NBSP, U+2001 EM QUAD — both counted as
        // whitespace by `str::trim`, so the early-return guard is bypassed)
        // made `expanded_to_raw` return a byte offset inside the char, panicking
        // when the indentation was sliced off. Column->byte mapping must land on
        // a char boundary.
        for src in [
            "1. x\n  \u{a0}\n",   // ordered marker, content column 3, NBSP
            "-  x\n  \u{a0}\n",   // wide bullet, NBSP straddles the slice point
            "1. x\n  \u{2001}\n", // 3-byte multi-byte whitespace
            "- x\n\t\u{a0}\n",    // tab + NBSP in the continuation indent
        ] {
            crate::invariants::assert_tree_wellformed(&parse_tree(src, None));
        }
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

    // --- Line splitting (encoding edge cases, ticket 21) ---

    #[test]
    fn split_lines_unix() {
        assert_eq!(
            split_lines("a\nb\nc"),
            vec!["a\n", "b\n", "c"],
            "LF lines retain their trailing newline; last line has none"
        );
        assert_eq!(
            split_lines("a\nb\n"),
            vec!["a\n", "b\n"],
            "a trailing LF does not produce an empty final line"
        );
    }

    #[test]
    fn split_lines_crlf() {
        assert_eq!(
            split_lines("a\r\nb\r\n"),
            vec!["a\r\n", "b\r\n"],
            "CRLF is kept whole in each slice"
        );
    }

    #[test]
    fn split_lines_bare_cr() {
        assert_eq!(
            split_lines("a\rb\rc"),
            vec!["a\r", "b\r", "c"],
            "bare CR (legacy Mac) is recognized as a line break"
        );
    }

    #[test]
    fn split_lines_mixed_endings() {
        assert_eq!(
            split_lines("a\nb\r\nc\rd"),
            vec!["a\n", "b\r\n", "c\r", "d"],
            "LF, CRLF, and bare CR coexist in one document"
        );
    }

    #[test]
    fn split_lines_reconstructs_source() {
        for src in [
            "a\nb\r\nc\rd",
            "\r\n\n\r",
            "no endings",
            "trailing\r\n",
            "中\r日\n本\r\n",
        ] {
            let joined: String = split_lines(src).concat();
            assert_eq!(
                joined, src,
                "concatenating the slices must reproduce the source exactly: {src:?}"
            );
        }
    }

    #[test]
    fn line_content_end_all_endings() {
        assert_eq!(line_content_end("ab\ncd", 0), 2, "stops at the LF byte");
        assert_eq!(
            line_content_end("ab\r\ncd", 0),
            2,
            "stops at the CR of a CRLF pair (the content boundary)"
        );
        assert_eq!(line_content_end("ab\rcd", 0), 2, "stops at a bare CR");
        assert_eq!(
            line_content_end("abcd", 0),
            4,
            "runs to end of input when there is no line ending"
        );
    }

    #[test]
    fn first_line_breaks_on_all_endings() {
        assert_eq!(first_line("ab\ncd"), "ab", "breaks on LF");
        assert_eq!(first_line("ab\r\ncd"), "ab", "breaks on CRLF");
        assert_eq!(first_line("ab\rcd"), "ab", "breaks on bare CR");
        assert_eq!(first_line("ab"), "ab", "whole string when no ending");
        assert_eq!(first_line(""), "", "empty input yields empty first line");
    }

    #[test]
    fn content_lines_matches_str_lines_plus_bare_cr() {
        fn collect(s: &str) -> Vec<&str> {
            content_lines(s).collect()
        }

        // Matches `str::lines` for the common cases.
        assert_eq!(collect(""), Vec::<&str>::new(), "empty yields no lines");
        assert_eq!(collect("a"), vec!["a"], "single line, no ending");
        assert_eq!(
            collect("a\n"),
            vec!["a"],
            "trailing LF yields no empty line"
        );
        assert_eq!(collect("a\nb"), vec!["a", "b"], "LF separates lines");
        assert_eq!(collect("a\n\n"), vec!["a", ""], "interior blank line kept");
        assert_eq!(collect("a\r\nb"), vec!["a", "b"], "CRLF separates lines");

        // Unlike `str::lines`, a bare CR also splits.
        assert_eq!(
            collect("a\rb\rc"),
            vec!["a", "b", "c"],
            "bare CR separates lines (str::lines would not)"
        );
        assert_eq!(
            collect("a\r"),
            vec!["a"],
            "trailing bare CR yields no empty line"
        );
    }

    #[test]
    fn bare_cr_splits_block_structure() {
        // Three ATX headings separated only by bare CRs must be recognized
        // as three separate headings, not one run-on line.
        let tree = parse("# A\r# B\r# C");
        let headings = tree.headings();
        assert_eq!(
            headings.len(),
            3,
            "bare CR must separate the three headings, got {}",
            headings.len()
        );
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

    // --- Admonitions ---

    #[test]
    fn gfm_admonition_warning() {
        let source = "> [!WARNING]\n> Be careful!\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert_kind(
            &tree,
            children[0],
            &ElementKind::Admonition {
                kind: "WARNING".to_string(),
            },
        );
    }

    #[test]
    fn gfm_admonition_note() {
        let source = "> [!NOTE]\n> Some note text\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert_kind(
            &tree,
            children[0],
            &ElementKind::Admonition {
                kind: "NOTE".to_string(),
            },
        );
    }

    #[test]
    fn gfm_admonition_case_insensitive() {
        let source = "> [!tip]\n> Some tip\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert_kind(
            &tree,
            children[0],
            &ElementKind::Admonition {
                kind: "TIP".to_string(),
            },
        );
    }

    #[test]
    fn plain_blockquote_not_admonition() {
        let source = "> Just a quote\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one block");
        assert_kind(&tree, children[0], &ElementKind::QuoteBlock);
    }

    #[test]
    fn admonition_has_paragraph_children() {
        let source = "> [!WARNING]\n> Be careful!\n";
        let tree = parse(source);
        let children = root_children(&tree);
        let adm_children = tree.children(children[0]);

        assert!(
            adm_children
                .iter()
                .any(|&c| matches!(tree.node(c).kind, ElementKind::Paragraph)),
            "admonition should contain paragraph children"
        );
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
        let source = "<div>\ncontent\n</div>\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 1, "should find one container");
        let node = assert_kind(&tree, children[0], &ElementKind::Container);
        assert_eq!(node.syntax, Syntax::Html, "syntax is Html");
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
    fn blank_within_item_makes_loose() {
        let source = "- a\n\n  b\n- c\n";
        let tree = parse(source);
        let children = root_children(&tree);
        let list = tree.node(children[0]);

        // Per CommonMark, a blank line within any list item makes
        // the entire list loose — all items get paragraph wrappers.
        assert!(
            matches!(list.kind, ElementKind::List { tight: false, .. }),
            "blank within item makes list loose"
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

    #[test]
    fn blockquote_list_closed_by_lazy_list_marker() {
        // `> - foo` opens a list inside a block quote; the unmarked `- bar`
        // cannot lazily continue (a list marker is a block construct), so the
        // quote and its list close and a new top-level list begins.
        //
        // Regression: closing the quote must keep `list_stack` in sync with
        // `scope_stack` so the subsequent item transition does not spin
        // popping a list item that was already removed.
        let source = "> - foo\n- bar\n";
        let tree = parse(source);
        let children = root_children(&tree);

        assert_eq!(children.len(), 2, "block quote then a top-level list");
        assert!(
            matches!(tree.node(children[0]).kind, ElementKind::QuoteBlock),
            "first child is the block quote"
        );
        assert!(
            matches!(tree.node(children[1]).kind, ElementKind::List { .. }),
            "second child is a new top-level list"
        );
        // The quoted list is nested inside the block quote, not the top list.
        let quoted_lists = tree
            .children(children[0])
            .iter()
            .filter(|&&id| matches!(tree.node(id).kind, ElementKind::List { .. }))
            .count();
        assert_eq!(quoted_lists, 1, "one list nested in the block quote");
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

    // =======================================================================
    // HTML tag integration
    // =======================================================================

    // --- Equivalence: same ElementKind for markdown and HTML syntax ---

    #[test]
    fn html_blockquote_same_kind_as_markdown() {
        let md = parse("> quoted\n");
        let html = parse("<blockquote>\n\nquoted\n\n</blockquote>\n");

        let md_kind = &md.node(root_children(&md)[0]).kind;
        let html_kind = &html.node(root_children(&html)[0]).kind;
        assert_eq!(md_kind, html_kind, "both produce QuoteBlock");
    }

    #[test]
    fn html_heading_same_kind_as_markdown() {
        let md = parse("# Heading\n");
        let html = parse("<h1>Heading</h1>\n");

        let md_kind = &md.node(root_children(&md)[0]).kind;
        let html_kind = &html.node(root_children(&html)[0]).kind;
        assert_eq!(md_kind, html_kind, "both produce Heading level 1");
    }

    #[test]
    fn html_hr_same_kind_as_markdown() {
        let md = parse("---\n");
        let html = parse("<hr>\n");

        let md_kind = &md.node(root_children(&md)[0]).kind;
        let html_kind = &html.node(root_children(&html)[0]).kind;
        assert_eq!(md_kind, html_kind, "both produce Rules");
    }

    // --- HTML syntax produces Syntax::Html ---

    #[test]
    fn html_blockquote_has_html_syntax() {
        let tree = parse("<blockquote>\n\nquoted\n\n</blockquote>\n");
        let children = root_children(&tree);
        let node = tree.node(children[0]);
        assert_eq!(node.syntax, Syntax::Html, "HTML blockquote has Html syntax");
        assert_eq!(node.kind, ElementKind::QuoteBlock, "kind is QuoteBlock");
    }

    #[test]
    fn html_heading_has_html_syntax() {
        let tree = parse("<h1>Heading</h1>\n");
        let children = root_children(&tree);
        let node = tree.node(children[0]);
        assert_eq!(node.syntax, Syntax::Html, "HTML heading has Html syntax");
        assert_eq!(
            node.kind,
            ElementKind::Heading { level: 1 },
            "kind is Heading level 1"
        );
    }

    #[test]
    fn html_h2_through_h6() {
        for level in 2..=6u8 {
            let source = format!("<h{level}>text</h{level}>\n");
            let tree = parse(&source);
            let children = root_children(&tree);
            assert_eq!(children.len(), 1, "h{level} produces one node");
            assert_kind(&tree, children[0], &ElementKind::Heading { level });
        }
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

    #[test]
    fn table_cell_double_backtick_wraps_longer_run() {
        // A `` span containing a longer ``` run closes at the next `` — the
        // inner triple-backtick run is literal content. The `|` delimiters
        // outside the span must still split all three cells. Regression for the
        // splitter matching the first N backticks of a longer run and merging
        // the trailing cells.
        let source = "| A | B | C |\n|---|---|---|\n| Code block | `` ``` `` | `Object` |\n";
        let tree = parse(source);
        let children = root_children(&tree);
        let rows = tree.children(children[0]);

        let body_cells = tree.children(rows[1]);
        assert_eq!(
            body_cells.len(),
            3,
            "double-backtick span wrapping a longer run must not swallow pipes"
        );
        assert_eq!(
            tree.text(&tree.node(body_cells[1]).span),
            "`` ``` ``",
            "middle cell is the full code span, not merged with the next cell"
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
    fn html_heading_multiline_span() {
        let source = "<h2>\nHeading Text\n</h2>\n";
        let tree = parse(source);
        let children = root_children(&tree);
        assert_eq!(children.len(), 1, "one heading");
        let node = assert_kind(&tree, children[0], &ElementKind::Heading { level: 2 });
        assert_eq!(
            node.span,
            Span::new(0, source.len()),
            "span covers opening through closing tag"
        );
    }

    #[test]
    fn html_hr_has_html_syntax() {
        let tree = parse("<hr>\n");
        let children = root_children(&tree);
        let node = tree.node(children[0]);
        assert_eq!(node.syntax, Syntax::Html, "HTML hr has Html syntax");
        assert_eq!(node.kind, ElementKind::Rules, "kind is Rules");
    }

    #[test]
    fn html_hr_self_closing() {
        let tree = parse("<hr/>\n");
        let children = root_children(&tree);
        assert_eq!(children.len(), 1, "one node");
        assert_kind(&tree, children[0], &ElementKind::Rules);
    }

    // --- Void elements ---

    #[test]
    fn void_element_never_pushed_to_scope() {
        let tree = parse("<hr>\n<br>\n");
        let children = root_children(&tree);
        // Void elements are leaves, not containers.
        assert_eq!(children.len(), 2, "two void element leaves");
        assert_kind(&tree, children[0], &ElementKind::Rules);
        // <br> has no structural mapping so falls through to HtmlBlock.
    }

    #[test]
    fn img_void_element() {
        let tree = parse("<img src=\"photo.jpg\" />\n");
        let children = root_children(&tree);
        assert_eq!(children.len(), 1, "one image node");
        let node = tree.node(children[0]);
        assert_eq!(node.syntax, Syntax::Html, "Html syntax");
        assert!(
            matches!(node.kind, ElementKind::Image { .. }),
            "kind is Image"
        );
    }

    // --- Container scoping ---

    #[test]
    fn details_container_scope() {
        let tree = parse("<details>\n\ncontent\n\n</details>\n");
        let children = root_children(&tree);
        assert_eq!(children.len(), 1, "one details container");
        assert_kind(&tree, children[0], &ElementKind::Details);
        let inner = tree.children(children[0]);
        assert!(
            !inner.is_empty(),
            "details has children (content parsed as markdown)"
        );
    }

    #[test]
    fn nested_html_containers() {
        let source = "<div>\n\n<blockquote>\n\ntext\n\n</blockquote>\n\n</div>\n";
        let tree = parse(source);
        let children = root_children(&tree);
        assert_eq!(children.len(), 1, "one div container");
        assert_kind(&tree, children[0], &ElementKind::Container);
    }

    // --- HTML inside block quotes ---

    #[test]
    fn html_container_inside_blockquote() {
        let source = "> <div>\n> content\n> </div>\n";
        let tree = parse(source);
        let children = root_children(&tree);
        assert_eq!(children.len(), 1, "one block quote");
        assert_kind(&tree, children[0], &ElementKind::QuoteBlock);
        // The div container should be a child of the block quote.
        let quote_children = tree.children(children[0]);
        assert!(
            quote_children
                .iter()
                .any(|&id| tree.node(id).kind == ElementKind::Container),
            "div container inside block quote: {quote_children:?}"
        );
        // The container should be properly closed (no unclosed diagnostic).
        assert!(
            !tree
                .diagnostics()
                .iter()
                .any(|d| d.message.contains("unclosed")),
            "no unclosed tag diagnostic: {:?}",
            tree.diagnostics()
        );
    }

    #[test]
    fn html_heading_inside_blockquote() {
        let source = "> <h2>Title</h2>\n";
        let tree = parse(source);
        let children = root_children(&tree);
        assert_eq!(children.len(), 1, "one block quote");
        let quote_children = tree.children(children[0]);
        assert!(
            quote_children
                .iter()
                .any(|&id| matches!(tree.node(id).kind, ElementKind::Heading { level: 2 })),
            "heading inside block quote: {quote_children:?}"
        );
    }

    // --- Error recovery ---

    #[test]
    fn unclosed_html_tag_diagnostic() {
        let tree = parse("<div>\n\ncontent\n");
        let diags = tree.diagnostics();
        assert!(
            diags.iter().any(|d| d.message.contains("unclosed")),
            "should have unclosed tag diagnostic: {diags:?}"
        );
    }

    #[test]
    fn unexpected_close_tag_diagnostic() {
        let tree = parse("</div>\n");
        let diags = tree.diagnostics();
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("unexpected closing tag")),
            "should have unexpected close tag diagnostic: {diags:?}"
        );
    }

    #[test]
    fn mismatched_nesting_recovery() {
        // <div><section></div> should close section implicitly
        let tree = parse("<div>\n\n<section>\n\ntext\n\n</div>\n");
        let diags = tree.diagnostics();
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("unclosed `<section>`")),
            "should flag unclosed section: {diags:?}"
        );
        // The div should still be properly closed.
        let children = root_children(&tree);
        assert_eq!(children.len(), 1, "one root container");
    }

    // --- Markdown inside HTML blocks ---

    #[test]
    fn markdown_in_html_with_blank_lines() {
        let source = "<div>\n\n## Heading\n\n</div>\n";
        let tree = parse(source);
        let children = root_children(&tree);
        assert_eq!(children.len(), 1, "one container");
        assert_kind(&tree, children[0], &ElementKind::Container);
        // The heading should be a child of the container.
        let inner = tree.children(children[0]);
        assert!(
            inner
                .iter()
                .any(|&id| matches!(tree.node(id).kind, ElementKind::Heading { level: 2 })),
            "heading parsed inside container"
        );
    }

    #[test]
    fn raw_html_without_blank_lines() {
        let source = "<div>\n## Not a heading\n</div>\n";
        let tree = parse(source);
        let children = root_children(&tree);
        assert_eq!(children.len(), 1, "one container");
        // Content without blank lines is raw: no heading child.
        let inner = tree.children(children[0]);
        assert!(
            !inner
                .iter()
                .any(|&id| matches!(tree.node(id).kind, ElementKind::Heading { .. })),
            "no heading in raw mode"
        );
    }

    // --- <pre><code> → CodeBlock ---

    #[test]
    fn pre_code_produces_code_block() {
        let tree = parse("<pre><code>\nfn main() {}\n</code></pre>\n");
        let children = root_children(&tree);
        assert_eq!(children.len(), 1, "one block");
        let node = assert_kind(&tree, children[0], &ElementKind::CodeBlock);
        assert_eq!(node.syntax, Syntax::Html, "Html syntax");
    }

    #[test]
    fn pre_code_with_language() {
        let tree = parse("<pre><code class=\"language-rust\">\nfn main() {}\n</code></pre>\n");
        let children = root_children(&tree);
        assert_eq!(children.len(), 1, "one block");
        assert_kind(&tree, children[0], &ElementKind::CodeBlock);
    }

    #[test]
    fn pre_code_same_kind_as_fenced() {
        let md = parse("```\ncode\n```\n");
        let html = parse("<pre><code>\ncode\n</code></pre>\n");

        let md_kind = &md.node(root_children(&md)[0]).kind;
        let html_kind = &html.node(root_children(&html)[0]).kind;
        assert_eq!(md_kind, html_kind, "both produce CodeBlock");
    }

    #[test]
    fn pre_code_span_covers_full_block() {
        let source = "<pre><code>\nline1\nline2\n</code></pre>\n";
        let tree = parse(source);
        let children = root_children(&tree);
        let node = tree.node(children[0]);
        assert_eq!(
            node.span,
            Span::new(0, source.len()),
            "span covers opening through closing tag"
        );
    }

    // --- Standalone <pre> stays opaque ---

    #[test]
    fn html_block_type1_pre_stays_opaque() {
        let tree = parse("<pre>\ncode\n</pre>\n");
        let children = root_children(&tree);
        // Standalone <pre> (without <code>) stays as HtmlBlock.
        assert_eq!(children.len(), 1, "one block");
        assert_kind(&tree, children[0], &ElementKind::HtmlBlock);
    }

    #[test]
    fn html_block_type2_comment_stays_opaque() {
        let tree = parse("<!-- comment -->\n");
        let children = root_children(&tree);
        assert_eq!(children.len(), 1, "one block");
        assert_kind(&tree, children[0], &ElementKind::HtmlBlock);
    }

    // --- Table (HTML) elements ---

    #[test]
    fn html_table_container() {
        let tree = parse("<table>\n\n<tr><td>cell</td></tr>\n\n</table>\n");
        let children = root_children(&tree);
        assert_eq!(children.len(), 1, "one table container");
        assert!(
            matches!(&tree.node(children[0]).kind, ElementKind::Table { .. }),
            "kind is Table"
        );
        assert_eq!(tree.node(children[0]).syntax, Syntax::Html, "Html syntax");
    }

    // --- Section/article/aside all map to Container ---

    #[test]
    fn section_maps_to_container() {
        let tree = parse("<section>\n\ncontent\n\n</section>\n");
        let children = root_children(&tree);
        assert_eq!(children.len(), 1, "one container");
        assert_kind(&tree, children[0], &ElementKind::Container);
    }

    // --- HTML admonition containers ---

    #[test]
    fn html_div_warning_is_admonition() {
        let tree = parse("<div class=\"warning\">\n\nBe careful!\n\n</div>\n");
        let children = root_children(&tree);
        assert_eq!(children.len(), 1, "one admonition container");
        assert_kind(
            &tree,
            children[0],
            &ElementKind::Admonition {
                kind: "WARNING".to_string(),
            },
        );
    }

    #[test]
    fn html_div_note_is_admonition() {
        let tree = parse("<div class=\"note\">\n\nNote text.\n\n</div>\n");
        let children = root_children(&tree);
        assert_eq!(children.len(), 1, "one admonition container");
        assert_kind(
            &tree,
            children[0],
            &ElementKind::Admonition {
                kind: "NOTE".to_string(),
            },
        );
    }

    #[test]
    fn html_div_plain_is_container() {
        let tree = parse("<div>\n\ncontent\n\n</div>\n");
        let children = root_children(&tree);
        assert_eq!(children.len(), 1, "one container");
        assert_kind(&tree, children[0], &ElementKind::Container);
    }

    // --- Media elements ---

    #[test]
    fn html_video_produces_video() {
        let tree = parse("<video src=\"vid.mp4\"></video>\n");
        let children = root_children(&tree);
        assert_eq!(children.len(), 1, "one media element");
        let kind = &tree.node(children[0]).kind;
        assert!(
            matches!(kind, ElementKind::Video { url, .. } if url == "vid.mp4"),
            "video should produce Video with src extracted"
        );
    }

    #[test]
    fn html_audio_produces_audio() {
        let tree = parse("<audio src=\"song.mp3\"></audio>\n");
        let children = root_children(&tree);
        assert_eq!(children.len(), 1, "one media element");
        let kind = &tree.node(children[0]).kind;
        assert!(
            matches!(kind, ElementKind::Audio { url, .. } if url == "song.mp3"),
            "audio should produce Audio with src extracted"
        );
    }

    #[test]
    fn html_iframe_produces_image() {
        let tree = parse("<iframe src=\"page.html\"></iframe>\n");
        let children = root_children(&tree);
        assert_eq!(children.len(), 1, "one media element");
        let kind = &tree.node(children[0]).kind;
        assert!(
            matches!(kind, ElementKind::Image { url, .. } if url == "page.html"),
            "iframe should produce Image with src extracted"
        );
    }

    #[test]
    fn markdown_image_mp4_produces_video() {
        let tree = parse("![demo](demo.mp4)\n");
        let children = root_children(&tree);
        assert_eq!(children.len(), 1, "one paragraph");
        let para_children = tree.children(children[0]);
        let kind = &tree.node(para_children[0]).kind;
        assert!(
            matches!(kind, ElementKind::Video { url, .. } if url == "demo.mp4"),
            "![](*.mp4) should produce Video"
        );
    }

    #[test]
    fn markdown_image_mp3_produces_audio() {
        let tree = parse("![song](track.mp3)\n");
        let children = root_children(&tree);
        assert_eq!(children.len(), 1, "one paragraph");
        let para_children = tree.children(children[0]);
        let kind = &tree.node(para_children[0]).kind;
        assert!(
            matches!(kind, ElementKind::Audio { url, .. } if url == "track.mp3"),
            "![](*.mp3) should produce Audio"
        );
    }

    #[test]
    fn markdown_image_png_stays_image() {
        let tree = parse("![photo](pic.png)\n");
        let children = root_children(&tree);
        let para_children = tree.children(children[0]);
        let kind = &tree.node(para_children[0]).kind;
        assert!(
            matches!(kind, ElementKind::Image { url, .. } if url == "pic.png"),
            "![](*.png) should stay Image"
        );
    }

    // --- Form elements ---

    #[test]
    fn html_input_produces_form_control() {
        let tree = parse("<input type=\"text\">\n");
        let children = root_children(&tree);
        assert_eq!(children.len(), 1, "one form element");
        assert_kind(&tree, children[0], &ElementKind::FormControl);
    }

    #[test]
    fn html_select_produces_form_control() {
        let tree = parse("<select>\n<option>A</option>\n</select>\n");
        let children = root_children(&tree);
        assert_eq!(children.len(), 1, "one form element");
        assert_kind(&tree, children[0], &ElementKind::FormControl);
    }

    #[test]
    fn html_textarea_produces_form_control() {
        let tree = parse("<textarea>content</textarea>\n");
        let children = root_children(&tree);
        assert_eq!(children.len(), 1, "one form element");
        assert_kind(&tree, children[0], &ElementKind::FormControl);
    }

    // --- Table structure (main's table tests) ---

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

    // --- Nested HTML containers without blank lines (ticket 15) ---

    #[test]
    fn compact_dl_produces_children() {
        // <dl> with <dt>/<dd> on separate lines, no blank lines.
        let source = "<dl>\n<dt>API</dt>\n<dd>Description</dd>\n</dl>\n";
        let tree = parse(source);
        let children = root_children(&tree);
        assert_eq!(children.len(), 1, "one definition list");
        assert_kind(&tree, children[0], &ElementKind::DefinitionList);

        let dl_children = tree.children(children[0]);
        assert_eq!(dl_children.len(), 2, "dt and dd children");
        assert_kind(&tree, dl_children[0], &ElementKind::DefinitionTerm);
        assert_kind(&tree, dl_children[1], &ElementKind::DefinitionDesc);

        assert!(
            tree.diagnostics()
                .iter()
                .all(|d| !d.message.contains("unclosed")),
            "no unclosed diagnostics: {:?}",
            tree.diagnostics()
        );
    }

    #[test]
    fn compact_details_summary() {
        let source = "<details>\n<summary>Title</summary>\n<p>content</p>\n</details>\n";
        let tree = parse(source);
        let children = root_children(&tree);
        assert_eq!(children.len(), 1, "one details container");
        assert_kind(&tree, children[0], &ElementKind::Details);

        let inner = tree.children(children[0]);
        assert!(
            inner
                .iter()
                .any(|&id| tree.node(id).kind == ElementKind::DetailsSummary),
            "has DetailsSummary child: {inner:?}"
        );
        assert!(
            inner
                .iter()
                .any(|&id| tree.node(id).kind == ElementKind::Paragraph),
            "has Paragraph child: {inner:?}"
        );

        assert!(
            tree.diagnostics()
                .iter()
                .all(|d| !d.message.contains("unclosed")),
            "no unclosed diagnostics: {:?}",
            tree.diagnostics()
        );
    }

    #[test]
    fn compact_ul_with_li_children() {
        let source = "<ul>\n<li>item 1</li>\n<li>item 2</li>\n</ul>\n";
        let tree = parse(source);
        let children = root_children(&tree);
        assert_eq!(children.len(), 1, "one list");
        assert!(
            matches!(&tree.node(children[0]).kind, ElementKind::List { .. }),
            "kind is List"
        );

        let list_children = tree.children(children[0]);
        assert_eq!(list_children.len(), 2, "two list items");
        assert!(
            matches!(
                &tree.node(list_children[0]).kind,
                ElementKind::ListItem { .. }
            ),
            "first child is ListItem"
        );
        assert!(
            matches!(
                &tree.node(list_children[1]).kind,
                ElementKind::ListItem { .. }
            ),
            "second child is ListItem"
        );

        assert!(
            tree.diagnostics()
                .iter()
                .all(|d| !d.message.contains("unclosed")),
            "no unclosed diagnostics: {:?}",
            tree.diagnostics()
        );
    }

    #[test]
    fn compact_html_mixed_with_blank_lines() {
        // Some content with blank lines, some without.
        let source = "<dl>\n<dt>Term 1</dt>\n\nSome markdown\n\n<dd>Desc</dd>\n</dl>\n";
        let tree = parse(source);
        let children = root_children(&tree);
        assert_eq!(children.len(), 1, "one definition list");
        assert_kind(&tree, children[0], &ElementKind::DefinitionList);

        let dl_children = tree.children(children[0]);
        assert!(
            dl_children
                .iter()
                .any(|&id| tree.node(id).kind == ElementKind::DefinitionTerm),
            "has DefinitionTerm child"
        );
        assert!(
            dl_children
                .iter()
                .any(|&id| tree.node(id).kind == ElementKind::DefinitionDesc),
            "has DefinitionDesc child"
        );
    }

    #[test]
    fn compact_html_preserves_raw_non_html() {
        // Non-HTML content without blank lines is still opaque.
        let source = "<div>\n## Not a heading\n<p>also raw</p>\n</div>\n";
        let tree = parse(source);
        let children = root_children(&tree);
        assert_eq!(children.len(), 1, "one container");
        let inner = tree.children(children[0]);
        // The ## line is opaque, but <p> IS dispatched as a child.
        assert!(
            !inner
                .iter()
                .any(|&id| matches!(tree.node(id).kind, ElementKind::Heading { .. })),
            "heading is raw, not parsed"
        );
        assert!(
            inner
                .iter()
                .any(|&id| tree.node(id).kind == ElementKind::Paragraph),
            "<p> dispatched as Paragraph child"
        );
    }

    #[test]
    fn compact_nested_close_tag() {
        // Close tag for inner container dispatched from raw mode.
        let source = "<div>\n<section>\n<p>text</p>\n</section>\n</div>\n";
        let tree = parse(source);
        let children = root_children(&tree);
        assert_eq!(children.len(), 1, "one root container");
        assert_kind(&tree, children[0], &ElementKind::Container);

        let div_children = tree.children(children[0]);
        assert!(
            div_children
                .iter()
                .any(|&id| tree.node(id).kind == ElementKind::Container),
            "section child dispatched inside div"
        );

        assert!(
            tree.diagnostics()
                .iter()
                .all(|d| !d.message.contains("unclosed")),
            "no unclosed diagnostics: {:?}",
            tree.diagnostics()
        );
    }

    // --- Pathological input limits (ticket 20) ---

    use crate::limits;
    use std::time::Instant;

    /// Parsing must always terminate quickly; this generous bound catches
    /// quadratic or runaway behavior without being flaky under CI load.
    const SLOW_BOUND: std::time::Duration = std::time::Duration::from_secs(10);

    #[test]
    fn deeply_nested_block_quotes_hit_limit() {
        // 10,000 `>` markers on one line. Block quotes are parsed iteratively,
        // but the nesting cap must still fire so node growth is bounded.
        let source = format!("{} text\n", ">".repeat(10_000));
        let start = Instant::now();
        let tree = parse(&source);
        assert!(
            start.elapsed() < SLOW_BOUND,
            "block quote nesting must not hang"
        );

        let quotes = tree
            .nodes()
            .iter()
            .filter(|n| matches!(n.kind, ElementKind::QuoteBlock))
            .count();
        assert!(
            quotes <= limits::MAX_QUOTE_NESTING,
            "quote nesting capped at {}, got {quotes}",
            limits::MAX_QUOTE_NESTING
        );
        assert!(
            tree.diagnostics()
                .iter()
                .any(|d| d.message.contains("block quote nesting exceeds")),
            "expected a block quote nesting diagnostic: {:?}",
            tree.diagnostics()
        );
    }

    #[test]
    fn same_line_nested_list_markers_hit_limit() {
        // `- - - - ... x` recurses through `classify_item_content`; without a
        // cap this overflows the stack.
        let source = format!("{}x\n", "- ".repeat(10_000));
        let start = Instant::now();
        let tree = parse(&source);
        assert!(
            start.elapsed() < SLOW_BOUND,
            "list marker recursion must not hang"
        );

        let lists = tree
            .nodes()
            .iter()
            .filter(|n| matches!(n.kind, ElementKind::List { .. }))
            .count();
        assert!(
            lists <= limits::MAX_LIST_NESTING,
            "list nesting capped at {}, got {lists}",
            limits::MAX_LIST_NESTING
        );
        assert!(
            tree.diagnostics()
                .iter()
                .any(|d| d.message.contains("list nesting exceeds")),
            "expected a list nesting diagnostic: {:?}",
            tree.diagnostics()
        );
    }

    #[test]
    fn deeply_nested_lists_across_lines_hit_limit() {
        // Each line indents two more spaces, opening a new nested list level.
        let mut source = String::new();
        for depth in 0..2_000 {
            source.push_str(&" ".repeat(depth * 2));
            source.push_str("- item\n");
        }
        let start = Instant::now();
        let tree = parse(&source);
        assert!(start.elapsed() < SLOW_BOUND, "nested lists must not hang");

        let lists = tree
            .nodes()
            .iter()
            .filter(|n| matches!(n.kind, ElementKind::List { .. }))
            .count();
        assert!(
            lists <= limits::MAX_LIST_NESTING,
            "list nesting capped at {}, got {lists}",
            limits::MAX_LIST_NESTING
        );
        assert!(
            tree.diagnostics()
                .iter()
                .any(|d| d.message.contains("list nesting exceeds")),
            "expected a list nesting diagnostic: {:?}",
            tree.diagnostics()
        );
    }

    #[test]
    fn deeply_nested_html_containers_hit_limit() {
        // Nested `<div>` containers are parsed recursively
        // (`consume_html_raw` -> `handle_html_open`); the cap bounds recursion
        // depth and prevents stack overflow.
        let source = "<div>\n".repeat(10_000);
        let start = Instant::now();
        let tree = parse(&source);
        assert!(start.elapsed() < SLOW_BOUND, "nested HTML must not hang");

        assert!(
            tree.diagnostics()
                .iter()
                .any(|d| d.message.contains("HTML container nesting exceeds")),
            "expected an HTML nesting diagnostic: {:?}",
            tree.diagnostics()
        );
    }

    #[test]
    fn scope_stack_depth_is_hard_limited() {
        // 90 block quotes (under the quote cap) then a deep same-line list.
        // Each list level adds two scopes (List + ListItem), so the scope
        // stack reaches its hard cap before the list cap — exercising the
        // cross-container backstop.
        let source = format!("{}{}x\n", "> ".repeat(90), "- ".repeat(100));
        let start = Instant::now();
        let tree = parse(&source);
        assert!(
            start.elapsed() < SLOW_BOUND,
            "mixed deep nesting must not hang"
        );
        assert!(
            tree.diagnostics()
                .iter()
                .any(|d| d.message.contains("maximum scope depth")),
            "expected a scope-depth diagnostic: {:?}",
            tree.diagnostics()
        );
    }

    #[test]
    fn node_count_limit_is_enforced() {
        // More headings than the node cap; the parser must stop allocating
        // nodes, emit a diagnostic, and still return a tree.
        let source = "# h\n".repeat(limits::MAX_NODES + 100);
        let tree = parse(&source);
        assert!(
            tree.len() <= limits::MAX_NODES,
            "tree node count capped at {}, got {}",
            limits::MAX_NODES,
            tree.len()
        );
        assert!(
            tree.diagnostics()
                .iter()
                .any(|d| d.message.contains("-node limit")),
            "expected a node-count diagnostic: {:?}",
            tree.diagnostics()
        );
    }

    #[test]
    fn table_row_with_many_pipes_is_linear() {
        // A 10,000-cell row must split linearly.
        let header = format!("{}|\n", "|a".repeat(10_000));
        let delim = format!("{}|\n", "|-".repeat(10_000));
        let row = format!("{}|\n", "|b".repeat(10_000));
        let source = format!("{header}{delim}{row}");
        let start = Instant::now();
        let tree = parse(&source);
        assert!(
            start.elapsed() < SLOW_BOUND,
            "table cell splitting must be linear"
        );
        assert!(
            tree.nodes()
                .iter()
                .any(|n| matches!(n.kind, ElementKind::Table { .. })),
            "a table should be recognized"
        );
    }

    #[test]
    fn many_reference_definitions_are_bounded() {
        // Thousands of reference definitions: label normalization and lookup
        // must stay near-linear.
        use std::fmt::Write as _;
        let mut source = String::new();
        for i in 0..10_000 {
            let _ = writeln!(source, "[ref{i}]: https://example.com/{i}");
        }
        let start = Instant::now();
        let _tree = parse(&source);
        assert!(
            start.elapsed() < SLOW_BOUND,
            "reference definitions must not be quadratic"
        );
    }

    #[test]
    fn large_mixed_document_parses_quickly() {
        // ~1 MB of mixed structure parses well within the bound.
        let unit = "# Heading\n\nSome [text](./target.md \"references\") and `code`.\n\n\
                    - item one\n- item two\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n\
                    > a quote\n\n```rust\nlet x = 1;\n```\n\n";
        let mut source = String::with_capacity(1_100_000);
        while source.len() < 1_000_000 {
            source.push_str(unit);
        }
        let start = Instant::now();
        let tree = parse(&source);
        let elapsed = start.elapsed();
        assert!(
            elapsed < SLOW_BOUND,
            "1 MB document should parse quickly, took {elapsed:?}"
        );
        assert!(tree.len() > 1, "tree should contain structure");
    }

    mod commonmark_spec {
        include!("commonmark_spec_tests.rs");
    }
}
