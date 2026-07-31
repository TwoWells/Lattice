// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! The two rename surfaces, and the one edit mapping they share.
//!
//! Decision 020 treats a heading rename and a file move as the same kind of
//! event on two different axes: `textDocument/rename` changes a coordinate on
//! the *fragment* axis, `workspace/willRenameFiles` changes one on the *path*
//! axis. Both ask [`crate::mv`] for the complete forced edit set and both hand
//! it to [`merge_span_edits`], so neither surface has a private notion of what
//! a workspace edit is — and a refused move is a JSON-RPC error that aborts the
//! rename client-side, so the file never moves.
//!
//! Edits read the **current** view (decision 024 clause 9): a `WorkspaceEdit` is
//! consumed synchronously by the client that owns those buffers, so it must be
//! anchored in the text on screen.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use crate::lsp;
use crate::uri::{path_to_uri, uri_to_path};

use super::helpers::{heading_at_line, span_to_lsp_range};
use super::workspaces::Workspaces;

// ---------------------------------------------------------------------------
// prepareRename / rename (ticket 04)
// ---------------------------------------------------------------------------

/// Find the heading at a cursor position, returning its text range.
///
/// Uses the tree's `text_span` to compute the exact text range, supporting
/// ATX, setext, and HTML headings without prefix assumptions.
pub fn prepare_rename(
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
pub fn do_rename(
    workspaces: &Workspaces,
    params: &lsp::RenameParams,
) -> Option<lsp::WorkspaceEdit> {
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
pub fn will_rename_files(
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
pub fn merge_span_edits(
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
