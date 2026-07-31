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
mod dispatch;
mod folding;
mod formatting;
mod helpers;
mod hover;
mod navigation;
mod notify;
mod publish;
mod rename;
mod semantic_tokens;
mod symbols;
mod workspaces;

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use lsp_server::{Connection, Message, Request, RequestId, Response};

use crate::lsp;

use self::dispatch::handle_request;
use self::notify::handle_notification;
use self::publish::publish_all_diagnostics;
use self::semantic_tokens::{
    SEMANTIC_MODIFIER_BOLD, SEMANTIC_MODIFIER_ITALIC, SEMANTIC_MODIFIER_STRIKETHROUGH,
    SEMANTIC_TOKEN_TYPE_MARKUP,
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

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests use expect and panic for clarity"
)]
mod tests;
