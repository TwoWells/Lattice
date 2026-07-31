// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! LSP server for Lattice.
//!
//! Publishes diagnostics on file open, save, and change. Provides workspace
//! symbols, rename, references, type hierarchy, and call hierarchy for
//! headings. Supports multiple workspace folders.
//!
//! # Which copy does this surface read? (decision 024 clause 9)
//!
//! Content lives in the two-tier [`crate::store`]: a **saved** copy per indexed
//! document (disk truth) and an **overlay** copy per open, diverged buffer.
//! Every surface declares which it reads, and the asymmetry is deliberate — a
//! `WorkspaceEdit` is consumed synchronously by the client that owns those
//! buffers, whereas a diagnostic persists and would be re-resolved later
//! against a state that may have moved.
//!
//! - **Diagnostics read perspective.** A document's rows are computed from the
//!   saved world with *its own* buffer overlaid, and nobody else's
//!   ([`merge_perspectives`]). This is buffer locality, the model's headline.
//! - **Reads and edits read current.** Hover, symbols, folding, semantic
//!   tokens, navigation, completion, formatting, `rename` and
//!   `willRenameFiles` all resolve through [`Workspaces::resolve_document`] /
//!   [`Workspaces::current_view`]: buffer where one exists, saved copy
//!   everywhere else. An edit computed against saved coordinates and applied to
//!   a diverged buffer would land in the wrong place, and a position-bearing
//!   answer must be anchored in the text on screen.
//!
//! # Standing constraint: no cross-file positions in diagnostics (clause 10)
//!
//! A [`Diagnostic`] carries no `relatedInformation`, and under this model it
//! must not gain one that points into another file: there is no vocabulary for
//! a position in another document's *current* state. Such a position would be
//! saved-state coordinates resolved against a possibly-diverged buffer — wrong
//! by construction, and silently so.

mod diagnostics;
mod workspaces;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};

use crate::block::{
    ElementKind, Heading, HeadingId, LinkKind, NodeId, Syntax, Tree, content_lines, first_line,
    normalize_label,
};
use crate::completion::Context as CompletionContext;
use crate::config::{Config, ConfigError, FragmentAlgorithm};
use crate::line_index::LineIndex;
use crate::lsp;
use crate::overrides::{self, OverrideVerdicts, VerdictKind};
use crate::span::Span;
use crate::store::DiskUpdate;
use crate::uri::{path_to_uri, uri_to_path};
use crate::validation::{self, Diagnostic, Severity};
use crate::workspace::{FileData, WorkspaceView, target_to_key};

use self::diagnostics::{file_desired, to_lsp_diagnostic};
use self::workspaces::{PublishedDiagnostics, RootMeta, Workspaces};

pub use self::diagnostics::collect_all_diagnostics;

// ---------------------------------------------------------------------------
// Semantic tokens legend (ticket integration 15)
// ---------------------------------------------------------------------------

/// The single semantic token type Lattice emits. All emphasis runs carry this
/// base type and distinguish themselves through modifiers, so overlapping runs
/// (strong inside emphasis) compose into one token with combined modifiers
/// rather than two illegal overlapping tokens.
const SEMANTIC_TOKEN_TYPE_MARKUP: &str = "markup";
/// Modifier name for strong (`**bold**`) runs.
const SEMANTIC_MODIFIER_BOLD: &str = "bold";
/// Modifier name for emphasis (`*italic*`) runs.
const SEMANTIC_MODIFIER_ITALIC: &str = "italic";
/// Modifier name for strikethrough (`~~struck~~`) runs.
const SEMANTIC_MODIFIER_STRIKETHROUGH: &str = "strikethrough";

/// Token-type index into the legend's `tokenTypes` array. Only `markup`
/// (index 0) exists.
const SEMANTIC_TOKEN_TYPE_MARKUP_INDEX: u32 = 0;
/// Modifier bit for `bold` — index 0 in the legend's `tokenModifiers` array.
const SEMANTIC_MODIFIER_BOLD_BIT: u32 = 1 << 0;
/// Modifier bit for `italic` — index 1 in the legend's `tokenModifiers` array.
const SEMANTIC_MODIFIER_ITALIC_BIT: u32 = 1 << 1;
/// Modifier bit for `strikethrough` — index 2 in the legend's `tokenModifiers`
/// array.
const SEMANTIC_MODIFIER_STRIKETHROUGH_BIT: u32 = 1 << 2;

/// Fixed registration id for the `.lattice.toml` watcher.
///
/// Registration is fire-and-forget — Lattice registers the watcher once after
/// initialization and never unregisters it — so a constant id suffices
/// (decision 017, ticket server 08).
const WATCHED_FILES_REGISTRATION_ID: &str = "lattice-watched-files";

/// Fixed request id for the server-originated `client/registerCapability`
/// request. The client's response is discarded by [`main_loop`], so a constant
/// id is fine.
const REGISTER_CAPABILITY_REQUEST_ID: &str = "lattice-register-capability";

/// Glob the marker watcher subscribes to: the project-level `.lattice.toml`
/// at any depth under a workspace folder (decision 017, ticket server 08).
const LATTICE_TOML_WATCH_GLOB: &str = "**/.lattice.toml";

/// Glob the document watcher subscribes to: every markdown file at any depth
/// under a workspace folder (decision 017 §1, ticket server 09). It is the sole
/// writer of the saved store's `.md` content alongside the initial scan and
/// `didSave`, so there is nothing to reconcile it against: the document-sync
/// channel writes the overlay store instead (decision 024).
const MD_WATCH_GLOB: &str = "**/*.md";

/// Run the LSP server on stdio.
///
/// # Errors
///
/// Returns an error if the connection or initialization fails.
pub fn run() -> Result<()> {
    let (connection, io_threads) = Connection::stdio();
    serve(&connection)?;
    drop(connection); // Close channels so IO threads can exit.
    io_threads.join()?;
    Ok(())
}

/// Drive the LSP lifecycle on an established connection: the capabilities
/// handshake, the watched-files registration, then the message loop.
///
/// Split out from [`run`] so the wire protocol can be exercised over an
/// in-memory connection in tests without spawning real stdio IO threads.
///
/// # Errors
///
/// Returns an error if initialization or the message loop fails.
fn serve(connection: &Connection) -> Result<()> {
    // Two-phase init so the capabilities we advertise can depend on the client's
    // own capabilities: `workspace/willRenameFiles` is advertised only to a
    // client that sends it (decision 020 clause 2). `initialize_start` returns
    // the client params before we must send our own, so we parse them first,
    // build the capabilities conditionally, then finish the handshake.
    let (init_id, init_value) = connection.initialize_start()?;
    let params: lsp::InitializeParams =
        serde_json::from_value(init_value).context("failed to parse InitializeParams")?;

    let capabilities = server_capabilities(&params);
    connection.initialize_finish(init_id, serde_json::json!({ "capabilities": capabilities }))?;

    let mut workspaces = Workspaces::from_params(&params);

    // File watchers are dynamic-registration only, so register the
    // `.lattice.toml` watcher now — after `initialized` — when the client
    // advertises support. A client without it degrades to startup-only config
    // (decision 017); Lattice never runs its own watcher.
    //
    // Registration goes out *before* the cold-start publish below: it is a
    // fire-and-forget request (its response is drained by `main_loop`), so
    // arming the watch first means no disk change can slip past while the
    // initial pass computes. The two are otherwise independent.
    if params.supports_watched_files_dynamic_registration() {
        register_watched_files(connection)?;
    }

    // The cold-start publish. The scan has run, so the server already knows
    // every finding in the workspace; on a push-only transport (decision 022)
    // it owes the client that knowledge rather than sitting on it until some
    // event happens to trigger a full pass.
    //
    // This does not weaken decision 024 clause 2. Clause 2 governs what may
    // *move* a document's rows, and nothing here moves anything: no buffer
    // exists yet, every row is computed from the saved world, and the publish
    // diff sends only what the client does not already hold. Before the split,
    // the first `didOpen`'s full pass introduced the workspace as a side
    // effect; scoping buffer events to one document (clause 2's enforceable
    // pair) removed that accident, so the introduction becomes explicit here
    // instead of being lost. `Commit` adjudication, config channels included —
    // it is a disk-anchored pass like any other commitment.
    //
    // Exactly once: every subsequent commitment runs the same diff against the
    // cache this pass populates, so nothing is double-sent.
    publish_all_diagnostics(connection, &mut workspaces, &HashSet::new())?;

    main_loop(connection, workspaces)?;

    Ok(())
}

/// Build the server capabilities to advertise, gating client-dependent surfaces.
///
/// Every static capability is unconditional; `workspace.fileOperations.willRename`
/// is advertised only when the client sends `workspace/willRenameFiles`
/// (decision 020 clause 2), with registration filters that scope the request to
/// markdown files and folders so the client never sends it for an unrelated
/// asset. A client without the capability gets no `fileOperations` block, so an
/// editor rename behaves exactly as before.
fn server_capabilities(params: &lsp::InitializeParams) -> serde_json::Value {
    let mut capabilities = serde_json::json!({
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
        // Diagnostics are push-only by design (decision 022), not by revert.
        // Push (`publishDiagnostics`) is the only transport that proactively
        // covers the *closed* target file — where a backlink diagnostic lands
        // when its source is edited — so it is the right transport for a graph
        // linter, not a fallback. `didOpen` resets the per-URI publish diff
        // because a client's memory of a reopened document is unknowable, and
        // `didSave` is answered unconditionally because push-only silence is
        // indistinguishable from no answer (`force_republish`, issue 062) —
        // which closes the only gaps pull papered over.
        // Advertising pull (`diagnosticProvider`) *and* pushing makes
        // spec-compliant clients (e.g. Neovim 0.11) render every diagnostic
        // twice, so pull is not advertised — and any future pull support must
        // be capability-negotiated per session with disjoint open/closed
        // transports, never merely "don't advertise both".
        "documentFormattingProvider": true,
        "completionProvider": {
            // Destination open, path separator, fragment, title quote, and
            // reference/footnote open (ticket integration 14).
            "triggerCharacters": ["(", "/", "#", "\"", "[", "^"]
        },
        // Inline emphasis highlighting (ticket integration 15). One custom
        // token type, `markup`, carrying `bold` / `italic` / `strikethrough`
        // modifiers, so a character covered by overlapping runs (e.g. the
        // `foo` in `***foo***`) gets a single token with both modifiers.
        // Custom legend entries are spec-legal; clients that don't recognize
        // them skip them. The legend index is positional: `tokenType` and the
        // `tokenModifiers` bitmask in each emitted quintuple index into these
        // arrays. `full/delta` is not advertised — re-encoding only the
        // emphasis runs is already cheap, and a delta seam waits on the perf
        // workstream's "what changed" diff (see `semantic_tokens_full`).
        "semanticTokensProvider": {
            "legend": {
                "tokenTypes": [SEMANTIC_TOKEN_TYPE_MARKUP],
                "tokenModifiers": [
                    SEMANTIC_MODIFIER_BOLD,
                    SEMANTIC_MODIFIER_ITALIC,
                    SEMANTIC_MODIFIER_STRIKETHROUGH,
                ]
            },
            "full": true,
            "range": true
        },
        "workspace": {
            "workspaceFolders": {
                "supported": true,
                "changeNotifications": true
            }
        }
    });

    // The move surface (decision 020 clause 2) is advertised only to a client
    // that sends `workspace/willRenameFiles`. Its registration filters scope the
    // request to markdown files and folders, matching the engine's move domain
    // — an asset rename never trips the client into asking. A client without the
    // capability sees no `fileOperations` block, so it moves files blind, exactly
    // as before this ticket.
    if params.supports_will_rename_files()
        && let Some(workspace) = capabilities
            .get_mut("workspace")
            .and_then(serde_json::Value::as_object_mut)
    {
        workspace.insert(
            "fileOperations".to_string(),
            serde_json::json!({
                "willRename": {
                    "filters": [
                        { "scheme": "file", "pattern": { "glob": MD_WATCH_GLOB, "matches": "file" } },
                        { "scheme": "file", "pattern": { "glob": "**/*", "matches": "folder" } }
                    ]
                }
            }),
        );
    }

    capabilities
}

/// Register the watched-file globs with the client.
///
/// Sends a `client/registerCapability` request for
/// `workspace/didChangeWatchedFiles` with two watcher globs: the marker
/// [`LATTICE_TOML_WATCH_GLOB`] (ticket server 08) and the document
/// [`MD_WATCH_GLOB`] (ticket server 09). This is the only way to subscribe to
/// file changes — there is no static server-capability field for watchers
/// (decision 017). The request is fire-and-forget: the client's `Response` is
/// discarded by [`main_loop`], so a fixed registration id and request id are
/// sufficient.
fn register_watched_files(connection: &Connection) -> Result<()> {
    let params = serde_json::json!({
        "registrations": [
            {
                "id": WATCHED_FILES_REGISTRATION_ID,
                "method": lsp::method::DID_CHANGE_WATCHED_FILES,
                "registerOptions": {
                    "watchers": [
                        { "globPattern": LATTICE_TOML_WATCH_GLOB },
                        { "globPattern": MD_WATCH_GLOB }
                    ]
                }
            }
        ]
    });
    let req = Request::new(
        RequestId::from(REGISTER_CAPABILITY_REQUEST_ID.to_string()),
        lsp::method::REGISTER_CAPABILITY.to_string(),
        params,
    );
    connection.sender.send(Message::Request(req))?;
    Ok(())
}

/// Whether a URI names a markdown file (a case-insensitive `.md` extension),
/// matching how [`crate::workspace`] discovers indexed files. Used to route a
/// `workspace/didChangeWatchedFiles` event onto the document-sync path
/// (ticket server 09).
fn is_markdown_uri(uri: &str) -> bool {
    Path::new(uri)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
}

/// Whether a URI names a `.lattice.toml` marker — the config channel's URI
/// space (decision 023 clause 4). Routes both the watched-marker pass and the
/// accepted-never-required document-sync events for the config: a config
/// URI is a diagnosed document, never an indexed or buffer-authoritative one.
fn is_config_uri(uri: &str) -> bool {
    uri.ends_with(".lattice.toml")
}

/// A watched-file event's change kind as a log-friendly name (issue 068).
///
/// A trace for a dropped event has to say *what* was dropped — a create going
/// missing is a membership phantom, a change going missing is stale content —
/// and a bare protocol integer makes the reader look that up. Anything outside
/// [`lsp::file_change_type`]'s three values is `unknown`: the server has no
/// handling for it, which is itself the thing worth reporting.
const fn file_change_kind(change_type: u8) -> &'static str {
    match change_type {
        lsp::file_change_type::CREATED => "created",
        lsp::file_change_type::CHANGED => "changed",
        lsp::file_change_type::DELETED => "deleted",
        _ => "unknown",
    }
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
        lsp::method::WILL_RENAME_FILES => {
            let params: lsp::RenameFilesParams = serde_json::from_value(req.params)?;
            match will_rename_files(workspaces, &params) {
                // The forced edit set: the client applies it, then performs the
                // rename (decision 020 clause 2).
                Ok(edit) => Response::new_ok(req.id, edit),
                // A refused move (decision 020 clause 6). The message names the
                // fix; the JSON-RPC error aborts the rename client-side, so the
                // file does not move.
                Err(message) => {
                    Response::new_err(req.id, lsp_server::ErrorCode::RequestFailed as i32, message)
                }
            }
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
        lsp::method::FORMATTING => {
            let params: lsp::DocumentFormattingParams = serde_json::from_value(req.params)?;
            let edits = format_document(workspaces, &params.text_document.uri);
            Response::new_ok(req.id, edits)
        }
        lsp::method::COMPLETION => {
            // `context` (the trigger char) is ignored — the surface and partial
            // are recovered from the line prefix. The extra field deserializes
            // fine into `TextDocumentPositionParams` (unknown fields skipped).
            let params: lsp::TextDocumentPositionParams = serde_json::from_value(req.params)?;
            let list = completion(workspaces, &params);
            Response::new_ok(req.id, list)
        }
        lsp::method::SEMANTIC_TOKENS_FULL => {
            let params: lsp::SemanticTokensParams = serde_json::from_value(req.params)?;
            let tokens = semantic_tokens_full(workspaces, &params.text_document.uri);
            Response::new_ok(req.id, tokens)
        }
        lsp::method::SEMANTIC_TOKENS_RANGE => {
            let params: lsp::SemanticTokensRangeParams = serde_json::from_value(req.params)?;
            let tokens =
                semantic_tokens_range(workspaces, &params.text_document.uri, &params.range);
            Response::new_ok(req.id, tokens)
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
        ElementKind::FootnoteDef { .. } => Some(lsp::symbol_kind::CONSTANT),
        ElementKind::FormControl => Some(lsp::symbol_kind::EVENT),
        ElementKind::FrontmatterKey { .. } => Some(lsp::symbol_kind::FIELD),
        // Not emitted: leaf content nodes, structural internals, and thematic
        // breaks (`---`/`***`/`___`) — they are visual separators, not outline
        // entries, and only clutter the symbol list.
        ElementKind::Rules
        | ElementKind::Document
        | ElementKind::Paragraph
        | ElementKind::HtmlBlock
        | ElementKind::InlineCode
        | ElementKind::InlineMath
        | ElementKind::InlineHtml
        | ElementKind::Strong
        | ElementKind::Emphasis
        | ElementKind::Strikethrough
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
                Syntax::Toml => "TOML",
                Syntax::Json => "JSON",
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
    let trimmed = first_line(raw).trim();
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
    let trimmed = first_line(raw).trim();
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
    let trimmed = first_line(raw).trim();
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

    let trimmed = first_line(raw).trim_start();

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
    let start_line = (crate::block::byte_offset_to_line(source, node.span.start) - 1) as u32;
    let end_line = (crate::block::byte_offset_to_line(source, node.span.end) - 1) as u32;
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
    let (workspace, rel_path) = workspaces.resolve_document(uri)?;
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

    // Enumerate rooted documents only — a rootless single-file document (issue
    // 051) is deliberately absent from workspace symbols, as it was when the
    // graph tier enumerated `inner` alone. Each document is visited once under
    // its deepest root, so overlapping folders do not double-list it. A read
    // surface reads **current** text (decision 024 clause 9's audit): the
    // symbol a user is looking for is the one in the buffer on screen.
    for (abs, doc) in workspaces.store.current_documents() {
        let Some(root) = doc.primary_root.as_deref() else {
            continue;
        };
        let rel_path = abs.strip_prefix(root).unwrap_or(abs);
        collect_workspace_symbols(&doc.data.tree, &query_lower, root, rel_path, &mut symbols);
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

        let start_line = (crate::block::byte_offset_to_line(source, node.span.start) - 1) as u32;

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
    let (workspace, rel_path) = workspaces.resolve_document(&params.text_document.uri)?;
    let file_data = workspace.file(&rel_path)?;
    let heading = heading_at_line(&file_data.headings, params.position.line)?;

    Some(span_to_lsp_range(
        file_data.tree.source(),
        &file_data.line_index,
        &heading.text_span,
    ))
}

/// Rename a heading — its own text *and* every fragment that referred to it.
///
/// A heading rename is a coordinate change on the fragment axis exactly as a file
/// move is one on the path axis (issue 057, decision 020), so it rides the same
/// engine: [`crate::mv::compute_heading_rename_edits`] returns the complete
/// forced edit set — the heading's `text_span` (ATX, setext, and HTML alike),
/// every cross-file `file.md#slug` referrer, and every same-document `#slug`
/// anchor — and the same [`merge_span_edits`] mapping turns it into one atomic
/// [`lsp::WorkspaceEdit`]. Path spellings, embeds, prose mentions of the old
/// title, and exception keys are untouched: the judgment surface stays in the
/// loop (decision 020 clause 5).
fn do_rename(workspaces: &Workspaces, params: &lsp::RenameParams) -> Option<lsp::WorkspaceEdit> {
    let (workspace, rel_path) = workspaces.resolve_document(&params.text_document.uri)?;
    let file_data = workspace.file(&rel_path)?;
    let heading = heading_at_line(&file_data.headings, params.position.line)?;

    let edits = crate::mv::compute_heading_rename_edits(
        &workspace,
        &rel_path,
        heading.line,
        &params.new_name,
    )?;

    let mut changes: HashMap<String, Vec<lsp::TextEdit>> = HashMap::new();
    merge_span_edits(workspaces, &edits, &mut changes);

    Some(lsp::WorkspaceEdit {
        changes: Some(changes),
    })
}

// ---------------------------------------------------------------------------
// Editor move surface — workspace/willRenameFiles (ticket mv/02, decision 020)
// ---------------------------------------------------------------------------

/// Answer a `workspace/willRenameFiles` request with the move engine's forced
/// edit set (decision 020 clause 2).
///
/// Each `(oldUri, newUri)` is translated into a [`crate::mv::compute_move_edits`]
/// call over the source's covering scope at each document's **current** text
/// (decision 024 clause 9); every file's edits are converted to LSP ranges
/// (through that file's cached [`LineIndex`]) and merged into one
/// [`lsp::WorkspaceEdit`]. The client applies it to the buffers it holds, then
/// performs the rename; the aftermath needs no special path, because the edits
/// re-enter through the channels that already exist — buffer edits as
/// `didChange`, disk writes as watcher events — plus
/// `workspace/didRenameFiles`'s re-keying.
///
/// A source outside every scope contributes no edits (there is no edit set to
/// compute — a plain rename already does everything Lattice could; decision 020
/// clause 6), so the rename proceeds unimpeded. Any other refusal
/// (cross-marker, existing destination, markdown-ness flip, …) short-circuits
/// the whole batch: `Err(message)` carries the alias-steering / fix-naming
/// text, which the caller returns as a JSON-RPC error so the client aborts the
/// rename and no file moves.
///
/// # Errors
///
/// Returns the refusal message (a [`crate::mv::MoveError`] `Display`) for the
/// first rename the engine refuses.
fn will_rename_files(
    workspaces: &Workspaces,
    params: &lsp::RenameFilesParams,
) -> Result<lsp::WorkspaceEdit, String> {
    let mut changes: HashMap<String, Vec<lsp::TextEdit>> = HashMap::new();

    for rename in &params.files {
        let old_abs = uri_to_path(&rename.old_uri);
        let new_abs = uri_to_path(&rename.new_uri);

        // Without a covering scope there is no keyspace to compute an edit set
        // over — the source is outside every graph. Contribute nothing and let
        // the client's rename proceed (decision 020 clause 6); refusing here
        // would block a legitimate rename of a file Lattice does not manage.
        let Some(root) = workspaces.deepest_root_for(&old_abs) else {
            continue;
        };

        // Decision 024 clause 9: an edit surface computes spans against each
        // touched document's **current** text, because the client applies the
        // returned edits to the buffers it holds. An edit computed against
        // saved coordinates and applied to a diverged buffer lands in the
        // wrong place. "Current" collapses to "saved" for a closed document,
        // so openness is not a condition on service.
        let view = workspaces.current_view(&root);
        let fs_exists = |p: &Path| p.is_file() || p.is_dir();
        let edits = crate::mv::compute_move_edits(&view, &old_abs, &new_abs, &fs_exists)
            .map_err(|e| e.to_string())?;

        merge_span_edits(workspaces, &edits.edits, &mut changes);
    }

    Ok(lsp::WorkspaceEdit {
        changes: Some(changes),
    })
}

/// Convert an engine's per-file byte-span edits into LSP `TextEdit`s and merge
/// them into `changes` (keyed by document URI).
///
/// Each edited file's source and cached [`LineIndex`] come from its **current**
/// copy — the buffer where the client holds one, the saved copy elsewhere
/// (decision 024 clause 9) — which is the same text the engine computed the
/// spans over, so the byte→UTF-16 conversion lands where the client will apply
/// it. A file the store does not hold is skipped — the engines only enumerate
/// files in the view, so this is defensive.
///
/// Shared by both coordinate axes: the path-axis move engine
/// ([`will_rename_files`]) and the fragment-axis heading rename
/// ([`do_rename`]) hand their edit sets to the same mapping, so neither surface
/// has a private notion of a workspace edit.
fn merge_span_edits(
    workspaces: &Workspaces,
    edits: &BTreeMap<PathBuf, Vec<crate::mv::MoveTextEdit>>,
    changes: &mut HashMap<String, Vec<lsp::TextEdit>>,
) {
    for (abs_path, file_edits) in edits {
        let Some(doc) = workspaces.store.current(abs_path) else {
            continue;
        };
        let source = doc.data.tree.source();
        let index = &doc.data.line_index;
        let uri = path_to_uri(abs_path);
        let entry = changes.entry(uri).or_default();
        for edit in file_edits {
            entry.push(lsp::TextEdit {
                range: span_to_lsp_range(source, index, &edit.span),
                new_text: edit.new_text.clone(),
            });
        }
        // A file touched by more than one rename in the batch accumulates edits
        // out of order; sort so the client applies them deterministically.
        entry.sort_by_key(|e| (e.range.start.line, e.range.start.character));
    }
}

/// Find the heading whose line matches the cursor's 0-based line number.
fn heading_at_line(headings: &[Heading], lsp_line: u32) -> Option<&Heading> {
    heading_index_at_line(headings, lsp_line).and_then(|index| headings.get(index))
}

/// The *position* of the heading on the cursor's 0-based line number.
///
/// Fragment resolution answers in heading indices (a fragment names the first
/// heading that answers to it, in document order), so a surface that must
/// compare that answer against the cursor's heading needs its index, not a
/// borrow of it.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn heading_index_at_line(headings: &[Heading], lsp_line: u32) -> Option<usize> {
    headings
        .iter()
        .position(|h| h.line.saturating_sub(1) as u32 == lsp_line)
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
    let Some((workspace, rel_path)) = workspaces.resolve_document(&params.text_document.uri) else {
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

    // Determine if the cursor is on a heading (to filter by fragment). The
    // cached extractions are the ones the rename engine resolves against, so
    // both surfaces answer over the same heading and anchor lists.
    let target_heading = heading_index_at_line(&file_data.headings, params.position.line);
    let algorithm = workspace.config().policy.fragments;

    let mut locations = Vec::new();

    // Scan every rooted document's links for edges to the cursor's document,
    // matching in absolute space: a source's link target resolved against the
    // source's own root equals the cursor document's absolute path exactly when
    // it physically points there (ticket server 11).
    //
    // The match is restricted to the cursor's **own scope** (decision 019):
    // `find_references` is a graph-edge query — "who links here" — and scopes are
    // disjoint graphs, so a physical `../` reference from a foreign scope is a
    // clause-3 defect, not an edge, and must not surface as a reference. (Plain
    // navigation — go-to-definition, outgoing calls — still follows a link
    // physically; only the reverse graph queries honor the partition.)
    //
    // A read surface reads **current** text (decision 024 clause 9's audit):
    // the reference list must name the link the user can see, and a location it
    // returns is resolved by the client against the buffer it holds. Whether
    // `find_references` should instead answer over the saved graph — so the
    // answer matches the committed world — is issue 072's question, left open
    // deliberately rather than settled here.
    let cursor_abs = uri_to_path(&params.text_document.uri);
    let cursor_root = workspaces.store.primary_root(&cursor_abs);
    for (abs, doc) in workspaces.store.current_documents() {
        let Some(root) = doc.primary_root.as_deref() else {
            continue;
        };
        if Some(root) != cursor_root.as_deref() {
            continue;
        }
        let links = doc.data.tree.links(abs);
        for link in &links {
            let LinkKind::IntraProject {
                target, fragment, ..
            } = &link.kind
            else {
                continue;
            };
            if root.join(target) != cursor_abs {
                continue;
            }
            // If cursor is on a heading, only match links whose fragment names
            // *that* heading. Resolution is the shared authority's (issue 072):
            // the same eligible slug forms the fragment diagnostic validates
            // against under the same `[policy] fragments` pin, the same
            // first-match-in-document-order heading identity the rename engine
            // retargets by, and the same explicit raw-HTML anchors — so a
            // spelling the configured renderer would not resolve is not listed
            // as a live edge, and an id an author pinned with `<a id>` is.
            if let Some(heading_index) = target_heading {
                let Some(frag) = fragment else {
                    continue;
                };
                if !crate::fragment::names_heading(
                    &file_data.headings,
                    &file_data.anchors,
                    file_data.tree.source(),
                    algorithm,
                    frag,
                    heading_index,
                ) {
                    continue;
                }
            }
            let line = link.line.saturating_sub(1) as u32;
            locations.push(lsp::Location {
                uri: path_to_uri(abs),
                range: lsp::Range {
                    start: lsp::Position { line, character: 0 },
                    end: lsp::Position { line, character: 0 },
                },
            });
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
    let Some((workspace, rel_path)) = workspaces.resolve_document(uri) else {
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
///
/// The **lenient navigation** matcher: it accepts any of the three computed
/// conventions regardless of `[policy] fragments`, so jumping to a heading and
/// previewing it still work on a fragment the configured renderer would not
/// resolve — following a link physically, as plain navigation does. It is
/// deliberately *not* the resolver the graph surfaces use: whether a fragment
/// is a live edge is [`crate::fragment`]'s answer, shared by the fragment
/// diagnostic, the heading-rename engine, and `find_references` (issue 072).
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
    let (workspace, rel_path) = workspaces.resolve_document(&params.text_document.uri)?;
    let file_data = workspace.file(&rel_path)?;
    let source = file_data.tree.source();
    let offset = lsp_position_to_byte_offset(source, params.position);

    let (_, node) = file_data.tree.find_link_at_offset(offset)?;

    // If it's a reference-style link, go to the ref def.
    if let Some(label) = link_ref_label(source, &node.span) {
        let (_, def_node) = file_data.tree.find_ref_def(&label)?;
        return Some(lsp::Location {
            uri: params.text_document.uri.clone(),
            range: span_to_lsp_range(source, &file_data.line_index, &def_node.span),
        });
    }

    // Inline link — fall through to definition (target document).
    go_to_definition(workspaces, params)
}

/// Go to the definition of a link.
///
/// A cross-file or non-markdown link resolves to the target document. A
/// same-document anchor (`[…](#heading)`) resolves the fragment against this
/// file's own headings and goes to the heading line — an in-page anchor's
/// "target document" is itself, so the heading is the meaningful destination
/// (issue 021).
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn go_to_definition(
    workspaces: &Workspaces,
    params: &lsp::TextDocumentPositionParams,
) -> Option<lsp::Location> {
    let (workspace, rel_path) = workspaces.resolve_document(&params.text_document.uri)?;
    let file_data = workspace.file(&rel_path)?;
    let source = file_data.tree.source();
    let offset = lsp_position_to_byte_offset(source, params.position);

    let (_, node) = file_data.tree.find_link_at_offset(offset)?;
    if !matches!(node.kind, ElementKind::Link { .. }) {
        return None;
    }

    let root = workspace.root();
    let link = find_classified_link(&file_data.tree, &root.join(&rel_path), node.span)?;

    match &link.kind {
        LinkKind::IntraProject { target, .. }
        | LinkKind::NonMarkdown { target }
        | LinkKind::Embed { target } => {
            // `root.join` yields the target's absolute path for either target
            // form: it replaces on an absolute (document-relative) target and
            // appends onto a root-relative remainder.
            Some(lsp::Location {
                uri: path_to_uri(&root.join(target)),
                range: lsp::Range::default(),
            })
        }
        LinkKind::IntraDocument { fragment } => {
            let heading = file_data
                .tree
                .headings()
                .into_iter()
                .find(|h| heading_matches_fragment(h, fragment))?;
            let heading_line = heading.line.saturating_sub(1) as u32;
            Some(lsp::Location {
                uri: params.text_document.uri.clone(),
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
        LinkKind::External { .. } => None,
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
    let (workspace, rel_path) = workspaces.resolve_document(&params.text_document.uri)?;
    let file_data = workspace.file(&rel_path)?;
    let source = file_data.tree.source();
    let offset = lsp_position_to_byte_offset(source, params.position);

    let (_, node) = file_data.tree.find_link_at_offset(offset)?;

    let root = workspace.root();
    let link = find_classified_link(&file_data.tree, &root.join(&rel_path), node.span)?;

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

/// A zero-width LSP location at the start of 0-based `line` in `abs_path`.
fn point_location(abs_path: &Path, line: u32) -> lsp::Location {
    lsp::Location {
        uri: path_to_uri(abs_path),
        range: lsp::Range {
            start: lsp::Position { line, character: 0 },
            end: lsp::Position { line, character: 0 },
        },
    }
}

/// Go to the *implementation* of the predicate edge at the cursor.
///
/// An edge is reconcilable from either end (decision 008), so navigation has two
/// entry points:
///
/// - **Body link** `S --[P]--> T`: jump to the edge's counterpart authored on
///   `T` — a reciprocal forward link `T --[opposite_of(P)]--> S`, or, failing
///   that, the frontmatter backlink entry on `T` keyed by `opposite_of(P)`.
/// - **Frontmatter backlink** entry on `T`: jump to the source link in `S` that
///   derives it — `S --[opposite_of(K)]--> T`, where `K` is the backlink key in
///   *either* direction.
///
/// `textDocument/definition` stays distinct: on a body link it resolves to the
/// target *document* (see [`go_to_definition`]), never the counterpart edge.
fn go_to_implementation(
    workspaces: &Workspaces,
    params: &lsp::TextDocumentPositionParams,
) -> Option<lsp::Location> {
    let (workspace, rel_path) = workspaces.resolve_document(&params.text_document.uri)?;
    let file_data = workspace.file(&rel_path)?;

    implementation_from_body_link(&workspace, &rel_path, file_data, params)
        .or_else(|| implementation_from_backlink(&workspace, &rel_path, file_data, params))
}

/// Body-link entry point for [`go_to_implementation`].
///
/// From a body link `S --[P]--> T` under the cursor, resolve the counterpart of
/// the edge as authored on `T`: a reciprocal forward link
/// `T --[opposite_of(P)]--> S` if one exists, else the frontmatter backlink
/// entry on `T` keyed by `opposite_of(P)` listing `S`. Returns `None` when the
/// cursor is not on an intra-project body link, or the target carries no
/// counterpart for the edge.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn implementation_from_body_link(
    workspace: &WorkspaceView,
    rel_path: &Path,
    file_data: &FileData,
    params: &lsp::TextDocumentPositionParams,
) -> Option<lsp::Location> {
    let source = file_data.tree.source();
    let offset = lsp_position_to_byte_offset(source, params.position);
    let (_, node) = file_data.tree.find_link_at_offset(offset)?;
    let root = workspace.root();
    let cursor_link = find_classified_link(&file_data.tree, &root.join(rel_path), node.span)?;

    let LinkKind::IntraProject {
        target, predicate, ..
    } = &cursor_link.kind
    else {
        return None;
    };

    // The counterpart authored on T carries the opposite predicate. `target` is
    // T's absolute path (a document-relative cursor link resolves root-free), so
    // it doubles as the argument that classifies T's own links root-free.
    let paired = workspace.config().opposite_of(predicate)?;
    let target_data = workspace.file(target)?;

    // Prefer a reciprocal body link T --[opposite_of(P)]--> S. T's link target
    // `t` is root-free; map it onto its stored key and compare to S (`rel_path`).
    let target_links = target_data.tree.links(target);
    let reciprocal = target_links.iter().find(|l| {
        let LinkKind::IntraProject {
            target: t,
            predicate: p,
            ..
        } = &l.kind
        else {
            return false;
        };
        p == paired && workspace.resolve_key(t).is_some_and(|k| k == rel_path)
    });
    if let Some(recip) = reciprocal {
        let line = recip.line.saturating_sub(1) as u32;
        return Some(point_location(&root.join(target), line));
    }

    // Otherwise a frontmatter backlink entry on T keyed by opposite_of(P) and
    // listing S. Backlink paths are file-relative to T, so resolve each against
    // T's directory (`target` is T's absolute path) and map the result onto its
    // stored key before comparing to S.
    let lists_source = target_data
        .frontmatter
        .as_ref()
        .and_then(|fm| fm.backlinks.get(paired))
        .is_some_and(|paths| {
            paths.iter().any(|p| {
                workspace
                    .resolve_key(&validation::resolve_backlink_path(target, p))
                    .is_some_and(|k| k == rel_path)
            })
        });
    if lists_source {
        let line = backlink_key_line(target_data, paired)?;
        return Some(point_location(&root.join(target), line));
    }

    None
}

/// Frontmatter entry point for [`go_to_implementation`].
///
/// When the cursor is on a backlink path like `    - decisions/38.md` in the
/// frontmatter of `T`, navigate to the forward link line in the source document
/// `S` that derives it. The justifying link is always `S --[opposite_of(K)]--> T`
/// regardless of the backlink key `K`'s direction (decision 008), so a key that
/// is a forward label (e.g. `supersedes:`) resolves just as an inverse one does.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn implementation_from_backlink(
    workspace: &WorkspaceView,
    rel_path: &Path,
    file_data: &FileData,
    params: &lsp::TextDocumentPositionParams,
) -> Option<lsp::Location> {
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

    // Find the backlink key listing this path. Decision 008 lets the key name
    // either direction of a vocabulary pair, so accept any known predicate and
    // skip keys unknown in both directions.
    let config = workspace.config();
    let backlink_key = fm.backlinks.iter().find_map(|(key, paths)| {
        (config.is_known_predicate(key) && paths.iter().any(|p| p == path_text))
            .then_some(key.as_str())
    })?;

    // The justifying source link is S --[opposite_of(K)]--> T.
    let paired_predicate = config.opposite_of(backlink_key)?;

    // Find the source document and the forward link. Backlink paths are
    // file-relative to T, so resolve against T's directory (matching validation)
    // before looking S up in the workspace index.
    let source_path = validation::resolve_backlink_path(rel_path, path_text);
    let source_data = workspace.file(&source_path)?;
    let source_abs = workspace.root().join(&source_path);
    let source_links = source_data.tree.links(&source_abs);

    let forward_link = source_links.iter().find(|l| {
        let LinkKind::IntraProject {
            target, predicate, ..
        } = &l.kind
        else {
            return false;
        };
        // S's link target is root-free; map it onto its stored key to compare
        // to T (`rel_path`).
        predicate == paired_predicate
            && workspace.resolve_key(target).is_some_and(|k| k == rel_path)
    })?;

    let line = forward_link.line.saturating_sub(1) as u32;
    Some(point_location(&workspace.root().join(&source_path), line))
}

/// Line (0-based) of the `backlinks` predicate key `predicate` in `file_data`'s
/// frontmatter, or `None` when the file has no such key.
///
/// Resolves to the predicate key line (e.g. `superseded_by:`), the same anchor
/// backlink diagnostics use, rather than an individual list entry.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn backlink_key_line(file_data: &FileData, predicate: &str) -> Option<u32> {
    let tree = &file_data.tree;
    let backlinks_id = tree.nodes().iter().position(
        |n| matches!(&n.kind, ElementKind::FrontmatterMap { key } if key == "backlinks"),
    )?;
    let key_node = tree.children(backlinks_id).iter().find_map(|&cid| {
        let node = tree.node(cid);
        let (ElementKind::FrontmatterKey { key, .. } | ElementKind::FrontmatterMap { key }) =
            &node.kind
        else {
            return None;
        };
        (key == predicate).then_some(node)
    })?;
    let line = crate::block::byte_offset_to_line(tree.source(), key_node.span.start);
    Some(line.saturating_sub(1) as u32)
}

// ---------------------------------------------------------------------------
// Type hierarchy (ticket 08)
// ---------------------------------------------------------------------------

/// Prepare a type hierarchy item for the heading at the cursor.
fn prepare_type_hierarchy(
    workspaces: &Workspaces,
    params: &lsp::TextDocumentPositionParams,
) -> Option<Vec<lsp::HierarchyItem>> {
    let (workspace, rel_path) = workspaces.resolve_document(&params.text_document.uri)?;
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
    let (workspace, rel_path) = workspaces.resolve_document(&item.uri)?;
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
    let (workspace, rel_path) = workspaces.resolve_document(&item.uri)?;
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
    let (workspace, rel_path) = workspaces.resolve_document(&params.text_document.uri)?;
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
    if workspaces.resolve_document(&item.uri).is_none() {
        return Vec::new();
    }

    let mut calls = Vec::new();

    // Match in absolute space: a source's link target resolved against the
    // source's own root equals the cursor document's absolute path exactly when
    // it points there (ticket server 11). Restricted to the cursor's own scope
    // (decision 019): incoming calls are a graph-edge query, and scopes are
    // disjoint graphs, so a cross-boundary physical reference is a defect, not a
    // caller.
    let cursor_abs = uri_to_path(&item.uri);
    let cursor_root = workspaces.store.primary_root(&cursor_abs);
    for (abs, doc) in workspaces.store.current_documents() {
        let Some(root) = doc.primary_root.as_deref() else {
            continue;
        };
        if Some(root) != cursor_root.as_deref() {
            continue;
        }
        let src_path = abs.strip_prefix(root).unwrap_or(abs);
        let links = doc.data.tree.links(abs);
        let headings = doc.data.tree.headings();
        for link in &links {
            let LinkKind::IntraProject { target, .. } = &link.kind else {
                continue;
            };
            if root.join(target) != cursor_abs {
                continue;
            }
            let caller_heading = enclosing_heading(&headings, link.line);

            let caller_item = caller_heading.map_or_else(
                || file_hierarchy_item(abs, src_path),
                |ch| heading_to_hierarchy_item(ch, abs),
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
    let Some((workspace, rel_path)) = workspaces.resolve_document(&item.uri) else {
        return Vec::new();
    };
    let Some(file_data) = workspace.file(&rel_path) else {
        return Vec::new();
    };
    let headings = file_data.tree.headings();
    let links = file_data.tree.links(&workspace.root().join(&rel_path));

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
        let target_key = target_to_key(root, target);
        let target_headings = workspace.file(target).map(|fd| fd.tree.headings());
        let target_item = target_headings
            .as_ref()
            .and_then(|h| h.first())
            .map_or_else(
                || file_hierarchy_item(&target_abs, &target_key),
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
    let Some((workspace, rel_path)) = workspaces.resolve_document(uri) else {
        return Vec::new();
    };
    let Some(file_data) = workspace.file(&rel_path) else {
        return Vec::new();
    };
    let root = workspace.root();
    let file_links = file_data.tree.links(&root.join(&rel_path));

    let mut links = Vec::new();

    for link in &file_links {
        // DocumentLink is intentionally *file-granularity*. `DocumentLink.target`
        // is a bare URI with no position field, so it can only open a document,
        // never land on a heading. Hence cross-file links deliberately drop their
        // fragment (the `..` below), and same-document anchors are skipped
        // entirely: an in-page anchor's only useful destination is a heading in
        // *this* file, which a URI can't express. Heading-precise navigation is
        // delivered by go-to-definition instead, which returns a `Location` with
        // a range (see `go_to_definition`, issue 021). Do NOT "fix" the skip by
        // emitting a file-top link here — it would send an in-page anchor to the
        // top of the file you're already in, which reads as broken.
        let target_uri = match &link.kind {
            LinkKind::IntraProject { target, .. }
            | LinkKind::NonMarkdown { target }
            | LinkKind::Embed { target } => path_to_uri(&root.join(target)),
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
    let (workspace, rel_path) = workspaces.resolve_document(&params.text_document.uri)?;
    let file_data = workspace.file(&rel_path)?;
    let root = workspace.root();
    let file_links = file_data.tree.links(&root.join(&rel_path));

    // Find the link on the cursor's line. Embeds are skipped rather than
    // matched-and-rejected: an embed asserts no relation, so it has no hover to
    // show, and letting one win the line would hide the hover of a real link
    // sharing that line (issue 058).
    let cursor_line = params.position.line;
    let link = file_links.iter().find(|l| {
        l.line.saturating_sub(1) as u32 == cursor_line && !matches!(l.kind, LinkKind::Embed { .. })
    })?;

    let (target, fragment, predicate) = match &link.kind {
        LinkKind::IntraProject {
            target,
            fragment,
            predicate,
            ..
        } => (target.clone(), fragment.clone(), predicate.as_str()),
        LinkKind::NonMarkdown { target } => (target.clone(), None, "references"),
        // No hover for external or intra-document links, nor for embeds (which
        // the candidate filter above already excluded).
        LinkKind::External { .. } | LinkKind::IntraDocument { .. } | LinkKind::Embed { .. } => {
            return None;
        }
    };

    let target_data = workspace.file(&target)?;

    // For a graph edge whose predicate was explicitly authored, surface the
    // opposite label the edge derives on its target's backlinks, so an agent
    // sees both ends of the relationship without opening the target (decision
    // 008). Implicit `references` links, non-markdown links, and unknown
    // predicates have no informative paired label, so the clause is omitted.
    let opposite = match &link.kind {
        LinkKind::IntraProject {
            explicit_predicate: true,
            ..
        } => workspace.config().opposite_of(predicate),
        _ => None,
    };

    let preview = build_hover_preview(target_data, fragment.as_deref());
    // Display the root-relative form, not the root-free absolute target.
    let target_key = target_to_key(root, &target);
    let target_display = target_key.display();
    let header = opposite.map_or_else(
        || format!("**{predicate}** → `{target_display}`"),
        |opposite| {
            format!(
                "**{predicate}** → `{target_display}` (derives **{opposite}** on `{target_display}`)"
            )
        },
    );

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
    let lines: Vec<&str> = content_lines(content).collect();
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
    let Some((workspace, rel_path)) = workspaces.resolve_document(uri) else {
        return Vec::new();
    };
    let Some(file_data) = workspace.file(&rel_path) else {
        return Vec::new();
    };

    let total_lines = crate::fm::line_count(file_data.tree.source()) as u32;

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
// Semantic tokens (ticket integration 15)
// ---------------------------------------------------------------------------

/// A maximal disjoint byte region carrying the union of emphasis modifiers
/// active over it. Reconstructed from the parser's flat, *overlapping* sibling
/// emphasis spans so the emitted token stream can be non-overlapping (an LSP
/// hard requirement), while still styling the `foo` in `***foo***` as both
/// bold and italic.
#[derive(Debug, Clone, Copy)]
struct EmphasisRegion {
    /// Byte start (inclusive) in the source.
    start: usize,
    /// Byte end (exclusive) in the source.
    end: usize,
    /// OR of the `SEMANTIC_MODIFIER_*_BIT` flags active over `[start, end)`.
    modifiers: u32,
}

/// Map an emphasis [`ElementKind`] to its modifier bit, or `None` if the node
/// is not an emphasis run.
fn emphasis_modifier_bit(kind: &ElementKind) -> Option<u32> {
    match kind {
        ElementKind::Strong => Some(SEMANTIC_MODIFIER_BOLD_BIT),
        ElementKind::Emphasis => Some(SEMANTIC_MODIFIER_ITALIC_BIT),
        ElementKind::Strikethrough => Some(SEMANTIC_MODIFIER_STRIKETHROUGH_BIT),
        _ => None,
    }
}

/// Reconstruct the maximal disjoint regions from the parser's overlapping
/// emphasis spans, each tagged with the union of modifiers active over it.
///
/// Parser 26 emits emphasis as flat, *overlapping* sibling spans (e.g.
/// `***foo***` yields a `Strong` over `**foo**` and an `Emphasis` over the
/// whole `***foo***`), but the LSP semantic-tokens protocol requires a flat,
/// non-overlapping token list. We flatten by collecting every emphasis span's
/// endpoints as cut points, then, for each adjacent pair of cut points, OR the
/// modifiers of every span that fully covers that sub-segment. Segments with
/// no active modifier (the gaps between runs) are dropped. The result is sorted
/// by start and pairwise non-overlapping.
///
/// Emphasis runs never appear inside code spans or code blocks — the inline
/// parser excludes those before delimiter matching — so this naturally emits no
/// tokens in code.
fn collect_emphasis_regions(tree: &Tree) -> Vec<EmphasisRegion> {
    // (start, end, modifier_bit) for every emphasis run.
    let mut spans: Vec<(usize, usize, u32)> = Vec::new();
    for node in tree.nodes() {
        if let Some(bit) = emphasis_modifier_bit(&node.kind) {
            spans.push((node.span.start, node.span.end, bit));
        }
    }
    if spans.is_empty() {
        return Vec::new();
    }

    // Sorted, deduped boundary set: every distinct start/end is a cut point.
    let mut cuts: Vec<usize> = Vec::with_capacity(spans.len() * 2);
    for &(start, end, _) in &spans {
        cuts.push(start);
        cuts.push(end);
    }
    cuts.sort_unstable();
    cuts.dedup();

    // For each adjacent cut-point pair, the modifier mask is the OR of every
    // span that fully covers the segment.
    let mut regions: Vec<EmphasisRegion> = Vec::new();
    for window in cuts.windows(2) {
        let (seg_start, seg_end) = (window[0], window[1]);
        let mut modifiers = 0;
        for &(start, end, bit) in &spans {
            if start <= seg_start && seg_end <= end {
                modifiers |= bit;
            }
        }
        if modifiers != 0 {
            regions.push(EmphasisRegion {
                start: seg_start,
                end: seg_end,
                modifiers,
            });
        }
    }
    regions
}

/// Encode emphasis regions as the LSP delta-quintuple stream, restricted to
/// `byte_filter` (the whole document for `/full`, or a range's byte span for
/// `/range`).
///
/// A single LSP token may not span a line break, so each region is split at
/// line boundaries before encoding. Byte→UTF-16 conversion is delegated to the
/// file's cached [`LineIndex`] (`span_to_lsp_range`), the same UTF-16-aware
/// mapping diagnostics use, so multibyte and astral characters map correctly.
/// Tokens are delta-encoded against the previous token's position, as the
/// protocol requires.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line/column values in markdown files won't exceed u32::MAX"
)]
fn encode_semantic_tokens(
    source: &str,
    index: &LineIndex,
    regions: &[EmphasisRegion],
    byte_filter: std::ops::Range<usize>,
) -> lsp::SemanticTokens {
    let mut data: Vec<u32> = Vec::new();
    // Previous token's absolute (line, char) for delta encoding.
    let mut prev_line = 0u32;
    let mut prev_char = 0u32;

    for region in regions {
        let start = region.start.max(byte_filter.start);
        let end = region.end.min(byte_filter.end);
        if start >= end {
            continue;
        }
        let range = span_to_lsp_range(source, index, &Span::new(start, end));
        // Split into one token per line the region touches: an LSP token is
        // single-line, so a region crossing a `\n` becomes several tokens.
        for line in range.start.line..=range.end.line {
            let line_start_char = if line == range.start.line {
                range.start.character
            } else {
                0
            };
            // The line's content end in UTF-16 units, or the region end on the
            // final line.
            let line_end_char = if line == range.end.line {
                range.end.character
            } else {
                let (ls, le) = line_byte_range(source, line);
                source[ls..le].chars().map(char::len_utf16).sum::<usize>() as u32
            };
            let length = line_end_char.saturating_sub(line_start_char);
            if length == 0 {
                continue;
            }
            let delta_line = line - prev_line;
            let delta_start = if delta_line == 0 {
                line_start_char - prev_char
            } else {
                line_start_char
            };
            data.extend_from_slice(&[
                delta_line,
                delta_start,
                length,
                SEMANTIC_TOKEN_TYPE_MARKUP_INDEX,
                region.modifiers,
            ]);
            prev_line = line;
            prev_char = line_start_char;
        }
    }

    lsp::SemanticTokens { data }
}

/// Answer `textDocument/semanticTokens/full`: emphasis tokens over the whole
/// document.
///
/// Returns an empty token set for unknown documents. Styling only — never
/// emits a diagnostic.
///
/// # Perf seam
///
/// `full/delta` is intentionally not served: re-encoding only the emphasis runs
/// is cheap, and a delta handler should consume the perf workstream's reusable
/// "what changed since last parse" diff rather than recompute one — wire it
/// here once that lands (ticket integration 15, perf seam).
fn semantic_tokens_full(workspaces: &Workspaces, uri: &str) -> lsp::SemanticTokens {
    let Some((workspace, rel_path)) = workspaces.resolve_document(uri) else {
        return lsp::SemanticTokens::default();
    };
    let Some(file_data) = workspace.file(&rel_path) else {
        return lsp::SemanticTokens::default();
    };
    let source = file_data.tree.source();
    let regions = collect_emphasis_regions(&file_data.tree);
    encode_semantic_tokens(source, &file_data.line_index, &regions, 0..source.len())
}

/// Answer `textDocument/semanticTokens/range`: emphasis tokens restricted to
/// `range` (the byte span between its endpoints), for large documents.
///
/// Returns an empty token set for unknown documents.
fn semantic_tokens_range(
    workspaces: &Workspaces,
    uri: &str,
    range: &lsp::Range,
) -> lsp::SemanticTokens {
    let Some((workspace, rel_path)) = workspaces.resolve_document(uri) else {
        return lsp::SemanticTokens::default();
    };
    let Some(file_data) = workspace.file(&rel_path) else {
        return lsp::SemanticTokens::default();
    };
    let source = file_data.tree.source();
    let start = file_data.line_index.offset(source, range.start);
    let end = file_data.line_index.offset(source, range.end);
    let regions = collect_emphasis_regions(&file_data.tree);
    encode_semantic_tokens(source, &file_data.line_index, &regions, start..end)
}

// ---------------------------------------------------------------------------
// Formatting (ticket 12)
// ---------------------------------------------------------------------------

/// Format a document's backlink frontmatter.
///
/// Delegates to the shared [`crate::format::format_source`] engine (the single
/// source of formatting semantics, shared with the `lattice format` CLI): it
/// sorts predicate keys alphabetically, sorts paths within each predicate,
/// normalizes whitespace, and — if the config specifies an external formatter —
/// pipes the full document through it after frontmatter sorting. The formatted
/// document is returned as a single whole-document [`lsp::TextEdit`].
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn format_document(workspaces: &Workspaces, uri: &str) -> Option<Vec<lsp::TextEdit>> {
    let (workspace, rel_path) = workspaces.resolve_document(uri)?;
    let file_data = workspace.file(&rel_path)?;

    let source = file_data.tree.source();
    let document = crate::format::format_source(
        source,
        file_data.frontmatter.as_ref(),
        workspace.config().format_command.as_deref(),
    )?;

    // Replace the entire document.
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
/// `abs_path` is the document's absolute path, so the classified target is
/// root-free (ticket server 11).
fn find_classified_link(
    tree: &crate::block::Tree,
    abs_path: &Path,
    node_span: Span,
) -> Option<crate::block::Link> {
    tree.links(abs_path)
        .into_iter()
        .find(|l| l.span == node_span)
}

/// Byte range `[start, content_end)` of 0-based `line` in `source`, excluding
/// the line's terminator. Recognizes `\n`, `\r\n`, and bare `\r`. A line past
/// the end of input yields an empty range at `source.len()`.
fn line_byte_range(source: &str, line: u32) -> (usize, usize) {
    let bytes = source.as_bytes();
    let mut idx = 0u32;
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let (is_break, next) = match bytes[i] {
            b'\n' => (true, i + 1),
            b'\r' => (
                true,
                if bytes.get(i + 1) == Some(&b'\n') {
                    i + 2
                } else {
                    i + 1
                },
            ),
            _ => (false, i + 1),
        };
        if is_break {
            if idx == line {
                return (start, i);
            }
            idx += 1;
            start = next;
        }
        i = next;
    }
    if idx == line {
        (start, bytes.len())
    } else {
        (bytes.len(), bytes.len())
    }
}

/// Convert an LSP 0-based position to a byte offset in `source`.
///
/// Recognizes `\n`, `\r\n`, and bare `\r`. `character` is a UTF-16 code-unit
/// offset within the line (the LSP default position encoding); it is walked
/// across the line's chars and clamped to the line's content length. A column
/// landing inside a surrogate pair rounds down to the enclosing char's start.
#[must_use]
pub fn lsp_position_to_byte_offset(source: &str, pos: lsp::Position) -> usize {
    let (start, end) = line_byte_range(source, pos.line);
    let mut remaining = pos.character as usize;
    let mut byte = start;
    for ch in source[start..end].chars() {
        let units = ch.len_utf16();
        if remaining < units {
            break;
        }
        remaining -= units;
        byte += ch.len_utf8();
    }
    byte
}

/// Convert a byte `Span` to an LSP `Range` through the file's cached
/// [`LineIndex`], so each endpoint is a binary search rather than an
/// `O(offset)` scan of `source`.
fn span_to_lsp_range(source: &str, index: &LineIndex, span: &Span) -> lsp::Range {
    let start = index.position(source, span.start);
    let end = index.position(source, span.end);
    lsp::Range { start, end }
}

/// Convert a byte offset to an LSP 0-based position.
///
/// Line counting recognizes `\n`, `\r\n`, and bare `\r`. The `character` field
/// is a UTF-16 code-unit offset within the line (the LSP default position
/// encoding), measured from the byte after the previous line break. A byte
/// offset that falls inside a multi-byte char is floored to that char's start
/// so the UTF-16 count cannot split a code point.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line/column values in markdown files won't exceed u32::MAX"
)]
#[must_use]
pub fn byte_offset_to_lsp_position(source: &str, offset: usize) -> lsp::Position {
    let mut offset = offset.min(source.len());
    while offset > 0 && !source.is_char_boundary(offset) {
        offset -= 1;
    }
    let line = (crate::block::byte_offset_to_line(source, offset) - 1) as u32;
    let line_start = source.as_bytes()[..offset]
        .iter()
        .rposition(|&b| b == b'\n' || b == b'\r')
        .map_or(0, |i| i + 1);
    let character = source[line_start..offset]
        .chars()
        .map(char::len_utf16)
        .sum::<usize>() as u32;
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

/// Get the text of a 0-based line in the source (recognizing `\n`, `\r\n`,
/// and bare `\r`), excluding the line terminator.
fn source_line_at(source: &str, lsp_line: u32) -> &str {
    let (start, end) = line_byte_range(source, lsp_line);
    &source[start..end]
}

// ---------------------------------------------------------------------------
// Completion (decision 007, ticket integration 14)
// ---------------------------------------------------------------------------

/// Build completion candidates for the construct under the cursor.
///
/// Returns `None` when the cursor is not in a completion site (prose) or sits
/// inside a code span, code block, or math node. Otherwise returns the
/// candidate list for the detected surface — possibly empty (e.g. a fragment
/// against a target that is not yet a resolvable file).
fn completion(
    workspaces: &Workspaces,
    params: &lsp::TextDocumentPositionParams,
) -> Option<lsp::CompletionList> {
    let (workspace, rel_path) = workspaces.resolve_document(&params.text_document.uri)?;
    let file_data = workspace.file(&rel_path)?;
    let tree = &file_data.tree;
    let source = tree.source();
    let offset = lsp_position_to_byte_offset(source, params.position);

    // No completion inside code or math — the tree is authoritative here, so a
    // link-shaped string in a code span (e.g. `` `[x](y` ``) is suppressed even
    // though its line prefix would otherwise look like a destination.
    if offset_in_code(tree, offset) {
        return None;
    }

    let (line_start, _) = line_byte_range(source, params.position.line);
    let prefix = &source[line_start..offset];
    let context = crate::completion::detect(prefix)?;

    let pos = params.position;
    let items = match context {
        CompletionContext::Path { partial } => {
            complete_path(&workspace, &rel_path, partial, source, offset, pos)
        }
        CompletionContext::Fragment { target, partial } => {
            complete_fragment(&workspace, &rel_path, target, partial, source, offset, pos)
        }
        CompletionContext::Predicate { target, partial } => {
            complete_predicate(workspace.config(), target, partial, source, offset, pos)
        }
        CompletionContext::ReferenceLabel { partial } => {
            complete_reference_label(tree, partial, source, offset, pos)
        }
        CompletionContext::Footnote { partial } => {
            complete_footnote(tree, partial, source, offset, pos)
        }
    };

    Some(lsp::CompletionList {
        is_incomplete: false,
        items,
    })
}

/// Whether `offset` falls inside a code span, code block, or math node.
fn offset_in_code(tree: &Tree, offset: usize) -> bool {
    tree.nodes().iter().any(|node| {
        matches!(
            node.kind,
            ElementKind::CodeBlock
                | ElementKind::Math
                | ElementKind::InlineCode
                | ElementKind::InlineMath
        ) && node.span.start <= offset
            && offset < node.span.end
    })
}

/// The range a completion replaces: the `partial`-length slice ending at the
/// cursor.
fn replace_range(
    source: &str,
    cursor_offset: usize,
    cursor_pos: lsp::Position,
    partial: &str,
) -> lsp::Range {
    let start = byte_offset_to_lsp_position(source, cursor_offset.saturating_sub(partial.len()));
    lsp::Range {
        start,
        end: cursor_pos,
    }
}

/// Build a completion item that replaces `range` with `label`.
fn completion_item(
    label: String,
    kind: u32,
    detail: Option<String>,
    sort_text: Option<String>,
    range: lsp::Range,
) -> lsp::CompletionItem {
    lsp::CompletionItem {
        filter_text: Some(label.clone()),
        text_edit: Some(lsp::TextEdit {
            range,
            new_text: label.clone(),
        }),
        label,
        kind: Some(kind),
        detail,
        sort_text,
    }
}

/// Case-insensitive prefix test for completion filtering.
fn matches_prefix(candidate: &str, partial: &str) -> bool {
    candidate
        .to_lowercase()
        .starts_with(&partial.to_lowercase())
}

/// Complete link-target paths in a destination: workspace files and
/// directories under the typed (relative) directory, with only the trailing
/// filename segment replaced.
fn complete_path(
    workspace: &WorkspaceView,
    rel_path: &Path,
    partial: &str,
    source: &str,
    offset: usize,
    pos: lsp::Position,
) -> Vec<lsp::CompletionItem> {
    // Split into the committed directory prefix and the filename being typed.
    let (dir_part, name_part) = partial
        .rfind('/')
        .map_or(("", partial), |i| (&partial[..=i], &partial[i + 1..]));

    let cur_dir = rel_path.parent().unwrap_or_else(|| Path::new(""));
    let rel_dir = crate::block::normalize_path(&cur_dir.join(dir_part));
    // Don't list outside the workspace — those files aren't graph nodes.
    if rel_dir.starts_with("..") {
        return Vec::new();
    }
    let base = workspace.root().join(&rel_dir);

    // Only the filename segment is replaced; the directory prefix stays put.
    let range = replace_range(source, offset, pos, name_part);

    // Walk just the immediate directory, honoring `.gitignore` and skipping
    // hidden entries (`.git`, dotfiles) exactly as workspace discovery does, so
    // path completion never offers files the index itself would exclude.
    let mut items = Vec::new();
    for entry in ignore::WalkBuilder::new(&base)
        .max_depth(Some(1))
        .build()
        .flatten()
    {
        if entry.depth() == 0 {
            continue; // the base directory itself
        }
        let Some(name) = entry.file_name().to_str() else {
            continue;
        };
        if !matches_prefix(name, name_part) {
            continue;
        }
        if entry.file_type().is_some_and(|t| t.is_dir()) {
            // Directories sort first (`0` prefix) and re-trigger on the `/`.
            items.push(completion_item(
                format!("{name}/"),
                lsp::completion_item_kind::FOLDER,
                None,
                Some(format!("0{name}")),
                range,
            ));
        } else {
            items.push(completion_item(
                name.to_string(),
                lsp::completion_item_kind::FILE,
                None,
                Some(format!("1{name}")),
                range,
            ));
        }
    }
    items
}

/// Complete heading fragments: the target document's anchors (explicit `{#id}`
/// and computed slugs), or the current document's for an in-doc `#`.
fn complete_fragment(
    workspace: &WorkspaceView,
    rel_path: &Path,
    target: &str,
    partial: &str,
    source: &str,
    offset: usize,
    pos: lsp::Position,
) -> Vec<lsp::CompletionItem> {
    let target_rel = if target.is_empty() {
        rel_path.to_path_buf()
    } else {
        resolve_fragment_target(rel_path, target)
    };
    let Some(target_data) = workspace.file(&target_rel) else {
        return Vec::new();
    };

    let config = workspace.config();
    let range = replace_range(source, offset, pos, partial);

    let mut items = Vec::new();
    let mut seen = HashSet::new();
    for heading in target_data.tree.headings() {
        for anchor in heading_anchors(&heading, config) {
            if matches_prefix(&anchor, partial) && seen.insert(anchor.clone()) {
                items.push(completion_item(
                    anchor,
                    lsp::completion_item_kind::VALUE,
                    Some(heading.text.clone()),
                    None,
                    range,
                ));
            }
        }
    }
    items
}

/// Resolve a half-typed destination path against the current file's directory.
fn resolve_fragment_target(rel_path: &Path, target: &str) -> PathBuf {
    let parent = rel_path.parent().unwrap_or_else(|| Path::new(""));
    crate::block::normalize_path(&parent.join(target))
}

/// The anchor IDs a heading offers for fragment completion.
///
/// An explicit `{#id}` is the sole anchor. Otherwise the computed slug(s): the
/// configured algorithm's slug when `fragments` is set, else all three
/// conventions (deduplicated) since the default validates against any.
fn heading_anchors(heading: &Heading, config: &Config) -> Vec<String> {
    match &heading.id {
        HeadingId::Explicit(id) => vec![id.clone()],
        HeadingId::Computed {
            github,
            gitlab,
            vscode,
        } => match config.policy.fragments {
            Some(FragmentAlgorithm::Github) => vec![github.clone()],
            Some(FragmentAlgorithm::Gitlab) => vec![gitlab.clone()],
            Some(FragmentAlgorithm::Vscode) => vec![vscode.clone()],
            None => {
                let mut anchors = vec![github.clone()];
                for slug in [gitlab, vscode] {
                    if !anchors.contains(slug) {
                        anchors.push(slug.clone());
                    }
                }
                anchors
            }
        },
    }
}

/// Complete the predicate vocabulary inside a title string.
///
/// Offers both members of each vocabulary pair (decision 008 — a link may name
/// either direction): the label is the predicate, the detail its opposite.
/// Yields nothing when the destination does not take a predicate (external or
/// non-markdown links carry a plain title, not a predicate).
fn complete_predicate(
    config: &Config,
    target: &str,
    partial: &str,
    source: &str,
    offset: usize,
    pos: lsp::Position,
) -> Vec<lsp::CompletionItem> {
    if !target_takes_predicate(target) {
        return Vec::new();
    }

    let range = replace_range(source, offset, pos, partial);
    let mut items = Vec::new();
    let mut seen = HashSet::new();
    for (forward, inverse) in &config.predicates {
        if matches_prefix(forward, partial) && seen.insert(forward.clone()) {
            items.push(completion_item(
                forward.clone(),
                lsp::completion_item_kind::KEYWORD,
                Some(inverse.clone()),
                None,
                range,
            ));
        }
        if matches_prefix(inverse, partial) && seen.insert(inverse.clone()) {
            items.push(completion_item(
                inverse.clone(),
                lsp::completion_item_kind::KEYWORD,
                Some(forward.clone()),
                None,
                range,
            ));
        }
    }
    items
}

/// Whether a destination URL takes a predicate — an intra-project markdown
/// link. External links and non-markdown targets carry a plain title; a
/// fragment-only link (`#section`) is not a graph edge.
fn target_takes_predicate(target: &str) -> bool {
    let target = target.trim();
    if target.is_empty()
        || target.starts_with("http://")
        || target.starts_with("https://")
        || target.starts_with("mailto:")
    {
        return false;
    }
    let path = target.split_once('#').map_or(target, |(p, _)| p);
    !path.is_empty()
        && Path::new(path)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
}

/// Complete the document's defined link reference labels.
fn complete_reference_label(
    tree: &Tree,
    partial: &str,
    source: &str,
    offset: usize,
    pos: lsp::Position,
) -> Vec<lsp::CompletionItem> {
    // Definition labels are stored normalized; match the partial the same way.
    let normalized = normalize_label(partial);
    let range = replace_range(source, offset, pos, partial);

    let mut items = Vec::new();
    let mut seen = HashSet::new();
    for node in tree.nodes() {
        if let ElementKind::ReferenceDef { label, url, .. } = &node.kind
            && label.starts_with(&normalized)
            && seen.insert(label.clone())
        {
            let detail = (!url.is_empty()).then(|| url.clone());
            items.push(completion_item(
                label.clone(),
                lsp::completion_item_kind::REFERENCE,
                detail,
                None,
                range,
            ));
        }
    }
    items
}

/// Complete the document's defined footnote labels.
fn complete_footnote(
    tree: &Tree,
    partial: &str,
    source: &str,
    offset: usize,
    pos: lsp::Position,
) -> Vec<lsp::CompletionItem> {
    let range = replace_range(source, offset, pos, partial);

    let mut items = Vec::new();
    let mut seen = HashSet::new();
    for node in tree.nodes() {
        if let ElementKind::FootnoteDef { label } = &node.kind
            && matches_prefix(label, partial)
            && seen.insert(label.clone())
        {
            items.push(completion_item(
                label.clone(),
                lsp::completion_item_kind::CONSTANT,
                Some("footnote".to_string()),
                None,
                range,
            ));
        }
    }
    items
}

// ---------------------------------------------------------------------------
// Notifications
// ---------------------------------------------------------------------------

/// The `.lattice.toml` URI a document-sync notification names, if any — the
/// routing probe for [`handle_config_sync`]. Every `textDocument/*` sync
/// notification carries its URI at `textDocument.uri`; notifications without
/// one (watched files, folder changes, renames) probe `None` and dispatch
/// normally.
fn config_sync_uri(notif: &Notification) -> Option<String> {
    let uri = notif.params.get("textDocument")?.get("uri")?.as_str()?;
    is_config_uri(uri).then(|| uri.to_string())
}

/// Handle a document-sync notification aimed at a `.lattice.toml` URI — the
/// accepted, never required config sync, save-anchored (decision 023
/// addendum):
///
/// - `didOpen` is decision 022's client-state boundary: reset the config
///   URI's publish record and re-answer. The TOML is never indexed as a
///   markdown document and its buffer gets no authority — 017 §3's
///   buffer-wins rule is markdown-only.
/// - `didSave` is a commitment: the same signal of intent as the watcher
///   event for the same write, honored (in a client that syncs the config
///   but does not watch files it is the only courier) and deduped against
///   it — the reload is idempotent, the publish diff swallows the echo's
///   no-op, and the config URI itself is always re-answered.
/// - `didChange` (and everything else, e.g. `didClose`) adjudicates nothing:
///   the draft lives in the editor and recommits over disk on save. It is
///   deliberately never tracked as dirty, so the config watcher event keeps
///   applying under an open config buffer.
fn handle_config_sync(
    connection: &Connection,
    workspaces: &mut Workspaces,
    method: &str,
    uri: &str,
) -> Result<()> {
    match method {
        lsp::method::DID_OPEN => force_republish_config(connection, workspaces, uri),
        lsp::method::DID_SAVE => {
            workspaces.handle_marker_event(uri);
            force_republish_config(connection, workspaces, uri)
        }
        _ => Ok(()),
    }
}

/// Dispatch a notification.
fn handle_notification(
    connection: &Connection,
    workspaces: &mut Workspaces,
    notif: Notification,
) -> Result<()> {
    // A client may route `.lattice.toml` to us (a broad document selector);
    // its document sync is accepted, never required, and save-anchored
    // (decision 023 addendum) — dispatched separately so the config never
    // enters the markdown sync path below.
    if let Some(uri) = config_sync_uri(&notif) {
        return handle_config_sync(connection, workspaces, &notif.method, &uri);
    }
    match notif.method.as_str() {
        lsp::method::DID_OPEN => {
            let params: lsp::DidOpenTextDocumentParams = serde_json::from_value(notif.params)?;
            // A `didOpen` is a **claim, not a source** (decision 024 clause 3):
            // it seeds only the buffer copy, asserting "here is what I am
            // holding", and never writes the saved store. That is what kills
            // issue 067's read-then-`didOpen` race by construction — a client
            // that read a file before an external edit and opened it afterwards
            // makes a stale claim that can mislead exactly one document's rows,
            // transiently, and can never reach the workspace.
            //
            // The buffer is recorded against the decoded path, so a watcher
            // that spells the URI differently still names the same document
            // (issue 069) — the one-buffer-per-document premise.
            let abs = uri_to_path(&params.text_document.uri);
            workspaces.open_documents.insert(abs);
            workspaces.open_buffer(&params.text_document.uri, &params.text_document.text);
            // A buffer event republishes **this document and nothing else**
            // (decision 024 clause 2): the saved world did not move, so no
            // other file's rows can have moved either.
            force_republish(
                connection,
                workspaces,
                &params.text_document.uri,
                &PublishScope::Only(uri_to_path(&params.text_document.uri)),
            )?;
        }
        lsp::method::DID_CLOSE => {
            let params: lsp::DidCloseTextDocumentParams = serde_json::from_value(notif.params)?;
            // The buffer is gone. With two stores there is nothing to revert —
            // the saved copy never held buffer content (decision 024 clause 4)
            // — so `didClose` is just: drop the overlay entry, then audit.
            let abs = uri_to_path(&params.text_document.uri);
            let rootless = workspaces
                .store
                .current(&abs)
                .is_some_and(|doc| doc.primary_root.is_none());
            workspaces.open_documents.remove(&abs);
            workspaces.close_buffer(&abs);
            if rootless {
                // A rootless single-file document (issue 051) has no
                // disk-backed root to audit against and published nothing.
                workspaces.remove_single_file(&params.text_document.uri);
            } else {
                close_document(connection, workspaces, &params.text_document.uri)?;
            }
        }
        lsp::method::DID_SAVE => {
            let params: lsp::DidSaveTextDocumentParams = serde_json::from_value(notif.params)?;
            // The commitment, and the one seam between the two stores
            // (decision 024 clause 1): the buffer's content becomes the saved
            // copy and the overlay entry is dropped.
            let abs = uri_to_path(&params.text_document.uri);
            if let Some(text) = &params.text {
                // `includeText` is authoritative: the notification's text is
                // byte-identical to what the client just wrote, so no disk read
                // is owed (decision 024 clause 3).
                workspaces.commit_save(&params.text_document.uri, text);
            } else {
                // The absent-text fallback re-reads disk.
                workspaces.commit_save_from_disk(&abs);
            }
            // A save is answered unconditionally (issue 062): the delta diff
            // reads an unchanged set as silence, which a push-only client
            // cannot tell from no answer. It is a commitment, so it runs the
            // full pass — anyone's rows may have moved.
            force_republish(
                connection,
                workspaces,
                &params.text_document.uri,
                &PublishScope::Full,
            )?;
        }
        lsp::method::DID_CHANGE => {
            let params: lsp::DidChangeTextDocumentParams = serde_json::from_value(notif.params)?;
            if let Some(change) = params.content_changes.into_iter().last() {
                // A `didChange` materializes the overlay entry and touches
                // nothing else: it targets an already-open document, and the
                // saved store has no writer here at all. Membership cannot
                // move, so no other document's rows can move — which is why
                // the publish below is scoped to this document alone
                // (decision 024 clause 2 and its performance corollary).
                let abs = uri_to_path(&params.text_document.uri);
                workspaces.change_buffer(&params.text_document.uri, &change.text);
                // Publish path by placement:
                //
                // - rooted with `.lattice.toml`: this document's rows under its
                //   own perspective — the saved world with its buffer overlaid
                //   — filtered through the override verdicts held from the last
                //   commitment (decision 023: a draft never re-adjudicates).
                // - rooted without config: the cheap structural-tier delta
                //   (issue 013 — stage 2.5), which is the same document-scoped
                //   answer computed without the graph collect.
                // - rootless (issue 051): publish nothing; the graph tier has
                //   nothing to say for a single file.
                let publish = workspaces.store.primary_root(&abs).map(|root| {
                    workspaces
                        .roots
                        .get(&root)
                        .is_some_and(|meta| meta.has_config)
                });
                match publish {
                    Some(true) => publish_draft_diagnostics(
                        connection,
                        workspaces,
                        &one_uri(&params.text_document.uri),
                        &abs,
                    )?,
                    Some(false) => {
                        publish_file_diagnostics(
                            connection,
                            workspaces,
                            &params.text_document.uri,
                        )?;
                    }
                    None => {}
                }
            }
        }
        lsp::method::DID_CHANGE_WORKSPACE_FOLDERS => {
            let params: lsp::DidChangeWorkspaceFoldersParams =
                serde_json::from_value(notif.params)?;
            for removed in &params.event.removed {
                workspaces.remove_folder(&removed.uri);
            }
            for added in &params.event.added {
                workspaces.add_folder(&added.uri);
            }
            // No single file's text changed — added folders bring cache-miss
            // files that re-materialize regardless, and removed ones are cleared
            // by the diff's absent-file pass. Open documents kept their buffers
            // across the change, so no dark window.
            publish_all_diagnostics(connection, workspaces, &HashSet::new())?;
        }
        lsp::method::DID_CHANGE_WATCHED_FILES => {
            let params: lsp::DidChangeWatchedFilesParams = serde_json::from_value(notif.params)?;
            handle_watched_files_change(connection, workspaces, &params)?;
        }
        lsp::method::DID_RENAME_FILES => {
            let params: lsp::RenameFilesParams = serde_json::from_value(notif.params)?;
            handle_did_rename_files(connection, workspaces, &params)?;
        }
        _ => {}
    }
    Ok(())
}

/// Apply one `workspace/didChangeWatchedFiles` batch and re-publish.
///
/// Two registered globs (decision 017): the `.lattice.toml` marker (ticket
/// server 08) and `**/*.md` documents (ticket server 09). Each URI takes its
/// own reconciliation path, in two passes over the batch.
///
/// The marker pass runs FIRST, over the whole batch, before any `.md` change
/// is applied: a client watcher that debounces coalesces a config edit and
/// the document edits around it into one notification, in arbitrary order,
/// and every document (re)parse below must happen under the config that was
/// on disk with it. `reload_config` re-parsing the whole index would paper
/// over the wrong order today, but ordering the passes makes the correctness
/// structural instead of incidental (issue 050).
fn handle_watched_files_change(
    connection: &Connection,
    workspaces: &mut Workspaces,
    params: &lsp::DidChangeWatchedFilesParams,
) -> Result<()> {
    let mut reloaded = false;
    // Config URIs owed a forced republish by this batch (decision 023
    // addendum): the config is unsynced by default, so no `didOpen` boundary
    // ever resets its publish record — every marker event re-answers the
    // config URI instead, diff or no diff.
    let mut forced_config_uris: Vec<String> = Vec::new();
    // Marker directories already handled in this batch: a create+modify pair
    // coalesced into one notification is applied once, not twice — each apply
    // is a full reparse of the affected scope (and, for split/merge, a
    // re-rooting).
    let mut handled_markers: HashSet<PathBuf> = HashSet::new();
    for change in &params.changes {
        // The marker watch is config: any event type reloads it. Guard on the
        // suffix so an unrelated future glob can never trigger a config
        // reload.
        if !is_config_uri(&change.uri) {
            continue;
        }
        let Some(marker_dir) = uri_to_path(&change.uri).parent().map(Path::to_path_buf) else {
            // A marker URI that decodes to a path with no parent names no
            // directory to reload, so there is nothing to do — but this is a
            // resolution failure, not a routing decision, and a config event
            // lost here leaves the scope on stale config with no account of
            // why (issue 068). Same level as the no-workspace miss below.
            tracing::warn!(
                uri = %change.uri,
                kind = file_change_kind(change.change_type),
                "config marker event resolves to no parent directory; reload skipped"
            );
            continue;
        };
        if !handled_markers.insert(marker_dir.clone()) {
            continue;
        }
        // A marker create/change/delete either reloads a scope's config, splits
        // a new nested scope out of its host, or merges a vanished one back in
        // (decision 019 clause 6). A miss leaves the workspace silently on stale
        // (or default) config until the next marker event — the config-dead
        // failure shape of issue 050 — so it is worth a trace.
        if workspaces.handle_marker_event(&change.uri) {
            reloaded = true;
            // Invalidate the config URI's publish record now — the batch
            // publish below then re-sends the channel's current set even
            // when unchanged — and remember it so a clean channel still
            // gets its explicit answer after the pass. A merged-away scope
            // resolves no root here; its cached entry (if any) is cleared
            // by the publish diff's absent-file pass instead.
            if let Some(root) = workspaces.registered_root_at(&marker_dir) {
                let config_uri = path_to_uri(&root.join(".lattice.toml"));
                workspaces.published.remove(&config_uri);
                forced_config_uris.push(config_uri);
            }
        } else {
            tracing::warn!(
                uri = %change.uri,
                "config marker event matches no workspace; reload skipped"
            );
        }
    }
    // `.md` URIs whose on-disk state was applied. The whole batch is
    // re-published in a single graph-aware pass below — a content or
    // membership change can move *other* files' backlink/forward edges, so
    // this mirrors how `didSave` re-publishes through
    // `publish_all_diagnostics`, but folds N changed files into one
    // O(workspace) recompute instead of N (ticket perf 07).
    let mut changed_docs: HashSet<String> = HashSet::new();
    // The structural debt the batch accrues (issue 063). Membership changes
    // owe one full sweep, paid once after every change is applied — a bulk
    // create/delete storm (a directory move) is O(batch + workspace), not
    // O(batch × workspace) — and the deferred sweep runs against the batch's
    // *final* membership, so it also covers every content change. Only when
    // no membership moved do content changes pay their own per-file caches.
    let mut membership_changed = false;
    let mut content_changed: Vec<PathBuf> = Vec::new();
    for change in &params.changes {
        if !is_markdown_uri(&change.uri) {
            continue;
        }
        // Decide identity once, here: the buffer-authority check below and the
        // store lookup are both by decoded path, so a watcher event that spells
        // the URI differently from the `didOpen` that recorded the buffer still
        // resolves to the same document (issue 069).
        let abs = uri_to_path(&change.uri);
        match change.change_type {
            // Every event type applies **unconditionally** (decision 024,
            // issue 067): created / changed / deleted alike route through
            // `apply_from_disk`, which re-reads disk — reparsing a
            // created/changed file, dropping a deleted one — and leaves the
            // structural recompute to the batch settlement below.
            //
            // There is no buffer-wins drop any more, and no dirty set to
            // consult. The watcher is the sole writer of the saved copy, so
            // there is nothing to clobber and nothing to arbitrate: an open
            // document's own rows keep reading its overlay, and where the
            // document is open and undiverged the store materializes the
            // pre-update saved parse into the overlay first (clause 1), so the
            // client's text is never replaced by content it never sent.
            //
            // Only files under a folder are tracked (the watcher glob is
            // folder-scoped), so an event outside every root is ignored — but
            // never in silence (issue 068).
            lsp::file_change_type::CREATED
            | lsp::file_change_type::CHANGED
            | lsp::file_change_type::DELETED => {
                if workspaces.deepest_root_for(&abs).is_none() {
                    // Discarding the event is the right behaviour; being
                    // unable to tell that it happened is not. Without this,
                    // an event the client delivered and the server threw away
                    // has exactly the same observable as an event that was
                    // never sent — nothing — and every staleness report in
                    // this family (061, 062, 063, 066) turns on telling those
                    // two apart. Same reasoning and same level as the marker
                    // pass's no-workspace warn above.
                    tracing::warn!(
                        uri = %change.uri,
                        path = %abs.display(),
                        kind = file_change_kind(change.change_type),
                        "watched .md event resolves under no registered root; event dropped"
                    );
                    continue;
                }
                match workspaces.apply_from_disk(&abs) {
                    DiskUpdate::Membership => membership_changed = true,
                    DiskUpdate::Content => content_changed.push(abs),
                    // Nothing indexed moved (e.g. a delete echo for a file
                    // never indexed, or bytes the server already holds):
                    // no debt, and no re-materialization to force. A genuine
                    // no-op — "nothing to do", not "I may be blind here" — so
                    // it takes `debug` and must not be conflated with the
                    // drop above by sharing its level.
                    DiskUpdate::Untouched => {
                        tracing::debug!(
                            uri = %change.uri,
                            kind = file_change_kind(change.change_type),
                            "watched .md event moved nothing indexed; no work owed"
                        );
                        continue;
                    }
                }
                changed_docs.insert(change.uri.clone());
            }
            // A `.md` event the server has no handling for: outside the three
            // `FileChangeType` values the protocol defines, so no conforming
            // client sends one — which makes an arrival worth reporting rather
            // than absorbing. With this arm and the root drop above traced,
            // every `.md` event is accounted for: applied, or named in the log.
            _ => {
                tracing::warn!(
                    uri = %change.uri,
                    change_type = change.change_type,
                    "watched .md event carries an unrecognized change type; event dropped"
                );
            }
        }
    }
    // Settle the batch's structural debt in one payment (issue 063).
    if membership_changed {
        workspaces.recompute_all_structural();
    } else {
        for abs in &content_changed {
            workspaces.recompute_structural(abs);
        }
    }
    // A marker change invalidates the whole workspace (predicates, artifacts,
    // overrides, and external aliases all feed parse and structural
    // analysis), so take the full re-publish path, not the
    // `has_config()`-gated single-file delta (decision 017). When `.md` files
    // also changed, the batched publish below already runs the full graph
    // diff against the freshly reloaded config, so a marker-only notification
    // is the only case that needs this empty-set publish.
    if reloaded && changed_docs.is_empty() {
        publish_all_diagnostics(connection, workspaces, &HashSet::new())?;
    }
    // Re-publish the whole applied `.md` batch in ONE graph-aware pass,
    // naming every changed document so each one's materialization is
    // refreshed unconditionally (its disk content changed). The single
    // whole-graph diff also catches any other file whose edges moved — one
    // recompute for the batch, not one per file (ticket perf 07).
    if !changed_docs.is_empty() {
        publish_all_diagnostics(connection, workspaces, &changed_docs)?;
    }
    // The forced config republish, second half (decision 023 addendum): a
    // channel the pass left uncached is clean, and push-only owes it the
    // explicit empty a synced document would get from `force_republish` —
    // so every marker event answers on its config URI, unconditionally.
    for config_uri in forced_config_uris {
        if !workspaces.published.contains_key(&config_uri) {
            let params = lsp::PublishDiagnosticsParams {
                uri: config_uri,
                diagnostics: Vec::new(),
            };
            let notif = Notification::new(lsp::method::PUBLISH_DIAGNOSTICS.to_string(), params);
            connection.sender.send(Message::Notification(notif))?;
        }
    }
    Ok(())
}

/// Apply a `workspace/didRenameFiles` confirmation (decision 020 clause 2):
/// re-key the store for every rename the client just performed, then re-publish.
///
/// The `willRenameFiles` handler already returned the forced edits, and the
/// client applied them to buffers and renamed on disk — so the content now
/// living at each new path is correct. This re-keys the parsed entries onto the
/// new coordinates **without a rescan** ([`Workspaces::rekey_rename`]),
/// preserving open buffers (both tiers re-key together). Each moved document's old URI gets
/// an explicit empty publish to clear the client's stale diagnostics under the
/// old name; then one graph-aware re-publish names the new URIs so their
/// diagnostics (and any referrer whose edge the coordinate change moved) land at
/// the renamed positions — the engine's isomorphism, observed end-to-end.
///
/// The watched-file create/delete channel (decision 017) delivers the same
/// membership change independently; this confirmation is idempotent with it —
/// a re-key of an already-moved key finds nothing and no-ops.
fn handle_did_rename_files(
    connection: &Connection,
    workspaces: &mut Workspaces,
    params: &lsp::RenameFilesParams,
) -> Result<()> {
    let mut cleared_uris: Vec<String> = Vec::new();
    let mut renamed_uris: HashSet<String> = HashSet::new();
    for rename in &params.files {
        let old_abs = uri_to_path(&rename.old_uri);
        let new_abs = uri_to_path(&rename.new_uri);
        let cleared = workspaces.rekey_rename(&old_abs, &new_abs);
        if !cleared.is_empty() || workspaces.deepest_root_for(&new_abs).is_some() {
            renamed_uris.insert(path_to_uri(&new_abs));
        }
        cleared_uris.extend(cleared);
    }
    // A membership change: any file's bare-path or backlink edge may have moved.
    workspaces.recompute_all_structural();

    // Clear the client's stale diagnostics under each vanished old URI.
    for uri in cleared_uris {
        let params = lsp::PublishDiagnosticsParams {
            uri,
            diagnostics: Vec::new(),
        };
        let notif = Notification::new(lsp::method::PUBLISH_DIAGNOSTICS.to_string(), params);
        connection.sender.send(Message::Notification(notif))?;
    }

    // Re-publish the whole graph in one pass, forcing the renamed documents to
    // re-materialize at their new coordinates.
    publish_all_diagnostics(connection, workspaces, &renamed_uris)?;
    Ok(())
}

/// Audit a just-closed document against disk and re-publish (decision 024
/// clause 4). The caller has already dropped the overlay entry.
///
/// With two stores the *reconciliation* half of `didClose` disappears: the
/// saved store never held buffer content, so there is nothing to revert. What
/// survives is the audit, and it is opportunistic rather than necessary —
/// re-read disk and compare against the **saved** copy, never against the
/// buffer, which is being discarded and has authority over nothing:
///
/// - **Bytes match** — no-op. The saved copy was already correct and the close
///   behaves as a pure buffer event.
/// - **Bytes differ** — a missed disk change has just been *caught*. Apply it
///   (the world genuinely moved, so this genuinely is a commitment) **and emit
///   a loud trace**, because a firing detector means the watcher channel
///   dropped an event (issue 068's territory, and the one place the drop
///   becomes visible without client-side wire captures). Never heal silently:
///   quiet healing is how issues 061 and 066 stayed unexplained for weeks.
///
/// Either way this is a commitment event and runs the full pass, naming the
/// URI: the document's own rows revert from its buffer's perspective to the
/// saved world, and a caught disk change can move anyone's.
fn close_document(connection: &Connection, workspaces: &mut Workspaces, uri: &str) -> Result<()> {
    let abs = uri_to_path(uri);
    let audit = workspaces.apply_from_disk(&abs);
    // A firing detector means the watcher channel dropped an event. Say so —
    // the alternative, healing quietly, converts a channel defect into an
    // invisible latency.
    if audit != DiskUpdate::Untouched {
        tracing::error!(
            uri = %uri,
            update = ?audit,
            "WATCHER MISS CAUGHT: the didClose audit found disk differing from the saved copy and \
             applied it. The watched-files channel dropped an event for this document — \
             diagnostics for every file judged against it were computed from stale content until \
             now (issue 068)."
        );
    }
    workspaces.settle(&abs, audit);
    publish_all_diagnostics(connection, workspaces, &one_uri(uri))
}

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

/// Build a one-element force-rematerialize set for the single-document callers
/// of [`publish_all_diagnostics`] / [`diff_diagnostics`] (a `didOpen` /
/// `didSave` / `didChange` / `didClose` names exactly the document it touched).
fn one_uri(uri: &str) -> HashSet<String> {
    let mut set = HashSet::with_capacity(1);
    set.insert(uri.to_string());
    set
}

/// Republish a document's current diagnostics unconditionally.
///
/// Two notification boundaries force a publish even when nothing changed:
///
/// - `didOpen` is a client-state boundary (decision 022): the server cannot
///   know what a client remembers about a document it just opened, so the
///   per-URI last-published record is invalidated before the sync's publish
///   pass — closing the false-clean gap a client that drops its per-URI
///   record on reopen would otherwise hit.
/// - `didSave` is a client-expectation boundary (issue 062): on a push-only
///   server (decision 022 removed the pull surface) a client that hears
///   nothing for a document it just saved cannot distinguish "no findings"
///   from "no answer" except by timeout. The delta diff reads an unchanged
///   set as nothing-to-send, so the save's answer is forced the same way.
///
/// A clean indexed file needs one extra step: its desired set is empty and it
/// holds no cache entry, so the diff suppresses it (an unchanged empty is not a
/// change). Push-only owes it an *explicit* empty publish there, not a skip — so
/// when the pass leaves this document with no cache entry (i.e. it is clean), an
/// empty `publishDiagnostics` is sent for it. A rootless or unindexed document
/// (issue 051) resolves to no workspace and publishes nothing, as before.
///
/// `scope` declares which column of decision 024 clause 2 the triggering event
/// sits in: a `didOpen` is a **buffer event** and republishes this document
/// alone; a `didSave` is a **commitment** and runs the full pass.
///
/// No diagnostics are recomputed beyond the ordinary publish pass.
fn force_republish(
    connection: &Connection,
    workspaces: &mut Workspaces,
    uri: &str,
    scope: &PublishScope,
) -> Result<()> {
    // The publish/cache key for this document when it is indexed under a root
    // (the same base `diff_diagnostics` keys the cache by). `None` for a
    // rootless or unindexed open, which publishes nothing.
    let canonical = workspaces
        .resolve(uri)
        .map(|(workspace, rel_path)| path_to_uri(&workspace.root().join(rel_path)));

    // Invalidate the last-published record so the diff re-sends the current set
    // even when the content is unchanged.
    if let Some(canonical) = &canonical {
        workspaces.published.remove(canonical);
    }

    let sets = diff_diagnostics_with(workspaces, &one_uri(uri), Adjudication::Commit, scope);
    send_publishes(connection, sets)?;

    // After the pass, a document that carries diagnostics has its cache entry
    // repopulated; a clean one has none (only non-empty entries are cached). The
    // clean file was suppressed by the diff, so send it the explicit empty
    // publish push-only owes it.
    if let Some(canonical) = canonical
        && !workspaces.published.contains_key(&canonical)
    {
        let params = lsp::PublishDiagnosticsParams {
            uri: canonical,
            diagnostics: Vec::new(),
        };
        let notif = Notification::new(lsp::method::PUBLISH_DIAGNOSTICS.to_string(), params);
        connection.sender.send(Message::Notification(notif))?;
    }

    Ok(())
}

/// Force-republish one root's `.lattice.toml` channel (decision 023
/// addendum), given any URI spelling of the marker.
///
/// The config is unsynced by default, so no `didOpen` boundary ever resets
/// its publish record client-side — the record is invalidated here so the
/// publish pass re-sends the current set even when unchanged, and a clean
/// channel (no cache entry after the pass) is still answered with the
/// explicit empty push-only owes it. Serves both accepted-sync couriers
/// (`didOpen` as the client-state boundary, `didSave` as the commitment);
/// the watched-marker batch path performs the same invalidation inline so
/// one batch pays one publish pass.
fn force_republish_config(
    connection: &Connection,
    workspaces: &mut Workspaces,
    marker_uri: &str,
) -> Result<()> {
    let marker_path = uri_to_path(marker_uri);
    let Some(root) = marker_path
        .parent()
        .and_then(|dir| workspaces.registered_root_at(dir))
    else {
        // A config outside every registered scope publishes nothing.
        return Ok(());
    };
    let config_uri = path_to_uri(&root.join(".lattice.toml"));
    workspaces.published.remove(&config_uri);
    publish_all_diagnostics(connection, workspaces, &HashSet::new())?;
    if !workspaces.published.contains_key(&config_uri) {
        let params = lsp::PublishDiagnosticsParams {
            uri: config_uri,
            diagnostics: Vec::new(),
        };
        let notif = Notification::new(lsp::method::PUBLISH_DIAGNOSTICS.to_string(), params);
        connection.sender.send(Message::Notification(notif))?;
    }
    Ok(())
}

/// Whether a publish pass is a save-point commitment or a mid-edit draft
/// (decision 023): a commitment re-adjudicates each root's `[[override]]`
/// expect verdicts from the freshly computed live set and holds them; a draft
/// filters through the verdicts held from the last commitment — counts move
/// mid-edit, the suppression decision does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Adjudication {
    /// Recompute each root's verdicts from this pass's live set and hold them
    /// (`didOpen` / `didSave` / watched-files batches and every other
    /// disk-anchored publish).
    Commit,
    /// Filter through the held verdicts without re-adjudicating (the
    /// `didChange` graph-tier arm — the draft).
    Held,
}

/// Publish diagnostics for the files whose diagnostics changed, at a
/// **commitment point**: the pass re-adjudicates every root's override
/// verdicts before filtering (decision 023).
///
/// The cheap whole-graph recompute still happens internally (see
/// [`diff_diagnostics`]), but the expensive per-diagnostic materialization and
/// the `publishDiagnostics` notifications are both restricted to the documents
/// an edit actually moved — collapsing the per-keystroke cost from `O(files)`
/// down to the handful that changed (issue 013 — publication diffing, then
/// ticket perf 02's materialization cache).
///
/// `changed_uris` names the documents whose source text just changed, if any,
/// so each one's materialization is refreshed unconditionally; see
/// [`diff_diagnostics`] for why an edited file cannot trust its cached LSP form,
/// and why a whole batch of changed files is forced together in one pass.
fn publish_all_diagnostics(
    connection: &Connection,
    workspaces: &mut Workspaces,
    changed_uris: &HashSet<String>,
) -> Result<()> {
    let sets = diff_diagnostics(workspaces, changed_uris);
    send_publishes(connection, sets)
}

/// Publish diagnostics for the files whose diagnostics changed, **between**
/// commitments (the `didChange` graph-tier arm): live diagnostics are
/// recomputed as always, but the published sets are filtered through each
/// root's held verdicts — no re-adjudication, so a mid-edit count crossing
/// never flashes a resurface (decision 023).
///
/// Scoped to `focus` alone (decision 024 clause 2): a buffer event may change
/// only that document's rows, so no other file's cache entry is read or
/// written.
fn publish_draft_diagnostics(
    connection: &Connection,
    workspaces: &mut Workspaces,
    changed_uris: &HashSet<String>,
    focus: &Path,
) -> Result<()> {
    let sets = diff_diagnostics_with(
        workspaces,
        changed_uris,
        Adjudication::Held,
        &PublishScope::Only(focus.to_path_buf()),
    );
    send_publishes(connection, sets)
}

/// Send one `publishDiagnostics` notification per diffed `(uri, set)` pair.
fn send_publishes(
    connection: &Connection,
    sets: Vec<(String, Vec<lsp::Diagnostic>)>,
) -> Result<()> {
    for (uri, diagnostics) in sets {
        let params = lsp::PublishDiagnosticsParams { uri, diagnostics };
        let notif = Notification::new(lsp::method::PUBLISH_DIAGNOSTICS.to_string(), params);
        connection.sender.send(Message::Notification(notif))?;
    }

    Ok(())
}

/// Publish the diagnostic delta for a single file (issue 013 — stage 2.5).
///
/// Recomputes the desired diagnostics for just `uri` and sends a
/// `publishDiagnostics` only if its vector changed. This avoids the
/// `O(workspace)` materialize/diff that [`publish_all_diagnostics`] pays every
/// sync. It is correct only when the triggering edit cannot affect any other
/// file's diagnostics — i.e. a content edit (no membership change) in the
/// structural tier — so the caller must gate on that.
fn publish_file_diagnostics(
    connection: &Connection,
    workspaces: &mut Workspaces,
    uri: &str,
) -> Result<()> {
    if let Some((uri, diagnostics)) = diff_file_diagnostics(workspaces, uri) {
        let params = lsp::PublishDiagnosticsParams { uri, diagnostics };
        let notif = Notification::new(lsp::method::PUBLISH_DIAGNOSTICS.to_string(), params);
        connection.sender.send(Message::Notification(notif))?;
    }

    Ok(())
}

/// Diff one file's freshly computed diagnostics against the last published set,
/// updating the cache and returning the `(uri, diagnostics)` to send when it
/// changed (including the transition to empty, which clears the file). Returns
/// `None` when nothing changed or the URI resolves to no workspace.
///
/// The single-file counterpart to [`diff_diagnostics`]; it touches only this
/// file's cache entry, leaving every other file's last-published set intact —
/// which is correct precisely under the structural-tier, no-membership-change
/// precondition its caller enforces.
fn diff_file_diagnostics(
    workspaces: &mut Workspaces,
    uri: &str,
) -> Option<(String, Vec<lsp::Diagnostic>)> {
    let (canonical, lattice, lsp) = {
        let (workspace, rel_path) = workspaces.resolve(uri)?;
        let canonical = path_to_uri(&workspace.root().join(&rel_path));
        let (lattice, lsp) = file_desired(&workspace, &rel_path);
        (canonical, lattice, lsp)
    };

    let unchanged = workspaces
        .published
        .get(&canonical)
        .map_or(lsp.is_empty(), |prev| prev.lsp == lsp);
    if unchanged {
        return None;
    }

    // Keep the cache invariant: only non-empty entries are stored, so an absent
    // entry means "the client currently holds none". Caching the Lattice vector
    // alongside the LSP form keeps this entry coherent with the full path's
    // change-detector (ticket perf 02).
    if lsp.is_empty() {
        workspaces.published.remove(&canonical);
    } else {
        workspaces.published.insert(
            canonical.clone(),
            PublishedDiagnostics {
                lattice,
                lsp: lsp.clone(),
            },
        );
    }

    Some((canonical, lsp))
}

/// Compute the full desired diagnostic set across all workspaces, keyed by
/// document URI, materializing every file from scratch.
///
/// Every indexed file gets an entry — an empty vector when it has no
/// diagnostics — so a caller can tell a file that just became clean apart from
/// one that left the workspace. This is the unconditional from-scratch
/// recompute that the differential tests use as their oracle; production goes
/// through [`diff_diagnostics`], which materializes only the files an edit
/// moved.
#[cfg(test)]
fn desired_diagnostics(workspaces: &Workspaces) -> BTreeMap<String, Vec<lsp::Diagnostic>> {
    let mut desired: BTreeMap<String, Vec<lsp::Diagnostic>> = BTreeMap::new();
    let empty = LineIndex::default();

    // Ascending root order: a document under overlapping folders is inserted by
    // the shallow root first and overwritten by the deepest, so the deepest
    // workspace's set wins the shared URI (matching `diff_diagnostics`).
    for (root, meta) in &workspaces.roots {
        // The desired set is post-adjudication: filtered through the root's
        // held override verdicts, exactly as production publishes (decision
        // 023). The oracle never re-adjudicates — it asks what the client
        // should currently hold, and that is the held verdict's answer
        // ([`Adjudication::Held`]) — and it carries the perspective merge, the
        // config channel, and the fabricated-config guard through the same
        // shared collection.
        let (mut by_file, _) = root_desired_rows(
            workspaces,
            root,
            meta,
            Adjudication::Held,
            &PublishScope::Full,
        );

        let config_rel = PathBuf::from(".lattice.toml");
        let config_channel =
            (meta.has_config || meta.config_error.is_some()).then_some(config_rel.clone());
        let rels: Vec<PathBuf> = workspaces
            .store
            .current_files(root)
            .into_keys()
            .chain(config_channel)
            .collect();
        for rel_path in rels {
            let uri = path_to_uri(&root.join(&rel_path));
            let lattice = by_file.remove(&rel_path).unwrap_or_default();
            // Materialize against the document's own current text: an overlay
            // document's rows are anchored in its buffer.
            let fd = workspaces
                .store
                .current(&root.join(&rel_path))
                .map(|doc| &doc.data);
            let source = fd.map_or("", |fd| fd.tree.source());
            let index = fd.map_or(&empty, |fd| &fd.line_index);
            let diagnostics = lattice
                .iter()
                .map(|d| to_lsp_diagnostic(d, source, index))
                .collect();
            desired.insert(uri, diagnostics);
        }
    }

    desired
}

/// Diff the freshly computed diagnostics against the last-published set,
/// returning only the `(uri, diagnostics)` pairs that must be sent and updating
/// the cache to match.
///
/// Runs the cheap whole-graph recompute ([`collect_all_diagnostics`]) and then,
/// per file, compares the new Lattice diagnostic vector against the cached one.
/// A file whose Lattice vector is unchanged keeps its cached materialization
/// untouched — the expensive UTF-16 [`to_lsp_diagnostic`] pass runs only for the
/// files the recompute shows actually moved — so a graph-tier (`.lattice.toml`)
/// sync no longer re-materializes every file on every keystroke. Detection,
/// not prediction: the whole-graph recompute already reflects cross-file edges
/// (a missing backlink reported on the *source*), so a dependent file an edit
/// touched only indirectly is caught the same way (issue 013 — ticket perf 02).
///
/// `changed_uris` names the documents whose source text just changed, if any.
/// Each is force-re-materialized unconditionally: a length-preserving edit can
/// leave the Lattice vector byte-identical yet shift a span's UTF-16 column (an
/// astral-plane swap upstream of the span on its line), so the cached LSP form
/// cannot be trusted for an edited file even when its Lattice vector matches.
/// Every *other* file's source is unchanged, so Lattice-vector equality there
/// does guarantee an identical materialization. Passing a set (rather than a
/// single URI) lets one pass force-re-materialize a whole batch of changed
/// files — a bulk on-disk change reconciles all of them in one O(workspace)
/// recompute instead of one per file (ticket perf 07). Pass an empty set when
/// no single file's text changed (e.g. a workspace-folder add/remove — newly
/// scanned files are cache misses and re-materialize regardless).
///
/// A pair is sent when its materialized vector differs from what the client last
/// received, including the transition to empty — a file that became clean, or
/// one that left the workspace (deleted, or its folder removed) — so stale
/// diagnostics are cleared. Only non-empty entries are cached, so an absent
/// entry means "the client currently holds none". The result is sorted by URI
/// for deterministic output.
///
/// The commitment-mode, full-scope entry point ([`Adjudication::Commit`],
/// [`PublishScope::Full`]); the `didChange` draft goes through
/// [`diff_diagnostics_with`] directly.
fn diff_diagnostics(
    workspaces: &mut Workspaces,
    changed_uris: &HashSet<String>,
) -> Vec<(String, Vec<lsp::Diagnostic>)> {
    diff_diagnostics_with(
        workspaces,
        changed_uris,
        Adjudication::Commit,
        &PublishScope::Full,
    )
}

/// Which documents a publish pass computes and diffs (decision 024 clause 2's
/// enforceable pair).
#[derive(Debug, Clone, PartialEq, Eq)]
enum PublishScope {
    /// Every document under every root — a **commitment event** (`didSave`, a
    /// watched-files batch, `didClose`, a folder or marker change). Anyone's
    /// rows may have moved.
    Full,
    /// One document's own rows — a **buffer event** (`didOpen`, `didChange`).
    /// The saved world did not move, so no other file's rows can have moved,
    /// and no other file's cache entry is read or written.
    Only(PathBuf),
}

/// The buffer-locality perspective merge (decision 024 clause 8) — the seam the
/// [`crate::invariants::assert_buffer_locality`] differential pins.
///
/// `saved_live` is the saved world's diagnostic vector: every document judged
/// against everyone's last-committed state. `perspectives` supplies, for each
/// document that has a diverged buffer, the view of the saved world with *that*
/// document's buffer swapped in. Its rows replace that document's saved rows —
/// **and only that document's**: every other row the perspective pass computed
/// describes a document judged from somewhere other than its own seat, and is
/// discarded unread.
///
/// That discard is the whole invariant. Merging any other row from a
/// perspective pass would let one document's buffer reach another document's
/// verdict, which is exactly what decision 024 forbids.
pub fn merge_perspectives(
    saved_live: Vec<Diagnostic>,
    perspectives: &[(PathBuf, WorkspaceView<'_>)],
) -> BTreeMap<PathBuf, Vec<Diagnostic>> {
    let mut by_file: BTreeMap<PathBuf, Vec<Diagnostic>> = BTreeMap::new();
    for diag in saved_live {
        by_file.entry(diag.file.clone()).or_default().push(diag);
    }
    for (rel, view) in perspectives {
        let rows = collect_all_diagnostics(view)
            .into_iter()
            .filter(|diag| &diag.file == rel)
            .collect();
        by_file.insert(rel.clone(), rows);
    }
    by_file
}

/// One root's desired per-file rows: the perspective merge, filtered through
/// the root's `[[override]]` verdicts (issue 064, decision 023).
///
/// At a commitment the verdicts are re-adjudicated and returned (for the caller
/// to hold on the [`RootMeta`] once its borrow ends); between commitments the
/// held verdicts bind and `None` is returned. Either way the matched members'
/// findings are filtered out here — exactly once, at the seam where desired
/// sets materialize, and nowhere else: `compute_structural` is shared with
/// `lint`, which runs its own aggregate pass, so filtering there would
/// double-apply.
///
/// **Adjudication reads the saved world only.** A count is an aggregate over
/// committed state; letting a diverged buffer move a verdict would suppress or
/// resurface *other* files' rows from one document's unsaved draft — a buffer
/// locality violation, and a conformance break against `lattice lint`, which
/// reads disk.
fn root_desired_rows(
    workspaces: &Workspaces,
    root: &Path,
    meta: &RootMeta,
    adjudication: Adjudication,
    scope: &PublishScope,
) -> (BTreeMap<PathBuf, Vec<Diagnostic>>, Option<OverrideVerdicts>) {
    // The focus of a scoped pass, as a root-relative key — `None` when the
    // scoped document does not belong to this root, in which case the pass has
    // nothing to say about it.
    let focus_rel = match scope {
        PublishScope::Full => None,
        PublishScope::Only(abs) => match abs.strip_prefix(root) {
            Ok(rel) if workspaces.store.primary_root(abs).as_deref() == Some(root) => {
                Some(rel.to_path_buf())
            }
            _ => return (BTreeMap::new(), None),
        },
    };

    // A root holding a fabricated config — broken at scope registration with
    // no last-good (decision 023 addendum, issue 065) — publishes nothing
    // computed under it: defaults are the semantics of an absent config, not
    // a broken one. Only the load error reaches the wire, on the config
    // channel; the next valid commitment restores the full surface.
    if !meta.config_committed {
        let mut by_file: BTreeMap<PathBuf, Vec<Diagnostic>> = BTreeMap::new();
        let channel = config_channel_diagnostics(meta, &meta.verdicts);
        if !channel.is_empty() {
            by_file.insert(PathBuf::from(".lattice.toml"), channel);
        }
        let committed = match adjudication {
            Adjudication::Commit => Some(OverrideVerdicts::default()),
            Adjudication::Held => None,
        };
        return (by_file, committed);
    }

    // A scoped draft needs no saved-world collect at all: it neither
    // adjudicates nor reports on any other document.
    let skip_saved_collect = focus_rel.is_some() && adjudication == Adjudication::Held;
    let saved_view = workspaces.saved_view(root);
    let saved_live = if skip_saved_collect {
        Vec::new()
    } else {
        collect_all_diagnostics(&saved_view)
    };
    let committed = match adjudication {
        Adjudication::Commit => Some(overrides::adjudicate(
            &meta.config.overrides,
            saved_view.files().keys().map(PathBuf::as_path),
            &saved_live,
        )),
        Adjudication::Held => None,
    };

    // The perspectives to merge: every diverged buffer this pass reports on.
    let overlay_keys: Vec<PathBuf> = focus_rel.as_ref().map_or_else(
        || workspaces.store.overlay_keys_of_root(root),
        |rel| vec![root.join(rel)],
    );
    let perspectives: Vec<(PathBuf, WorkspaceView<'_>)> = overlay_keys
        .iter()
        .filter(|abs| workspaces.store.has_overlay(abs))
        .filter_map(|abs| {
            abs.strip_prefix(root)
                .ok()
                .map(|rel| (rel.to_path_buf(), workspaces.perspective_view(root, abs)))
        })
        .collect();

    let mut by_file = merge_perspectives(saved_live, &perspectives);
    // A scoped pass reports on exactly one document; drop every other row the
    // saved-world collect produced (it was computed only to feed adjudication).
    if let Some(rel) = &focus_rel {
        let rows = by_file.remove(rel).unwrap_or_default();
        by_file = BTreeMap::new();
        by_file.insert(rel.clone(), rows);
    }

    let verdicts = committed.as_ref().unwrap_or(&meta.verdicts);
    for rows in by_file.values_mut() {
        overrides::suppress_matched(verdicts, rows);
    }

    // The config channel (decision 023 clause 4) rides the same per-root map
    // under the marker's pseudo-path, so the publish diff treats the config
    // URI exactly like a document's: sent when it changes, cleared when it
    // empties. A scoped **draft** leaves it alone — nothing it computes can
    // move a workspace-health flag — but a scoped commitment (`didOpen`) must
    // still answer it, because it re-adjudicated.
    if focus_rel.is_none() || adjudication == Adjudication::Commit {
        let channel = config_channel_diagnostics(meta, verdicts);
        if !channel.is_empty() {
            by_file.insert(PathBuf::from(".lattice.toml"), channel);
        }
    }
    (by_file, committed)
}

/// The `.lattice.toml` channel's desired diagnostic set for one root
/// (decision 023 clause 4): the config load error (severity error) and the
/// expect-drift / unused-override flags (severity warning, the CLI's exact
/// wording via the shared [`overrides`] renderers, hint suffix included).
///
/// All entries are file-level — no span, line 1 — and the config is never
/// indexed, so materialization takes the unindexed-file fallback (empty
/// source) and anchors every range at position 0,0. Stanza spans via
/// `toml::Spanned` are a possible refinement, not scope.
///
/// The flags describe the config that governs adjudication, so they render
/// from the same verdicts the publish pass filters through; a fabricated
/// default (broken at startup, no last-good) governs nothing and contributes
/// only its load error.
/// Render a [`ConfigError`] and its source chain as a single diagnostic
/// message. The error variants deliberately omit `{source}` from their Display
/// (it is a `.source()`), so the detail — the toml parse location, the io
/// cause — lives only in the chain; this walks it, mirroring what anyhow's
/// `{:#}` gives the CLI so both surfaces read identically.
fn config_error_message(error: &ConfigError) -> String {
    let mut message = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        let _ = write!(message, ": {cause}");
        source = cause.source();
    }
    message
}

fn config_channel_diagnostics(meta: &RootMeta, verdicts: &OverrideVerdicts) -> Vec<Diagnostic> {
    let file = PathBuf::from(".lattice.toml");
    let mut diagnostics = Vec::new();
    if let Some(error) = &meta.config_error {
        diagnostics.push(Diagnostic {
            file: file.clone(),
            line: 1,
            severity: Severity::Error,
            message: config_error_message(error),
            span: None,
        });
    }
    if !meta.config_committed {
        return diagnostics;
    }
    for &idx in &verdicts.unused {
        diagnostics.push(Diagnostic {
            file: file.clone(),
            line: 1,
            severity: Severity::Warning,
            message: overrides::unused_message(&meta.config.overrides[idx]),
            span: None,
        });
    }
    for verdict in &verdicts.entries {
        if let VerdictKind::Drifted { expect, found } = verdict.kind {
            diagnostics.push(Diagnostic {
                file: file.clone(),
                line: 1,
                severity: Severity::Warning,
                message: overrides::drift_message(
                    &meta.config.overrides[verdict.entry],
                    verdict.lint,
                    expect,
                    found,
                ),
                span: None,
            });
        }
    }
    diagnostics
}

/// Canonicalize each changed URI to the form the publish cache is keyed by, so
/// the force-re-materialize check lines up with the per-file URIs. A document's
/// canonical form joins its primary root's canonical scan path (which differs
/// from the client-supplied folder key only under a symlink — issue 047).
fn canonicalize_changed_uris(
    workspaces: &Workspaces,
    changed_uris: &HashSet<String>,
) -> HashSet<String> {
    changed_uris
        .iter()
        .filter_map(|uri| {
            let abs = uri_to_path(uri);
            let root = workspaces.store.primary_root(&abs)?;
            let meta = workspaces.roots.get(&root)?;
            let rel = abs.strip_prefix(&root).ok()?;
            Some(path_to_uri(&meta.canonical_root.join(rel)))
        })
        .collect()
}

/// A file the publish detector decided to (re-)materialize: its fresh Lattice
/// and LSP vectors, plus whether the LSP form differs from what the client
/// holds.
struct Materialized {
    /// The publish-cache key (the client-spelling URI).
    uri: String,
    /// The freshly computed Lattice vector — the cheap change-detection key.
    lattice: Vec<Diagnostic>,
    /// Its UTF-16 materialization — what would go on the wire.
    lsp: Vec<lsp::Diagnostic>,
    /// Whether that materialization differs from the client's copy.
    send: bool,
}

/// Materialize one root's share of a publish pass: decide, per file, whether
/// its vector moved, and materialize only those that did.
///
/// Split out of [`diff_diagnostics_with`]'s phase 1 so the pass reads as its
/// three phases (detect / apply / clear) rather than one long loop.
#[allow(
    clippy::too_many_arguments,
    reason = "the per-root detection step genuinely needs the store, the root and its metadata, the scope, its computed rows, the force set, and both accumulators; bundling them into a struct would only rename the same parameters"
)]
fn materialize_root(
    workspaces: &Workspaces,
    root: &Path,
    meta: &RootMeta,
    scope: &PublishScope,
    mut by_file: BTreeMap<PathBuf, Vec<Diagnostic>>,
    changed_canonical: &HashSet<String>,
    present: &mut HashSet<String>,
    materialized: &mut Vec<Materialized>,
) {
    // Fallback index for the defensive unindexed-file path (the config
    // channel); real files use their own cached `line_index`.
    let empty = LineIndex::default();

    // The publish/cache URI is keyed by the client-supplied folder path
    // (`root`); the force-re-materialize check lines up with
    // `changed_canonical`, derived from the root's canonical scan path. The two
    // bases coincide unless the client opened the folder through a symlink;
    // when they differ, the comparison is run on the canonical root so a moved
    // diagnostic is not skipped (issue 047).
    let root_is_canonical = root == meta.canonical_root.as_path();

    // The config channel: one per-root URI outside the indexed markdown set
    // (decision 023 clause 4), admitted to the publish-diff cache only while
    // the root has a config state to report on — a marker on disk, or a
    // recorded load error. On a config delete or root deregistration it drops
    // out of `present`, so the phase-3 clear empties the client's copy.
    let config_channel =
        (meta.has_config || meta.config_error.is_some()).then(|| PathBuf::from(".lattice.toml"));

    // A scoped pass visits exactly the keys `root_desired_rows` produced (its
    // focus, plus the config channel when it committed); a full pass visits
    // every current document of the root — the saved membership plus any
    // buffer-only document, which is a member of its own perspective alone.
    let rels: Vec<PathBuf> = match scope {
        PublishScope::Full => workspaces
            .store
            .current_files(root)
            .into_keys()
            .chain(config_channel)
            .collect(),
        PublishScope::Only(_) => by_file.keys().cloned().collect(),
    };

    for rel_path in rels {
        let uri = path_to_uri(&root.join(&rel_path));
        if !present.insert(uri.clone()) {
            // Already claimed by a deeper root in this pass.
            continue;
        }

        let lattice = by_file.remove(&rel_path).unwrap_or_default();
        let cached = workspaces.published.get(&uri);
        let force = if root_is_canonical {
            changed_canonical.contains(&uri)
        } else {
            changed_canonical.contains(&path_to_uri(&meta.canonical_root.join(&rel_path)))
        };

        // Reuse the cached materialization when this file's source is unchanged
        // (it is not the edited file) and its Lattice vector still matches what
        // produced the cached LSP form.
        if !force {
            match cached {
                Some(prev) if prev.lattice == lattice => continue,
                None if lattice.is_empty() => continue,
                _ => {}
            }
        }

        // Materialize against the document's **current** text: an overlay
        // document's rows are anchored in its buffer, which is the text the
        // client is displaying.
        let fd = workspaces
            .store
            .current(&root.join(&rel_path))
            .map(|doc| &doc.data);
        let source = fd.map_or("", |fd| fd.tree.source());
        let index = fd.map_or(&empty, |fd| &fd.line_index);
        let lsp: Vec<lsp::Diagnostic> = lattice
            .iter()
            .map(|d| to_lsp_diagnostic(d, source, index))
            .collect();
        let send = cached.map_or(!lsp.is_empty(), |prev| prev.lsp != lsp);
        materialized.push(Materialized {
            uri,
            lattice,
            lsp,
            send,
        });
    }
}

/// [`diff_diagnostics`] with an explicit adjudication mode and publish scope.
///
/// Adjudication (decision 023): under [`Adjudication::Commit`] each root's
/// verdicts are re-adjudicated from the **saved world** and held; under
/// [`Adjudication::Held`] the last commitment's verdicts bind unchanged.
///
/// Scope (decision 024 clause 2): under [`PublishScope::Full`] every document
/// is recomputed and diffed; under [`PublishScope::Only`] exactly one
/// document's rows are, and every other cache entry is left untouched — a
/// buffer event cannot move another file's rows, so re-reading them would be
/// work with no possible result. The phase-3 "cleared files" sweep is likewise
/// skipped for a scoped pass: nothing left the workspace.
///
/// The per-file computation itself lives in [`root_desired_rows`].
fn diff_diagnostics_with(
    workspaces: &mut Workspaces,
    changed_uris: &HashSet<String>,
    adjudication: Adjudication,
    scope: &PublishScope,
) -> Vec<(String, Vec<lsp::Diagnostic>)> {
    // Count this whole-workspace recompute pass so tests can assert that a
    // batched watched-file notification collapses to one pass, not one per
    // changed file (ticket perf 07). Compiled out of release builds.
    #[cfg(test)]
    RECOMPUTE_COUNT.with(|count| count.set(count.get() + 1));

    let changed_canonical = canonicalize_changed_uris(workspaces, changed_uris);

    let mut materialized: Vec<Materialized> = Vec::new();
    let mut present: HashSet<String> = HashSet::new();
    // The verdicts a commitment pass re-adjudicated, applied to each root's
    // `RootMeta` after the immutable phase-1 borrow ends (decision 023).
    let mut new_verdicts: Vec<(PathBuf, OverrideVerdicts)> = Vec::new();

    // Phase 1 — detection. With an immutable view of the store and the published
    // cache, recompute each file's Lattice vector, decide whether it changed,
    // and materialize only the changed files. Collect owned results so the cache
    // can be mutated afterward.
    //
    // Deepest root first (reverse key order), and each absolute URI is claimed
    // by the first (deepest) root that indexes it: nested roots range-scan the
    // same absolute file, and letting both compute the same publish-cache key
    // makes successive passes alternate between the two roots' diagnostic sets
    // — the deeper one's vector one pass, the shallower's the next (issue 050's
    // flip-flop shape). The deepest root owning the URI matches how `resolve`
    // routes document events and how the test oracle `desired_diagnostics`
    // settles the same URI.
    for (root, meta) in workspaces.roots.iter().rev() {
        let (by_file, committed) = root_desired_rows(workspaces, root, meta, adjudication, scope);
        if let Some(verdicts) = committed {
            new_verdicts.push((root.clone(), verdicts));
        }
        materialize_root(
            workspaces,
            root,
            meta,
            scope,
            by_file,
            &changed_canonical,
            &mut present,
            &mut materialized,
        );
    }

    // Hold the commitment's fresh verdicts (decision 023): every publish until
    // the next commitment — the `didChange` drafts — filters through these.
    for (root, verdicts) in new_verdicts {
        if let Some(meta) = workspaces.roots.get_mut(&root) {
            meta.verdicts = verdicts;
        }
    }

    // Keyed by URI so the result is deterministically ordered.
    let mut to_send: BTreeMap<String, Vec<lsp::Diagnostic>> = BTreeMap::new();

    // Phase 2 — apply. Update only the changed entries in place; untouched files
    // keep their cache entries, so this stays O(changed), not O(workspace).
    for entry in materialized {
        if entry.send {
            to_send.insert(entry.uri.clone(), entry.lsp.clone());
        }
        if entry.lsp.is_empty() {
            workspaces.published.remove(&entry.uri);
        } else {
            workspaces.published.insert(
                entry.uri,
                PublishedDiagnostics {
                    lattice: entry.lattice,
                    lsp: entry.lsp,
                },
            );
        }
    }

    // Phase 3 — clear files that left the workspace (cached but no longer
    // present): send an empty vector and drop the entry. Only a full pass can
    // conclude a file is absent — a scoped pass never enumerated the others.
    if *scope == PublishScope::Full {
        let absent: Vec<String> = workspaces
            .published
            .keys()
            .filter(|uri| !present.contains(uri.as_str()))
            .cloned()
            .collect();
        for uri in absent {
            workspaces.published.remove(&uri);
            to_send.insert(uri, Vec::new());
        }
    }

    to_send.into_iter().collect()
}

// Counts `diff_diagnostics` invocations — one per whole-workspace recompute /
// publish pass — so tests can assert that a batched watched-file notification
// collapses N changed files into a single pass, not N (ticket perf 07).
// Compiled out of release builds.
#[cfg(test)]
thread_local! {
    static RECOMPUTE_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests use expect and panic for clarity"
)]
mod tests;
