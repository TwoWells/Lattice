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

mod frontmatter;
mod parser;
mod scan;

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use crate::html::{self, HtmlTag};
use crate::span::Span;

// The scanner's and the tree builder's public entry points keep their
// `crate::block::…` spelling: they are what the inline parser, the structural
// scanners, the formatter and the move engine call into, and the split is meant
// to be invisible to them.
pub use self::parser::{content_lines, first_line, parse_tree_with_entries};
pub use self::scan::{
    extract_atx_content, link_destination_span, link_fragment_span, normalize_label,
};

// `parse_tree` is the no-frontmatter convenience wrapper, and outside this
// module only the test suites and the fuzz facade ever call it — hence the same
// `cfg` they compile under, so the re-export is never dead weight in a release
// build. Production parses always carry frontmatter and go through
// [`parse_tree_with_entries`].
#[cfg(any(test, feature = "fuzzing"))]
pub use self::parser::parse_tree;

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
