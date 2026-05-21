// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! LSP server for Lattice.
//!
//! Diagnostic-only server: publishes diagnostics on file open, save, and
//! change. No interactive features (completions, hover, etc.).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use lsp_server::{Connection, Message, Notification};
use lsp_types::notification::{
    DidChangeTextDocument, DidOpenTextDocument, DidSaveTextDocument, Notification as _,
};
use lsp_types::{
    DiagnosticSeverity, InitializeParams, SaveOptions, ServerCapabilities,
    TextDocumentSyncCapability, TextDocumentSyncKind, TextDocumentSyncOptions,
    TextDocumentSyncSaveOptions, Uri,
};

use crate::validation::{self, Diagnostic, Severity};
use crate::workspace::Workspace;

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
        ..ServerCapabilities::default()
    };

    let caps_json =
        serde_json::to_value(&capabilities).context("failed to serialize capabilities")?;
    let init_params = connection.initialize(caps_json)?;
    let params: InitializeParams =
        serde_json::from_value(init_params).context("failed to parse InitializeParams")?;

    let root = workspace_root(&params)?;
    let workspace = Workspace::scan(&root).context("failed to scan workspace")?;

    main_loop(&connection, workspace)?;
    io_threads.join()?;

    Ok(())
}

/// Extract the workspace root from initialize params.
fn workspace_root(params: &InitializeParams) -> Result<PathBuf> {
    // Try workspace folders first (modern LSP), fall back to deprecated root_uri.
    if let Some(folders) = &params.workspace_folders
        && let Some(folder) = folders.first()
    {
        return Ok(uri_to_path(&folder.uri));
    }

    #[allow(deprecated, reason = "root_uri is the fallback for older clients")]
    if let Some(root_uri) = &params.root_uri {
        return Ok(uri_to_path(root_uri));
    }

    bail!("no workspace root provided by client")
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
fn main_loop(connection: &Connection, mut workspace: Workspace) -> Result<()> {
    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                if connection.handle_shutdown(&req)? {
                    return Ok(());
                }
            }
            Message::Notification(notif) => {
                handle_notification(connection, &mut workspace, notif)?;
            }
            Message::Response(_) => {}
        }
    }
    Ok(())
}

/// Dispatch a notification.
fn handle_notification(
    connection: &Connection,
    workspace: &mut Workspace,
    notif: Notification,
) -> Result<()> {
    match notif.method.as_str() {
        DidOpenTextDocument::METHOD => {
            let params: lsp_types::DidOpenTextDocumentParams =
                serde_json::from_value(notif.params)?;
            let Some(rel_path) = to_rel_path(workspace, &params.text_document.uri) else {
                return Ok(());
            };
            workspace.update_content(&rel_path, &params.text_document.text);
            publish_all_diagnostics(connection, workspace)?;
        }
        DidSaveTextDocument::METHOD => {
            let params: lsp_types::DidSaveTextDocumentParams =
                serde_json::from_value(notif.params)?;
            let Some(rel_path) = to_rel_path(workspace, &params.text_document.uri) else {
                return Ok(());
            };
            if let Some(text) = &params.text {
                workspace.update_content(&rel_path, text);
            } else {
                // Re-read from disk if no text included.
                let _ = workspace.update(&rel_path);
            }
            publish_all_diagnostics(connection, workspace)?;
        }
        DidChangeTextDocument::METHOD => {
            let params: lsp_types::DidChangeTextDocumentParams =
                serde_json::from_value(notif.params)?;
            let Some(rel_path) = to_rel_path(workspace, &params.text_document.uri) else {
                return Ok(());
            };
            // We requested FULL sync, so the last change has the full text.
            if let Some(change) = params.content_changes.into_iter().last() {
                workspace.update_content(&rel_path, &change.text);
                publish_all_diagnostics(connection, workspace)?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Convert an LSP URI to a workspace-relative path.
fn to_rel_path(workspace: &Workspace, uri: &Uri) -> Option<PathBuf> {
    let path = uri_to_path(uri);
    path.strip_prefix(workspace.root()).ok().map(PathBuf::from)
}

/// Publish diagnostics for all files that have issues, and clear diagnostics
/// for files that no longer have issues.
fn publish_all_diagnostics(connection: &Connection, workspace: &Workspace) -> Result<()> {
    let all_diagnostics = validation::collect_all(workspace);

    // Group diagnostics by file.
    let mut by_file: std::collections::BTreeMap<PathBuf, Vec<lsp_types::Diagnostic>> =
        std::collections::BTreeMap::new();

    for diag in &all_diagnostics {
        by_file
            .entry(diag.file.clone())
            .or_default()
            .push(to_lsp_diagnostic(diag));
    }

    // Collect all files that should have diagnostics published.
    let mut published: BTreeSet<PathBuf> = by_file.keys().cloned().collect();

    // Also publish empty diagnostics for all known files (to clear stale ones).
    for path in workspace.files().keys() {
        published.insert(path.clone());
    }

    for rel_path in &published {
        let abs_path = workspace.root().join(rel_path);
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
