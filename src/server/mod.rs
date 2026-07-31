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

mod completion;
mod diagnostics;
mod folding;
mod formatting;
mod helpers;
mod hover;
mod navigation;
mod notify;
mod publish;
mod rename;
mod semantic_tokens;
mod workspaces;

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use lsp_server::{Connection, Message, Request, RequestId, Response};

use crate::block::{ElementKind, NodeId, Syntax, Tree, first_line};
use crate::lsp;
use crate::uri::path_to_uri;

use self::completion::completion;
use self::folding::folding_ranges;
use self::formatting::format_document;
use self::hover::hover_preview;
use self::navigation::{
    call_hierarchy_incoming, call_hierarchy_outgoing, document_links, find_references,
    go_to_declaration, go_to_definition, go_to_implementation, go_to_type_definition,
    prepare_call_hierarchy, prepare_type_hierarchy, type_hierarchy_subtypes,
    type_hierarchy_supertypes,
};
use self::notify::handle_notification;
use self::publish::publish_all_diagnostics;
use self::rename::{do_rename, prepare_rename, will_rename_files};
use self::semantic_tokens::{
    SEMANTIC_MODIFIER_BOLD, SEMANTIC_MODIFIER_ITALIC, SEMANTIC_MODIFIER_STRIKETHROUGH,
    SEMANTIC_TOKEN_TYPE_MARKUP, semantic_tokens_full, semantic_tokens_range,
};
use self::workspaces::Workspaces;

// Re-exported so `crate::server::byte_offset_to_lsp_position`,
// `crate::server::lsp_position_to_byte_offset`,
// `crate::server::collect_all_diagnostics` and
// `crate::server::merge_perspectives` keep naming these wherever the submodules
// put them: the shared invariants harness ([`crate::invariants`]) asserts
// against all four — the position pair is also the contract
// [`crate::line_index`] documents itself as mirroring — and it is the only
// caller outside this module, hence the same `cfg` it is compiled under, so the
// re-exports are never dead weight in a release build.
#[cfg(any(test, feature = "fuzzing"))]
pub use self::diagnostics::collect_all_diagnostics;
#[cfg(any(test, feature = "fuzzing"))]
pub use self::helpers::{byte_offset_to_lsp_position, lsp_position_to_byte_offset};
#[cfg(any(test, feature = "fuzzing"))]
pub use self::publish::merge_perspectives;

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

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests use expect and panic for clarity"
)]
mod tests;
