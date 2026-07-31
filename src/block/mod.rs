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

mod scan;

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use crate::html::{self, HtmlTag};
use crate::span::Span;

use self::scan::{
    ListContext, ListMarkerInfo, atx_heading_level, block_math_close, block_math_open,
    count_indent, detect_admonition, expand_all_tabs, expand_leading_tabs, expanded_to_raw,
    fenced_code_close, fenced_code_open, first_line_opens_refdef, has_close_on_same_line,
    html_block_end, is_pre_code_open, is_table_row, is_thematic_break, parse_delimiter_row,
    parse_footnote_def_start, recognize_list_marker, recognize_task, scan_one_refdef, setext_level,
    split_table_cells, strip_blockquote_marker,
};

// The scanner's public recognizers keep their `crate::block::…` spelling: they
// are what the inline parser, the structural scanners, the formatter and the
// move engine call into, and the split is meant to be invisible to them.
pub use self::scan::{
    extract_atx_content, html_block_start, link_destination_span, link_fragment_span,
    normalize_label,
};

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
    /// Strong emphasis run (`**...**` or `__...__`). The span covers the
    /// delimiters and their content. Styling/parse data only — never a
    /// diagnostic.
    Strong,
    /// Emphasis run (`*...*` or `_..._`). The span covers the delimiters and
    /// their content. Styling/parse data only — never a diagnostic.
    Emphasis,
    /// GFM strikethrough run (`~~...~~` or single `~...~`). The span covers the
    /// delimiters and their content. Styling/parse data only — never a
    /// diagnostic.
    Strikethrough,
    /// Generic inline raw-HTML open tag bearing an anchor `id` (e.g. a
    /// mid-paragraph `<span id="x">`). Materialized so the tag's `id` is
    /// visible to the same `Syntax::Html` surface that anchor resolution and
    /// the structural duplicate-`id` pass already walk — `<a>`/`<img>` keep
    /// their richer [`Link`]/[`Image`] kinds.
    ///
    /// [`Link`]: ElementKind::Link
    /// [`Image`]: ElementKind::Image
    InlineHtml,
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
#[derive(Debug, PartialEq, Eq)]
pub struct Link {
    /// 1-based line number in the source.
    pub line: usize,
    /// Byte span of the link in the source.
    pub span: Span,
    /// Classification and resolved details.
    pub kind: LinkKind,
}

/// Classification of a link.
#[derive(Debug, PartialEq, Eq)]
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
        /// Resolved path to the target: absolute for a document-relative link,
        /// or the relative remainder for a root-relative (`/x`) one (decision
        /// 019 clause 8). `WorkspaceLike` maps it onto a stored key / abs path.
        target: PathBuf,
    },
    /// Embed of an in-project resource — an image, video, or audio source
    /// (`![alt](x.png)`, `<img src>`, `<video src>`, `<audio src>`).
    ///
    /// An embed is a graph edge like any other path-bearing reference: its
    /// target is existence-checked (issue 058) and re-rendered by the move
    /// engine, so a moved asset never leaves a broken embed behind. It carries
    /// no predicate and derives no backlink obligation — an embed renders a
    /// resource, it does not assert a relation — so only the target is stored.
    /// A `#` fragment (as in `x.svg#view`) rides along verbatim in the source
    /// and is excluded from `target`, matching the move rule for fragments
    /// (decision 020 clause 4).
    Embed {
        /// Resolved path to the embedded resource, in the same root-free form
        /// as [`NonMarkdown::target`](Self::NonMarkdown): absolute for a
        /// document-relative source, the relative remainder for a root-relative
        /// (`/x`) one (decision 019 clause 8).
        target: PathBuf,
    },
    /// Intra-project link to a markdown file.
    IntraProject {
        /// Resolved path to the target `.md` file: absolute for a
        /// document-relative link, or the relative remainder for a root-relative
        /// (`/x`) one (decision 019 clause 8), resolved against a root only when
        /// matched or displayed.
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
#[derive(Debug, PartialEq, Eq)]
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
#[derive(Debug, PartialEq, Eq)]
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

/// An explicit in-page anchor target defined by a raw-HTML open tag.
///
/// A fragment `#x` resolves against any element bearing `id="x"`
/// (`<div id="x">`, `<section id="x">`, `<a id="x">`, …) and against the legacy
/// `<a name="x"></a>` idiom — matching GitHub. Such an element is a link
/// *target*, not a link *source*; an anchor-only `<a>` legitimately carries no
/// `href`. Each harvested `id`/`name` value becomes one entry, so a single
/// `<a id="a" name="b">` yields both.
#[derive(Debug, PartialEq, Eq)]
pub struct Anchor {
    /// 1-based line number in the source.
    pub line: usize,
    /// The anchor's fragment id (the `id` or `name` attribute value).
    pub id: String,
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
#[must_use]
pub fn parse_tree(source: &str, frontmatter_span: Option<Span>) -> Tree {
    parse_tree_with_entries(source, frontmatter_span, Syntax::Yaml, None)
}

/// Extended variant of [`parse_tree`] that accepts parsed frontmatter entries
/// for child expansion.
#[must_use]
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
            // No match — unexpected close tag. `span_end` is an end-of-line
            // position, and the close tag may be followed by trailing content
            // (possibly multi-byte) on the same line, so back-computing the
            // span from `span_end` by tag length can split a UTF-8 character
            // (fuzz_structural soak finding). Locate the tag's actual bytes
            // instead: an ASCII-case-insensitive backward search for `</tag`,
            // extended through an adjacent `>`. Tag names are ASCII, so the
            // resulting span always lands on char boundaries.
            let bytes = &self.source.as_bytes()[..span_end.min(self.source.len())];
            let needle = tag.as_bytes();
            let span = bytes
                .windows(needle.len() + 2)
                .rposition(|w| w[0] == b'<' && w[1] == b'/' && w[2..].eq_ignore_ascii_case(needle))
                .map_or_else(
                    // Not found (defensive): an empty span at the end-of-line
                    // offset, which is a char boundary by construction.
                    || Span::new(span_end, span_end),
                    |start| {
                        let after_name = start + needle.len() + 2;
                        let end = bytes[after_name..]
                            .iter()
                            .position(|b| !b.is_ascii_whitespace())
                            .map(|i| after_name + i)
                            .filter(|&i| bytes[i] == b'>')
                            .map_or(after_name, |i| i + 1);
                        Span::new(start, end)
                    },
                );
            self.diagnostics.push(Diagnostic {
                level: DiagnosticLevel::Error,
                span,
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

/// Check whether a URL is external — anything that is not a workspace path.
///
/// Recognition is by *grammar*, not by a list of known schemes (issue 071): a
/// destination carrying any URI scheme is a URI, so it is never resolved
/// against the source document. A prefix list answers the question backwards —
/// every scheme nobody enumerated (`data:`, `tel:`, `sms:`, `ftp:`, `file:`,
/// the `javascript:` an author quotes as an example) is diagnosed as a missing
/// file, and a base64-inlined `![](data:image/png;base64,…)` embed is a common,
/// sanctioned way to write a self-contained document.
///
/// A protocol-relative URL (`//host/path`) is external too: a renderer
/// resolves it against the current scheme and host, never against the
/// repository root, so it must not be read as a root-relative workspace path
/// (issue 028).
///
/// See [`has_uri_scheme`] for the scheme grammar and the one boundary it
/// decides — `C:\notes.md`.
fn is_external(url: &str) -> bool {
    url.starts_with("//") || has_uri_scheme(url)
}

/// Whether `url` opens with an RFC 3986 scheme — `ALPHA *( ALPHA / DIGIT / "+"
/// / "-" / "." ) ":"` — requiring at least two scheme characters.
///
/// The grammar is the same production [`crate::html::try_autolink`] uses to
/// tell a URI autolink from an email one, so a destination and an autolink
/// agree on what a scheme is. The run must start at byte 0: a `/` (or any other
/// character outside the scheme set) before the `:` breaks it, which is what
/// keeps `docs/a:b.md` and `12:30` out of the external bucket.
///
/// **The two-character minimum is the deliberate boundary.** A single ALPHA
/// followed by `:` is read as a Windows drive letter — `C:\notes.md` is a path,
/// not a URI — because one-letter schemes are essentially nonexistent in the
/// wild while the drive spelling has real users. `CommonMark` draws the line in
/// the same place: its own absolute-URI production requires a scheme of 2–32
/// characters. Above that floor `CommonMark`'s reading governs, so a bare
/// `foo:bar` *is* a URI (that is how a browser resolves an `href`), not a
/// relative path that happens to contain a colon.
///
/// No upper bound is imposed: `CommonMark`'s 32-character cap is an autolink
/// restriction, not an RFC 3986 one, and a longer scheme run is still not a
/// workspace path.
fn has_uri_scheme(url: &str) -> bool {
    // ASCII-only comparisons: a multi-byte character's bytes are all >= 0x80,
    // so none of them can match the scheme set or the terminating `:`, and the
    // run simply stops there.
    let Some((first, rest)) = url.as_bytes().split_first() else {
        return false;
    };
    if !first.is_ascii_alphabetic() {
        return false;
    }
    let tail = rest
        .iter()
        .take_while(|&&b| b.is_ascii_alphanumeric() || b == b'+' || b == b'-' || b == b'.')
        .count();
    tail >= 1 && rest.get(tail) == Some(&b':')
}

/// Resolve a link target path string against the source document's path.
///
/// `doc_path` is the document's **absolute** path in production (its key in the
/// server's flat store, or `root.join(rel)` for the CLI's owning workspace), so
/// a document-relative target resolves to an absolute path that encodes *no*
/// workspace root — the coordinate move of decision 019 clause 8. A root
/// re-enters only where a target is matched or displayed.
///
/// A leading single `/` is **root-relative**: GitHub and web renderers resolve
/// `/foo.md` against the repository (workspace) root, not the filesystem root
/// (issue 028). The root is not known at parse time, so such a target keeps its
/// deferred form — the leading `/` is stripped and the relative remainder is
/// stored verbatim, to be joined onto whichever root matches it at query time.
/// Stripping the `/` also keeps it inside the workspace: it can never escape to
/// an absolute filesystem path. The result is normalized in both cases.
///
/// The two forms are self-describing by absoluteness: a document-relative
/// target is absolute (given an absolute `doc_path`), a root-relative remainder
/// is relative. `WorkspaceLike` uses exactly this distinction to map a target
/// back onto its stored key.
fn resolve_target_path(path_str: &str, doc_path: &Path) -> PathBuf {
    // `//host/...` is handled as external before this point; a single leading
    // `/` here is unambiguously root-relative — strip it and keep the relative
    // remainder for query-time root resolution. Otherwise resolve against the
    // source document's parent directory (absolute in production).
    path_str.strip_prefix('/').map_or_else(
        || {
            let parent = doc_path.parent().unwrap_or_else(|| Path::new(""));
            normalize_path(&parent.join(path_str))
        },
        |rooted| normalize_path(Path::new(rooted)),
    )
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
///
/// `doc_path` is the source document's absolute path (see
/// [`resolve_target_path`]); an [`LinkKind::IntraProject`] / [`LinkKind::NonMarkdown`]
/// `target` is therefore absolute for a document-relative link and a relative
/// remainder for a root-relative (`/x`) one.
fn classify_link(url: &str, title: &str, doc_path: &Path, line: usize, span: Span) -> Option<Link> {
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
        let target = resolve_target_path(path_str, doc_path);

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

/// Classify an embed source URL (image / video / audio) into a [`Link`].
///
/// Mirrors [`classify_link`]'s resolution — same [`is_external`] oracle, same
/// [`resolve_target_path`] coordinates — but lands every in-project destination
/// in [`LinkKind::Embed`] regardless of extension: an embed asserts no relation,
/// so it never becomes an [`LinkKind::IntraProject`] edge with a predicate and a
/// backlink obligation. Returns `None` for an empty source and for a
/// fragment-only one (`![](#x)`), neither of which denotes a file.
fn classify_embed(url: &str, doc_path: &Path, line: usize, span: Span) -> Option<Link> {
    if url.is_empty() || url.starts_with('#') {
        return None;
    }

    let kind = if is_external(url) {
        LinkKind::External {
            url: url.to_string(),
        }
    } else {
        let (path_str, _fragment) = split_url_fragment(url);
        if path_str.is_empty() {
            return None;
        }
        LinkKind::Embed {
            target: resolve_target_path(path_str, doc_path),
        }
    };

    Some(Link { line, span, kind })
}

/// Classify an import directive path into a [`Link`].
fn classify_import(path: &str, doc_path: &Path, line: usize, span: Span) -> Link {
    let target = resolve_target_path(path, doc_path);
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
pub fn github_slug(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-' || *c == ' ')
        .map(|c| if c == ' ' { '-' } else { c })
        .collect()
}

/// GitLab heading slug.
pub fn gitlab_slug(text: &str) -> String {
    let raw: String = text
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-' || *c == ' ')
        .map(|c| if c == ' ' { '-' } else { c })
        .collect();

    collapse_hyphens(&raw).trim_matches('-').to_string()
}

/// VS Code heading slug.
pub fn vscode_slug(text: &str) -> String {
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

/// File extensions recognized in `@path` import directives.
const IMPORT_EXTENSIONS: &[&str] = &[".json", ".md", ".toml", ".txt", ".xml", ".yaml", ".yml"];

/// Check whether a string looks like a bare markdown path.
///
/// Scoped to `.md` only (issue 028): `.md` is the extension that forms a graph
/// edge, so it is the only intra-repo path-shape worth nudging into a link. A
/// trailing `#fragment` is stripped before the extension check, so
/// `foo.md#section` (a genuine anchored reference) is recognized just like
/// `foo.md`.
///
/// An external-namespace token (`{Name}/…`, issue 030) is the one exception to
/// the `.md` scope: it is recognized regardless of extension, so a cross-repo
/// directory or non-`.md` reference (`{Archive}/docs`, `{Archive}/schema.txt`)
/// is collected and existence-checked against its alias directory. The `.md`
/// rationale does not apply — an external reference never forms a graph edge
/// (decision 010), and the explicit `{Name}/` brace is a deliberate opt-in, not
/// the ambiguous prose mention the `.md` scope guards against.
///
/// Shapes that are not workspace paths are rejected outright: a `~`-leading
/// token (home-relative, out of the repo), a token containing `<` or `>` (a
/// placeholder), a token containing `*` (a glob), and a token containing an
/// ellipsis — `…` (U+2026) or `...` — which is documentation shorthand for "a
/// path of this shape" (e.g. the `{repo}/…` syntax this very tool teaches), not
/// a real file. These mirror the same exclusions in the prose path scan
/// ([`crate::structural`]) and apply to external tokens too — a `{Name}/…`
/// placeholder is exempt, while a concrete `{Name}/path` is resolved.
fn is_bare_path(s: &str) -> bool {
    let path = split_path_fragment(s).0;
    !is_import_directive(path)
        && !path.starts_with('~')
        && !path.contains('<')
        && !path.contains('>')
        && !path.contains('*')
        && !path.contains('…')
        && !path.contains("...")
        && path.contains('/')
        && (is_markdown_ext(Path::new(path)) || external_namespace(path).is_some())
}

/// Recognize an external-namespace reference of the form `{<identifier>}/rest`.
///
/// Returns `(alias, rest)` — the bare alias name (inside the braces) and the
/// path following the `}/` — when the token is shaped as an external reference
/// (issue 030, decision 010). This is the single recognizer shared by the bare
/// scanner ([`is_bare_path`]) and the prose/quoted/backtick scanners
/// ([`crate::structural`]), so the surfaces cannot drift. It is matched
/// **before** the normal dir/root resolution so the literal `{Name}` component
/// is never dir-joined and mis-flagged as a dangling intra-repo path, and
/// independently of the `.md` extension scope so a cross-repo directory or
/// non-`.md` file is recognized.
///
/// An identifier is one or more of `[A-Za-z0-9_-]`; the braces must wrap a
/// non-empty identifier and be immediately followed by `/` and a non-empty
/// remainder. `{}/x`, `{ }/x`, `{a b}/x`, a bare `{Name}` with no trailing `/`,
/// and `{Name}/` with no remainder are all rejected — they are not external
/// references and fall through to ordinary handling.
pub fn external_namespace(s: &str) -> Option<(&str, &str)> {
    let after_brace = s.strip_prefix('{')?;
    let close = after_brace.find('}')?;
    let alias = &after_brace[..close];
    let rest = after_brace[close + 1..].strip_prefix('/')?;
    if alias.is_empty()
        || rest.is_empty()
        || !alias
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return None;
    }
    Some((alias, rest))
}

/// Split a path-shaped token into its path and optional `#fragment`.
///
/// Mirrors the link-target classifier's fragment handling (issue 028): a
/// markdown link can target `path#fragment`, so the dark-matter scan must
/// strip the fragment before resolving the path part for existence.
fn split_path_fragment(s: &str) -> (&str, Option<&str>) {
    match s.split_once('#') {
        Some((path, frag)) => (path, Some(frag)),
        None => (s, None),
    }
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
///
/// Quote characters are deliberately **not** trimmed (issue 032): a quoted
/// dir-bearing token like `"docs/x.md"` is owned by the structural quoted-path
/// scanner ([`crate::structural`]), the sole owner of quoted content. Trimming
/// the quotes here would let the bare-path surface also claim the inner string,
/// double-emitting the stale-reference (or make-it-a-link) diagnostic. Leaving
/// the quotes attached makes the token fail the extension check, so the two
/// surfaces partition the text instead of overlapping. Only prose-adjacent
/// punctuation and bracketing are stripped.
fn scan_bare_paths_in_text(text: &str, base_line: usize, out: &mut Vec<BarePath>) {
    for (line_idx, line_text) in text.split('\n').enumerate() {
        for word in line_text.split_whitespace() {
            let cleaned = word
                .trim_start_matches(['(', '['])
                .trim_end_matches([',', '.', ';', ':', '!', '?', ')', ']']);

            if is_bare_path(cleaned) {
                // Store the fragment-stripped path so existence resolution and
                // the emitted message agree on the file the reference targets.
                let path = split_path_fragment(cleaned).0;
                out.push(BarePath {
                    line: base_line + line_idx,
                    path: path.to_string(),
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

// Counts actual `Tree::headings()` / `Tree::links()` extraction passes so a test
// can prove the per-file `FileData` cache (ticket perf 06) serves repeated graph
// validator reads from a single per-reparse extraction — not one `headings()`
// pass per fragment-link, nor one `links()` pass per file per sync. The analog of
// ticket 02's materialization counter; compiled out of release builds, so the hot
// path pays nothing.
#[cfg(test)]
thread_local! {
    static HEADINGS_EXTRACT_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static LINKS_EXTRACT_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Reset both extraction counters to zero (test instrumentation, ticket perf 06).
#[cfg(test)]
pub fn reset_extract_counts() {
    HEADINGS_EXTRACT_COUNT.with(|count| count.set(0));
    LINKS_EXTRACT_COUNT.with(|count| count.set(0));
}

/// Number of `Tree::headings()` extraction passes since the last reset.
#[cfg(test)]
pub fn headings_extract_count() -> usize {
    HEADINGS_EXTRACT_COUNT.with(std::cell::Cell::get)
}

/// Number of `Tree::links()` extraction passes since the last reset.
#[cfg(test)]
pub fn links_extract_count() -> usize {
    LINKS_EXTRACT_COUNT.with(std::cell::Cell::get)
}

impl Tree {
    /// Extract links from the tree, resolving targets against `doc_path`.
    ///
    /// `doc_path` is the source document's **absolute** path in production, so
    /// an intra-project / non-markdown target is root-free — absolute for a
    /// document-relative link, a relative remainder for a root-relative (`/x`)
    /// one (decision 019 clause 8; see [`resolve_target_path`]). Passing a
    /// relative `doc_path` (as unit tests do) simply yields relative targets;
    /// the resolution logic is identical either way.
    #[must_use]
    pub fn links(&self, doc_path: &Path) -> Vec<Link> {
        #[cfg(test)]
        LINKS_EXTRACT_COUNT.with(|count| count.set(count.get() + 1));

        let mut links = Vec::new();

        for node in &self.nodes {
            match &node.kind {
                ElementKind::Link { url, title } => {
                    let line = byte_offset_to_line(&self.source, node.span.start);
                    if let Some(link) = classify_link(url, title, doc_path, line, node.span) {
                        links.push(link);
                    }
                }
                ElementKind::Image { url, .. }
                | ElementKind::Video { url, .. }
                | ElementKind::Audio { url, .. } => {
                    // An embed is a path-bearing edge too (issue 058): its
                    // target is existence-checked and re-rendered by the move
                    // engine, so it belongs in the extracted link set. The
                    // `Embed` kind keeps it out of the predicate / backlink /
                    // connectivity passes, which key on `IntraProject`.
                    let line = byte_offset_to_line(&self.source, node.span.start);
                    if let Some(embed) = classify_embed(url, doc_path, line, node.span) {
                        links.push(embed);
                    }
                }
                ElementKind::Import { path } => {
                    let line = byte_offset_to_line(&self.source, node.span.start);
                    links.push(classify_import(path, doc_path, line, node.span));
                }
                _ => {}
            }
        }

        links
    }

    /// Extract headings with computed slugs.
    #[must_use]
    pub fn headings(&self) -> Vec<Heading> {
        #[cfg(test)]
        HEADINGS_EXTRACT_COUNT.with(|count| count.set(count.get() + 1));

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

    /// Extract explicit in-page anchor targets from raw-HTML open tags.
    ///
    /// Harvests anchor `id`/`name` attribute values from raw-HTML open tags,
    /// covering both opaque HTML blocks ([`ElementKind::HtmlBlock`]) and inline
    /// raw HTML ([`Syntax::Html`] nodes) — the same node surface the structural
    /// HTML pass walks, so the fragment resolver and the `<a>` `href` check
    /// agree on what an anchor is. Each harvested value becomes one [`Anchor`];
    /// a single `<a id="a" name="b">` yields both `a` and `b`. Closing tags
    /// (`</a>`) and empty values are skipped.
    ///
    /// This matches GitHub: a fragment `#x` resolves against *any* element that
    /// bears `id="x"` — `<div id>`, `<span id>`, `<section id>`, `<a id>`, … —
    /// so `id` is harvested from any open tag. The legacy `<a name="x">` anchor
    /// idiom is `<a>`-specific (a `name` on a non-`<a>` element is not an
    /// anchor), so `name` is harvested only from `<a>` tags.
    ///
    /// The inline raw-HTML node surface materializes only `<a>` and `<img>`
    /// tags (the inline scanner skips other tags without emitting a node), so a
    /// non-`<a>` `id` is harvested only when it appears as a standalone HTML
    /// block. This mirrors the structural HTML pass exactly.
    #[must_use]
    pub fn anchors(&self) -> Vec<Anchor> {
        let mut anchors = Vec::new();

        for node in &self.nodes {
            let is_html_node = node.syntax == Syntax::Html;
            let is_html_block = matches!(node.kind, ElementKind::HtmlBlock);
            if !is_html_node && !is_html_block {
                continue;
            }

            let raw = &self.source[node.span.start..node.span.end];
            // For an opaque HTML block the tag is on the first line; an inline
            // raw-HTML node is the tag itself. Mirror the structural pass.
            let tag_text = if is_html_block {
                raw.lines().next().unwrap_or("").trim()
            } else {
                raw.trim()
            };

            // `tokenize_tag` returns `Open` only for open tags, so closing tags
            // (`</div>`) are skipped here.
            let Some(HtmlTag::Open { name, attrs, .. }) = html::tokenize_tag(tag_text, 0) else {
                continue;
            };

            let line = byte_offset_to_line(&self.source, node.span.start);
            for attr in &attrs {
                // `id` is an anchor on any element; `name` is an anchor only on
                // `<a>` (the legacy `<a name>` idiom).
                let is_anchor_attr = attr.name == "id" || (attr.name == "name" && name == "a");
                if is_anchor_attr
                    && let Some(value) = &attr.value
                    && !value.is_empty()
                {
                    anchors.push(Anchor {
                        line,
                        id: value.clone(),
                    });
                }
            }
        }

        anchors
    }

    /// Scan inline hosts (paragraphs and table cells) for bare file paths.
    ///
    /// Table cells are scanned so this dark-matter surface matches the inline
    /// hosts the link/edge extractor walks; a bare path in a cell is otherwise
    /// invisible to the nudge that would convert it, yet becomes a real edge
    /// once linked.
    #[must_use]
    pub fn bare_paths(&self) -> Vec<BarePath> {
        let mut bare_paths = Vec::new();

        for (id, node) in self.nodes.iter().enumerate() {
            if !matches!(node.kind, ElementKind::Paragraph | ElementKind::TableCell) {
                continue;
            }
            self.scan_bare_paths_in_node(id, &mut bare_paths);
        }

        bare_paths
    }

    /// Extract heading display text, optional explicit ID, and text byte span.
    #[must_use]
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

    /// Scan an inline host (paragraph or table cell) for bare paths,
    /// excluding inline children.
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
mod tests;
