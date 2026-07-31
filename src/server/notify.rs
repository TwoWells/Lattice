// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Notification handling: document sync, watched files, folder and rename
//! events — every message that *moves* server state.
//!
//! These are the writers. A request handler takes `&Workspaces` and answers a
//! question; everything here takes `&mut Workspaces`, drives the store's
//! writers, and ends at a publish. The routing decisions they encode are
//! decision 024 clause 2's columns — which events are buffer events and which
//! are commitments — and decision 017's watched-files channel, the sole writer
//! of the saved store's `.md` content alongside the initial scan and `didSave`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::Result;
use lsp_server::{Connection, Message, Notification};

use crate::lsp;
use crate::store::DiskUpdate;
use crate::uri::{path_to_uri, uri_to_path};

use super::publish::{
    PublishScope, force_republish, force_republish_config, one_uri, publish_all_diagnostics,
    publish_draft_diagnostics, publish_file_diagnostics,
};
use super::workspaces::Workspaces;
use super::{file_change_kind, is_config_uri, is_markdown_uri};

// ---------------------------------------------------------------------------
// Notifications
// ---------------------------------------------------------------------------

/// The `.lattice.toml` URI a document-sync notification names, if any — the
/// routing probe for [`handle_config_sync`]. Every `textDocument/*` sync
/// notification carries its URI at `textDocument.uri`; notifications without
/// one (watched files, folder changes, renames) probe `None` and dispatch
/// normally.
pub fn config_sync_uri(notif: &Notification) -> Option<String> {
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
pub fn handle_config_sync(
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
pub fn handle_notification(
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
pub fn handle_watched_files_change(
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
pub fn handle_did_rename_files(
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
pub fn close_document(
    connection: &Connection,
    workspaces: &mut Workspaces,
    uri: &str,
) -> Result<()> {
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
