// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! LSP server for Lattice.
//!
//! Diagnostic-only server: publishes diagnostics on file open, save, and
//! change. Supports multiple workspace folders.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use lsp_server::{Connection, Message, Notification, Response};
use lsp_types::notification::{
    DidChangeTextDocument, DidChangeWorkspaceFolders, DidOpenTextDocument, DidSaveTextDocument,
    Notification as _,
};
use lsp_types::request::DocumentSymbolRequest;
use lsp_types::{
    DiagnosticSeverity, DocumentSymbol, InitializeParams, OneOf, SaveOptions, ServerCapabilities,
    SymbolKind, TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
    TextDocumentSyncSaveOptions, Uri,
};

use crate::markdown::Heading;
use crate::validation::{self, Diagnostic, Severity};
use crate::workspace::Workspace;

/// Multiple workspaces keyed by root path.
struct Workspaces {
    inner: BTreeMap<PathBuf, Workspace>,
}

impl Workspaces {
    /// Create from the initial set of workspace folders.
    fn from_params(params: &InitializeParams) -> Self {
        let mut inner = BTreeMap::new();

        // Try workspace folders first.
        if let Some(folders) = &params.workspace_folders {
            for folder in folders {
                let root = uri_to_path(&folder.uri);
                if let Ok(ws) = Workspace::scan(&root) {
                    inner.insert(root, ws);
                }
            }
        }

        // Fall back to deprecated root_uri if no folders.
        if inner.is_empty() {
            #[allow(deprecated, reason = "root_uri is the fallback for older clients")]
            if let Some(root_uri) = &params.root_uri {
                let root = uri_to_path(root_uri);
                if let Ok(ws) = Workspace::scan(&root) {
                    inner.insert(root, ws);
                }
            }
        }

        Self { inner }
    }

    /// Add a workspace folder.
    fn add(&mut self, uri: &Uri) {
        let root = uri_to_path(uri);
        if let Ok(ws) = Workspace::scan(&root) {
            self.inner.insert(root, ws);
        }
    }

    /// Remove a workspace folder.
    fn remove(&mut self, uri: &Uri) {
        let root = uri_to_path(uri);
        self.inner.remove(&root);
    }

    /// Find the workspace that contains a file URI, returning the workspace
    /// and the file's workspace-relative path.
    fn resolve(&self, uri: &Uri) -> Option<(&Workspace, PathBuf)> {
        let path = uri_to_path(uri);
        // Find the workspace with the longest matching root prefix.
        self.inner.iter().rev().find_map(|(root, ws)| {
            path.strip_prefix(root)
                .ok()
                .map(|rel| (ws, rel.to_path_buf()))
        })
    }

    /// Find the workspace that contains a file URI (mutable).
    fn resolve_mut(&mut self, uri: &Uri) -> Option<(&mut Workspace, PathBuf)> {
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

    let capabilities = ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Options(
            TextDocumentSyncOptions {
                open_close: Some(true),
                change: Some(TextDocumentSyncKind::FULL),
                save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                    include_text: Some(true),
                })),
                ..TextDocumentSyncOptions::default()
            },
        )),
        document_symbol_provider: Some(OneOf::Left(true)),
        ..ServerCapabilities::default()
    };

    let mut caps_json =
        serde_json::to_value(&capabilities).context("failed to serialize capabilities")?;
    caps_json["workspace"] = serde_json::json!({
        "workspaceFolders": {
            "supported": true,
            "changeNotifications": true
        }
    });
    let init_params = connection.initialize(caps_json)?;
    let params: InitializeParams =
        serde_json::from_value(init_params).context("failed to parse InitializeParams")?;

    let workspaces = Workspaces::from_params(&params);

    main_loop(&connection, workspaces)?;
    io_threads.join()?;

    Ok(())
}

/// Convert an LSP URI to a filesystem path.
fn uri_to_path(uri: &Uri) -> PathBuf {
    PathBuf::from(uri.path().as_str())
}

/// Convert a filesystem path to an LSP URI.
fn path_to_uri(path: &Path) -> Result<Uri> {
    let s = format!("file://{}", path.display());
    s.parse().with_context(|| format!("invalid URI: {s}"))
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
    let resp = if req.method == <DocumentSymbolRequest as lsp_types::request::Request>::METHOD {
        let params: lsp_types::DocumentSymbolParams = serde_json::from_value(req.params)?;
        let symbols = document_symbols(workspaces, &params.text_document.uri);
        Response::new_ok(req.id, symbols)
    } else {
        Response::new_err(
            req.id,
            lsp_server::ErrorCode::MethodNotFound as i32,
            format!("method not found: {}", req.method),
        )
    };
    connection.sender.send(Message::Response(resp))?;
    Ok(())
}

/// Build document symbols (headings) for a file.
fn document_symbols(workspaces: &Workspaces, uri: &Uri) -> Option<Vec<DocumentSymbol>> {
    let (workspace, rel_path) = workspaces.resolve(uri)?;
    let file_data = workspace.file(&rel_path)?;
    Some(build_heading_symbols(&file_data.headings))
}

/// Convert a flat list of headings into a nested symbol tree.
///
/// Headings are nested by level: H2 is a child of the preceding H1,
/// H3 is a child of the preceding H2, etc.
#[allow(
    deprecated,
    reason = "DocumentSymbol::deprecated field is deprecated in lsp-types but required by the struct"
)]
fn build_heading_symbols(headings: &[Heading]) -> Vec<DocumentSymbol> {
    // Stack of (level, symbol) pairs for building hierarchy.
    let mut stack: Vec<(u8, DocumentSymbol)> = Vec::new();
    let mut result: Vec<DocumentSymbol> = Vec::new();

    for heading in headings {
        let symbol = heading_to_symbol(heading);

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

    // Drain remaining stack.
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
#[allow(
    deprecated,
    reason = "DocumentSymbol::deprecated field is deprecated in lsp-types but required by the struct"
)]
fn heading_to_symbol(heading: &Heading) -> DocumentSymbol {
    let line = heading.line.saturating_sub(1) as u32;
    let range = lsp_types::Range {
        start: lsp_types::Position { line, character: 0 },
        end: lsp_types::Position { line, character: 0 },
    };

    DocumentSymbol {
        name: heading.text.clone(),
        detail: None,
        kind: SymbolKind::STRING,
        tags: None,
        deprecated: None,
        range,
        selection_range: range,
        children: None,
    }
}

/// Dispatch a notification.
fn handle_notification(
    connection: &Connection,
    workspaces: &mut Workspaces,
    notif: Notification,
) -> Result<()> {
    match notif.method.as_str() {
        DidOpenTextDocument::METHOD => {
            let params: lsp_types::DidOpenTextDocumentParams =
                serde_json::from_value(notif.params)?;
            if let Some((ws, rel_path)) = workspaces.resolve_mut(&params.text_document.uri) {
                ws.update_content(&rel_path, &params.text_document.text);
            }
            publish_all_diagnostics(connection, workspaces)?;
        }
        DidSaveTextDocument::METHOD => {
            let params: lsp_types::DidSaveTextDocumentParams =
                serde_json::from_value(notif.params)?;
            if let Some((ws, rel_path)) = workspaces.resolve_mut(&params.text_document.uri) {
                if let Some(text) = &params.text {
                    ws.update_content(&rel_path, text);
                } else {
                    let _ = ws.update(&rel_path);
                }
            }
            publish_all_diagnostics(connection, workspaces)?;
        }
        DidChangeTextDocument::METHOD => {
            let params: lsp_types::DidChangeTextDocumentParams =
                serde_json::from_value(notif.params)?;
            // We requested FULL sync, so the last change has the full text.
            if let Some(change) = params.content_changes.into_iter().last() {
                if let Some((ws, rel_path)) = workspaces.resolve_mut(&params.text_document.uri) {
                    ws.update_content(&rel_path, &change.text);
                }
                publish_all_diagnostics(connection, workspaces)?;
            }
        }
        DidChangeWorkspaceFolders::METHOD => {
            let params: lsp_types::DidChangeWorkspaceFoldersParams =
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

/// Publish diagnostics for all files across all workspaces.
fn publish_all_diagnostics(connection: &Connection, workspaces: &Workspaces) -> Result<()> {
    for (root, workspace) in workspaces.iter() {
        let all_diagnostics = validation::collect_all(workspace);

        // Group diagnostics by file.
        let mut by_file: BTreeMap<PathBuf, Vec<lsp_types::Diagnostic>> = BTreeMap::new();

        for diag in &all_diagnostics {
            by_file
                .entry(diag.file.clone())
                .or_default()
                .push(to_lsp_diagnostic(diag));
        }

        // Collect all files that should have diagnostics published.
        let mut published: BTreeSet<PathBuf> = by_file.keys().cloned().collect();
        for path in workspace.files().keys() {
            published.insert(path.clone());
        }

        for rel_path in &published {
            let abs_path = root.join(rel_path);
            let uri = path_to_uri(&abs_path)?;
            let diagnostics = by_file.remove(rel_path).unwrap_or_default();

            let params = lsp_types::PublishDiagnosticsParams {
                uri,
                diagnostics,
                version: None,
            };
            let notif = Notification::new(
                lsp_types::notification::PublishDiagnostics::METHOD.to_string(),
                params,
            );
            connection.sender.send(Message::Notification(notif))?;
        }
    }

    Ok(())
}

/// Convert a Lattice diagnostic to an LSP diagnostic.
fn to_lsp_diagnostic(diag: &Diagnostic) -> lsp_types::Diagnostic {
    let severity = match diag.severity {
        Severity::Error => DiagnosticSeverity::ERROR,
        Severity::Warning => DiagnosticSeverity::WARNING,
        Severity::Info => DiagnosticSeverity::INFORMATION,
    };

    // Lines are 1-based in Lattice, 0-based in LSP.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "line numbers in markdown files won't exceed u32::MAX"
    )]
    let line = diag.line.saturating_sub(1) as u32;
    let range = lsp_types::Range {
        start: lsp_types::Position { line, character: 0 },
        end: lsp_types::Position { line, character: 0 },
    };

    lsp_types::Diagnostic {
        range,
        severity: Some(severity),
        code: None,
        code_description: None,
        source: Some("lattice".to_string()),
        message: diag.message.clone(),
        related_information: None,
        tags: None,
        data: None,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for clarity")]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn error_maps_to_lsp_error() {
        let diag = Diagnostic {
            file: PathBuf::from("a.md"),
            line: 3,
            severity: Severity::Error,
            message: "target does not exist".to_string(),
        };
        let lsp = to_lsp_diagnostic(&diag);
        assert_eq!(
            lsp.severity,
            Some(DiagnosticSeverity::ERROR),
            "error should map to LSP ERROR"
        );
        assert_eq!(lsp.range.start.line, 2, "line 3 should map to LSP line 2");
        assert_eq!(
            lsp.source.as_deref(),
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
        let lsp = to_lsp_diagnostic(&diag);
        assert_eq!(
            lsp.severity,
            Some(DiagnosticSeverity::WARNING),
            "warning should map to LSP WARNING"
        );
        assert_eq!(lsp.range.start.line, 0, "line 1 should map to LSP line 0");
    }

    #[test]
    fn info_maps_to_lsp_information() {
        let diag = Diagnostic {
            file: PathBuf::from("c.md"),
            line: 5,
            severity: Severity::Info,
            message: "no explicit predicate".to_string(),
        };
        let lsp = to_lsp_diagnostic(&diag);
        assert_eq!(
            lsp.severity,
            Some(DiagnosticSeverity::INFORMATION),
            "info should map to LSP INFORMATION"
        );
    }

    #[test]
    #[allow(
        deprecated,
        reason = "DocumentSymbol::deprecated field is deprecated in lsp-types"
    )]
    fn heading_symbols_nest_by_level() {
        use crate::markdown::HeadingId;

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
        let uri: Uri = "file:///home/user/project/doc.md"
            .parse()
            .expect("valid URI");
        let path = uri_to_path(&uri);
        assert_eq!(
            path,
            PathBuf::from("/home/user/project/doc.md"),
            "should extract filesystem path from URI"
        );
    }

    #[test]
    fn path_to_uri_creates_file_uri() {
        let uri = path_to_uri(Path::new("/home/user/project/doc.md")).expect("should create URI");
        assert_eq!(
            uri.as_str(),
            "file:///home/user/project/doc.md",
            "should create file:// URI"
        );
    }
}
