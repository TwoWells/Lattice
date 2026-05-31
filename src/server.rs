// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! LSP server for Lattice.
//!
//! Publishes diagnostics on file open, save, and change. Provides workspace
//! symbols, rename, references, type hierarchy, and call hierarchy for
//! headings. Supports multiple workspace folders.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use lsp_server::{Connection, Message, Notification, Response};

use crate::block::{
    ElementKind, Heading, HeadingId, LinkKind, NodeId, Syntax, Tree, normalize_label,
};
use crate::lsp;
use crate::span::Span;
use crate::structural;
use crate::validation::{self, Diagnostic, Severity};
use crate::workspace::Workspace;

/// Multiple workspaces keyed by root path.
struct Workspaces {
    inner: BTreeMap<PathBuf, Workspace>,
}

impl Workspaces {
    /// Create from the initial set of workspace folders.
    fn from_params(params: &lsp::InitializeParams) -> Self {
        let mut inner = BTreeMap::new();

        if let Some(folders) = &params.workspace_folders {
            for folder in folders {
                let root = uri_to_path(&folder.uri);
                if let Ok(ws) = Workspace::scan(&root) {
                    inner.insert(root, ws);
                }
            }
        }

        // Fall back to deprecated root_uri if no folders.
        if let Some(root_uri) = params.root_uri.as_ref().filter(|_| inner.is_empty()) {
            let root = uri_to_path(root_uri);
            if let Ok(ws) = Workspace::scan(&root) {
                inner.insert(root, ws);
            }
        }

        Self { inner }
    }

    /// Add a workspace folder.
    fn add(&mut self, uri: &str) {
        let root = uri_to_path(uri);
        if let Ok(ws) = Workspace::scan(&root) {
            self.inner.insert(root, ws);
        }
    }

    /// Remove a workspace folder.
    fn remove(&mut self, uri: &str) {
        let root = uri_to_path(uri);
        self.inner.remove(&root);
    }

    /// Find the workspace that contains a file URI, returning the workspace
    /// and the file's workspace-relative path.
    fn resolve(&self, uri: &str) -> Option<(&Workspace, PathBuf)> {
        let path = uri_to_path(uri);
        self.inner.iter().rev().find_map(|(root, ws)| {
            path.strip_prefix(root)
                .ok()
                .map(|rel| (ws, rel.to_path_buf()))
        })
    }

    /// Find the workspace that contains a file URI (mutable).
    fn resolve_mut(&mut self, uri: &str) -> Option<(&mut Workspace, PathBuf)> {
        let path = uri_to_path(uri);
        self.inner.iter_mut().rev().find_map(|(root, ws)| {
            path.strip_prefix(root)
                .ok()
                .map(|rel| (ws, rel.to_path_buf()))
        })
    }

    /// Iterate over all workspaces.
    fn iter(&self) -> impl Iterator<Item = (&PathBuf, &Workspace)> {
        self.inner.iter()
    }
}

/// Run the LSP server on stdio.
///
/// # Errors
///
/// Returns an error if the connection or initialization fails.
pub fn run() -> Result<()> {
    let (connection, io_threads) = Connection::stdio();

    let capabilities = serde_json::json!({
        "textDocumentSync": {
            "openClose": true,
            "change": 1,
            "save": { "includeText": true }
        },
        "documentSymbolProvider": true,
        "workspaceSymbolProvider": true,
        "renameProvider": { "prepareProvider": true },
        "referencesProvider": true,
        "declarationProvider": true,
        "definitionProvider": true,
        "typeDefinitionProvider": true,
        "implementationProvider": true,
        "typeHierarchyProvider": true,
        "callHierarchyProvider": true,
        "documentLinkProvider": {},
        "foldingRangeProvider": true,
        "hoverProvider": true,
        "diagnosticProvider": {
            "interFileDependencies": true,
            "workspaceDiagnostics": true
        },
        "documentFormattingProvider": true,
        "workspace": {
            "workspaceFolders": {
                "supported": true,
                "changeNotifications": true
            }
        }
    });

    let init_params = connection.initialize(capabilities)?;
    let params: lsp::InitializeParams =
        serde_json::from_value(init_params).context("failed to parse InitializeParams")?;

    let workspaces = Workspaces::from_params(&params);

    main_loop(&connection, workspaces)?;
    drop(connection); // Close channels so IO threads can exit.
    io_threads.join()?;

    Ok(())
}

/// Convert an LSP URI to a filesystem path.
fn uri_to_path(uri: &str) -> PathBuf {
    PathBuf::from(uri.strip_prefix("file://").unwrap_or(uri))
}

/// Convert a filesystem path to an LSP URI string.
fn path_to_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

/// Main message loop.
fn main_loop(connection: &Connection, mut workspaces: Workspaces) -> Result<()> {
    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                if connection.handle_shutdown(&req)? {
                    return Ok(());
                }
                let id = req.id.clone();
                if let Err(err) = handle_request(connection, &workspaces, req) {
                    tracing::error!("request {id} failed: {err:#}");
                    let resp = Response::new_err(
                        id,
                        lsp_server::ErrorCode::InternalError as i32,
                        format!("{err:#}"),
                    );
                    connection.sender.send(Message::Response(resp))?;
                }
            }
            Message::Notification(notif) => {
                if let Err(err) = handle_notification(connection, &mut workspaces, notif) {
                    tracing::error!("notification failed: {err:#}");
                }
            }
            Message::Response(_) => {}
        }
    }
    Ok(())
}

/// Dispatch a request.
#[allow(
    clippy::too_many_lines,
    reason = "flat dispatch table, not complex logic"
)]
fn handle_request(
    connection: &Connection,
    workspaces: &Workspaces,
    req: lsp_server::Request,
) -> Result<()> {
    let resp = match req.method.as_str() {
        lsp::method::DOCUMENT_SYMBOL => {
            let params: lsp::DocumentSymbolParams = serde_json::from_value(req.params)?;
            let symbols = document_symbols(workspaces, &params.text_document.uri);
            Response::new_ok(req.id, symbols)
        }
        lsp::method::WORKSPACE_SYMBOL => {
            let params: lsp::WorkspaceSymbolParams = serde_json::from_value(req.params)?;
            let symbols = workspace_symbols(workspaces, &params.query);
            Response::new_ok(req.id, symbols)
        }
        lsp::method::PREPARE_RENAME => {
            let params: lsp::TextDocumentPositionParams = serde_json::from_value(req.params)?;
            let result = prepare_rename(workspaces, &params);
            Response::new_ok(req.id, result)
        }
        lsp::method::RENAME => {
            let params: lsp::RenameParams = serde_json::from_value(req.params)?;
            let edit = do_rename(workspaces, &params);
            Response::new_ok(req.id, edit)
        }
        lsp::method::REFERENCES => {
            let params: lsp::ReferenceParams = serde_json::from_value(req.params)?;
            let locations = find_references(workspaces, &params);
            Response::new_ok(req.id, locations)
        }
        lsp::method::DECLARATION => {
            let params: lsp::TextDocumentPositionParams = serde_json::from_value(req.params)?;
            let location = go_to_declaration(workspaces, &params);
            Response::new_ok(req.id, location)
        }
        lsp::method::DEFINITION => {
            let params: lsp::TextDocumentPositionParams = serde_json::from_value(req.params)?;
            let location = go_to_definition(workspaces, &params);
            Response::new_ok(req.id, location)
        }
        lsp::method::TYPE_DEFINITION => {
            let params: lsp::TextDocumentPositionParams = serde_json::from_value(req.params)?;
            let location = go_to_type_definition(workspaces, &params);
            Response::new_ok(req.id, location)
        }
        lsp::method::IMPLEMENTATION => {
            let params: lsp::TextDocumentPositionParams = serde_json::from_value(req.params)?;
            let location = go_to_implementation(workspaces, &params);
            Response::new_ok(req.id, location)
        }
        lsp::method::PREPARE_TYPE_HIERARCHY => {
            let params: lsp::TextDocumentPositionParams = serde_json::from_value(req.params)?;
            let items = prepare_type_hierarchy(workspaces, &params);
            Response::new_ok(req.id, items)
        }
        lsp::method::TYPE_HIERARCHY_SUPERTYPES => {
            let params: lsp::TypeHierarchyParams = serde_json::from_value(req.params)?;
            let items = type_hierarchy_supertypes(workspaces, &params.item);
            Response::new_ok(req.id, items)
        }
        lsp::method::TYPE_HIERARCHY_SUBTYPES => {
            let params: lsp::TypeHierarchyParams = serde_json::from_value(req.params)?;
            let items = type_hierarchy_subtypes(workspaces, &params.item);
            Response::new_ok(req.id, items)
        }
        lsp::method::PREPARE_CALL_HIERARCHY => {
            let params: lsp::TextDocumentPositionParams = serde_json::from_value(req.params)?;
            let items = prepare_call_hierarchy(workspaces, &params);
            Response::new_ok(req.id, items)
        }
        lsp::method::CALL_HIERARCHY_INCOMING => {
            let params: lsp::CallHierarchyParams = serde_json::from_value(req.params)?;
            let calls = call_hierarchy_incoming(workspaces, &params.item);
            Response::new_ok(req.id, calls)
        }
        lsp::method::CALL_HIERARCHY_OUTGOING => {
            let params: lsp::CallHierarchyParams = serde_json::from_value(req.params)?;
            let calls = call_hierarchy_outgoing(workspaces, &params.item);
            Response::new_ok(req.id, calls)
        }
        lsp::method::DOCUMENT_LINK => {
            let params: lsp::DocumentSymbolParams = serde_json::from_value(req.params)?;
            let links = document_links(workspaces, &params.text_document.uri);
            Response::new_ok(req.id, links)
        }
        lsp::method::FOLDING_RANGE => {
            let params: lsp::DocumentSymbolParams = serde_json::from_value(req.params)?;
            let ranges = folding_ranges(workspaces, &params.text_document.uri);
            Response::new_ok(req.id, ranges)
        }
        lsp::method::HOVER => {
            let params: lsp::TextDocumentPositionParams = serde_json::from_value(req.params)?;
            let hover = hover_preview(workspaces, &params);
            Response::new_ok(req.id, hover)
        }
        lsp::method::DOCUMENT_DIAGNOSTIC => {
            let params: lsp::DocumentDiagnosticParams = serde_json::from_value(req.params)?;
            let report = document_diagnostic(workspaces, &params.text_document.uri);
            Response::new_ok(req.id, report)
        }
        lsp::method::WORKSPACE_DIAGNOSTIC => {
            let report = workspace_diagnostic(workspaces);
            Response::new_ok(req.id, report)
        }
        lsp::method::FORMATTING => {
            let params: lsp::DocumentFormattingParams = serde_json::from_value(req.params)?;
            let edits = format_document(workspaces, &params.text_document.uri);
            Response::new_ok(req.id, edits)
        }
        _ => Response::new_err(
            req.id,
            lsp_server::ErrorCode::MethodNotFound as i32,
            format!("method not found: {}", req.method),
        ),
    };
    connection.sender.send(Message::Response(resp))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Document symbols
// ---------------------------------------------------------------------------

/// Maximum length for truncated symbol names.
const SYMBOL_NAME_MAX: usize = 60;

/// Truncate a string to `SYMBOL_NAME_MAX` characters, appending `…` if cut.
fn truncate_name(s: &str) -> String {
    if s.len() <= SYMBOL_NAME_MAX {
        return s.to_string();
    }
    let mut end = SYMBOL_NAME_MAX;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Map an `ElementKind` to its LSP `SymbolKind`, or `None` if the node
/// should not be emitted as a symbol.
fn element_symbol_kind(kind: &ElementKind) -> Option<u32> {
    match kind {
        ElementKind::Heading { .. } => Some(lsp::symbol_kind::CLASS),
        ElementKind::Link { .. } | ElementKind::Import { .. } => Some(lsp::symbol_kind::FUNCTION),
        ElementKind::Image { .. } | ElementKind::Video { .. } | ElementKind::Audio { .. } => {
            Some(lsp::symbol_kind::FILE)
        }
        ElementKind::List { .. }
        | ElementKind::Table { .. }
        | ElementKind::DefinitionList
        | ElementKind::Frontmatter
        | ElementKind::FrontmatterMap { .. } => Some(lsp::symbol_kind::STRUCT),
        ElementKind::CodeBlock | ElementKind::Math => Some(lsp::symbol_kind::OBJECT),
        ElementKind::QuoteBlock
        | ElementKind::Admonition { .. }
        | ElementKind::Details
        | ElementKind::Container => Some(lsp::symbol_kind::MODULE),
        ElementKind::Rules => Some(lsp::symbol_kind::OPERATOR),
        ElementKind::FootnoteDef { .. } => Some(lsp::symbol_kind::CONSTANT),
        ElementKind::FormControl => Some(lsp::symbol_kind::EVENT),
        ElementKind::FrontmatterKey { .. } => Some(lsp::symbol_kind::FIELD),
        // Not emitted: leaf content nodes, structural internals.
        ElementKind::Document
        | ElementKind::Paragraph
        | ElementKind::HtmlBlock
        | ElementKind::InlineCode
        | ElementKind::InlineMath
        | ElementKind::FootnoteRef { .. }
        | ElementKind::ReferenceDef { .. }
        | ElementKind::DetailsSummary
        | ElementKind::ListItem { .. }
        | ElementKind::TableRow { .. }
        | ElementKind::TableCell
        | ElementKind::DefinitionTerm
        | ElementKind::DefinitionDesc => None,
    }
}

/// Whether an element is a scope boundary (headings inside it do not
/// participate in the document's heading hierarchy).
fn is_scope_boundary(kind: &ElementKind) -> bool {
    matches!(
        kind,
        ElementKind::QuoteBlock
            | ElementKind::Admonition { .. }
            | ElementKind::Details
            | ElementKind::Container
    )
}

/// Generate the symbol name and optional detail for a tree node.
#[allow(
    clippy::too_many_lines,
    reason = "single match over all ElementKind variants"
)]
fn symbol_name(tree: &Tree, node_id: NodeId) -> (String, Option<String>) {
    let node = tree.node(node_id);
    let source = tree.source();
    let raw = &source[node.span.start..node.span.end];

    match &node.kind {
        ElementKind::Heading { level } => {
            let (text, _, _) = tree.heading_content(node_id);
            (format!("H{level}: {text}"), None)
        }
        ElementKind::Link { url, title } => {
            let predicate = if title.is_empty() {
                "references"
            } else {
                title
            };
            let name = format!("Link: {predicate}({url})");
            let display = link_display_text(raw);
            let detail = if display.is_empty() {
                None
            } else {
                Some(display)
            };
            (truncate_name(&name), detail)
        }
        ElementKind::Import { path } => (truncate_name(&format!("Link: import({path})")), None),
        ElementKind::Image { url, .. } => {
            let detail_type = if raw.trim_start().starts_with("<iframe") {
                "iframe"
            } else {
                "image"
            };
            let name = if url.is_empty() {
                format!("File: {detail_type}")
            } else {
                format!("File: {url}")
            };
            (truncate_name(&name), Some(detail_type.to_string()))
        }
        ElementKind::Video { url, .. } => {
            let name = if url.is_empty() {
                "File: video".to_string()
            } else {
                format!("File: {url}")
            };
            (truncate_name(&name), Some("video".to_string()))
        }
        ElementKind::Audio { url, .. } => {
            let name = if url.is_empty() {
                "File: audio".to_string()
            } else {
                format!("File: {url}")
            };
            (truncate_name(&name), Some("audio".to_string()))
        }
        ElementKind::CodeBlock => {
            let lang = code_block_language(raw);
            let title = code_block_title(raw);
            let name = lang.map_or_else(|| "CodeBlock".to_string(), |l| format!("CodeBlock: {l}"));
            (name, title)
        }
        ElementKind::Math => ("Math".to_string(), None),
        ElementKind::Table { .. } => {
            let data_rows = node
                .children
                .iter()
                .filter(|&&c| matches!(tree.node(c).kind, ElementKind::TableRow { header: false }))
                .count();
            ("Table".to_string(), Some(data_rows.to_string()))
        }
        ElementKind::DefinitionList => {
            let term_count = node
                .children
                .iter()
                .filter(|&&c| matches!(tree.node(c).kind, ElementKind::DefinitionTerm))
                .count();
            ("Definitions".to_string(), Some(term_count.to_string()))
        }
        ElementKind::List { ordered, .. } => {
            let item_count = node
                .children
                .iter()
                .filter(|&&c| matches!(tree.node(c).kind, ElementKind::ListItem { .. }))
                .count();
            let name = if *ordered { "Ordered List" } else { "List" };
            (name.to_string(), Some(item_count.to_string()))
        }
        ElementKind::QuoteBlock => ("Blockquote".to_string(), None),
        ElementKind::Admonition { kind } => (format!("Admonition: {kind}"), None),
        ElementKind::Details => {
            let text = details_summary_text(tree, node_id);
            if text.is_empty() {
                ("Details".to_string(), None)
            } else {
                (format!("Details: {}", truncate_name(&text)), None)
            }
        }
        ElementKind::FootnoteDef { label } => (format!("Footnote: [^{label}]"), None),
        ElementKind::Rules => ("Break".to_string(), None),
        ElementKind::Container => {
            let tag = container_tag_name(raw);
            (format!("Container: {tag}"), None)
        }
        ElementKind::FormControl => {
            let tag = container_tag_name(raw);
            (format!("Form: {tag}"), None)
        }
        ElementKind::Frontmatter => {
            let syntax_label = match node.syntax {
                Syntax::Yaml => "YAML",
                Syntax::Html => "HTML",
                Syntax::Markdown => "Markdown",
            };
            let key_count = node
                .children
                .iter()
                .filter(|&&c| {
                    matches!(
                        tree.node(c).kind,
                        ElementKind::FrontmatterKey { .. } | ElementKind::FrontmatterMap { .. }
                    )
                })
                .count();
            let detail = if key_count > 0 {
                Some(key_count.to_string())
            } else {
                None
            };
            (format!("Frontmatter: {syntax_label}"), detail)
        }
        ElementKind::FrontmatterMap { key } => {
            let child_count = node.children.len();
            let detail = if child_count > 0 {
                Some(child_count.to_string())
            } else {
                None
            };
            (key.clone(), detail)
        }
        ElementKind::FrontmatterKey { key, .. } => {
            let detail = frontmatter_key_detail(tree, node_id);
            (format!("Field: {key}"), detail)
        }
        _ => (String::new(), None),
    }
}

/// Compute detail for a `FrontmatterKey` node.
///
/// If the key has a non-zero leaf count (sequence items), returns the count
/// as detail. This covers both block sequences and flow sequences.
fn frontmatter_key_detail(tree: &Tree, node_id: NodeId) -> Option<String> {
    let node = tree.node(node_id);

    // Only show detail when the parent is a FrontmatterMap (nested key).
    let parent_id = node.parent?;
    let parent = tree.node(parent_id);
    if !matches!(parent.kind, ElementKind::FrontmatterMap { .. }) {
        return None;
    }

    if let ElementKind::FrontmatterKey { leaf_count, .. } = &node.kind
        && *leaf_count > 0
    {
        return Some(leaf_count.to_string());
    }
    None
}

/// Extract the display text from a markdown link like `[text](url)`.
fn link_display_text(raw: &str) -> String {
    if raw.starts_with('[') {
        if let Some(end) = raw.find("](") {
            return raw[1..end].trim().to_string();
        }
        if let Some(end) = raw.find("][") {
            return raw[1..end].trim().to_string();
        }
        if raw.ends_with(']') && !raw.contains("](") {
            return raw[1..raw.len() - 1].trim().to_string();
        }
    }
    // HTML <a> tag: extract inner text
    if let Some(text) = raw
        .find('>')
        .and_then(|start| {
            raw.rfind("</")
                .filter(|&end| end > start)
                .map(|end| (start, end))
        })
        .map(|(s, e)| raw[s + 1..e].trim())
    {
        return text.to_string();
    }
    String::new()
}

/// Extract the language tag from a fenced code block.
fn code_block_language(raw: &str) -> Option<String> {
    let first_line = raw.lines().next().unwrap_or("");
    let trimmed = first_line.trim();
    // Fenced: ```lang or ~~~lang
    if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
        let fence_char = &trimmed[..1];
        let after_fence = trimmed.trim_start_matches(fence_char.chars().next().unwrap_or('`'));
        let lang = after_fence.trim();
        if lang.is_empty() {
            return None;
        }
        // Strip info string after first space
        let lang = lang.split_whitespace().next().unwrap_or(lang);
        return Some(lang.to_string());
    }
    // Block math
    if trimmed.starts_with("$$") {
        return Some("math".to_string());
    }
    None
}

/// Extract the title (info string after the language) from a fenced code block.
fn code_block_title(raw: &str) -> Option<String> {
    let first_line = raw.lines().next().unwrap_or("");
    let trimmed = first_line.trim();
    if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
        let fence_char = trimmed.chars().next().unwrap_or('`');
        let after_fence = trimmed.trim_start_matches(fence_char);
        let info = after_fence.trim();
        // Split into language and rest of info string
        let mut parts = info.splitn(2, char::is_whitespace);
        let _lang = parts.next();
        if let Some(rest) = parts.next() {
            let rest = rest.trim();
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
    }
    None
}

/// Extract the `<summary>` text from a `<details>` node.
fn details_summary_text(tree: &Tree, details_id: NodeId) -> String {
    let details = tree.node(details_id);
    let source = tree.source();
    for &child_id in &details.children {
        let child = tree.node(child_id);
        if matches!(child.kind, ElementKind::DetailsSummary) {
            let text = &source[child.span.start..child.span.end];
            // Strip <summary> tags — the span may extend past </summary>.
            let inner = text.trim().strip_prefix("<summary>").unwrap_or(text);
            return inner.find("</summary>").map_or_else(
                || inner.trim().to_string(),
                |end| inner[..end].trim().to_string(),
            );
        }
    }
    String::new()
}

/// Extract the tag name from a generic container's opening tag.
fn container_tag_name(raw: &str) -> String {
    let first_line = raw.lines().next().unwrap_or("");
    let trimmed = first_line.trim();
    if let Some(after) = trimmed.strip_prefix('<') {
        let end = after
            .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
            .unwrap_or(after.len());
        return after[..end].to_lowercase();
    }
    "container".to_string()
}

/// Extract the first meaningful text from a list item.
fn list_item_text(tree: &Tree, item_id: NodeId) -> String {
    let node = tree.node(item_id);
    let source = tree.source();
    let raw = &source[node.span.start..node.span.end];

    let first_line = raw.lines().next().unwrap_or("");
    let trimmed = first_line.trim_start();

    // Strip list marker and optional task checkbox
    let text = if trimmed.starts_with("- [")
        || trimmed.starts_with("* [")
        || trimmed.starts_with("+ [")
    {
        let after_marker = &trimmed[2..];
        after_marker
            .strip_prefix("[x] ")
            .or_else(|| after_marker.strip_prefix("[X] "))
            .or_else(|| after_marker.strip_prefix("[ ] "))
            .unwrap_or(after_marker)
    } else if trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("+ ") {
        &trimmed[2..]
    } else {
        // Ordered: strip digits and `. ` or `) `
        let digit_end = trimmed.find(|c: char| !c.is_ascii_digit()).unwrap_or(0);
        if digit_end > 0
            && (trimmed[digit_end..].starts_with(". ") || trimmed[digit_end..].starts_with(") "))
        {
            &trimmed[digit_end + 2..]
        } else {
            trimmed
        }
    };

    text.trim().to_string()
}

/// Build a span-to-line range for a node.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn node_range(tree: &Tree, node_id: NodeId) -> lsp::Range {
    let node = tree.node(node_id);
    let source = tree.source();
    let start_line = source[..node.span.start]
        .bytes()
        .filter(|&b| b == b'\n')
        .count() as u32;
    let end_line = source[..node.span.end.min(source.len())]
        .bytes()
        .filter(|&b| b == b'\n')
        .count() as u32;
    lsp::Range {
        start: lsp::Position {
            line: start_line,
            character: 0,
        },
        end: lsp::Position {
            line: end_line,
            character: 0,
        },
    }
}

/// Build document symbols for a file by walking the tree.
fn document_symbols(workspaces: &Workspaces, uri: &str) -> Option<Vec<lsp::DocumentSymbol>> {
    let (workspace, rel_path) = workspaces.resolve(uri)?;
    let file_data = workspace.file(&rel_path)?;
    let tree = &file_data.tree;
    let root = 0; // Document root is always node 0
    let children = tree.node(root).children.clone();
    Some(build_symbol_tree(tree, &children, false))
}

/// A tagged symbol for the nesting pass. Headings carry their level
/// so the nesting algorithm can build the correct hierarchy.
struct TaggedSymbol {
    /// Heading level (1–6), or 0 for non-heading symbols.
    level: u8,
    /// The LSP symbol.
    symbol: lsp::DocumentSymbol,
}

/// Recursively build the symbol tree from a list of child node IDs.
///
/// `inside_scope` is true when we're inside a scope boundary (block quote,
/// details). Headings inside scopes are emitted as flat symbols, not
/// participating in the heading hierarchy.
fn build_symbol_tree(
    tree: &Tree,
    children: &[NodeId],
    inside_scope: bool,
) -> Vec<lsp::DocumentSymbol> {
    let mut tagged: Vec<TaggedSymbol> = Vec::new();

    for &node_id in children {
        let node = tree.node(node_id);

        // Paragraphs: float links up.
        if matches!(node.kind, ElementKind::Paragraph) {
            for sym in collect_floated_links(tree, node_id) {
                tagged.push(TaggedSymbol {
                    level: 0,
                    symbol: sym,
                });
            }
            continue;
        }

        let Some(kind) = element_symbol_kind(&node.kind) else {
            continue;
        };

        let heading_level = match &node.kind {
            ElementKind::Heading { level } => *level,
            _ => 0,
        };

        let (name, detail) = symbol_name(tree, node_id);
        let range = node_range(tree, node_id);

        // Build children based on element type.
        let sym_children = match &node.kind {
            // Tables: emit Field children from header row cells only.
            ElementKind::Table { .. } => {
                let fields = build_table_field_children(tree, node_id);
                if fields.is_empty() {
                    None
                } else {
                    Some(fields)
                }
            }
            // Lists: emit nested sub-list children only.
            ElementKind::List { .. } => {
                let nested = build_nested_list_children(tree, node_id);
                if nested.is_empty() {
                    None
                } else {
                    Some(nested)
                }
            }
            // Definition lists: emit Field children from terms.
            ElementKind::DefinitionList => {
                let fields = build_definition_list_children(tree, node_id);
                if fields.is_empty() {
                    None
                } else {
                    Some(fields)
                }
            }
            // Opaque content blocks and leaf elements: no children.
            ElementKind::CodeBlock
            | ElementKind::Math
            | ElementKind::Link { .. }
            | ElementKind::Image { .. }
            | ElementKind::Video { .. }
            | ElementKind::Audio { .. }
            | ElementKind::Import { .. }
            | ElementKind::FrontmatterKey { .. } => None,
            // Scope containers: recurse normally.
            _ => {
                let node_children = &tree.node(node_id).children;
                if node_children.is_empty() {
                    None
                } else {
                    let in_scope = inside_scope || is_scope_boundary(&node.kind);
                    let child_syms = build_symbol_tree(tree, node_children, in_scope);
                    if child_syms.is_empty() {
                        None
                    } else {
                        Some(child_syms)
                    }
                }
            }
        };

        tagged.push(TaggedSymbol {
            level: heading_level,
            symbol: lsp::DocumentSymbol {
                name,
                detail,
                kind,
                range,
                selection_range: range,
                children: sym_children,
            },
        });
    }

    // If we're inside a scope boundary, headings are flat — no nesting.
    if inside_scope {
        return tagged.into_iter().map(|t| t.symbol).collect();
    }

    // Outside scopes, nest headings by level (H2 under H1, etc.)
    // and attach non-heading symbols to their preceding heading.
    nest_by_heading_level(tagged)
}

/// Nest symbols by heading level: H2 under H1, H3 under H2, etc.
/// Non-heading symbols are attached as children of their preceding heading.
fn nest_by_heading_level(tagged: Vec<TaggedSymbol>) -> Vec<lsp::DocumentSymbol> {
    if !tagged.iter().any(|t| t.level > 0) {
        return tagged.into_iter().map(|t| t.symbol).collect();
    }

    let mut stack: Vec<(u8, lsp::DocumentSymbol)> = Vec::new();
    let mut result: Vec<lsp::DocumentSymbol> = Vec::new();

    for item in tagged {
        if item.level > 0 {
            // Pop symbols at same or deeper level — they're complete.
            while stack.last().is_some_and(|(lvl, _)| *lvl >= item.level) {
                let Some((_, finished)) = stack.pop() else {
                    break;
                };
                if let Some((_, parent)) = stack.last_mut() {
                    parent.children.get_or_insert_with(Vec::new).push(finished);
                } else {
                    result.push(finished);
                }
            }
            stack.push((item.level, item.symbol));
        } else {
            // Non-heading: attach to last heading on stack, else top-level.
            if let Some((_, parent)) = stack.last_mut() {
                parent
                    .children
                    .get_or_insert_with(Vec::new)
                    .push(item.symbol);
            } else {
                result.push(item.symbol);
            }
        }
    }

    // Flush remaining stack.
    while let Some((_, finished)) = stack.pop() {
        if let Some((_, parent)) = stack.last_mut() {
            parent.children.get_or_insert_with(Vec::new).push(finished);
        } else {
            result.push(finished);
        }
    }

    result
}

/// Collect link symbols from a paragraph node (float-up).
fn collect_floated_links(tree: &Tree, para_id: NodeId) -> Vec<lsp::DocumentSymbol> {
    let node = tree.node(para_id);
    let mut links = Vec::new();
    for &child_id in &node.children {
        let child = tree.node(child_id);
        if element_symbol_kind(&child.kind).is_some()
            && matches!(
                child.kind,
                ElementKind::Link { .. }
                    | ElementKind::Image { .. }
                    | ElementKind::Video { .. }
                    | ElementKind::Audio { .. }
                    | ElementKind::Import { .. }
            )
        {
            let kind = element_symbol_kind(&child.kind).unwrap_or(lsp::symbol_kind::FUNCTION);
            let (name, detail) = symbol_name(tree, child_id);
            let range = node_range(tree, child_id);
            links.push(lsp::DocumentSymbol {
                name,
                detail,
                kind,
                range,
                selection_range: range,
                children: None,
            });
        }
    }
    links
}

/// Build `Field` children from a table's header row cells.
fn build_table_field_children(tree: &Tree, table_id: NodeId) -> Vec<lsp::DocumentSymbol> {
    let table = tree.node(table_id);
    let source = tree.source();
    let mut fields = Vec::new();

    for &child_id in &table.children {
        let child = tree.node(child_id);
        if matches!(child.kind, ElementKind::TableRow { header: true }) {
            for &cell_id in &child.children {
                let cell = tree.node(cell_id);
                let text = source[cell.span.start..cell.span.end]
                    .trim()
                    .trim_matches('|')
                    .trim();
                let name = format!("Field: {}", truncate_name(text));
                let range = node_range(tree, cell_id);
                fields.push(lsp::DocumentSymbol {
                    name,
                    detail: None,
                    kind: lsp::symbol_kind::FIELD,
                    range,
                    selection_range: range,
                    children: None,
                });
            }
            break;
        }
    }
    fields
}

/// Build `Field` children from a definition list's term nodes.
fn build_definition_list_children(tree: &Tree, dl_id: NodeId) -> Vec<lsp::DocumentSymbol> {
    let dl = tree.node(dl_id);
    let source = tree.source();
    let mut fields = Vec::new();

    for &child_id in &dl.children {
        let child = tree.node(child_id);
        if matches!(child.kind, ElementKind::DefinitionTerm) {
            let text = source[child.span.start..child.span.end].trim();
            // Strip <dt> and </dt> tags if present (HTML syntax).
            let text = text
                .strip_prefix("<dt>")
                .unwrap_or(text)
                .strip_suffix("</dt>")
                .unwrap_or(text)
                .trim();
            let name = format!("Field: {}", truncate_name(text));
            let range = node_range(tree, child_id);
            fields.push(lsp::DocumentSymbol {
                name,
                detail: None,
                kind: lsp::symbol_kind::FIELD,
                range,
                selection_range: range,
                children: None,
            });
        }
    }
    fields
}

/// Build `Struct` children for nested sub-lists within a list.
///
/// For each `ListItem` that contains a child `List`, emits a `Struct`
/// symbol named by the parent item's text. Items without sub-lists
/// are not emitted.
fn build_nested_list_children(tree: &Tree, list_id: NodeId) -> Vec<lsp::DocumentSymbol> {
    let list = tree.node(list_id);
    let mut children = Vec::new();

    for &item_id in &list.children {
        let item = tree.node(item_id);
        if !matches!(item.kind, ElementKind::ListItem { .. }) {
            continue;
        }

        for &sub_id in &item.children {
            let sub = tree.node(sub_id);
            if let ElementKind::List { ordered, .. } = &sub.kind {
                let item_text = list_item_text(tree, item_id);
                let prefix = if *ordered { "Ordered List" } else { "List" };
                let name = if item_text.is_empty() {
                    prefix.to_string()
                } else {
                    format!("{prefix}: {}", truncate_name(&item_text))
                };

                let sub_item_count = sub
                    .children
                    .iter()
                    .filter(|&&c| matches!(tree.node(c).kind, ElementKind::ListItem { .. }))
                    .count();

                let range = node_range(tree, sub_id);

                // Recurse for deeper nesting.
                let nested = build_nested_list_children(tree, sub_id);
                let nested_children = if nested.is_empty() {
                    None
                } else {
                    Some(nested)
                };

                children.push(lsp::DocumentSymbol {
                    name,
                    detail: Some(sub_item_count.to_string()),
                    kind: lsp::symbol_kind::STRUCT,
                    range,
                    selection_range: range,
                    children: nested_children,
                });
            }
        }
    }
    children
}

// ---------------------------------------------------------------------------
// Workspace symbols
// ---------------------------------------------------------------------------

/// Search symbols across all workspaces, filtered by query.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn workspace_symbols(workspaces: &Workspaces, query: &str) -> Vec<lsp::SymbolInformation> {
    let query_lower = query.to_lowercase();
    let mut symbols = Vec::new();

    for (root, workspace) in workspaces.iter() {
        for (rel_path, file_data) in workspace.files() {
            let tree = &file_data.tree;
            collect_workspace_symbols(tree, &query_lower, root, rel_path, &mut symbols);
        }
    }

    symbols
}

/// Collect flat workspace symbols from a tree, filtered by query.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn collect_workspace_symbols(
    tree: &Tree,
    query_lower: &str,
    root: &Path,
    rel_path: &Path,
    out: &mut Vec<lsp::SymbolInformation>,
) {
    let abs_path = root.join(rel_path);
    let uri = path_to_uri(&abs_path);
    let source = tree.source();

    for (node_id, node) in tree.nodes().iter().enumerate() {
        let Some(kind) = element_symbol_kind(&node.kind) else {
            continue;
        };

        // Skip nested lists — only top-level data containers in workspace.
        if matches!(node.kind, ElementKind::List { .. })
            && node
                .parent
                .is_some_and(|p| matches!(tree.node(p).kind, ElementKind::ListItem { .. }))
        {
            continue;
        }

        // Skip frontmatter children — only the top-level container in workspace.
        if matches!(
            node.kind,
            ElementKind::FrontmatterKey { .. } | ElementKind::FrontmatterMap { .. }
        ) {
            continue;
        }

        let (name, _) = symbol_name(tree, node_id);
        if name.is_empty() {
            continue;
        }

        if !query_lower.is_empty() && !name.to_lowercase().contains(query_lower) {
            continue;
        }

        let start_line = source[..node.span.start]
            .bytes()
            .filter(|&b| b == b'\n')
            .count() as u32;

        out.push(lsp::SymbolInformation {
            name,
            kind,
            location: lsp::Location {
                uri: uri.clone(),
                range: lsp::Range {
                    start: lsp::Position {
                        line: start_line,
                        character: 0,
                    },
                    end: lsp::Position {
                        line: start_line,
                        character: 0,
                    },
                },
            },
            container_name: Some(rel_path.display().to_string()),
        });
    }
}

// ---------------------------------------------------------------------------
// prepareRename / rename (ticket 04)
// ---------------------------------------------------------------------------

/// Find the heading at a cursor position, returning its text range.
///
/// Uses the tree's `text_span` to compute the exact text range, supporting
/// ATX, setext, and HTML headings without prefix assumptions.
fn prepare_rename(
    workspaces: &Workspaces,
    params: &lsp::TextDocumentPositionParams,
) -> Option<lsp::Range> {
    let (workspace, rel_path) = workspaces.resolve(&params.text_document.uri)?;
    let file_data = workspace.file(&rel_path)?;
    let headings = file_data.tree.headings();
    let heading = heading_at_line(&headings, params.position.line)?;

    Some(span_to_lsp_range(
        file_data.tree.source(),
        &heading.text_span,
    ))
}

/// Rename a heading's text.
///
/// Uses the tree's `text_span` for the edit range, supporting ATX, setext,
/// and HTML headings.
fn do_rename(workspaces: &Workspaces, params: &lsp::RenameParams) -> Option<lsp::WorkspaceEdit> {
    let (workspace, rel_path) = workspaces.resolve(&params.text_document.uri)?;
    let file_data = workspace.file(&rel_path)?;
    let headings = file_data.tree.headings();
    let heading = heading_at_line(&headings, params.position.line)?;

    let range = span_to_lsp_range(file_data.tree.source(), &heading.text_span);

    let mut changes = std::collections::HashMap::new();
    changes.insert(
        params.text_document.uri.clone(),
        vec![lsp::TextEdit {
            range,
            new_text: params.new_name.clone(),
        }],
    );

    Some(lsp::WorkspaceEdit {
        changes: Some(changes),
    })
}

/// Find the heading whose line matches the cursor's 0-based line number.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn heading_at_line(headings: &[Heading], lsp_line: u32) -> Option<&Heading> {
    headings
        .iter()
        .find(|h| h.line.saturating_sub(1) as u32 == lsp_line)
}

// ---------------------------------------------------------------------------
// Find references (ticket 05)
// ---------------------------------------------------------------------------

/// Find all documents that link to the file or heading at the cursor,
/// or all call sites of a reference definition.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn find_references(workspaces: &Workspaces, params: &lsp::ReferenceParams) -> Vec<lsp::Location> {
    let Some((workspace, rel_path)) = workspaces.resolve(&params.text_document.uri) else {
        return Vec::new();
    };
    let Some(file_data) = workspace.file(&rel_path) else {
        return Vec::new();
    };

    // Check if cursor is on a reference definition — find all call sites.
    let offset = lsp_position_to_byte_offset(file_data.tree.source(), params.position);
    if let Some(label) = ref_def_label_at_offset(&file_data.tree, offset) {
        return find_ref_def_call_sites(workspaces, &params.text_document.uri, &label);
    }

    // Determine if the cursor is on a heading (to filter by fragment).
    let file_headings = file_data.tree.headings();
    let target_heading = heading_at_line(&file_headings, params.position.line);

    let mut locations = Vec::new();

    for (root, ws) in workspaces.iter() {
        for (src_path, src_data) in ws.files() {
            let links = src_data.tree.links(src_path);
            for link in &links {
                let LinkKind::IntraProject {
                    target, fragment, ..
                } = &link.kind
                else {
                    continue;
                };
                if target != &rel_path {
                    continue;
                }
                // If cursor is on a heading, only match links with a fragment to that heading.
                if let Some(heading) = target_heading {
                    let Some(frag) = fragment else {
                        continue;
                    };
                    if !heading_matches_fragment(heading, frag) {
                        continue;
                    }
                }
                let abs_path = root.join(src_path);
                let line = link.line.saturating_sub(1) as u32;
                locations.push(lsp::Location {
                    uri: path_to_uri(&abs_path),
                    range: lsp::Range {
                        start: lsp::Position { line, character: 0 },
                        end: lsp::Position { line, character: 0 },
                    },
                });
            }
        }
    }

    locations
}

/// Find all reference-style link call sites that use a given label.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn find_ref_def_call_sites(workspaces: &Workspaces, uri: &str, label: &str) -> Vec<lsp::Location> {
    let Some((workspace, rel_path)) = workspaces.resolve(uri) else {
        return Vec::new();
    };
    let Some(file_data) = workspace.file(&rel_path) else {
        return Vec::new();
    };
    let root = workspace.root();
    let source = file_data.tree.source();
    let mut locations = Vec::new();

    for node in file_data.tree.nodes() {
        if !matches!(node.kind, ElementKind::Link { .. }) {
            continue;
        }
        if let Some(ref_label) = link_ref_label(source, &node.span)
            && ref_label == label
        {
            let line = crate::block::byte_offset_to_line(source, node.span.start);
            let line_lsp = line.saturating_sub(1) as u32;
            locations.push(lsp::Location {
                uri: path_to_uri(&root.join(&rel_path)),
                range: lsp::Range {
                    start: lsp::Position {
                        line: line_lsp,
                        character: 0,
                    },
                    end: lsp::Position {
                        line: line_lsp,
                        character: 0,
                    },
                },
            });
        }
    }

    locations
}

/// Check whether a fragment matches any of a heading's anchor IDs.
fn heading_matches_fragment(heading: &Heading, fragment: &str) -> bool {
    match &heading.id {
        HeadingId::Explicit(id) => id == fragment,
        HeadingId::Computed {
            github,
            gitlab,
            vscode,
        } => fragment == github || fragment == gitlab || fragment == vscode,
    }
}

// ---------------------------------------------------------------------------
// Navigation — go to declaration / definition / type definition / implementation
// ---------------------------------------------------------------------------

/// Go to the declaration of a link.
///
/// For reference-style links (`[text][ref]`), goes to the `[ref]: url`
/// definition line. For inline links, falls through to the target document.
fn go_to_declaration(
    workspaces: &Workspaces,
    params: &lsp::TextDocumentPositionParams,
) -> Option<lsp::Location> {
    let (workspace, rel_path) = workspaces.resolve(&params.text_document.uri)?;
    let file_data = workspace.file(&rel_path)?;
    let source = file_data.tree.source();
    let offset = lsp_position_to_byte_offset(source, params.position);

    let (_, node) = file_data.tree.find_link_at_offset(offset)?;

    // If it's a reference-style link, go to the ref def.
    if let Some(label) = link_ref_label(source, &node.span) {
        let (_, def_node) = file_data.tree.find_ref_def(&label)?;
        return Some(lsp::Location {
            uri: params.text_document.uri.clone(),
            range: span_to_lsp_range(source, &def_node.span),
        });
    }

    // Inline link — fall through to definition (target document).
    go_to_definition(workspaces, params)
}

/// Go to the definition (target document) of a link.
fn go_to_definition(
    workspaces: &Workspaces,
    params: &lsp::TextDocumentPositionParams,
) -> Option<lsp::Location> {
    let (workspace, rel_path) = workspaces.resolve(&params.text_document.uri)?;
    let file_data = workspace.file(&rel_path)?;
    let source = file_data.tree.source();
    let offset = lsp_position_to_byte_offset(source, params.position);

    let (_, node) = file_data.tree.find_link_at_offset(offset)?;
    if !matches!(node.kind, ElementKind::Link { .. }) {
        return None;
    }

    let link = find_classified_link(&file_data.tree, &rel_path, node.span)?;

    match &link.kind {
        LinkKind::IntraProject { target, .. } | LinkKind::NonMarkdown { target } => {
            let root = workspace.root();
            Some(lsp::Location {
                uri: path_to_uri(&root.join(target)),
                range: lsp::Range::default(),
            })
        }
        _ => None,
    }
}

/// Go to the type definition of a link.
///
/// For links with a fragment, goes to the heading in the target document.
/// Without a fragment, falls through to definition (the document itself).
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn go_to_type_definition(
    workspaces: &Workspaces,
    params: &lsp::TextDocumentPositionParams,
) -> Option<lsp::Location> {
    let (workspace, rel_path) = workspaces.resolve(&params.text_document.uri)?;
    let file_data = workspace.file(&rel_path)?;
    let source = file_data.tree.source();
    let offset = lsp_position_to_byte_offset(source, params.position);

    let (_, node) = file_data.tree.find_link_at_offset(offset)?;

    let link = find_classified_link(&file_data.tree, &rel_path, node.span)?;

    let LinkKind::IntraProject {
        target, fragment, ..
    } = &link.kind
    else {
        return go_to_definition(workspaces, params);
    };

    let Some(fragment) = fragment.as_deref() else {
        // No fragment — fall through to definition (the document itself).
        return go_to_definition(workspaces, params);
    };

    let root = workspace.root();
    let target_data = workspace.file(target)?;
    let target_headings = target_data.tree.headings();
    let heading = target_headings
        .iter()
        .find(|h| heading_matches_fragment(h, fragment))?;

    let heading_line = heading.line.saturating_sub(1) as u32;
    Some(lsp::Location {
        uri: path_to_uri(&root.join(target)),
        range: lsp::Range {
            start: lsp::Position {
                line: heading_line,
                character: 0,
            },
            end: lsp::Position {
                line: heading_line,
                character: 0,
            },
        },
    })
}

/// Go to the implementation (forward link) of a backlink entry in frontmatter.
///
/// When the cursor is on a backlink path like `    - decisions/38.md` in the
/// frontmatter, navigates to the forward link line in the source document.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn go_to_implementation(
    workspaces: &Workspaces,
    params: &lsp::TextDocumentPositionParams,
) -> Option<lsp::Location> {
    let (workspace, rel_path) = workspaces.resolve(&params.text_document.uri)?;
    let file_data = workspace.file(&rel_path)?;
    let fm = file_data.frontmatter.as_ref()?;

    // Check cursor is inside frontmatter.
    let cursor_line_1based = params.position.line as usize + 1;
    if cursor_line_1based < fm.start_line || cursor_line_1based > fm.end_line {
        return None;
    }

    // Extract the backlink path from the cursor line.
    let source = file_data.tree.source();
    let line_text = source_line_at(source, params.position.line);
    let path_text = line_text.trim().strip_prefix("- ")?.trim();
    if path_text.is_empty() {
        return None;
    }

    // Find which inverse predicate this path belongs to.
    let inverse_predicate = fm.backlinks.iter().find_map(|(pred, paths)| {
        paths
            .iter()
            .any(|p| p == path_text)
            .then_some(pred.as_str())
    })?;

    // Map inverse → forward predicate.
    let forward_predicate = workspace.config().forward_of(inverse_predicate)?;

    // Find the source document and the forward link.
    let source_path = PathBuf::from(path_text);
    let source_data = workspace.file(&source_path)?;
    let source_links = source_data.tree.links(&source_path);

    let forward_link = source_links.iter().find(|l| {
        let LinkKind::IntraProject {
            target, predicate, ..
        } = &l.kind
        else {
            return false;
        };
        target == &rel_path && predicate == forward_predicate
    })?;

    let root = workspace.root();
    let line = forward_link.line.saturating_sub(1) as u32;
    Some(lsp::Location {
        uri: path_to_uri(&root.join(&source_path)),
        range: lsp::Range {
            start: lsp::Position { line, character: 0 },
            end: lsp::Position { line, character: 0 },
        },
    })
}

// ---------------------------------------------------------------------------
// Type hierarchy (ticket 08)
// ---------------------------------------------------------------------------

/// Prepare a type hierarchy item for the heading at the cursor.
fn prepare_type_hierarchy(
    workspaces: &Workspaces,
    params: &lsp::TextDocumentPositionParams,
) -> Option<Vec<lsp::HierarchyItem>> {
    let (workspace, rel_path) = workspaces.resolve(&params.text_document.uri)?;
    let file_data = workspace.file(&rel_path)?;
    let headings = file_data.tree.headings();
    let heading = heading_at_line(&headings, params.position.line)?;
    let item = heading_to_hierarchy_item(heading, &workspace.root().join(&rel_path));
    Some(vec![item])
}

/// Return the parent heading (supertype) of a heading.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn type_hierarchy_supertypes(
    workspaces: &Workspaces,
    item: &lsp::HierarchyItem,
) -> Option<Vec<lsp::HierarchyItem>> {
    let (workspace, rel_path) = workspaces.resolve(&item.uri)?;
    let file_data = workspace.file(&rel_path)?;
    let abs_path = workspace.root().join(&rel_path);
    let headings = file_data.tree.headings();

    let target_level = hierarchy_item_level(item);
    if target_level <= 1 {
        return Some(Vec::new());
    }

    let target_line = item.selection_range.start.line;
    let parent = headings.iter().rev().find(|h| {
        let h_line = h.line.saturating_sub(1) as u32;
        h_line < target_line && h.level < target_level
    });

    let items = parent
        .map(|h| heading_to_hierarchy_item(h, &abs_path))
        .into_iter()
        .collect();
    Some(items)
}

/// Return the immediate child headings (subtypes) of a heading.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn type_hierarchy_subtypes(
    workspaces: &Workspaces,
    item: &lsp::HierarchyItem,
) -> Option<Vec<lsp::HierarchyItem>> {
    let (workspace, rel_path) = workspaces.resolve(&item.uri)?;
    let file_data = workspace.file(&rel_path)?;
    let abs_path = workspace.root().join(&rel_path);
    let headings = file_data.tree.headings();

    let target_level = hierarchy_item_level(item);
    let child_level = target_level + 1;
    let target_line = item.selection_range.start.line;

    let mut children = Vec::new();
    let mut started = false;

    for heading in &headings {
        let h_line = heading.line.saturating_sub(1) as u32;

        if h_line == target_line {
            started = true;
            continue;
        }
        if !started {
            continue;
        }
        // Stop at same or higher level — we've left this section.
        if heading.level <= target_level {
            break;
        }
        // Only include direct children.
        if heading.level == child_level {
            children.push(heading_to_hierarchy_item(heading, &abs_path));
        }
    }

    Some(children)
}

// ---------------------------------------------------------------------------
// Call hierarchy (ticket 07)
// ---------------------------------------------------------------------------

/// Prepare a call hierarchy item for the heading at the cursor.
fn prepare_call_hierarchy(
    workspaces: &Workspaces,
    params: &lsp::TextDocumentPositionParams,
) -> Option<Vec<lsp::HierarchyItem>> {
    let (workspace, rel_path) = workspaces.resolve(&params.text_document.uri)?;
    let file_data = workspace.file(&rel_path)?;
    let headings = file_data.tree.headings();
    let heading = heading_at_line(&headings, params.position.line)?;
    let item = heading_to_hierarchy_item(heading, &workspace.root().join(&rel_path));
    Some(vec![item])
}

/// Find all incoming calls — links from other files that target this document.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn call_hierarchy_incoming(
    workspaces: &Workspaces,
    item: &lsp::HierarchyItem,
) -> Vec<lsp::CallHierarchyIncomingCall> {
    let Some((_, rel_path)) = workspaces.resolve(&item.uri) else {
        return Vec::new();
    };

    let mut calls = Vec::new();

    for (root, ws) in workspaces.iter() {
        for (src_path, file_data) in ws.files() {
            let links = file_data.tree.links(src_path);
            let headings = file_data.tree.headings();
            for link in &links {
                let LinkKind::IntraProject { target, .. } = &link.kind else {
                    continue;
                };
                if target != &rel_path {
                    continue;
                }
                let abs_src = root.join(src_path);
                let caller_heading = enclosing_heading(&headings, link.line);

                let caller_item = caller_heading.map_or_else(
                    || file_hierarchy_item(&abs_src, src_path),
                    |ch| heading_to_hierarchy_item(ch, &abs_src),
                );

                let line = link.line.saturating_sub(1) as u32;
                calls.push(lsp::CallHierarchyIncomingCall {
                    from: caller_item,
                    from_ranges: vec![lsp::Range {
                        start: lsp::Position { line, character: 0 },
                        end: lsp::Position { line, character: 0 },
                    }],
                });
            }
        }
    }

    calls
}

/// Find all outgoing calls — links within the heading's section to other files.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn call_hierarchy_outgoing(
    workspaces: &Workspaces,
    item: &lsp::HierarchyItem,
) -> Vec<lsp::CallHierarchyOutgoingCall> {
    let Some((workspace, rel_path)) = workspaces.resolve(&item.uri) else {
        return Vec::new();
    };
    let Some(file_data) = workspace.file(&rel_path) else {
        return Vec::new();
    };
    let headings = file_data.tree.headings();
    let links = file_data.tree.links(&rel_path);

    let item_line = item.selection_range.start.line;
    let item_level = hierarchy_item_level(item);

    // Find the end of this heading's section.
    let section_end: u32 = headings
        .iter()
        .find(|h| {
            let h_line = h.line.saturating_sub(1) as u32;
            h_line > item_line && h.level <= item_level
        })
        .map_or(u32::MAX, |h| h.line.saturating_sub(1) as u32);

    let root = workspace.root();
    let mut calls = Vec::new();

    for link in &links {
        let LinkKind::IntraProject { target, .. } = &link.kind else {
            continue;
        };
        let link_line = link.line.saturating_sub(1) as u32;
        if link_line < item_line || link_line >= section_end {
            continue;
        }

        let target_abs = root.join(target);
        let target_headings = workspace.file(target).map(|fd| fd.tree.headings());
        let target_item = target_headings
            .as_ref()
            .and_then(|h| h.first())
            .map_or_else(
                || file_hierarchy_item(&target_abs, target),
                |h| heading_to_hierarchy_item(h, &target_abs),
            );

        calls.push(lsp::CallHierarchyOutgoingCall {
            to: target_item,
            from_ranges: vec![lsp::Range {
                start: lsp::Position {
                    line: link_line,
                    character: 0,
                },
                end: lsp::Position {
                    line: link_line,
                    character: 0,
                },
            }],
        });
    }

    calls
}

// ---------------------------------------------------------------------------
// Document link (ticket 06)
// ---------------------------------------------------------------------------

/// Return clickable document links for all intra-project links.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn document_links(workspaces: &Workspaces, uri: &str) -> Vec<lsp::DocumentLink> {
    let Some((workspace, rel_path)) = workspaces.resolve(uri) else {
        return Vec::new();
    };
    let Some(file_data) = workspace.file(&rel_path) else {
        return Vec::new();
    };
    let file_links = file_data.tree.links(&rel_path);

    let root = workspace.root();
    let mut links = Vec::new();

    for link in &file_links {
        let target_uri = match &link.kind {
            LinkKind::IntraProject { target, .. } | LinkKind::NonMarkdown { target } => {
                path_to_uri(&root.join(target))
            }
            // Skip external and intra-document links.
            LinkKind::External { .. } | LinkKind::IntraDocument { .. } => continue,
        };
        let line = link.line.saturating_sub(1) as u32;
        links.push(lsp::DocumentLink {
            range: lsp::Range {
                start: lsp::Position { line, character: 0 },
                end: lsp::Position { line, character: 0 },
            },
            target: Some(target_uri),
        });
    }

    links
}

// ---------------------------------------------------------------------------
// Pull diagnostics (ticket 09)
// ---------------------------------------------------------------------------

/// Collect all diagnostics for a workspace: structural (unconditional) +
/// graph (gated by `.lattice.toml`).
fn collect_all_diagnostics(workspace: &Workspace) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    // Structural diagnostics: always run, no config required.
    let config = workspace.config();
    for (path, file_data) in workspace.files() {
        let file_exists = |target: &std::path::Path| workspace.file(target).is_some();
        diagnostics.extend(structural::collect(
            &file_data.tree,
            path,
            config,
            &file_exists,
        ));

        // Frontmatter parse errors are structural (unconditional).
        for pd in &file_data.parse_diagnostics {
            diagnostics.push(Diagnostic {
                file: path.clone(),
                line: pd.line,
                severity: Severity::Error,
                message: format!("frontmatter error: {}", pd.message),
            });
        }
    }

    // Graph diagnostics: only when .lattice.toml is present.
    if workspace.has_config() {
        diagnostics.extend(validation::collect_all(workspace));
    }

    diagnostics.sort_by(|a, b| a.file.cmp(&b.file).then(a.line.cmp(&b.line)));
    diagnostics
}

/// Return diagnostics for a single document.
fn document_diagnostic(workspaces: &Workspaces, uri: &str) -> lsp::FullDocumentDiagnosticReport {
    let items = if let Some((workspace, rel_path)) = workspaces.resolve(uri) {
        let all = collect_all_diagnostics(workspace);
        all.iter()
            .filter(|d| d.file == rel_path)
            .map(to_lsp_diagnostic)
            .collect()
    } else {
        Vec::new()
    };

    lsp::FullDocumentDiagnosticReport {
        kind: "full".to_string(),
        items,
    }
}

/// Return diagnostics for all files across all workspaces.
fn workspace_diagnostic(workspaces: &Workspaces) -> lsp::WorkspaceDiagnosticReport {
    let mut reports = Vec::new();

    for (root, workspace) in workspaces.iter() {
        let all = collect_all_diagnostics(workspace);
        let mut by_file: BTreeMap<PathBuf, Vec<lsp::Diagnostic>> = BTreeMap::new();

        for diag in &all {
            by_file
                .entry(diag.file.clone())
                .or_default()
                .push(to_lsp_diagnostic(diag));
        }

        for (rel_path, items) in by_file {
            reports.push(lsp::WorkspaceDocumentDiagnosticReport {
                kind: "full".to_string(),
                uri: path_to_uri(&root.join(rel_path)),
                items,
            });
        }
    }

    lsp::WorkspaceDiagnosticReport { items: reports }
}

// ---------------------------------------------------------------------------
// Hover preview (ticket 10)
// ---------------------------------------------------------------------------

/// Show a preview of the link target on hover.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn hover_preview(
    workspaces: &Workspaces,
    params: &lsp::TextDocumentPositionParams,
) -> Option<lsp::Hover> {
    let (workspace, rel_path) = workspaces.resolve(&params.text_document.uri)?;
    let file_data = workspace.file(&rel_path)?;
    let file_links = file_data.tree.links(&rel_path);

    // Find the link on the cursor's line.
    let cursor_line = params.position.line;
    let link = file_links
        .iter()
        .find(|l| l.line.saturating_sub(1) as u32 == cursor_line)?;

    let (target, fragment, predicate) = match &link.kind {
        LinkKind::IntraProject {
            target,
            fragment,
            predicate,
            ..
        } => (target.clone(), fragment.clone(), predicate.as_str()),
        LinkKind::NonMarkdown { target } => (target.clone(), None, "references"),
        // No hover for external or intra-document links.
        LinkKind::External { .. } | LinkKind::IntraDocument { .. } => return None,
    };

    let target_data = workspace.file(&target)?;

    let preview = build_hover_preview(target_data, fragment.as_deref());
    let header = format!("**{predicate}** → `{}`", target.display());

    Some(lsp::Hover {
        contents: lsp::MarkupContent {
            kind: "markdown".to_string(),
            value: format!("{header}\n\n---\n\n{preview}"),
        },
    })
}

/// Build a ~5 line preview from the target file content.
fn build_hover_preview(target_data: &crate::workspace::FileData, fragment: Option<&str>) -> String {
    let content = target_data.tree.source();
    let lines: Vec<&str> = content.lines().collect();
    let headings = target_data.tree.headings();

    // Determine the start line for the preview.
    let start = fragment.map_or_else(
        // No fragment — skip frontmatter.
        || target_data.frontmatter.as_ref().map_or(0, |fm| fm.end_line),
        // Fragment — find the matching heading.
        |frag| {
            headings
                .iter()
                .find(|h| heading_matches_fragment(h, frag))
                .map_or(0, |h| h.line.saturating_sub(1))
        },
    );

    lines
        .iter()
        .skip(start)
        .filter(|l| !l.trim().is_empty())
        .take(5)
        .copied()
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// Folding range (ticket 11)
// ---------------------------------------------------------------------------

/// Return folding ranges for headings and frontmatter.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn folding_ranges(workspaces: &Workspaces, uri: &str) -> Vec<lsp::FoldingRange> {
    let Some((workspace, rel_path)) = workspaces.resolve(uri) else {
        return Vec::new();
    };
    let Some(file_data) = workspace.file(&rel_path) else {
        return Vec::new();
    };

    let total_lines = file_data.tree.source().lines().count() as u32;

    let mut ranges = Vec::new();

    // Frontmatter folding range.
    if let Some(fm) = &file_data.frontmatter {
        let start = fm.start_line.saturating_sub(1) as u32;
        let end = fm.end_line.saturating_sub(1) as u32;
        if end > start {
            ranges.push(lsp::FoldingRange {
                start_line: start,
                end_line: end,
                kind: Some("region".to_string()),
            });
        }
    }

    // Heading folding ranges.
    let headings = file_data.tree.headings();
    for (i, heading) in headings.iter().enumerate() {
        let start = heading.line.saturating_sub(1) as u32;
        // End is the line before the next heading at same or higher level, or EOF.
        let end = headings[i + 1..]
            .iter()
            .find(|h| h.level <= heading.level)
            .map_or_else(
                || total_lines.saturating_sub(1),
                |h| (h.line.saturating_sub(1) as u32).saturating_sub(1),
            );
        if end > start {
            ranges.push(lsp::FoldingRange {
                start_line: start,
                end_line: end,
                kind: Some("region".to_string()),
            });
        }
    }

    ranges
}

// ---------------------------------------------------------------------------
// Formatting (ticket 12)
// ---------------------------------------------------------------------------

/// Format a document's backlink frontmatter.
///
/// Sorts predicate keys alphabetically, sorts paths within each predicate,
/// and normalizes whitespace. If the config specifies an external formatter,
/// pipes the full document through it after frontmatter sorting.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn format_document(workspaces: &Workspaces, uri: &str) -> Option<Vec<lsp::TextEdit>> {
    let (workspace, rel_path) = workspaces.resolve(uri)?;
    let file_data = workspace.file(&rel_path)?;

    let has_backlinks = file_data
        .frontmatter
        .as_ref()
        .is_some_and(|fm| !fm.backlinks.is_empty());

    let format_command = workspace.config().format_command.as_deref();

    // Nothing to do if there are no backlinks to sort and no external formatter.
    if !has_backlinks && format_command.is_none() {
        return None;
    }

    // Step 1: Sort frontmatter backlinks.
    let mut document = file_data.tree.source().to_string();
    if let Some(fm) = &file_data.frontmatter
        && !fm.backlinks.is_empty()
    {
        let mut sorted: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (pred, paths) in &fm.backlinks {
            let mut path_refs: Vec<&str> = paths.iter().map(String::as_str).collect();
            path_refs.sort_unstable();
            sorted.insert(pred.as_str(), path_refs);
        }

        let mut yaml = String::from("---\nbacklinks:\n");
        for (pred, paths) in &sorted {
            let _ = writeln!(yaml, "  {pred}:");
            for path in paths {
                let _ = writeln!(yaml, "    - {path}");
            }
        }
        yaml.push_str("---");

        document.replace_range(fm.byte_range.clone(), &yaml);
    }

    // Step 2: Pipe through external formatter if configured.
    if let Some(cmd) = format_command
        && let Some(formatted) = run_formatter(cmd, &document)
    {
        document = formatted;
    }

    // Replace the entire document.
    let source = file_data.tree.source();
    let total_lines = source.lines().count() as u32;
    let last_line_len = source.lines().last().map_or(0, str::len) as u32;

    let range = lsp::Range {
        start: lsp::Position {
            line: 0,
            character: 0,
        },
        end: lsp::Position {
            line: total_lines.saturating_sub(1),
            character: last_line_len,
        },
    };

    Some(vec![lsp::TextEdit {
        range,
        new_text: document,
    }])
}

/// Run an external formatter command, piping content through stdin/stdout.
///
/// The command is passed to `sh -c` so shell features (pipes, quoted args,
/// environment variables) work as expected.
fn run_formatter(command: &str, content: &str) -> Option<String> {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    let mut child = Command::new("sh")
        .args(["-c", command])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(content.as_bytes());
    }

    let output = child.wait_with_output().ok()?;
    if output.status.success() {
        String::from_utf8(output.stdout).ok()
    } else {
        tracing::warn!(
            "formatter exited with status {}: {}",
            output.status,
            command
        );
        None
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Convert a heading to a hierarchy item (used for both type and call hierarchy).
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn heading_to_hierarchy_item(heading: &Heading, abs_path: &Path) -> lsp::HierarchyItem {
    let line = heading.line.saturating_sub(1) as u32;
    let range = lsp::Range {
        start: lsp::Position { line, character: 0 },
        end: lsp::Position { line, character: 0 },
    };

    lsp::HierarchyItem {
        name: heading.text.clone(),
        kind: lsp::symbol_kind::CLASS,
        uri: path_to_uri(abs_path),
        range,
        selection_range: range,
        detail: Some(format!("H{}", heading.level)),
        data: None,
    }
}

/// Create a file-level hierarchy item when a link has no enclosing heading.
fn file_hierarchy_item(abs_path: &Path, rel_path: &Path) -> lsp::HierarchyItem {
    let range = lsp::Range::default();
    lsp::HierarchyItem {
        name: rel_path.display().to_string(),
        kind: lsp::symbol_kind::FILE,
        uri: path_to_uri(abs_path),
        range,
        selection_range: range,
        detail: None,
        data: None,
    }
}

/// Find the heading that encloses a given 1-based line number.
fn enclosing_heading(headings: &[Heading], line: usize) -> Option<&Heading> {
    headings.iter().rev().find(|h| h.line < line)
}

/// Extract the heading level from a hierarchy item's detail field.
fn hierarchy_item_level(item: &lsp::HierarchyItem) -> u8 {
    item.detail
        .as_deref()
        .and_then(|d| d.strip_prefix('H'))
        .and_then(|n| n.parse::<u8>().ok())
        .unwrap_or(1)
}

/// Find the classified [`Link`] whose span matches a node span.
///
/// Bridges the gap between `find_link_at_offset` (which finds the tree node)
/// and the classified links from `Tree::links` (which resolve targets).
fn find_classified_link(
    tree: &crate::block::Tree,
    rel_path: &Path,
    node_span: Span,
) -> Option<crate::block::Link> {
    tree.links(rel_path)
        .into_iter()
        .find(|l| l.span == node_span)
}

/// Convert an LSP 0-based position to a byte offset in `source`.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn lsp_position_to_byte_offset(source: &str, pos: lsp::Position) -> usize {
    let mut offset = 0;
    for (i, line) in source.lines().enumerate() {
        if i == pos.line as usize {
            return offset + (pos.character as usize).min(line.len());
        }
        offset += line.len() + 1; // +1 for newline
    }
    source.len()
}

/// Convert a byte `Span` to an LSP `Range`.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line/column values in markdown files won't exceed u32::MAX"
)]
fn span_to_lsp_range(source: &str, span: &Span) -> lsp::Range {
    let start = byte_offset_to_lsp_position(source, span.start);
    let end = byte_offset_to_lsp_position(source, span.end);
    lsp::Range { start, end }
}

/// Convert a byte offset to an LSP 0-based position.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line/column values in markdown files won't exceed u32::MAX"
)]
fn byte_offset_to_lsp_position(source: &str, offset: usize) -> lsp::Position {
    let offset = offset.min(source.len());
    let before = &source[..offset];
    let line = before.bytes().filter(|&b| b == b'\n').count() as u32;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let character = (offset - line_start) as u32;
    lsp::Position { line, character }
}

/// Extract the normalized reference label from a link's source text,
/// if the link uses reference-style syntax.
///
/// Reference-style links look like `[text][label]`, `[text][]`, or `[text]`
/// (shortcut). Inline links contain `(` after the `]`.
///
/// Uses [`inline::find_matching_bracket`] for correct handling of nested
/// brackets, backslash escapes, and backtick spans.
fn link_ref_label(source: &str, span: &Span) -> Option<String> {
    let raw = &source[span.start..span.end];

    // Skip image prefix.
    let text = raw.strip_prefix('!').unwrap_or(raw);
    if !text.starts_with('[') {
        return None;
    }

    // Find the closing `]` for the link text.
    let text_close = crate::inline::find_matching_bracket(text.as_bytes(), 0)?;
    let after = &text[text_close + 1..];

    // Inline link: [text](url)
    if after.starts_with('(') {
        return None;
    }

    // Full reference: [text][label]
    if after.starts_with('[') {
        let label_start = 1;
        let label_end = after.find(']').unwrap_or(after.len());
        let label_text = &after[label_start..label_end];
        if label_text.is_empty() {
            // Collapsed reference [text][] — label is the link text
            let link_text = &text[1..text_close];
            return Some(normalize_label(link_text));
        }
        return Some(normalize_label(label_text));
    }

    // Shortcut reference: [text] — label is the link text
    let link_text = &text[1..text_close];
    Some(normalize_label(link_text))
}

/// Check if the byte offset falls on a `ReferenceDef` node, returning
/// its normalized label.
fn ref_def_label_at_offset(tree: &crate::block::Tree, offset: usize) -> Option<String> {
    for node in tree.nodes() {
        if let ElementKind::ReferenceDef { label, .. } = &node.kind
            && node.span.start <= offset
            && offset < node.span.end
        {
            return Some(label.clone());
        }
    }
    None
}

/// Get the text of a 0-based line in the source.
fn source_line_at(source: &str, lsp_line: u32) -> &str {
    source.lines().nth(lsp_line as usize).unwrap_or("")
}

// ---------------------------------------------------------------------------
// Notifications
// ---------------------------------------------------------------------------

/// Dispatch a notification.
fn handle_notification(
    connection: &Connection,
    workspaces: &mut Workspaces,
    notif: Notification,
) -> Result<()> {
    match notif.method.as_str() {
        lsp::method::DID_OPEN => {
            let params: lsp::DidOpenTextDocumentParams = serde_json::from_value(notif.params)?;
            if let Some((ws, rel_path)) = workspaces.resolve_mut(&params.text_document.uri) {
                ws.update_content(&rel_path, &params.text_document.text);
            }
            publish_all_diagnostics(connection, workspaces)?;
        }
        lsp::method::DID_SAVE => {
            let params: lsp::DidSaveTextDocumentParams = serde_json::from_value(notif.params)?;
            if let Some((ws, rel_path)) = workspaces.resolve_mut(&params.text_document.uri) {
                if let Some(text) = &params.text {
                    ws.update_content(&rel_path, text);
                } else {
                    let _ = ws.update(&rel_path);
                }
            }
            publish_all_diagnostics(connection, workspaces)?;
        }
        lsp::method::DID_CHANGE => {
            let params: lsp::DidChangeTextDocumentParams = serde_json::from_value(notif.params)?;
            if let Some(change) = params.content_changes.into_iter().last() {
                if let Some((ws, rel_path)) = workspaces.resolve_mut(&params.text_document.uri) {
                    ws.update_content(&rel_path, &change.text);
                }
                publish_all_diagnostics(connection, workspaces)?;
            }
        }
        lsp::method::DID_CHANGE_WORKSPACE_FOLDERS => {
            let params: lsp::DidChangeWorkspaceFoldersParams =
                serde_json::from_value(notif.params)?;
            for removed in &params.event.removed {
                workspaces.remove(&removed.uri);
            }
            for added in &params.event.added {
                workspaces.add(&added.uri);
            }
            publish_all_diagnostics(connection, workspaces)?;
        }
        _ => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

/// Publish diagnostics for all files across all workspaces.
fn publish_all_diagnostics(connection: &Connection, workspaces: &Workspaces) -> Result<()> {
    for (root, workspace) in workspaces.iter() {
        let all_diagnostics = collect_all_diagnostics(workspace);

        let mut by_file: BTreeMap<PathBuf, Vec<lsp::Diagnostic>> = BTreeMap::new();

        for diag in &all_diagnostics {
            by_file
                .entry(diag.file.clone())
                .or_default()
                .push(to_lsp_diagnostic(diag));
        }

        let mut published: BTreeSet<PathBuf> = by_file.keys().cloned().collect();
        for path in workspace.files().keys() {
            published.insert(path.clone());
        }

        for rel_path in &published {
            let abs_path = root.join(rel_path);
            let uri = path_to_uri(&abs_path);
            let diagnostics = by_file.remove(rel_path).unwrap_or_default();

            let params = lsp::PublishDiagnosticsParams { uri, diagnostics };
            let notif = Notification::new(lsp::method::PUBLISH_DIAGNOSTICS.to_string(), params);
            connection.sender.send(Message::Notification(notif))?;
        }
    }

    Ok(())
}

/// Convert a Lattice diagnostic to an LSP diagnostic.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn to_lsp_diagnostic(diag: &Diagnostic) -> lsp::Diagnostic {
    let severity = match diag.severity {
        Severity::Error => lsp::diagnostic_severity::ERROR,
        Severity::Warning => lsp::diagnostic_severity::WARNING,
        Severity::Info => lsp::diagnostic_severity::INFORMATION,
        Severity::Hint => lsp::diagnostic_severity::HINT,
    };

    let line = diag.line.saturating_sub(1) as u32;
    let range = lsp::Range {
        start: lsp::Position { line, character: 0 },
        end: lsp::Position { line, character: 0 },
    };

    lsp::Diagnostic {
        range,
        severity: Some(severity),
        source: Some("lattice".to_string()),
        message: diag.message.clone(),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for clarity")]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;
    use crate::block::{HeadingId, Syntax};
    use crate::span::Span;

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    /// Build a test heading with default `text_span` and `syntax` fields.
    fn test_heading(line: usize, level: u8, text: &str, id: HeadingId) -> Heading {
        Heading {
            line,
            level,
            text: text.to_string(),
            id,
            text_span: Span::new(0, 0),
            syntax: Syntax::Markdown,
        }
    }

    /// Create a temp directory with `.git` marker and the given files.
    fn workspace_with_files(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("create temp dir");
        fs::create_dir(dir.path().join(".git")).expect("create .git");
        for (path, content) in files {
            let full = dir.path().join(path);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).expect("create parent dirs");
            }
            fs::write(&full, content).expect("write file");
        }
        dir
    }

    /// Build a `Workspaces` from a temp directory.
    fn scan_workspaces(dir: &tempfile::TempDir) -> Workspaces {
        let root = dir.path().to_path_buf();
        let ws = Workspace::scan(&root).expect("scan should succeed");
        Workspaces {
            inner: BTreeMap::from([(root, ws)]),
        }
    }

    /// Build a file URI from a temp directory and a relative path.
    fn file_uri(dir: &tempfile::TempDir, rel: &str) -> String {
        path_to_uri(&dir.path().join(rel))
    }

    // -----------------------------------------------------------------------
    // Existing tests: diagnostics and document symbols
    // -----------------------------------------------------------------------

    #[test]
    fn error_maps_to_lsp_error() {
        let diag = Diagnostic {
            file: PathBuf::from("a.md"),
            line: 3,
            severity: Severity::Error,
            message: "target does not exist".to_string(),
        };
        let d = to_lsp_diagnostic(&diag);
        assert_eq!(
            d.severity,
            Some(lsp::diagnostic_severity::ERROR),
            "error should map to LSP ERROR"
        );
        assert_eq!(d.range.start.line, 2, "line 3 should map to LSP line 2");
        assert_eq!(
            d.source.as_deref(),
            Some("lattice"),
            "source should be lattice"
        );
    }

    #[test]
    fn warning_maps_to_lsp_warning() {
        let diag = Diagnostic {
            file: PathBuf::from("b.md"),
            line: 1,
            severity: Severity::Warning,
            message: "missing backlink".to_string(),
        };
        let d = to_lsp_diagnostic(&diag);
        assert_eq!(
            d.severity,
            Some(lsp::diagnostic_severity::WARNING),
            "warning should map to LSP WARNING"
        );
        assert_eq!(d.range.start.line, 0, "line 1 should map to LSP line 0");
    }

    #[test]
    fn info_maps_to_lsp_information() {
        let diag = Diagnostic {
            file: PathBuf::from("c.md"),
            line: 5,
            severity: Severity::Info,
            message: "no explicit predicate".to_string(),
        };
        let d = to_lsp_diagnostic(&diag);
        assert_eq!(
            d.severity,
            Some(lsp::diagnostic_severity::INFORMATION),
            "info should map to LSP INFORMATION"
        );
    }

    #[test]
    fn heading_symbols_nest_by_level() {
        let tagged = vec![
            TaggedSymbol {
                level: 1,
                symbol: lsp::DocumentSymbol {
                    name: "Title".to_string(),
                    detail: None,
                    kind: lsp::symbol_kind::CLASS,
                    range: lsp::Range::default(),
                    selection_range: lsp::Range::default(),
                    children: None,
                },
            },
            TaggedSymbol {
                level: 2,
                symbol: lsp::DocumentSymbol {
                    name: "Section".to_string(),
                    detail: None,
                    kind: lsp::symbol_kind::CLASS,
                    range: lsp::Range::default(),
                    selection_range: lsp::Range::default(),
                    children: None,
                },
            },
            TaggedSymbol {
                level: 2,
                symbol: lsp::DocumentSymbol {
                    name: "Another".to_string(),
                    detail: None,
                    kind: lsp::symbol_kind::CLASS,
                    range: lsp::Range::default(),
                    selection_range: lsp::Range::default(),
                    children: None,
                },
            },
        ];

        let symbols = nest_by_heading_level(tagged);
        assert_eq!(symbols.len(), 1, "should have one top-level symbol");
        assert_eq!(symbols[0].name, "Title", "top-level should be the H1");
        let children = symbols[0]
            .children
            .as_ref()
            .expect("H1 should have children");
        assert_eq!(children.len(), 2, "H1 should have two H2 children");
        assert_eq!(children[0].name, "Section", "first child should be Section");
        assert_eq!(
            children[1].name, "Another",
            "second child should be Another"
        );
    }

    #[test]
    fn uri_to_path_extracts_path() {
        let path = uri_to_path("file:///home/user/project/doc.md");
        assert_eq!(
            path,
            PathBuf::from("/home/user/project/doc.md"),
            "should extract filesystem path from URI"
        );
    }

    #[test]
    fn path_to_uri_creates_file_uri() {
        let uri = path_to_uri(Path::new("/home/user/project/doc.md"));
        assert_eq!(
            uri, "file:///home/user/project/doc.md",
            "should create file:// URI"
        );
    }

    // -----------------------------------------------------------------------
    // Helper unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn heading_at_line_finds_match() {
        let headings = vec![
            test_heading(
                1,
                1,
                "Title",
                HeadingId::Computed {
                    github: "title".to_string(),
                    gitlab: "title".to_string(),
                    vscode: "title".to_string(),
                },
            ),
            test_heading(
                5,
                2,
                "Section",
                HeadingId::Computed {
                    github: "section".to_string(),
                    gitlab: "section".to_string(),
                    vscode: "section".to_string(),
                },
            ),
        ];

        let h = heading_at_line(&headings, 0);
        assert_eq!(
            h.map(|h| h.text.as_str()),
            Some("Title"),
            "LSP line 0 should match line 1 heading"
        );

        let h = heading_at_line(&headings, 4);
        assert_eq!(
            h.map(|h| h.text.as_str()),
            Some("Section"),
            "LSP line 4 should match line 5 heading"
        );

        assert!(
            heading_at_line(&headings, 2).is_none(),
            "no heading on line 3"
        );
    }

    #[test]
    fn heading_matches_explicit_fragment() {
        let heading = test_heading(1, 1, "Custom ID", HeadingId::Explicit("my-id".to_string()));
        assert!(
            heading_matches_fragment(&heading, "my-id"),
            "explicit ID should match"
        );
        assert!(
            !heading_matches_fragment(&heading, "custom-id"),
            "slug should not match explicit ID"
        );
    }

    #[test]
    fn heading_matches_computed_fragments() {
        let heading = test_heading(
            1,
            2,
            "Hello World!",
            HeadingId::Computed {
                github: "hello-world".to_string(),
                gitlab: "hello-world-1".to_string(),
                vscode: "hello-world-2".to_string(),
            },
        );
        assert!(
            heading_matches_fragment(&heading, "hello-world"),
            "github slug should match"
        );
        assert!(
            heading_matches_fragment(&heading, "hello-world-1"),
            "gitlab slug should match"
        );
        assert!(
            heading_matches_fragment(&heading, "hello-world-2"),
            "vscode slug should match"
        );
        assert!(
            !heading_matches_fragment(&heading, "other"),
            "unrelated fragment should not match"
        );
    }

    #[test]
    fn enclosing_heading_finds_nearest_above() {
        let headings = vec![
            test_heading(1, 1, "Title", HeadingId::Explicit("title".to_string())),
            test_heading(5, 2, "Section", HeadingId::Explicit("section".to_string())),
        ];

        assert!(
            enclosing_heading(&headings, 1).is_none(),
            "line 1 has no enclosing heading (it IS line 1)"
        );
        assert_eq!(
            enclosing_heading(&headings, 3).map(|h| h.text.as_str()),
            Some("Title"),
            "line 3 is enclosed by Title"
        );
        assert_eq!(
            enclosing_heading(&headings, 8).map(|h| h.text.as_str()),
            Some("Section"),
            "line 8 is enclosed by Section"
        );
    }

    #[test]
    fn hierarchy_item_level_parses_detail() {
        let item = lsp::HierarchyItem {
            name: String::new(),
            kind: lsp::symbol_kind::CLASS,
            uri: String::new(),
            range: lsp::Range::default(),
            selection_range: lsp::Range::default(),
            detail: Some("H3".to_string()),
            data: None,
        };
        assert_eq!(hierarchy_item_level(&item), 3, "should parse H3 as level 3");

        let no_detail = lsp::HierarchyItem {
            detail: None,
            ..item
        };
        assert_eq!(
            hierarchy_item_level(&no_detail),
            1,
            "missing detail should default to 1"
        );
    }

    // -----------------------------------------------------------------------
    // Workspace symbols (ticket 13)
    // -----------------------------------------------------------------------

    #[test]
    fn workspace_symbols_returns_all_on_empty_query() {
        let dir = workspace_with_files(&[("a.md", "# Alpha\n## Beta\n"), ("b.md", "# Gamma\n")]);
        let workspaces = scan_workspaces(&dir);

        let symbols = workspace_symbols(&workspaces, "");
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"H1: Alpha"), "should contain H1: Alpha");
        assert!(names.contains(&"H2: Beta"), "should contain H2: Beta");
        assert!(names.contains(&"H1: Gamma"), "should contain H1: Gamma");
        assert_eq!(symbols.len(), 3, "should return all 3 headings");
    }

    #[test]
    fn workspace_symbols_filters_by_query() {
        let dir = workspace_with_files(&[("a.md", "# Alpha\n## Beta\n"), ("b.md", "# Gamma\n")]);
        let workspaces = scan_workspaces(&dir);

        let symbols = workspace_symbols(&workspaces, "alph");
        assert_eq!(symbols.len(), 1, "should match only Alpha");
        assert_eq!(
            symbols[0].name, "H1: Alpha",
            "should be case-insensitive match"
        );
    }

    #[test]
    fn workspace_symbols_includes_container_name() {
        let dir = workspace_with_files(&[("docs/guide.md", "# Guide\n")]);
        let workspaces = scan_workspaces(&dir);

        let symbols = workspace_symbols(&workspaces, "");
        assert_eq!(symbols.len(), 1, "should find one heading");
        assert_eq!(
            symbols[0].container_name.as_deref(),
            Some("docs/guide.md"),
            "container should be the relative file path"
        );
    }

    // -----------------------------------------------------------------------
    // prepareRename / rename (ticket 04)
    // -----------------------------------------------------------------------

    #[test]
    fn prepare_rename_returns_heading_range() {
        let dir = workspace_with_files(&[("a.md", "# Title\n\nSome text\n")]);
        let workspaces = scan_workspaces(&dir);

        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "a.md"),
            },
            position: lsp::Position {
                line: 0,
                character: 0,
            },
        };
        let range = prepare_rename(&workspaces, &params).expect("should find heading");
        // "# Title" → text starts at character 2 (after "# "), length 5
        assert_eq!(range.start.character, 2, "text starts after '# '");
        assert_eq!(range.end.character, 7, "text ends at 2 + len('Title')");
    }

    #[test]
    fn prepare_rename_returns_none_on_prose() {
        let dir = workspace_with_files(&[("a.md", "# Title\n\nSome text\n")]);
        let workspaces = scan_workspaces(&dir);

        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "a.md"),
            },
            position: lsp::Position {
                line: 2,
                character: 0,
            },
        };
        assert!(
            prepare_rename(&workspaces, &params).is_none(),
            "prose line should not be renamable"
        );
    }

    #[test]
    fn rename_produces_correct_edit() {
        let dir = workspace_with_files(&[("a.md", "## Old Name\n")]);
        let workspaces = scan_workspaces(&dir);
        let uri = file_uri(&dir, "a.md");

        let params = lsp::RenameParams {
            text_document: lsp::TextDocumentIdentifier { uri: uri.clone() },
            position: lsp::Position {
                line: 0,
                character: 0,
            },
            new_name: "New Name".to_string(),
        };
        let edit = do_rename(&workspaces, &params).expect("should produce edit");
        let changes = edit.changes.expect("should have changes");
        let edits = changes.get(&uri).expect("should have edits for the file");
        assert_eq!(edits.len(), 1, "should have one edit");
        assert_eq!(edits[0].new_text, "New Name", "new text should match");
        // "## Old Name" → text starts at 3 (after "## "), length 8
        assert_eq!(edits[0].range.start.character, 3, "edit starts after '## '");
        assert_eq!(
            edits[0].range.end.character, 11,
            "edit ends at 3 + len('Old Name')"
        );
    }

    // -----------------------------------------------------------------------
    // Find references (ticket 05)
    // -----------------------------------------------------------------------

    #[test]
    fn find_references_returns_linking_files() {
        let dir = workspace_with_files(&[
            ("a.md", "# A\n\n[see B](b.md \"references\")\n"),
            ("b.md", "# B\n\nSome content\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        // Cursor on a non-heading line returns all links targeting the file.
        let params = lsp::ReferenceParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "b.md"),
            },
            position: lsp::Position {
                line: 2,
                character: 0,
            },
        };
        let locations = find_references(&workspaces, &params);
        assert_eq!(locations.len(), 1, "b.md should have one reference");
        assert!(
            locations[0].uri.ends_with("a.md"),
            "reference should come from a.md"
        );
    }

    #[test]
    fn find_references_on_heading_filters_by_fragment() {
        let dir = workspace_with_files(&[
            (
                "a.md",
                "# A\n\n[whole file](b.md \"references\")\n[section](b.md#details \"references\")\n",
            ),
            ("b.md", "# B\n\n## Details\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        // Cursor on "## Details" heading (line 3 in file, LSP line 2).
        let params = lsp::ReferenceParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "b.md"),
            },
            position: lsp::Position {
                line: 2,
                character: 0,
            },
        };
        let locations = find_references(&workspaces, &params);
        assert_eq!(
            locations.len(),
            1,
            "only the fragment link should match, not the whole-file link"
        );
    }

    #[test]
    fn find_references_no_links_returns_empty() {
        let dir = workspace_with_files(&[("a.md", "# A\n"), ("b.md", "# B\n")]);
        let workspaces = scan_workspaces(&dir);

        let params = lsp::ReferenceParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "a.md"),
            },
            position: lsp::Position {
                line: 0,
                character: 0,
            },
        };
        assert!(
            find_references(&workspaces, &params).is_empty(),
            "no links to a.md should mean empty results"
        );
    }

    // -----------------------------------------------------------------------
    // Type hierarchy (ticket 08)
    // -----------------------------------------------------------------------

    #[test]
    fn prepare_type_hierarchy_on_heading() {
        let dir = workspace_with_files(&[("a.md", "# Title\n\n## Section\n")]);
        let workspaces = scan_workspaces(&dir);

        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "a.md"),
            },
            position: lsp::Position {
                line: 2,
                character: 0,
            },
        };
        let items = prepare_type_hierarchy(&workspaces, &params).expect("should find heading");
        assert_eq!(items.len(), 1, "should return one item");
        assert_eq!(items[0].name, "Section", "should be the H2");
        assert_eq!(
            items[0].detail.as_deref(),
            Some("H2"),
            "detail should encode level"
        );
    }

    #[test]
    fn type_hierarchy_supertypes_returns_parent() {
        let dir = workspace_with_files(&[("a.md", "# Title\n\n## Section\n\n### Sub\n")]);
        let workspaces = scan_workspaces(&dir);

        // Start from the H3.
        let h3 = test_heading(
            5,
            3,
            "Sub",
            HeadingId::Computed {
                github: "sub".to_string(),
                gitlab: "sub".to_string(),
                vscode: "sub".to_string(),
            },
        );
        let h3_item = heading_to_hierarchy_item(&h3, &dir.path().join("a.md"));
        let parents =
            type_hierarchy_supertypes(&workspaces, &h3_item).expect("should return parents");
        assert_eq!(parents.len(), 1, "H3 should have one parent");
        assert_eq!(parents[0].name, "Section", "parent should be the H2");
    }

    #[test]
    fn type_hierarchy_subtypes_returns_children() {
        let dir = workspace_with_files(&[("a.md", "# Title\n\n## One\n\n## Two\n\n### Nested\n")]);
        let workspaces = scan_workspaces(&dir);

        // Start from the H1.
        let h1 = test_heading(
            1,
            1,
            "Title",
            HeadingId::Computed {
                github: "title".to_string(),
                gitlab: "title".to_string(),
                vscode: "title".to_string(),
            },
        );
        let h1_item = heading_to_hierarchy_item(&h1, &dir.path().join("a.md"));
        let children =
            type_hierarchy_subtypes(&workspaces, &h1_item).expect("should return children");
        let names: Vec<&str> = children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["One", "Two"], "H1 children should be the H2s");
    }

    #[test]
    fn type_hierarchy_h1_has_no_supertypes() {
        let dir = workspace_with_files(&[("a.md", "# Title\n")]);
        let workspaces = scan_workspaces(&dir);

        let h1 = test_heading(
            1,
            1,
            "Title",
            HeadingId::Computed {
                github: "title".to_string(),
                gitlab: "title".to_string(),
                vscode: "title".to_string(),
            },
        );
        let h1_item = heading_to_hierarchy_item(&h1, &dir.path().join("a.md"));
        let parents =
            type_hierarchy_supertypes(&workspaces, &h1_item).expect("should return empty");
        assert!(parents.is_empty(), "H1 should have no supertypes");
    }

    // -----------------------------------------------------------------------
    // Call hierarchy (ticket 07)
    // -----------------------------------------------------------------------

    #[test]
    fn call_hierarchy_outgoing_finds_links_in_section() {
        let dir = workspace_with_files(&[
            ("a.md", "# A\n\n[see B](b.md \"references\")\n"),
            ("b.md", "# B\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        let h1 = test_heading(
            1,
            1,
            "A",
            HeadingId::Computed {
                github: "a".to_string(),
                gitlab: "a".to_string(),
                vscode: "a".to_string(),
            },
        );
        let h1_item = heading_to_hierarchy_item(&h1, &dir.path().join("a.md"));
        let calls = call_hierarchy_outgoing(&workspaces, &h1_item);
        assert_eq!(calls.len(), 1, "should have one outgoing call");
        assert_eq!(calls[0].to.name, "B", "outgoing call should target B");
    }

    #[test]
    fn call_hierarchy_incoming_finds_callers() {
        let dir = workspace_with_files(&[
            ("a.md", "# A\n\n[see B](b.md \"references\")\n"),
            ("b.md", "# B\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        let b = test_heading(
            1,
            1,
            "B",
            HeadingId::Computed {
                github: "b".to_string(),
                gitlab: "b".to_string(),
                vscode: "b".to_string(),
            },
        );
        let b_item = heading_to_hierarchy_item(&b, &dir.path().join("b.md"));
        let calls = call_hierarchy_incoming(&workspaces, &b_item);
        assert_eq!(calls.len(), 1, "should have one incoming call");
        assert_eq!(
            calls[0].from.name, "A",
            "incoming call should come from A's heading"
        );
    }

    #[test]
    fn call_hierarchy_outgoing_scoped_to_section() {
        let dir = workspace_with_files(&[
            (
                "a.md",
                "# A\n\n## S1\n\n[link1](b.md \"references\")\n\n## S2\n\n[link2](c.md \"references\")\n",
            ),
            ("b.md", "# B\n"),
            ("c.md", "# C\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        // Ask for outgoing calls from S1 only.
        let s1 = test_heading(
            3,
            2,
            "S1",
            HeadingId::Computed {
                github: "s1".to_string(),
                gitlab: "s1".to_string(),
                vscode: "s1".to_string(),
            },
        );
        let s1_item = heading_to_hierarchy_item(&s1, &dir.path().join("a.md"));
        let calls = call_hierarchy_outgoing(&workspaces, &s1_item);
        assert_eq!(calls.len(), 1, "S1 should have one outgoing call");
        assert_eq!(calls[0].to.name, "B", "S1's link goes to B, not C");
    }

    #[test]
    fn call_hierarchy_incoming_uses_file_item_when_no_heading() {
        // Link appears before any heading in the file.
        let dir = workspace_with_files(&[
            ("a.md", "[see B](b.md \"references\")\n\n# A\n"),
            ("b.md", "# B\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        let b = test_heading(
            1,
            1,
            "B",
            HeadingId::Computed {
                github: "b".to_string(),
                gitlab: "b".to_string(),
                vscode: "b".to_string(),
            },
        );
        let b_item = heading_to_hierarchy_item(&b, &dir.path().join("b.md"));
        let calls = call_hierarchy_incoming(&workspaces, &b_item);
        assert_eq!(calls.len(), 1, "should have one incoming call");
        assert_eq!(
            calls[0].from.kind,
            lsp::symbol_kind::FILE,
            "caller with no enclosing heading should be a FILE item"
        );
    }

    // -----------------------------------------------------------------------
    // Document link (ticket 06)
    // -----------------------------------------------------------------------

    #[test]
    fn document_links_returns_intra_project_links() {
        let dir = workspace_with_files(&[
            ("a.md", "# A\n\n[see B](b.md \"references\")\n"),
            ("b.md", "# B\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        let links = document_links(&workspaces, &file_uri(&dir, "a.md"));
        assert_eq!(links.len(), 1, "should return one document link");
        let target = links[0].target.as_ref().expect("should have target URI");
        assert!(target.ends_with("b.md"), "target should point to b.md");
    }

    #[test]
    fn document_links_skips_external() {
        let dir =
            workspace_with_files(&[("a.md", "# A\n\n[ext](https://example.com)\n[b](b.md)\n")]);
        let workspaces = scan_workspaces(&dir);

        let links = document_links(&workspaces, &file_uri(&dir, "a.md"));
        // Only the intra-project link to b.md, not the https link.
        assert_eq!(links.len(), 1, "should skip external links");
    }

    // -----------------------------------------------------------------------
    // Pull diagnostics (ticket 09)
    // -----------------------------------------------------------------------

    #[test]
    fn document_diagnostic_returns_file_errors() {
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            ("a.md", "# A\n\n[broken](nonexistent.md \"references\")\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        let report = document_diagnostic(&workspaces, &file_uri(&dir, "a.md"));
        assert_eq!(report.kind, "full", "report kind should be full");
        assert!(
            !report.items.is_empty(),
            "should have diagnostics for broken link"
        );
    }

    #[test]
    fn document_diagnostic_clean_file_returns_empty() {
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            ("a.md", "# A\n\n[see B](b.md \"references\")\n"),
            (
                "b.md",
                "---\nbacklinks:\n  referenced_by:\n    - a.md\n---\n\n# B\n",
            ),
        ]);
        let workspaces = scan_workspaces(&dir);

        let report = document_diagnostic(&workspaces, &file_uri(&dir, "b.md"));
        assert!(
            report.items.is_empty(),
            "clean file should have no diagnostics"
        );
    }

    #[test]
    fn workspace_diagnostic_covers_all_files() {
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            ("a.md", "# A\n\n[broken](nonexistent.md \"references\")\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        let report = workspace_diagnostic(&workspaces);
        assert!(
            !report.items.is_empty(),
            "workspace diagnostic should include reports"
        );
    }

    // -----------------------------------------------------------------------
    // Hover preview (ticket 10)
    // -----------------------------------------------------------------------

    #[test]
    fn hover_on_link_shows_preview() {
        let dir = workspace_with_files(&[
            ("a.md", "# A\n\n[see B](b.md \"supersedes\")\n"),
            ("b.md", "# B\n\nFirst line.\n\nSecond line.\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "a.md"),
            },
            position: lsp::Position {
                line: 2,
                character: 0,
            },
        };
        let hover = hover_preview(&workspaces, &params).expect("should produce hover");
        assert!(
            hover.contents.value.contains("supersedes"),
            "hover should include predicate"
        );
        assert!(
            hover.contents.value.contains("# B"),
            "hover should include target content"
        );
    }

    #[test]
    fn hover_on_fragment_link_shows_heading_content() {
        let dir = workspace_with_files(&[
            ("a.md", "# A\n\n[details](b.md#details \"references\")\n"),
            ("b.md", "# B\n\nPreamble.\n\n## Details\n\nThe details.\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "a.md"),
            },
            position: lsp::Position {
                line: 2,
                character: 0,
            },
        };
        let hover = hover_preview(&workspaces, &params).expect("should produce hover");
        assert!(
            hover.contents.value.contains("## Details"),
            "hover should start from the fragment heading"
        );
    }

    #[test]
    fn hover_on_prose_returns_none() {
        let dir = workspace_with_files(&[("a.md", "# A\n\nJust text.\n")]);
        let workspaces = scan_workspaces(&dir);

        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "a.md"),
            },
            position: lsp::Position {
                line: 2,
                character: 0,
            },
        };
        assert!(
            hover_preview(&workspaces, &params).is_none(),
            "prose should not produce hover"
        );
    }

    // -----------------------------------------------------------------------
    // Folding range (ticket 11)
    // -----------------------------------------------------------------------

    #[test]
    fn folding_ranges_for_headings() {
        let dir = workspace_with_files(&[("a.md", "# Title\n\nContent\n\n## Section\n\nMore\n")]);
        let workspaces = scan_workspaces(&dir);

        let ranges = folding_ranges(&workspaces, &file_uri(&dir, "a.md"));
        assert!(
            ranges.len() >= 2,
            "should have folding ranges for H1 and H2"
        );
        // H1 should fold from line 0.
        assert_eq!(ranges[0].start_line, 0, "H1 folding should start at line 0");
    }

    #[test]
    fn folding_ranges_include_frontmatter() {
        let dir = workspace_with_files(&[(
            "a.md",
            "---\nbacklinks:\n  referenced_by:\n    - b.md\n---\n\n# Title\n",
        )]);
        let workspaces = scan_workspaces(&dir);

        let ranges = folding_ranges(&workspaces, &file_uri(&dir, "a.md"));
        let fm_range = ranges
            .iter()
            .find(|r| r.start_line == 0)
            .expect("should have frontmatter folding range");
        assert!(
            fm_range.end_line >= 4,
            "frontmatter fold should cover the --- delimiters"
        );
    }

    // -----------------------------------------------------------------------
    // Formatting (ticket 12)
    // -----------------------------------------------------------------------

    #[test]
    fn format_sorts_backlink_predicates() {
        let dir = workspace_with_files(&[(
            "a.md",
            "---\nbacklinks:\n  referenced_by:\n    - c.md\n  amended_by:\n    - b.md\n---\n\n# A\n",
        )]);
        let workspaces = scan_workspaces(&dir);

        let edits =
            format_document(&workspaces, &file_uri(&dir, "a.md")).expect("should produce edits");
        assert_eq!(edits.len(), 1, "should have one edit replacing frontmatter");
        let new_text = &edits[0].new_text;
        let amended_pos = new_text
            .find("amended_by")
            .expect("should contain amended_by");
        let referenced_pos = new_text
            .find("referenced_by")
            .expect("should contain referenced_by");
        assert!(
            amended_pos < referenced_pos,
            "amended_by should come before referenced_by (alphabetical)"
        );
    }

    #[test]
    fn format_sorts_paths_within_predicate() {
        let dir = workspace_with_files(&[(
            "a.md",
            "---\nbacklinks:\n  referenced_by:\n    - z.md\n    - a.md\n---\n\n# A\n",
        )]);
        let workspaces = scan_workspaces(&dir);

        let edits =
            format_document(&workspaces, &file_uri(&dir, "a.md")).expect("should produce edits");
        let new_text = &edits[0].new_text;
        let a_pos = new_text.find("a.md").expect("should contain a.md");
        let z_pos = new_text.find("z.md").expect("should contain z.md");
        assert!(a_pos < z_pos, "a.md should come before z.md (alphabetical)");
    }

    #[test]
    fn format_returns_none_without_backlinks() {
        let dir = workspace_with_files(&[("a.md", "# A\n\nNo frontmatter.\n")]);
        let workspaces = scan_workspaces(&dir);

        assert!(
            format_document(&workspaces, &file_uri(&dir, "a.md")).is_none(),
            "no frontmatter should mean no formatting edits"
        );
    }

    // -----------------------------------------------------------------------
    // Navigation — declaration (ticket 08)
    // -----------------------------------------------------------------------

    #[test]
    fn declaration_reference_link_goes_to_def() {
        let dir = workspace_with_files(&[(
            "a.md",
            "# A\n\n[see B][ref]\n\n[ref]: b.md \"references\"\n",
        )]);
        let workspaces = scan_workspaces(&dir);

        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "a.md"),
            },
            position: lsp::Position {
                line: 2,
                character: 5,
            },
        };
        let loc = go_to_declaration(&workspaces, &params).expect("should find declaration");
        assert!(
            loc.uri.ends_with("a.md"),
            "declaration should be in the same file"
        );
        assert_eq!(
            loc.range.start.line, 4,
            "declaration should point to the ref def line"
        );
    }

    // -----------------------------------------------------------------------
    // Symbol emission (ticket 09)
    // -----------------------------------------------------------------------

    /// Parse content and return document symbols.
    fn symbols_for(content: &str) -> Vec<lsp::DocumentSymbol> {
        let dir = workspace_with_files(&[("test.md", content)]);
        let workspaces = scan_workspaces(&dir);
        document_symbols(&workspaces, &file_uri(&dir, "test.md")).expect("should produce symbols")
    }

    /// Recursively find all symbols matching a predicate.
    fn find_symbols(
        syms: &[lsp::DocumentSymbol],
        pred: &dyn Fn(&lsp::DocumentSymbol) -> bool,
    ) -> Vec<lsp::DocumentSymbol> {
        let mut found = Vec::new();
        for sym in syms {
            if pred(sym) {
                found.push(sym.clone());
            }
            if let Some(children) = &sym.children {
                found.extend(find_symbols(children, pred));
            }
        }
        found
    }

    #[test]
    fn heading_emits_class_kind() {
        let syms = symbols_for("# Title\n\n## Section\n");
        assert_eq!(syms.len(), 1, "should have one top-level heading");
        assert_eq!(
            syms[0].kind,
            lsp::symbol_kind::CLASS,
            "heading should be CLASS"
        );
        assert_eq!(syms[0].name, "H1: Title", "heading name should embed level");
    }

    #[test]
    fn heading_nested_levels_in_name() {
        let syms = symbols_for("# Top\n\n## Mid\n\n### Deep\n");
        let all = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::CLASS);
        assert_eq!(all.len(), 3, "should have three headings");
        assert_eq!(all[0].name, "H1: Top", "H1 should have level in name");
        assert_eq!(all[1].name, "H2: Mid", "H2 should have level in name");
        assert_eq!(all[2].name, "H3: Deep", "H3 should have level in name");
    }

    #[test]
    fn link_emits_function_kind() {
        let syms = symbols_for("# H\n\n[text](other.md \"references\")\n");
        let links = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::FUNCTION);
        assert_eq!(links.len(), 1, "should have one link symbol");
        assert_eq!(
            links[0].name, "Link: references(other.md)",
            "link name should have Link: prefix"
        );
        assert_eq!(
            links[0].detail.as_deref(),
            Some("text"),
            "link detail should be display text"
        );
    }

    #[test]
    fn link_without_predicate_defaults_to_references() {
        let syms = symbols_for("[go](other.md)\n");
        let links = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::FUNCTION);
        assert_eq!(links.len(), 1, "should have one link");
        assert!(
            links[0].name.starts_with("Link: references("),
            "should default to references predicate"
        );
    }

    #[test]
    fn image_emits_file_kind() {
        let syms = symbols_for("![alt text](image.png)\n");
        let images = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::FILE);
        assert_eq!(images.len(), 1, "should have one image symbol");
        assert_eq!(
            images[0].name, "File: image.png",
            "image name should be File: url"
        );
        assert_eq!(
            images[0].detail.as_deref(),
            Some("image"),
            "image detail should be 'image'"
        );
    }

    #[test]
    fn ordered_list_emits_struct() {
        let syms = symbols_for("1. first\n2. second\n");
        let lists = find_symbols(&syms, &|s| {
            s.kind == lsp::symbol_kind::STRUCT && s.name.starts_with("Ordered List")
        });
        assert_eq!(lists.len(), 1, "should have one ordered list");
        assert_eq!(
            lists[0].name, "Ordered List",
            "ordered list name should be 'Ordered List'"
        );
        assert_eq!(
            lists[0].detail.as_deref(),
            Some("2"),
            "ordered list detail should be item count"
        );
    }

    #[test]
    fn unordered_list_emits_struct() {
        let syms = symbols_for("- alpha\n- beta\n");
        let lists = find_symbols(&syms, &|s| {
            s.kind == lsp::symbol_kind::STRUCT && s.name == "List"
        });
        assert_eq!(lists.len(), 1, "should have one unordered list");
        assert_eq!(
            lists[0].detail.as_deref(),
            Some("2"),
            "list detail should be item count"
        );
    }

    #[test]
    fn flat_list_has_no_children() {
        let syms = symbols_for("- alpha\n- beta\n- gamma\n");
        let lists = find_symbols(&syms, &|s| {
            s.kind == lsp::symbol_kind::STRUCT && s.name == "List"
        });
        assert_eq!(lists.len(), 1, "should have one list");
        assert!(
            lists[0].children.is_none(),
            "flat list should have no children"
        );
        assert_eq!(
            lists[0].detail.as_deref(),
            Some("3"),
            "detail should be item count"
        );
    }

    #[test]
    fn nested_list_emits_struct_children() {
        let syms = symbols_for("- parent\n  - child\n");
        let top = find_symbols(&syms, &|s| {
            s.kind == lsp::symbol_kind::STRUCT && s.name == "List"
        });
        assert_eq!(top.len(), 1, "should have one top-level list");
        assert_eq!(top[0].detail.as_deref(), Some("1"), "top list has 1 item");
        let children = top[0]
            .children
            .as_ref()
            .expect("nested list should have children");
        let sub_list = children
            .iter()
            .find(|c| c.name == "List: parent")
            .expect("should have sub-list named by parent item");
        assert_eq!(
            sub_list.kind,
            lsp::symbol_kind::STRUCT,
            "sub-list should be Struct"
        );
        assert_eq!(
            sub_list.detail.as_deref(),
            Some("1"),
            "sub-list should have item count"
        );
    }

    #[test]
    fn deeply_nested_lists_preserve_hierarchy() {
        let syms = symbols_for("- A\n  - B\n    - C\n");
        let top = find_symbols(&syms, &|s| {
            s.kind == lsp::symbol_kind::STRUCT && s.name == "List"
        });
        assert_eq!(top.len(), 1, "should have one top-level list");
        let level1 = top[0]
            .children
            .as_ref()
            .expect("should have nested children");
        assert_eq!(level1.len(), 1, "one sub-list under A");
        assert_eq!(level1[0].name, "List: A", "sub-list named by parent A");
        let level2 = level1[0]
            .children
            .as_ref()
            .expect("should have deeper nesting");
        assert_eq!(level2.len(), 1, "one sub-sub-list under B");
        assert_eq!(level2[0].name, "List: B", "sub-sub-list named by parent B");
        assert_eq!(
            level2[0].detail.as_deref(),
            Some("1"),
            "deepest list has 1 item"
        );
    }

    #[test]
    fn table_emits_struct_with_field_children() {
        let syms = symbols_for(
            "| status | issue |\n|--------|-------|\n| open | bug |\n| closed | fix |\n",
        );
        let tables = find_symbols(&syms, &|s| {
            s.kind == lsp::symbol_kind::STRUCT && s.name == "Table"
        });
        assert_eq!(tables.len(), 1, "should have one table");
        assert_eq!(
            tables[0].detail.as_deref(),
            Some("2"),
            "table detail should be data row count"
        );
        let children = tables[0]
            .children
            .as_ref()
            .expect("table should have Field children");
        assert_eq!(children.len(), 2, "should have two Field children");
        assert_eq!(
            children[0].name, "Field: status",
            "first field should be status"
        );
        assert_eq!(
            children[1].name, "Field: issue",
            "second field should be issue"
        );
        assert!(
            children.iter().all(|c| c.kind == lsp::symbol_kind::FIELD),
            "all children should be Field kind"
        );
    }

    #[test]
    fn code_block_emits_object_no_property_child() {
        let syms = symbols_for("```rust\nfn main() {}\n```\n");
        let blocks = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::OBJECT);
        assert_eq!(blocks.len(), 1, "should have one code block");
        assert_eq!(
            blocks[0].name, "CodeBlock: rust",
            "code block name should include language"
        );
        assert!(
            blocks[0].children.is_none(),
            "code block should have no children"
        );
    }

    #[test]
    fn code_block_with_title_in_detail() {
        let syms = symbols_for("```rust title=config.toml\nlet x = 1;\n```\n");
        let blocks = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::OBJECT);
        assert_eq!(blocks.len(), 1, "should have one code block");
        assert_eq!(
            blocks[0].name, "CodeBlock: rust",
            "code block name should include language"
        );
        assert_eq!(
            blocks[0].detail.as_deref(),
            Some("title=config.toml"),
            "code block detail should be the title"
        );
    }

    #[test]
    fn code_block_without_language() {
        let syms = symbols_for("```\nsome code\n```\n");
        let blocks = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::OBJECT);
        assert_eq!(blocks.len(), 1, "should have one code block");
        assert_eq!(
            blocks[0].name, "CodeBlock",
            "unnamed code block should be 'CodeBlock'"
        );
    }

    #[test]
    fn blockquote_emits_module_named_blockquote() {
        let syms = symbols_for("> quoted text\n");
        let quotes = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::MODULE);
        assert_eq!(quotes.len(), 1, "should have one blockquote");
        assert_eq!(
            quotes[0].name, "Blockquote",
            "blockquote name should be 'Blockquote'"
        );
    }

    #[test]
    fn admonition_emits_module_with_prefix() {
        let syms = symbols_for("> [!WARNING]\n> Be careful!\n");
        let modules = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::MODULE);
        assert_eq!(modules.len(), 1, "should have one admonition");
        assert_eq!(
            modules[0].name, "Admonition: WARNING",
            "admonition name should have prefix"
        );
        assert!(
            modules[0].detail.is_none(),
            "admonition should have no detail"
        );
    }

    #[test]
    fn html_div_warning_emits_admonition_module() {
        let syms = symbols_for("<div class=\"warning\">\n\nBe careful!\n\n</div>\n");
        let modules = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::MODULE);
        assert_eq!(modules.len(), 1, "should have one admonition");
        assert_eq!(
            modules[0].name, "Admonition: WARNING",
            "HTML admonition name should be the type"
        );
    }

    #[test]
    fn html_video_emits_file() {
        let syms = symbols_for("<video src=\"vid.mp4\"></video>\n");
        let files = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::FILE);
        assert_eq!(files.len(), 1, "video should emit one File symbol");
        assert_eq!(
            files[0].name, "File: vid.mp4",
            "video name should be File: url"
        );
        assert_eq!(
            files[0].detail.as_deref(),
            Some("video"),
            "detail should be 'video'"
        );
    }

    #[test]
    fn markdown_video_emits_file() {
        let syms = symbols_for("![demo](demo.mp4)\n");
        let files = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::FILE);
        assert_eq!(files.len(), 1, "video image should emit one File symbol");
        assert_eq!(
            files[0].name, "File: demo.mp4",
            "video name should be File: url"
        );
        assert_eq!(
            files[0].detail.as_deref(),
            Some("video"),
            "detail should be 'video'"
        );
    }

    #[test]
    fn html_form_control_emits_event() {
        let syms = symbols_for("<input type=\"text\">\n");
        let events = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::EVENT);
        assert_eq!(events.len(), 1, "input should emit one Event symbol");
        assert_eq!(
            events[0].name, "Form: input",
            "form control name should have Form: prefix"
        );
    }

    #[test]
    fn thematic_break_emits_operator() {
        let syms = symbols_for("---\n");
        let breaks = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::OPERATOR);
        assert_eq!(breaks.len(), 1, "should have one thematic break");
        assert_eq!(
            breaks[0].name, "Break",
            "thematic break name should be Break"
        );
    }

    #[test]
    fn paragraph_not_emitted_as_symbol() {
        let syms = symbols_for("Just a paragraph.\n");
        let all = find_symbols(&syms, &|_| true);
        assert!(
            all.iter().all(|s| s.name != "Just a paragraph."),
            "paragraphs should not be emitted as symbols"
        );
    }

    #[test]
    fn paragraph_links_float_up() {
        let syms = symbols_for("# Section\n\nSee [link](other.md) for details.\n");
        let links = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::FUNCTION);
        assert_eq!(links.len(), 1, "floated link should be emitted");
    }

    #[test]
    fn scope_boundary_headings_are_flat() {
        let content = "# Top\n\n> ## Quoted heading\n>\n> text\n\n## After\n";
        let syms = symbols_for(content);
        assert_eq!(syms.len(), 1, "should have one top-level heading");
        let top_children = syms[0].children.as_ref().expect("Top should have children");

        let after = top_children
            .iter()
            .find(|s| s.name == "H2: After")
            .expect("'H2: After' should be a child of 'Top'");
        assert_eq!(
            after.kind,
            lsp::symbol_kind::CLASS,
            "After should be a heading"
        );

        let quote = top_children
            .iter()
            .find(|s| s.kind == lsp::symbol_kind::MODULE)
            .expect("blockquote should be a child of Top");
        let inner_headings = find_symbols(quote.children.as_deref().unwrap_or(&[]), &|s| {
            s.kind == lsp::symbol_kind::CLASS
        });
        assert_eq!(
            inner_headings.len(),
            1,
            "quoted heading should be inside the blockquote"
        );
    }

    #[test]
    fn non_heading_symbols_nest_under_heading() {
        let tagged = vec![
            TaggedSymbol {
                level: 1,
                symbol: lsp::DocumentSymbol {
                    name: "Title".to_string(),
                    detail: None,
                    kind: lsp::symbol_kind::CLASS,
                    range: lsp::Range::default(),
                    selection_range: lsp::Range::default(),
                    children: None,
                },
            },
            TaggedSymbol {
                level: 0,
                symbol: lsp::DocumentSymbol {
                    name: "---".to_string(),
                    detail: None,
                    kind: lsp::symbol_kind::OPERATOR,
                    range: lsp::Range::default(),
                    selection_range: lsp::Range::default(),
                    children: None,
                },
            },
        ];

        let symbols = nest_by_heading_level(tagged);
        assert_eq!(symbols.len(), 1, "thematic break should nest under heading");
        let children = symbols[0]
            .children
            .as_ref()
            .expect("heading should have children");
        assert_eq!(
            children[0].kind,
            lsp::symbol_kind::OPERATOR,
            "child should be the thematic break"
        );
    }

    #[test]
    fn workspace_symbols_includes_links() {
        let dir = workspace_with_files(&[("a.md", "# Title\n\n[go](b.md)\n"), ("b.md", "# B\n")]);
        let workspaces = scan_workspaces(&dir);
        let syms = workspace_symbols(&workspaces, "");
        assert!(
            syms.iter().any(|s| s.kind == lsp::symbol_kind::FUNCTION),
            "workspace symbols should include links"
        );
    }

    #[test]
    fn workspace_symbols_query_filters_new_kinds() {
        let dir = workspace_with_files(&[("a.md", "# Title\n\n## Section\n")]);
        let workspaces = scan_workspaces(&dir);
        let syms = workspace_symbols(&workspaces, "Section");
        assert_eq!(syms.len(), 1, "query should filter to matching symbols");
        assert_eq!(
            syms[0].name, "H2: Section",
            "should match the section heading"
        );
    }

    #[test]
    fn footnote_def_emits_constant() {
        let syms = symbols_for("text[^1]\n\n[^1]: footnote content\n");
        let footnotes = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::CONSTANT);
        assert_eq!(footnotes.len(), 1, "should have one footnote definition");
        assert_eq!(
            footnotes[0].name, "Footnote: [^1]",
            "footnote name should have prefix and label"
        );
    }

    #[test]
    fn heading_nesting_deep() {
        let tagged = vec![
            TaggedSymbol {
                level: 1,
                symbol: lsp::DocumentSymbol {
                    name: "H1".to_string(),
                    detail: None,
                    kind: lsp::symbol_kind::CLASS,
                    range: lsp::Range::default(),
                    selection_range: lsp::Range::default(),
                    children: None,
                },
            },
            TaggedSymbol {
                level: 2,
                symbol: lsp::DocumentSymbol {
                    name: "H2".to_string(),
                    detail: None,
                    kind: lsp::symbol_kind::CLASS,
                    range: lsp::Range::default(),
                    selection_range: lsp::Range::default(),
                    children: None,
                },
            },
            TaggedSymbol {
                level: 3,
                symbol: lsp::DocumentSymbol {
                    name: "H3".to_string(),
                    detail: None,
                    kind: lsp::symbol_kind::CLASS,
                    range: lsp::Range::default(),
                    selection_range: lsp::Range::default(),
                    children: None,
                },
            },
        ];

        let symbols = nest_by_heading_level(tagged);
        assert_eq!(symbols.len(), 1, "should have one top-level H1");
        let h2 = &symbols[0].children.as_ref().expect("H1 children")[0];
        assert_eq!(h2.name, "H2", "child should be H2");
        let h3 = &h2.children.as_ref().expect("H2 children")[0];
        assert_eq!(h3.name, "H3", "grandchild should be H3");
    }

    #[test]
    fn no_freed_symbol_kinds_emitted() {
        // Verify freed kinds (String=15, Key=20, Array=18, Enum=10,
        // EnumMember=22, Boolean=17, Property=7, Namespace=3) are never used.
        let content = "# Title\n\n[link](a.md)\n\n- item\n  - sub\n\n| A |\n|---|\n| 1 |\n\n```rust\ncode\n```\n\n---\n\n> quote\n";
        let syms = symbols_for(content);
        let all = find_symbols(&syms, &|_| true);
        let freed = [3, 7, 10, 15, 17, 18, 20, 22];
        for sym in &all {
            assert!(
                !freed.contains(&sym.kind),
                "freed SymbolKind {} should not be emitted (symbol: {})",
                sym.kind,
                sym.name,
            );
        }
    }

    #[test]
    fn blockquote_children_show_internal_structure() {
        let syms = symbols_for("> # Inner heading\n>\n> [link](a.md)\n");
        let quotes = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::MODULE);
        assert_eq!(quotes.len(), 1, "should have one blockquote");
        let children = quotes[0]
            .children
            .as_ref()
            .expect("blockquote should have children");
        assert!(
            children.iter().any(|c| c.kind == lsp::symbol_kind::CLASS),
            "blockquote should contain heading"
        );
    }

    #[test]
    fn generic_container_emits_module() {
        let syms = symbols_for("<div>\n\nContent inside.\n\n</div>\n");
        let modules = find_symbols(&syms, &|s| {
            s.kind == lsp::symbol_kind::MODULE && s.name.starts_with("Container:")
        });
        assert_eq!(modules.len(), 1, "should have one container");
        assert_eq!(
            modules[0].name, "Container: div",
            "container name should include tag"
        );
    }

    #[test]
    fn details_emits_module_with_summary() {
        // Blank line after <details> triggers parsed container mode.
        let syms = symbols_for(
            "<details>\n\n<summary>Click here</summary>\n\nHidden content.\n\n</details>\n",
        );
        let modules = find_symbols(&syms, &|s| {
            s.kind == lsp::symbol_kind::MODULE && s.name.starts_with("Details")
        });
        assert_eq!(modules.len(), 1, "should have one details");
        assert_eq!(
            modules[0].name, "Details: Click here",
            "details name should include summary"
        );
    }

    #[test]
    fn ordered_list_name_distinguishes() {
        let syms = symbols_for("1. a\n2. b\n3. c\n");
        let lists = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::STRUCT);
        assert_eq!(lists.len(), 1, "should have one list");
        assert_eq!(
            lists[0].name, "Ordered List",
            "ordered list uses 'Ordered List'"
        );
    }

    #[test]
    fn workspace_symbols_skip_nested_lists() {
        let dir = workspace_with_files(&[("a.md", "- top\n  - nested\n")]);
        let workspaces = scan_workspaces(&dir);
        let syms = workspace_symbols(&workspaces, "");
        let list_count = syms
            .iter()
            .filter(|s| s.kind == lsp::symbol_kind::STRUCT)
            .count();
        assert_eq!(
            list_count, 1,
            "workspace should only include top-level list"
        );
    }

    #[test]
    fn workspace_symbols_include_tables() {
        let dir = workspace_with_files(&[("a.md", "| A |\n|---|\n| 1 |\n")]);
        let workspaces = scan_workspaces(&dir);
        let syms = workspace_symbols(&workspaces, "");
        assert!(
            syms.iter()
                .any(|s| s.kind == lsp::symbol_kind::STRUCT && s.name == "Table"),
            "workspace should include tables"
        );
    }

    #[test]
    fn math_emits_object() {
        let syms = symbols_for("$$\nx^2 + y^2 = z^2\n$$\n");
        let blocks = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::OBJECT);
        assert_eq!(blocks.len(), 1, "should have one math block");
        assert_eq!(blocks[0].name, "Math", "math block name should be 'Math'");
    }

    #[test]
    fn import_emits_function() {
        let syms = symbols_for("@./other.md\n");
        let links = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::FUNCTION);
        assert_eq!(links.len(), 1, "should have one import");
        assert_eq!(
            links[0].name, "Link: import(./other.md)",
            "import name should have Link: import prefix"
        );
    }

    // -----------------------------------------------------------------------
    // Navigation — declaration (ticket 08, continued)
    // -----------------------------------------------------------------------

    #[test]
    fn declaration_inline_link_falls_through_to_target() {
        let dir = workspace_with_files(&[
            ("a.md", "# A\n\n[see B](b.md \"references\")\n"),
            ("b.md", "# B\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "a.md"),
            },
            position: lsp::Position {
                line: 2,
                character: 5,
            },
        };
        let loc = go_to_declaration(&workspaces, &params).expect("should fall through to target");
        assert!(
            loc.uri.ends_with("b.md"),
            "inline link declaration should go to target document"
        );
    }

    // -----------------------------------------------------------------------
    // Navigation — definition (ticket 08)
    // -----------------------------------------------------------------------

    #[test]
    fn definition_link_goes_to_target() {
        let dir = workspace_with_files(&[
            ("a.md", "# A\n\n[see B](b.md \"references\")\n"),
            ("b.md", "# B\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "a.md"),
            },
            position: lsp::Position {
                line: 2,
                character: 5,
            },
        };
        let loc = go_to_definition(&workspaces, &params).expect("should find definition");
        assert!(
            loc.uri.ends_with("b.md"),
            "definition should go to target document"
        );
    }

    // -----------------------------------------------------------------------
    // Navigation — type definition (ticket 08)
    // -----------------------------------------------------------------------

    #[test]
    fn type_definition_with_fragment_goes_to_heading() {
        let dir = workspace_with_files(&[
            ("a.md", "# A\n\n[details](b.md#details \"references\")\n"),
            ("b.md", "# B\n\n## Details\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "a.md"),
            },
            position: lsp::Position {
                line: 2,
                character: 5,
            },
        };
        let loc = go_to_type_definition(&workspaces, &params).expect("should find heading");
        assert!(
            loc.uri.ends_with("b.md"),
            "type definition should go to target"
        );
        assert_eq!(
            loc.range.start.line, 2,
            "type definition should go to the heading line"
        );
    }

    #[test]
    fn type_definition_without_fragment_goes_to_document() {
        let dir = workspace_with_files(&[
            ("a.md", "# A\n\n[see B](b.md \"references\")\n"),
            ("b.md", "# B\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "a.md"),
            },
            position: lsp::Position {
                line: 2,
                character: 5,
            },
        };
        // Without fragment, type definition falls through to definition.
        let loc = go_to_type_definition(&workspaces, &params)
            .expect("should fall through to target document");
        assert!(
            loc.uri.ends_with("b.md"),
            "type definition without fragment should go to target document"
        );
    }

    // -----------------------------------------------------------------------
    // Navigation — implementation (ticket 08)
    // -----------------------------------------------------------------------

    #[test]
    fn implementation_backlink_goes_to_forward_link() {
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            ("a.md", "# A\n\n[see B](b.md \"supersedes\")\n"),
            (
                "b.md",
                "---\nbacklinks:\n  superseded_by:\n    - a.md\n---\n\n# B\n",
            ),
        ]);
        let workspaces = scan_workspaces(&dir);

        // Cursor on the backlink path "    - a.md" (line 3, 0-based).
        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "b.md"),
            },
            position: lsp::Position {
                line: 3,
                character: 6,
            },
        };
        let loc = go_to_implementation(&workspaces, &params).expect("should find forward link");
        assert!(
            loc.uri.ends_with("a.md"),
            "implementation should go to the source document"
        );
        assert_eq!(
            loc.range.start.line, 2,
            "implementation should point to the forward link line"
        );
    }

    // -----------------------------------------------------------------------
    // Find references from ref def (ticket 08)
    // -----------------------------------------------------------------------

    #[test]
    fn find_references_from_ref_def() {
        let dir = workspace_with_files(&[(
            "a.md",
            "# A\n\n[first][ref]\n\n[second][ref]\n\n[ref]: b.md \"references\"\n",
        )]);
        let workspaces = scan_workspaces(&dir);

        // Cursor on the ref def line (line 6, 0-based).
        let params = lsp::ReferenceParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "a.md"),
            },
            position: lsp::Position {
                line: 6,
                character: 0,
            },
        };
        let locations = find_references(&workspaces, &params);
        assert_eq!(locations.len(), 2, "ref def should have two call sites");
    }

    // -----------------------------------------------------------------------
    // Rename with tree spans (ticket 08)
    // -----------------------------------------------------------------------

    #[test]
    fn rename_setext_heading() {
        let dir = workspace_with_files(&[("a.md", "Title\n=====\n")]);
        let workspaces = scan_workspaces(&dir);
        let uri = file_uri(&dir, "a.md");

        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier { uri: uri.clone() },
            position: lsp::Position {
                line: 0,
                character: 0,
            },
        };
        let range = prepare_rename(&workspaces, &params).expect("should find setext heading");
        assert_eq!(range.start.line, 0, "setext heading is on line 0");
        assert_eq!(range.start.character, 0, "setext heading text starts at 0");
        assert_eq!(
            range.end.character, 5,
            "setext heading text ends at len('Title')"
        );

        let rename_params = lsp::RenameParams {
            text_document: lsp::TextDocumentIdentifier { uri },
            position: lsp::Position {
                line: 0,
                character: 0,
            },
            new_name: "New Title".to_string(),
        };
        let edit = do_rename(&workspaces, &rename_params).expect("should produce edit");
        let changes = edit.changes.expect("should have changes");
        assert!(!changes.is_empty(), "should have changes for the file");
    }

    #[test]
    fn rename_html_heading() {
        let dir = workspace_with_files(&[("a.md", "<h1>Title</h1>\n")]);
        let workspaces = scan_workspaces(&dir);
        let uri = file_uri(&dir, "a.md");

        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier { uri },
            position: lsp::Position {
                line: 0,
                character: 0,
            },
        };
        let range = prepare_rename(&workspaces, &params).expect("should find HTML heading");
        assert_eq!(
            range.start.character, 4,
            "HTML heading text starts after <h1>"
        );
        assert_eq!(
            range.end.character, 9,
            "HTML heading text ends before </h1>"
        );
    }

    // -----------------------------------------------------------------------
    // Helper unit tests (ticket 08)
    // -----------------------------------------------------------------------

    #[test]
    fn lsp_position_to_byte_offset_basic() {
        let source = "line1\nline2\nline3\n";
        assert_eq!(
            lsp_position_to_byte_offset(
                source,
                lsp::Position {
                    line: 0,
                    character: 0
                }
            ),
            0,
            "start of first line"
        );
        assert_eq!(
            lsp_position_to_byte_offset(
                source,
                lsp::Position {
                    line: 1,
                    character: 0
                }
            ),
            6,
            "start of second line"
        );
        assert_eq!(
            lsp_position_to_byte_offset(
                source,
                lsp::Position {
                    line: 1,
                    character: 3
                }
            ),
            9,
            "middle of second line"
        );
    }

    #[test]
    fn span_to_lsp_range_basic() {
        let source = "# Title\n\nContent\n";
        let span = Span::new(2, 7); // "Title"
        let range = span_to_lsp_range(source, &span);
        assert_eq!(range.start.line, 0, "span starts on line 0");
        assert_eq!(range.start.character, 2, "span starts at character 2");
        assert_eq!(range.end.line, 0, "span ends on line 0");
        assert_eq!(range.end.character, 7, "span ends at character 7");
    }

    #[test]
    fn link_ref_label_inline() {
        let source = "[text](url \"title\")";
        let span = Span::new(0, source.len());
        assert!(
            link_ref_label(source, &span).is_none(),
            "inline link should not have a ref label"
        );
    }

    #[test]
    fn link_ref_label_full_reference() {
        let source = "[text][my-ref]";
        let span = Span::new(0, source.len());
        assert_eq!(
            link_ref_label(source, &span).as_deref(),
            Some("my-ref"),
            "full reference label"
        );
    }

    #[test]
    fn link_ref_label_collapsed() {
        let source = "[My Ref][]";
        let span = Span::new(0, source.len());
        assert_eq!(
            link_ref_label(source, &span).as_deref(),
            Some("my ref"),
            "collapsed reference uses link text as label"
        );
    }

    #[test]
    fn link_ref_label_shortcut() {
        let source = "[shortcut]";
        let span = Span::new(0, source.len());
        assert_eq!(
            link_ref_label(source, &span).as_deref(),
            Some("shortcut"),
            "shortcut reference uses link text as label"
        );
    }

    // -----------------------------------------------------------------------
    // Frontmatter symbols (ticket 13)
    // -----------------------------------------------------------------------

    #[test]
    fn frontmatter_emits_struct_with_field_children() {
        let content = "---\ntitle: Doc\ndate: 2026-05-24\n---\n\n# Heading\n";
        let syms = symbols_for(content);
        let fm = find_symbols(&syms, &|s| {
            s.kind == lsp::symbol_kind::STRUCT && s.name.starts_with("Frontmatter:")
        });
        assert_eq!(fm.len(), 1, "should have one frontmatter symbol");
        assert_eq!(
            fm[0].name, "Frontmatter: YAML",
            "frontmatter name includes syntax"
        );
        assert_eq!(
            fm[0].detail.as_deref(),
            Some("2"),
            "detail is top-level key count"
        );
        let children = fm[0]
            .children
            .as_ref()
            .expect("frontmatter should have children");
        assert_eq!(children.len(), 2, "should have two Field children");
        assert_eq!(
            children[0].name, "Field: title",
            "first field should be title"
        );
        assert_eq!(
            children[1].name, "Field: date",
            "second field should be date"
        );
        assert!(
            children.iter().all(|c| c.kind == lsp::symbol_kind::FIELD),
            "all children should be Field kind"
        );
    }

    #[test]
    fn frontmatter_nested_map_emits_struct_children() {
        let content = "---\nbacklinks:\n  superseded_by:\n    - decisions/38.md\n  amended_by:\n    - decisions/38.md\n    - tickets/14h.md\n---\n";
        let syms = symbols_for(content);
        let fm = find_symbols(&syms, &|s| {
            s.kind == lsp::symbol_kind::STRUCT && s.name == "Frontmatter: YAML"
        });
        assert_eq!(fm.len(), 1, "should have one frontmatter");
        let children = fm[0]
            .children
            .as_ref()
            .expect("frontmatter should have children");
        assert_eq!(children.len(), 1, "should have one child (backlinks map)");
        assert_eq!(
            children[0].name, "backlinks",
            "map child should be named by key"
        );
        assert_eq!(
            children[0].kind,
            lsp::symbol_kind::STRUCT,
            "map child should be Struct"
        );
        assert_eq!(
            children[0].detail.as_deref(),
            Some("2"),
            "map detail is child count"
        );
    }

    #[test]
    fn frontmatter_backlinks_predicates_show_source_count() {
        let content = "---\nbacklinks:\n  superseded_by:\n    - decisions/38.md\n  amended_by:\n    - decisions/38.md\n    - tickets/14h.md\n---\n";
        let syms = symbols_for(content);
        let predicates = find_symbols(&syms, &|s| {
            s.kind == lsp::symbol_kind::FIELD && s.name.starts_with("Field: ")
        });
        let superseded = predicates
            .iter()
            .find(|s| s.name == "Field: superseded_by")
            .expect("should find superseded_by");
        assert_eq!(
            superseded.detail.as_deref(),
            Some("1"),
            "superseded_by has 1 source"
        );
        let amended = predicates
            .iter()
            .find(|s| s.name == "Field: amended_by")
            .expect("should find amended_by");
        assert_eq!(
            amended.detail.as_deref(),
            Some("2"),
            "amended_by has 2 sources"
        );
    }

    #[test]
    fn frontmatter_leaf_values_not_in_outline() {
        let content = "---\ntitle: \"Hooks Primary Capture\"\ndate: 2026-05-24\nbacklinks:\n  superseded_by:\n    - decisions/38.md\n---\n";
        let syms = symbols_for(content);
        let all = find_symbols(&syms, &|_| true);
        // Values like "Hooks Primary Capture", "2026-05-24", "decisions/38.md"
        // should never appear as symbol names.
        for sym in &all {
            assert!(
                !sym.name.contains("Hooks Primary Capture"),
                "leaf values should not appear in outline"
            );
            assert!(
                !sym.name.contains("2026-05-24"),
                "date values should not appear in outline"
            );
            assert!(
                !sym.name.contains("decisions/38.md"),
                "path values should not appear in outline"
            );
        }
    }

    #[test]
    fn frontmatter_selection_range_is_precise() {
        let content = "---\ntitle: Doc\ndate: 2026-05-24\n---\n\n# Heading\n";
        let syms = symbols_for(content);
        let fields = find_symbols(&syms, &|s| {
            s.kind == lsp::symbol_kind::FIELD && s.name.starts_with("Field:")
        });
        let fm = find_symbols(&syms, &|s| s.name == "Frontmatter: YAML");
        let fm_range = &fm[0].range;
        // Each field's selection_range should NOT equal the full frontmatter range.
        for field in &fields {
            assert_ne!(
                field.selection_range, *fm_range,
                "field selection_range should not be the full frontmatter block"
            );
        }
    }

    #[test]
    fn frontmatter_workspace_symbols_top_level_only() {
        let dir = workspace_with_files(&[(
            "a.md",
            "---\ntitle: Doc\nbacklinks:\n  superseded_by:\n    - b.md\n---\n\n# H\n",
        )]);
        let workspaces = scan_workspaces(&dir);
        let ws_syms = workspace_symbols(&workspaces, "");
        let fm_count = ws_syms
            .iter()
            .filter(|s| s.name.starts_with("Frontmatter:"))
            .count();
        assert_eq!(fm_count, 1, "workspace should have one frontmatter symbol");
        // Backlink predicates should NOT appear in workspace symbols.
        assert!(
            !ws_syms.iter().any(|s| s.name.contains("superseded_by")),
            "predicate keys should not appear in workspace symbols"
        );
    }

    #[test]
    fn frontmatter_top_level_scalar_has_no_detail() {
        let content = "---\ntitle: Doc\n---\n\n# Heading\n";
        let syms = symbols_for(content);
        let fields = find_symbols(&syms, &|s| s.name == "Field: title");
        assert_eq!(fields.len(), 1, "should have title field");
        assert_eq!(
            fields[0].detail, None,
            "top-level scalar key should have no detail"
        );
    }

    #[test]
    fn frontmatter_flow_sequence_counts_items() {
        let content = "---\nbacklinks:\n  referenced_by: [a.md, b.md, c.md]\n---\n";
        let syms = symbols_for(content);
        let fields = find_symbols(&syms, &|s| s.name == "Field: referenced_by");
        assert_eq!(fields.len(), 1, "should find referenced_by field");
        assert_eq!(
            fields[0].detail.as_deref(),
            Some("3"),
            "flow sequence should count all items"
        );
    }

    // -----------------------------------------------------------------------
    // Definition list symbols
    // -----------------------------------------------------------------------

    #[test]
    fn definition_list_emits_struct_with_field_children() {
        // Blank line after <dl> triggers markdown mode so nested tags are parsed.
        let syms = symbols_for(concat!(
            "<dl>\n\n",
            "<dt>API</dt>\n\n",
            "<dd>The public interface</dd>\n\n",
            "<dt>SDK</dt>\n\n",
            "<dd>Client libraries</dd>\n\n",
            "<dt>CLI</dt>\n\n",
            "<dd>Command-line tool</dd>\n\n",
            "</dl>\n",
        ));
        let defs = find_symbols(&syms, &|s| {
            s.kind == lsp::symbol_kind::STRUCT && s.name == "Definitions"
        });
        assert_eq!(defs.len(), 1, "should have one definition list");
        assert_eq!(
            defs[0].detail.as_deref(),
            Some("3"),
            "detail should be term count"
        );
        let children = defs[0]
            .children
            .as_ref()
            .expect("definition list should have Field children");
        assert_eq!(children.len(), 3, "should have three Field children");
        assert_eq!(children[0].name, "Field: API", "first term");
        assert_eq!(children[1].name, "Field: SDK", "second term");
        assert_eq!(children[2].name, "Field: CLI", "third term");
        assert!(
            children.iter().all(|c| c.kind == lsp::symbol_kind::FIELD),
            "all children should be Field kind"
        );
    }

    #[test]
    fn definition_list_descriptions_not_in_symbols() {
        let syms = symbols_for(concat!(
            "<dl>\n\n",
            "<dt>Term</dt>\n\n",
            "<dd>Description</dd>\n\n",
            "</dl>\n",
        ));
        let descs = find_symbols(&syms, &|s| {
            s.name.contains("Description") && s.kind != lsp::symbol_kind::FIELD
        });
        assert!(
            descs.is_empty(),
            "descriptions should not appear as standalone symbols"
        );
    }

    #[test]
    fn definition_list_workspace_symbols_include_container() {
        let dir = workspace_with_files(&[("a.md", "<dl>\n\n<dt>X</dt>\n\n<dd>Y</dd>\n\n</dl>\n")]);
        let workspaces = scan_workspaces(&dir);
        let syms = workspace_symbols(&workspaces, "");
        assert!(
            syms.iter()
                .any(|s| s.kind == lsp::symbol_kind::STRUCT && s.name == "Definitions"),
            "workspace symbols should include definition list container"
        );
    }

    #[test]
    fn definition_list_terms_not_in_workspace_symbols() {
        let dir = workspace_with_files(&[(
            "a.md",
            "<dl>\n\n<dt>API</dt>\n\n<dd>Interface</dd>\n\n</dl>\n",
        )]);
        let workspaces = scan_workspaces(&dir);
        let syms = workspace_symbols(&workspaces, "");
        assert!(
            !syms.iter().any(|s| s.name.contains("API")),
            "terms should not appear in workspace symbols"
        );
    }
}
