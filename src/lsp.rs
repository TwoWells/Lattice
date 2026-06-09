// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Minimal LSP protocol types.
//!
//! Only the subset Lattice actually uses. Serialized/deserialized with serde
//! to match the LSP JSON wire format.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Primitives
// ---------------------------------------------------------------------------

/// 0-based line and character offset.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    /// 0-based line number.
    pub line: u32,
    /// 0-based UTF-16 character offset.
    pub character: u32,
}

/// A range in a text document.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    /// Start position (inclusive).
    pub start: Position,
    /// End position (exclusive).
    pub end: Position,
}

/// A location in a document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Location {
    /// Document URI.
    pub uri: String,
    /// Range within the document.
    pub range: Range,
}

/// A text edit.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextEdit {
    /// Range to replace.
    pub range: Range,
    /// Replacement text.
    pub new_text: String,
}

// ---------------------------------------------------------------------------
// Initialization
// ---------------------------------------------------------------------------

/// Subset of `InitializeParams` we read.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeParams {
    /// Workspace folders.
    pub workspace_folders: Option<Vec<WorkspaceFolder>>,
    /// Deprecated root URI fallback.
    pub root_uri: Option<String>,
}

/// A workspace folder.
#[derive(Debug, Deserialize)]
pub struct WorkspaceFolder {
    /// Folder URI.
    pub uri: String,
}

// ---------------------------------------------------------------------------
// Text document identification
// ---------------------------------------------------------------------------

/// Identifies a text document.
#[derive(Debug, Deserialize)]
pub struct TextDocumentIdentifier {
    /// Document URI.
    pub uri: String,
}

/// Identifies a versioned text document.
#[derive(Debug, Deserialize)]
pub struct VersionedTextDocumentIdentifier {
    /// Document URI.
    pub uri: String,
}

/// A text document item (full content).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextDocumentItem {
    /// Document URI.
    pub uri: String,
    /// Document content.
    pub text: String,
}

/// Position in a text document.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextDocumentPositionParams {
    /// The document.
    pub text_document: TextDocumentIdentifier,
    /// The position.
    pub position: Position,
}

// ---------------------------------------------------------------------------
// Notifications
// ---------------------------------------------------------------------------

/// `textDocument/didOpen` params.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DidOpenTextDocumentParams {
    /// The opened document.
    pub text_document: TextDocumentItem,
}

/// `textDocument/didSave` params.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DidSaveTextDocumentParams {
    /// The saved document.
    pub text_document: TextDocumentIdentifier,
    /// Full content if `includeText` was requested.
    pub text: Option<String>,
}

/// `textDocument/didChange` params.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DidChangeTextDocumentParams {
    /// The changed document.
    pub text_document: VersionedTextDocumentIdentifier,
    /// Content changes (we request FULL sync, so last entry has full text).
    pub content_changes: Vec<TextDocumentContentChangeEvent>,
}

/// A content change event.
#[derive(Debug, Deserialize)]
pub struct TextDocumentContentChangeEvent {
    /// The new text of the document (for FULL sync).
    pub text: String,
}

/// `workspace/didChangeWorkspaceFolders` params.
#[derive(Debug, Deserialize)]
pub struct DidChangeWorkspaceFoldersParams {
    /// The change event.
    pub event: WorkspaceFoldersChangeEvent,
}

/// Workspace folder change event.
#[derive(Debug, Deserialize)]
pub struct WorkspaceFoldersChangeEvent {
    /// Added folders.
    pub added: Vec<WorkspaceFolder>,
    /// Removed folders.
    pub removed: Vec<WorkspaceFolder>,
}

// ---------------------------------------------------------------------------
// Requests — params
// ---------------------------------------------------------------------------

/// `textDocument/documentSymbol` params.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentSymbolParams {
    /// The document.
    pub text_document: TextDocumentIdentifier,
}

/// `workspace/symbol` params.
#[derive(Debug, Deserialize)]
pub struct WorkspaceSymbolParams {
    /// Search query.
    pub query: String,
}

/// `textDocument/rename` params.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenameParams {
    /// The document and position.
    pub text_document: TextDocumentIdentifier,
    /// The position.
    pub position: Position,
    /// The new name.
    pub new_name: String,
}

/// `textDocument/references` params.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReferenceParams {
    /// The document.
    pub text_document: TextDocumentIdentifier,
    /// The position.
    pub position: Position,
}

/// `typeHierarchy/supertypes` or `typeHierarchy/subtypes` params.
#[derive(Debug, Deserialize)]
pub struct TypeHierarchyParams {
    /// The item to resolve.
    pub item: HierarchyItem,
}

/// `callHierarchy/incomingCalls` or `callHierarchy/outgoingCalls` params.
#[derive(Debug, Deserialize)]
pub struct CallHierarchyParams {
    /// The item to resolve.
    pub item: HierarchyItem,
}

// ---------------------------------------------------------------------------
// Requests — responses
// ---------------------------------------------------------------------------

/// A document symbol with nested children.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentSymbol {
    /// Symbol name.
    pub name: String,
    /// Optional detail string (e.g. dimensions, display text).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Symbol kind (numeric LSP `SymbolKind`).
    pub kind: u32,
    /// Full range of the symbol.
    pub range: Range,
    /// Range of the symbol name for selection.
    pub selection_range: Range,
    /// Child symbols.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<Self>>,
}

/// A flat symbol (workspace symbol results).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SymbolInformation {
    /// Symbol name.
    pub name: String,
    /// Symbol kind.
    pub kind: u32,
    /// Location.
    pub location: Location,
    /// Container name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub container_name: Option<String>,
}

/// A workspace edit.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceEdit {
    /// URI → edits mapping.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changes: Option<std::collections::HashMap<String, Vec<TextEdit>>>,
}

/// An LSP diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    /// Range.
    pub range: Range,
    /// Severity (1=Error, 2=Warning, 3=Information, 4=Hint).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severity: Option<u32>,
    /// Source identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Message.
    pub message: String,
}

/// `textDocument/publishDiagnostics` params.
#[derive(Debug, Serialize)]
pub struct PublishDiagnosticsParams {
    /// Document URI.
    pub uri: String,
    /// Diagnostics.
    pub diagnostics: Vec<Diagnostic>,
}

/// A hierarchy item used for both type hierarchy and call hierarchy.
///
/// Serialized/deserialized identically for both protocols — the LSP wire
/// format is the same.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HierarchyItem {
    /// Symbol name.
    pub name: String,
    /// Symbol kind.
    pub kind: u32,
    /// Document URI.
    pub uri: String,
    /// Full range.
    pub range: Range,
    /// Selection range.
    pub selection_range: Range,
    /// Optional detail string.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Opaque data preserved between prepare and resolve.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// A document link (ctrl-clickable).
#[derive(Debug, Clone, Serialize)]
pub struct DocumentLink {
    /// Range of the link in the source document.
    pub range: Range,
    /// Target URI.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

/// A folding range.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FoldingRange {
    /// 0-based start line.
    pub start_line: u32,
    /// 0-based end line.
    pub end_line: u32,
    /// Folding range kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// A hover result.
#[derive(Debug, Clone, Serialize)]
pub struct Hover {
    /// Hover content.
    pub contents: MarkupContent,
}

/// Markup content for hover.
#[derive(Debug, Clone, Serialize)]
pub struct MarkupContent {
    /// Content kind ("markdown" or "plaintext").
    pub kind: String,
    /// The actual content.
    pub value: String,
}

/// Full document diagnostic report.
#[derive(Debug, Clone, Serialize)]
pub struct FullDocumentDiagnosticReport {
    /// Report kind — always "full".
    pub kind: String,
    /// Diagnostics.
    pub items: Vec<Diagnostic>,
}

/// A single workspace document diagnostic report.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceDocumentDiagnosticReport {
    /// Report kind — always "full".
    pub kind: String,
    /// Document URI.
    pub uri: String,
    /// Diagnostics.
    pub items: Vec<Diagnostic>,
}

/// Workspace diagnostic report.
#[derive(Debug, Clone, Serialize)]
pub struct WorkspaceDiagnosticReport {
    /// Per-document reports.
    pub items: Vec<WorkspaceDocumentDiagnosticReport>,
}

/// `textDocument/formatting` params.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentFormattingParams {
    /// The document to format.
    pub text_document: TextDocumentIdentifier,
}

/// `textDocument/diagnostic` params.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentDiagnosticParams {
    /// The document to get diagnostics for.
    pub text_document: TextDocumentIdentifier,
}

/// A completion item.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionItem {
    /// Display label (also the default inserted text).
    pub label: String,
    /// Completion item kind (numeric LSP `CompletionItemKind`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<u32>,
    /// Optional detail string (e.g. the inverse predicate, or a target URL).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Text the client filters against the typed prefix.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter_text: Option<String>,
    /// Text the client orders items by.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_text: Option<String>,
    /// Replacement edit applied when the item is selected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_edit: Option<TextEdit>,
}

/// A completion result list.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompletionList {
    /// Whether the list is incomplete (recomputed as the user types further).
    pub is_incomplete: bool,
    /// The completion items.
    pub items: Vec<CompletionItem>,
}

/// An incoming call.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CallHierarchyIncomingCall {
    /// The calling item.
    pub from: HierarchyItem,
    /// Ranges in the caller where the call appears.
    pub from_ranges: Vec<Range>,
}

/// An outgoing call.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CallHierarchyOutgoingCall {
    /// The called item.
    pub to: HierarchyItem,
    /// Ranges in the current item where the call appears.
    pub from_ranges: Vec<Range>,
}

// ---------------------------------------------------------------------------
// LSP constants
// ---------------------------------------------------------------------------

/// LSP `SymbolKind` constants (subset).
pub mod symbol_kind {
    /// File (media embeds: image, video, audio, iframe).
    pub const FILE: u32 = 1;
    /// Module (scope containers: blockquote, admonition, details, generic containers).
    pub const MODULE: u32 = 2;
    /// Class (headings — type definitions in the document hierarchy).
    pub const CLASS: u32 = 5;
    /// Field (table column headers).
    pub const FIELD: u32 = 8;
    /// Function (links — graph edges).
    pub const FUNCTION: u32 = 12;
    /// Constant (footnote definitions).
    pub const CONSTANT: u32 = 14;
    /// Object (opaque content blocks: code blocks, math).
    pub const OBJECT: u32 = 19;
    /// Struct (data containers: tables, lists).
    pub const STRUCT: u32 = 23;
    /// Event (form elements: `<input>`, `<select>`, `<textarea>`).
    pub const EVENT: u32 = 24;
    /// Operator (thematic breaks).
    pub const OPERATOR: u32 = 25;
}

/// LSP `CompletionItemKind` constants (subset).
pub mod completion_item_kind {
    /// Value (heading fragments / anchors).
    pub const VALUE: u32 = 12;
    /// Keyword (predicate vocabulary).
    pub const KEYWORD: u32 = 14;
    /// File (link-target files).
    pub const FILE: u32 = 17;
    /// Reference (link reference labels).
    pub const REFERENCE: u32 = 18;
    /// Folder (link-target directories).
    pub const FOLDER: u32 = 19;
    /// Constant (footnote labels).
    pub const CONSTANT: u32 = 21;
}

/// LSP `DiagnosticSeverity` constants.
pub mod diagnostic_severity {
    /// Error.
    pub const ERROR: u32 = 1;
    /// Warning.
    pub const WARNING: u32 = 2;
    /// Information.
    pub const INFORMATION: u32 = 3;
    /// Hint.
    pub const HINT: u32 = 4;
}

/// LSP method name constants.
pub mod method {
    // Notifications
    /// `textDocument/didOpen`.
    pub const DID_OPEN: &str = "textDocument/didOpen";
    /// `textDocument/didSave`.
    pub const DID_SAVE: &str = "textDocument/didSave";
    /// `textDocument/didChange`.
    pub const DID_CHANGE: &str = "textDocument/didChange";
    /// `workspace/didChangeWorkspaceFolders`.
    pub const DID_CHANGE_WORKSPACE_FOLDERS: &str = "workspace/didChangeWorkspaceFolders";
    /// `textDocument/publishDiagnostics`.
    pub const PUBLISH_DIAGNOSTICS: &str = "textDocument/publishDiagnostics";

    // Requests
    /// `textDocument/documentSymbol`.
    pub const DOCUMENT_SYMBOL: &str = "textDocument/documentSymbol";
    /// `workspace/symbol`.
    pub const WORKSPACE_SYMBOL: &str = "workspace/symbol";
    /// `textDocument/prepareRename`.
    pub const PREPARE_RENAME: &str = "textDocument/prepareRename";
    /// `textDocument/rename`.
    pub const RENAME: &str = "textDocument/rename";
    /// `textDocument/references`.
    pub const REFERENCES: &str = "textDocument/references";
    /// `textDocument/declaration`.
    pub const DECLARATION: &str = "textDocument/declaration";
    /// `textDocument/definition`.
    pub const DEFINITION: &str = "textDocument/definition";
    /// `textDocument/typeDefinition`.
    pub const TYPE_DEFINITION: &str = "textDocument/typeDefinition";
    /// `textDocument/implementation`.
    pub const IMPLEMENTATION: &str = "textDocument/implementation";
    /// `textDocument/prepareTypeHierarchy`.
    pub const PREPARE_TYPE_HIERARCHY: &str = "textDocument/prepareTypeHierarchy";
    /// `typeHierarchy/supertypes`.
    pub const TYPE_HIERARCHY_SUPERTYPES: &str = "typeHierarchy/supertypes";
    /// `typeHierarchy/subtypes`.
    pub const TYPE_HIERARCHY_SUBTYPES: &str = "typeHierarchy/subtypes";
    /// `textDocument/prepareCallHierarchy`.
    pub const PREPARE_CALL_HIERARCHY: &str = "textDocument/prepareCallHierarchy";
    /// `callHierarchy/incomingCalls`.
    pub const CALL_HIERARCHY_INCOMING: &str = "callHierarchy/incomingCalls";
    /// `callHierarchy/outgoingCalls`.
    pub const CALL_HIERARCHY_OUTGOING: &str = "callHierarchy/outgoingCalls";
    /// `textDocument/documentLink`.
    pub const DOCUMENT_LINK: &str = "textDocument/documentLink";
    /// `textDocument/foldingRange`.
    pub const FOLDING_RANGE: &str = "textDocument/foldingRange";
    /// `textDocument/hover`.
    pub const HOVER: &str = "textDocument/hover";
    /// `textDocument/diagnostic`.
    pub const DOCUMENT_DIAGNOSTIC: &str = "textDocument/diagnostic";
    /// `workspace/diagnostic`.
    pub const WORKSPACE_DIAGNOSTIC: &str = "workspace/diagnostic";
    /// `textDocument/formatting`.
    pub const FORMATTING: &str = "textDocument/formatting";
    /// `textDocument/completion`.
    pub const COMPLETION: &str = "textDocument/completion";
}
