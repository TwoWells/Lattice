// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! What a diagnostic row is, and how it materializes for the wire.
//!
//! Two things live here, and they are the two halves of one answer. The
//! collection side ([`collect_all_diagnostics`]) computes a workspace's rows —
//! structural unconditionally, graph gated on a committed config — and is
//! shared verbatim with the `lattice lint` CLI, so the editor and the terminal
//! can never disagree about what a finding is. The materialization side
//! ([`to_lsp_diagnostic`]) turns one row into its LSP form, routing both range
//! endpoints through the file's cached [`LineIndex`].
//!
//! Deciding *when* to send the result, and to whom, is the sibling
//! [`super::publish`] module's job.

use std::path::Path;

use crate::line_index::LineIndex;
use crate::lsp;
use crate::validation::{self, Diagnostic, Severity};
use crate::workspace::{FileData, WorkspaceView};

use super::helpers::{line_byte_range, span_to_lsp_range};

// ---------------------------------------------------------------------------
// Diagnostic collection (shared by the push path and `lattice lint`)
// ---------------------------------------------------------------------------

/// Collect all diagnostics for a workspace: structural (unconditional) +
/// graph (gated by `.lattice.toml`).
pub fn collect_all_diagnostics(workspace: &WorkspaceView) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    // Structural diagnostics: always run, no config required. Read from the
    // per-file cache, which the workspace refreshes only for the reparsed file
    // (or, on a membership change, all files) — so this no longer re-walks
    // every cached tree on each sync (issue 013 — stage 2).
    for (path, file_data) in workspace.files() {
        diagnostics.extend(file_local_diagnostics(file_data, path));
    }

    // Graph diagnostics: only when .lattice.toml is present.
    if workspace.has_config() {
        diagnostics.extend(validation::collect_all(workspace));
    }

    diagnostics.sort_by(|a, b| a.file.cmp(&b.file).then(a.line.cmp(&b.line)));
    diagnostics
}

/// The unconditional (config-independent) diagnostics for a single file: its
/// cached structural diagnostics (issue 013 — stage 2) plus frontmatter parse
/// diagnostics. Returned unsorted — callers sort as they need.
///
/// Shared by the full-workspace collect and the per-file incremental publish so
/// the two cannot drift (the stage-2.5 differential invariant).
pub fn file_local_diagnostics(file_data: &FileData, rel_path: &Path) -> Vec<Diagnostic> {
    let mut diagnostics = file_data.structural.clone();
    for pd in &file_data.parse_diagnostics {
        let severity = match pd.severity {
            crate::fm::FmSeverity::Error => Severity::Error,
            crate::fm::FmSeverity::Warning => Severity::Warning,
        };
        diagnostics.push(Diagnostic {
            file: rel_path.to_path_buf(),
            line: pd.line,
            severity,
            message: format!("frontmatter: {}", pd.message),
            span: None,
        });
    }
    diagnostics
}

/// The file-local diagnostics for a single file in both forms: the Lattice
/// vector (sorted by line — the change-detection key) and its materialization
/// against the file's source. Both are empty when the file is not indexed.
///
/// This is the structural-tier slice of the full desired set; it excludes graph
/// diagnostics, so [`diff_file_diagnostics`] is sound only in the structural
/// tier (its callers gate on `!has_config()`).
pub fn file_desired(
    workspace: &WorkspaceView,
    rel_path: &Path,
) -> (Vec<Diagnostic>, Vec<lsp::Diagnostic>) {
    let Some(file_data) = workspace.file(rel_path) else {
        return (Vec::new(), Vec::new());
    };
    let source = file_data.tree.source();
    let index = &file_data.line_index;
    let mut lattice = file_local_diagnostics(file_data, rel_path);
    lattice.sort_by_key(|d| d.line);
    let lsp = lattice
        .iter()
        .map(|d| to_lsp_diagnostic(d, source, index))
        .collect();
    (lattice, lsp)
}

// Counts `to_lsp_diagnostic` calls so tests can assert that an incremental
// publish re-materializes only the files whose diagnostics changed, rather than
// the whole workspace (ticket perf 02 acceptance). Compiled out of release
// builds, so the hot path pays nothing.
#[cfg(test)]
thread_local! {
    pub static MATERIALIZE_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Convert a Lattice diagnostic to an LSP diagnostic.
///
/// Builds the range from the diagnostic's byte span when present (precise
/// underline); otherwise falls back to a whole-line range anchored on
/// `diag.line`. `source` is the text of the file the diagnostic belongs to and
/// `index` is that file's cached [`LineIndex`], through which the byte→position
/// conversion is routed (ticket perf 01).
pub fn to_lsp_diagnostic(diag: &Diagnostic, source: &str, index: &LineIndex) -> lsp::Diagnostic {
    #[cfg(test)]
    MATERIALIZE_COUNT.with(|count| count.set(count.get() + 1));

    let severity = match diag.severity {
        Severity::Error => lsp::diagnostic_severity::ERROR,
        Severity::Warning => lsp::diagnostic_severity::WARNING,
        Severity::Info => lsp::diagnostic_severity::INFORMATION,
        Severity::Hint => lsp::diagnostic_severity::HINT,
    };

    let range = diag.span.map_or_else(
        || whole_line_range(source, index, diag.line),
        |span| span_to_lsp_range(source, index, &span),
    );

    lsp::Diagnostic {
        range,
        severity: Some(severity),
        source: Some("lattice".to_string()),
        message: diag.message.clone(),
    }
}

/// An LSP range covering an entire line's content (column 0 to the line's end,
/// excluding the terminator). Used for diagnostics that carry only a line
/// anchor, so the underline at least covers the line instead of a zero-width
/// point at column 0. The two endpoint conversions route through the file's
/// cached [`LineIndex`].
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
pub fn whole_line_range(source: &str, index: &LineIndex, line: usize) -> lsp::Range {
    let (start, end) = line_byte_range(source, line.saturating_sub(1) as u32);
    lsp::Range {
        start: index.position(source, start),
        end: index.position(source, end),
    }
}
