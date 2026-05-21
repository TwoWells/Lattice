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

use crate::lsp;
use crate::markdown::{Heading, HeadingId, LinkKind};
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
                handle_request(connection, &workspaces, req)?;
            }
            Message::Notification(notif) => {
                handle_notification(connection, &mut workspaces, notif)?;
            }
            Message::Response(_) => {}
        }
    }
    Ok(())
}

/// Dispatch a request.
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

/// Build document symbols (headings) for a file.
fn document_symbols(workspaces: &Workspaces, uri: &str) -> Option<Vec<lsp::DocumentSymbol>> {
    let (workspace, rel_path) = workspaces.resolve(uri)?;
    let file_data = workspace.file(&rel_path)?;
    Some(build_heading_symbols(&file_data.headings))
}

/// Convert a flat list of headings into a nested symbol tree.
///
/// Headings are nested by level: H2 is a child of the preceding H1,
/// H3 is a child of the preceding H2, etc.
fn build_heading_symbols(headings: &[Heading]) -> Vec<lsp::DocumentSymbol> {
    let mut stack: Vec<(u8, lsp::DocumentSymbol)> = Vec::new();
    let mut result: Vec<lsp::DocumentSymbol> = Vec::new();

    for heading in headings {
        let symbol = heading_to_document_symbol(heading);

        // Pop symbols at same or deeper level — they're complete.
        while stack.last().is_some_and(|(lvl, _)| *lvl >= heading.level) {
            let Some((_, finished)) = stack.pop() else {
                break;
            };
            if let Some((_, parent)) = stack.last_mut() {
                parent.children.get_or_insert_with(Vec::new).push(finished);
            } else {
                result.push(finished);
            }
        }

        stack.push((heading.level, symbol));
    }

    while let Some((_, finished)) = stack.pop() {
        if let Some((_, parent)) = stack.last_mut() {
            parent.children.get_or_insert_with(Vec::new).push(finished);
        } else {
            result.push(finished);
        }
    }

    result
}

/// Convert a single heading to a `DocumentSymbol`.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn heading_to_document_symbol(heading: &Heading) -> lsp::DocumentSymbol {
    let line = heading.line.saturating_sub(1) as u32;
    let range = lsp::Range {
        start: lsp::Position { line, character: 0 },
        end: lsp::Position { line, character: 0 },
    };

    lsp::DocumentSymbol {
        name: heading.text.clone(),
        kind: lsp::symbol_kind::STRING,
        range,
        selection_range: range,
        children: None,
    }
}

// ---------------------------------------------------------------------------
// Workspace symbols (ticket 13)
// ---------------------------------------------------------------------------

/// Search headings across all workspaces.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn workspace_symbols(workspaces: &Workspaces, query: &str) -> Vec<lsp::SymbolInformation> {
    let query_lower = query.to_lowercase();
    let mut symbols = Vec::new();

    for (root, workspace) in workspaces.iter() {
        for (rel_path, file_data) in workspace.files() {
            for heading in &file_data.headings {
                if !query.is_empty() && !heading.text.to_lowercase().contains(&query_lower) {
                    continue;
                }
                let abs_path = root.join(rel_path);
                let uri = path_to_uri(&abs_path);
                let line = heading.line.saturating_sub(1) as u32;
                symbols.push(lsp::SymbolInformation {
                    name: heading.text.clone(),
                    kind: lsp::symbol_kind::STRING,
                    location: lsp::Location {
                        uri,
                        range: lsp::Range {
                            start: lsp::Position { line, character: 0 },
                            end: lsp::Position { line, character: 0 },
                        },
                    },
                    container_name: Some(rel_path.display().to_string()),
                });
            }
        }
    }

    symbols
}

// ---------------------------------------------------------------------------
// prepareRename / rename (ticket 04)
// ---------------------------------------------------------------------------

/// Find the heading at a cursor position, returning its text range.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn prepare_rename(
    workspaces: &Workspaces,
    params: &lsp::TextDocumentPositionParams,
) -> Option<lsp::Range> {
    let (workspace, rel_path) = workspaces.resolve(&params.text_document.uri)?;
    let file_data = workspace.file(&rel_path)?;
    let heading = heading_at_line(&file_data.headings, params.position.line)?;

    let line = heading.line.saturating_sub(1) as u32;
    let text_len = heading.text.len() as u32;
    // Heading text starts after "# " (level hashes + space).
    let prefix_len = u32::from(heading.level) + 1;

    Some(lsp::Range {
        start: lsp::Position {
            line,
            character: prefix_len,
        },
        end: lsp::Position {
            line,
            character: prefix_len + text_len,
        },
    })
}

/// Rename a heading's text.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn do_rename(workspaces: &Workspaces, params: &lsp::RenameParams) -> Option<lsp::WorkspaceEdit> {
    let (workspace, rel_path) = workspaces.resolve(&params.text_document.uri)?;
    let file_data = workspace.file(&rel_path)?;
    let heading = heading_at_line(&file_data.headings, params.position.line)?;

    let line = heading.line.saturating_sub(1) as u32;
    let text_len = heading.text.len() as u32;
    let prefix_len = u32::from(heading.level) + 1;
    let range = lsp::Range {
        start: lsp::Position {
            line,
            character: prefix_len,
        },
        end: lsp::Position {
            line,
            character: prefix_len + text_len,
        },
    };

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

/// Find all documents that link to the file or heading at the cursor.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn find_references(workspaces: &Workspaces, params: &lsp::ReferenceParams) -> Vec<lsp::Location> {
    let Some((workspace, rel_path)) = workspaces.resolve(&params.text_document.uri) else {
        return Vec::new();
    };

    // Determine if the cursor is on a heading (to filter by fragment).
    let target_heading = workspace
        .file(&rel_path)
        .and_then(|fd| heading_at_line(&fd.headings, params.position.line));

    let mut locations = Vec::new();

    for (root, ws) in workspaces.iter() {
        for (src_path, file_data) in ws.files() {
            for link in &file_data.links {
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
// Type hierarchy (ticket 08)
// ---------------------------------------------------------------------------

/// Prepare a type hierarchy item for the heading at the cursor.
fn prepare_type_hierarchy(
    workspaces: &Workspaces,
    params: &lsp::TextDocumentPositionParams,
) -> Option<Vec<lsp::HierarchyItem>> {
    let (workspace, rel_path) = workspaces.resolve(&params.text_document.uri)?;
    let file_data = workspace.file(&rel_path)?;
    let heading = heading_at_line(&file_data.headings, params.position.line)?;
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

    let target_level = hierarchy_item_level(item);
    if target_level <= 1 {
        return Some(Vec::new());
    }

    let target_line = item.selection_range.start.line;
    let parent = file_data.headings.iter().rev().find(|h| {
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

    let target_level = hierarchy_item_level(item);
    let child_level = target_level + 1;
    let target_line = item.selection_range.start.line;

    let mut children = Vec::new();
    let mut started = false;

    for heading in &file_data.headings {
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
    let heading = heading_at_line(&file_data.headings, params.position.line)?;
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
            for link in &file_data.links {
                let LinkKind::IntraProject { target, .. } = &link.kind else {
                    continue;
                };
                if target != &rel_path {
                    continue;
                }
                let abs_src = root.join(src_path);
                let caller_heading = enclosing_heading(&file_data.headings, link.line);

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

    let item_line = item.selection_range.start.line;
    let item_level = hierarchy_item_level(item);

    // Find the end of this heading's section.
    let section_end: u32 = file_data
        .headings
        .iter()
        .find(|h| {
            let h_line = h.line.saturating_sub(1) as u32;
            h_line > item_line && h.level <= item_level
        })
        .map_or(u32::MAX, |h| h.line.saturating_sub(1) as u32);

    let root = workspace.root();
    let mut calls = Vec::new();

    for link in &file_data.links {
        let LinkKind::IntraProject { target, .. } = &link.kind else {
            continue;
        };
        let link_line = link.line.saturating_sub(1) as u32;
        if link_line < item_line || link_line >= section_end {
            continue;
        }

        let target_abs = root.join(target);
        let target_item = workspace
            .file(target)
            .and_then(|fd| fd.headings.first())
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

    let root = workspace.root();
    let mut links = Vec::new();

    for link in &file_data.links {
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

/// Return diagnostics for a single document.
fn document_diagnostic(workspaces: &Workspaces, uri: &str) -> lsp::FullDocumentDiagnosticReport {
    let items = if let Some((workspace, rel_path)) = workspaces.resolve(uri) {
        let all = validation::collect_all(workspace);
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
        let all = validation::collect_all(workspace);
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

    // Find the link on the cursor's line.
    let cursor_line = params.position.line;
    let link = file_data
        .links
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

    let preview = build_hover_preview(&target_data.content, target_data, fragment.as_deref());
    let header = format!("**{predicate}** → `{}`", target.display());

    Some(lsp::Hover {
        contents: lsp::MarkupContent {
            kind: "markdown".to_string(),
            value: format!("{header}\n\n---\n\n{preview}"),
        },
    })
}

/// Build a ~5 line preview from the target file content.
fn build_hover_preview(
    content: &str,
    target_data: &crate::workspace::FileData,
    fragment: Option<&str>,
) -> String {
    let lines: Vec<&str> = content.lines().collect();

    // Determine the start line for the preview.
    let start = fragment.map_or_else(
        // No fragment — skip frontmatter.
        || target_data.frontmatter.as_ref().map_or(0, |fm| fm.end_line),
        // Fragment — find the matching heading.
        |frag| {
            target_data
                .headings
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

    let total_lines = file_data.content.lines().count() as u32;

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
    let headings = &file_data.headings;
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
    let mut document = file_data.content.clone();
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
    let total_lines = file_data.content.lines().count() as u32;
    let last_line_len = file_data.content.lines().last().map_or(0, str::len) as u32;

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
        kind: lsp::symbol_kind::STRING,
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
        let all_diagnostics = validation::collect_all(workspace);

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
    use crate::markdown::HeadingId;

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

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
        let headings = vec![
            Heading {
                line: 1,
                level: 1,
                text: "Title".to_string(),
                id: HeadingId::Computed {
                    github: "title".to_string(),
                    gitlab: "title".to_string(),
                    vscode: "title".to_string(),
                },
            },
            Heading {
                line: 3,
                level: 2,
                text: "Section".to_string(),
                id: HeadingId::Computed {
                    github: "section".to_string(),
                    gitlab: "section".to_string(),
                    vscode: "section".to_string(),
                },
            },
            Heading {
                line: 5,
                level: 2,
                text: "Another".to_string(),
                id: HeadingId::Computed {
                    github: "another".to_string(),
                    gitlab: "another".to_string(),
                    vscode: "another".to_string(),
                },
            },
        ];

        let symbols = build_heading_symbols(&headings);
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
            Heading {
                line: 1,
                level: 1,
                text: "Title".to_string(),
                id: HeadingId::Computed {
                    github: "title".to_string(),
                    gitlab: "title".to_string(),
                    vscode: "title".to_string(),
                },
            },
            Heading {
                line: 5,
                level: 2,
                text: "Section".to_string(),
                id: HeadingId::Computed {
                    github: "section".to_string(),
                    gitlab: "section".to_string(),
                    vscode: "section".to_string(),
                },
            },
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
        let heading = Heading {
            line: 1,
            level: 1,
            text: "Custom ID".to_string(),
            id: HeadingId::Explicit("my-id".to_string()),
        };
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
        let heading = Heading {
            line: 1,
            level: 2,
            text: "Hello World!".to_string(),
            id: HeadingId::Computed {
                github: "hello-world".to_string(),
                gitlab: "hello-world-1".to_string(),
                vscode: "hello-world-2".to_string(),
            },
        };
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
            Heading {
                line: 1,
                level: 1,
                text: "Title".to_string(),
                id: HeadingId::Explicit("title".to_string()),
            },
            Heading {
                line: 5,
                level: 2,
                text: "Section".to_string(),
                id: HeadingId::Explicit("section".to_string()),
            },
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
            kind: lsp::symbol_kind::STRING,
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
        assert!(names.contains(&"Alpha"), "should contain Alpha");
        assert!(names.contains(&"Beta"), "should contain Beta");
        assert!(names.contains(&"Gamma"), "should contain Gamma");
        assert_eq!(symbols.len(), 3, "should return all 3 headings");
    }

    #[test]
    fn workspace_symbols_filters_by_query() {
        let dir = workspace_with_files(&[("a.md", "# Alpha\n## Beta\n"), ("b.md", "# Gamma\n")]);
        let workspaces = scan_workspaces(&dir);

        let symbols = workspace_symbols(&workspaces, "alph");
        assert_eq!(symbols.len(), 1, "should match only Alpha");
        assert_eq!(symbols[0].name, "Alpha", "should be case-insensitive match");
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
        let h3_item = heading_to_hierarchy_item(
            &Heading {
                line: 5,
                level: 3,
                text: "Sub".to_string(),
                id: HeadingId::Computed {
                    github: "sub".to_string(),
                    gitlab: "sub".to_string(),
                    vscode: "sub".to_string(),
                },
            },
            &dir.path().join("a.md"),
        );
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
        let h1_item = heading_to_hierarchy_item(
            &Heading {
                line: 1,
                level: 1,
                text: "Title".to_string(),
                id: HeadingId::Computed {
                    github: "title".to_string(),
                    gitlab: "title".to_string(),
                    vscode: "title".to_string(),
                },
            },
            &dir.path().join("a.md"),
        );
        let children =
            type_hierarchy_subtypes(&workspaces, &h1_item).expect("should return children");
        let names: Vec<&str> = children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["One", "Two"], "H1 children should be the H2s");
    }

    #[test]
    fn type_hierarchy_h1_has_no_supertypes() {
        let dir = workspace_with_files(&[("a.md", "# Title\n")]);
        let workspaces = scan_workspaces(&dir);

        let h1_item = heading_to_hierarchy_item(
            &Heading {
                line: 1,
                level: 1,
                text: "Title".to_string(),
                id: HeadingId::Computed {
                    github: "title".to_string(),
                    gitlab: "title".to_string(),
                    vscode: "title".to_string(),
                },
            },
            &dir.path().join("a.md"),
        );
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

        let h1_item = heading_to_hierarchy_item(
            &Heading {
                line: 1,
                level: 1,
                text: "A".to_string(),
                id: HeadingId::Computed {
                    github: "a".to_string(),
                    gitlab: "a".to_string(),
                    vscode: "a".to_string(),
                },
            },
            &dir.path().join("a.md"),
        );
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

        let b_item = heading_to_hierarchy_item(
            &Heading {
                line: 1,
                level: 1,
                text: "B".to_string(),
                id: HeadingId::Computed {
                    github: "b".to_string(),
                    gitlab: "b".to_string(),
                    vscode: "b".to_string(),
                },
            },
            &dir.path().join("b.md"),
        );
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
        let s1_item = heading_to_hierarchy_item(
            &Heading {
                line: 3,
                level: 2,
                text: "S1".to_string(),
                id: HeadingId::Computed {
                    github: "s1".to_string(),
                    gitlab: "s1".to_string(),
                    vscode: "s1".to_string(),
                },
            },
            &dir.path().join("a.md"),
        );
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

        let b_item = heading_to_hierarchy_item(
            &Heading {
                line: 1,
                level: 1,
                text: "B".to_string(),
                id: HeadingId::Computed {
                    github: "b".to_string(),
                    gitlab: "b".to_string(),
                    vscode: "b".to_string(),
                },
            },
            &dir.path().join("b.md"),
        );
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
        let dir =
            workspace_with_files(&[("a.md", "# A\n\n[broken](nonexistent.md \"references\")\n")]);
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
        let dir =
            workspace_with_files(&[("a.md", "# A\n\n[broken](nonexistent.md \"references\")\n")]);
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
}
