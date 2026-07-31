// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! `textDocument/formatting` — backlink frontmatter, normalized.
//!
//! Delegates to [`crate::format::format_source`], the single source of
//! formatting semantics shared with the `lattice format` CLI, and returns the
//! result as one whole-document edit.

use crate::lsp;

use super::workspaces::Workspaces;

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
pub fn format_document(workspaces: &Workspaces, uri: &str) -> Option<Vec<lsp::TextEdit>> {
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
