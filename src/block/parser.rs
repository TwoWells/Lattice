// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! The tree builder: the stateful half of the block parser.
//!
//! Where [`super::scan`] answers "what could this line be", this module
//! answers "what is it, here" — the same line reads as a list continuation, a
//! lazy paragraph continuation, or the start of a new block depending on the
//! open containers. That context is the scope stack, the list stack and the
//! HTML scope stack the [`TreeBuilder`] carries line by line.
//!
//! Block quotes are container nodes whose children are parsed inline: there is
//! no deferred re-parse pass, so a node's span is final the moment it closes.
//! [`LimitFlags`] records where a [`crate::limits`] cap truncated the walk, so
//! a pathological document degrades into a diagnostic rather than an
//! unbounded parse.

use crate::html::{self, HtmlTag};
use crate::span::Span;

use super::frontmatter::expand_frontmatter_entries;
use super::scan::{
    ListContext, ListMarkerInfo, atx_heading_level, block_math_close, block_math_open,
    count_indent, detect_admonition, expand_all_tabs, expand_leading_tabs, expanded_to_raw,
    fenced_code_close, fenced_code_open, first_line_opens_refdef, has_close_on_same_line,
    html_block_end, html_block_start, is_pre_code_open, is_table_row, is_thematic_break,
    parse_delimiter_row, parse_footnote_def_start, recognize_list_marker, recognize_task,
    scan_one_refdef, setext_level, split_table_cells, strip_blockquote_marker,
};
use super::{Diagnostic, DiagnosticLevel, ElementKind, Node, NodeId, Syntax, TableAlignment, Tree};

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
pub struct TreeBuilder<'a> {
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
    pub fn add_node(
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
pub fn split_lines(text: &str) -> Vec<&str> {
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
pub fn line_content_end(source: &str, start: usize) -> usize {
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
