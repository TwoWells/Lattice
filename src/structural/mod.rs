// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Structural diagnostics — document quality checks that run unconditionally.
//!
//! These diagnostics validate the document as a well-formed markdown/HTML
//! artifact, independent of Lattice's predicate graph. They run on every
//! file regardless of whether `.lattice.toml` is present.

mod content;
mod html;
mod ledger;
mod references;

use std::path::Path;

use crate::block::Tree;
use crate::config::{BarePathPolicy, Config, StaleReferencePolicy};
use crate::fm::Exceptions;
use crate::validation::Diagnostic;

use self::content::{
    emit_code_block_diagnostics, emit_heading_diagnostics, emit_image_diagnostics,
    emit_parser_diagnostics, emit_trailing_whitespace_diagnostics,
};
use self::html::{check_markdown_in_opaque_html, emit_html_diagnostics};
use self::ledger::ExceptionLookup;
use self::references::{emit_bare_path_diagnostics, emit_tree_bare_paths};

// The ledger types are the CLI's rendering vocabulary and `lint`'s aggregation
// vocabulary, so they keep their `crate::structural::…` spelling across the
// split.
pub use self::ledger::{FileSuppressions, SeverityCounts, classify_028_lint};

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Existence verdict from the external-alias `stat` oracle (issue 050).
///
/// A `stat` can fail without answering the existence question — permissions,
/// descriptor exhaustion, transient I/O. Folding that failure into "absent"
/// silently degraded a defined `{Name}/…` alias to the exempt tier and
/// misreported its frontmatter exception as unused, so the failure case is
/// kept distinct: the caller surfaces it instead of exempting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalExistence {
    /// The path exists.
    Present,
    /// `stat` answered: the path definitively does not exist.
    Absent,
    /// The `stat` itself failed — existence is unknown, not "absent".
    Unknown,
}

impl ExternalExistence {
    /// Classify `path` by `stat`, mapping the three outcomes of
    /// [`Path::try_exists`]: found, definitively absent, and "the check
    /// itself failed". The production oracles (workspace loader and CLI)
    /// use this; test and fuzz harnesses substitute deterministic verdicts.
    pub fn stat(path: &Path) -> Self {
        match path.try_exists() {
            Ok(true) => Self::Present,
            Ok(false) => Self::Absent,
            Err(err) => {
                tracing::warn!(path = %path.display(), "external-alias stat failed: {err}");
                Self::Unknown
            }
        }
    }
}

/// Collect all structural diagnostics for a single file.
///
/// `rel_path` is the workspace-relative path, used for bare path existence
/// checks via `file_exists`. `config` controls severity for configurable
/// diagnostics (code block language, admonitions).
///
/// `external_exists` `stat`s an **absolute** filesystem path; it backs the
/// existence-only resolution of `{Name}/…` external-namespace references (issue
/// 030, decision 010). Unlike `file_exists`, which answers workspace membership,
/// `external_exists` reaches outside the workspace to the configured alias
/// directory — but only ever to `stat`, never to read or index. Its verdict is
/// the tri-state [`ExternalExistence`]: a failed `stat` is `Unknown`, not
/// "absent", so an I/O flake surfaces as a diagnostic instead of silently
/// exempting the reference (issue 050).
///
/// `exceptions` is this file's parsed `exceptions` frontmatter block (issue 031,
/// decision 011): a path-shaped diagnostic whose reference matches an entry in
/// the corresponding `exceptions.<lint>` namespace is **suppressed**, and an
/// entry that matches no live diagnostic is reconciled afterward — flagged as an
/// *unused exception* echoing its reason, or as a missing-reason defect.
///
/// This is the diagnostics-only convenience wrapper over
/// [`collect_with_suppressions`]; it discards the suppression ledger. The
/// production path (the workspace loader) calls the suppressions form so the CLI
/// can render the ledger, while the property suite, the fuzz harness, and the
/// invariants module — which only assert on the diagnostics — use this form.
#[cfg(any(test, feature = "fuzzing"))]
pub fn collect(
    tree: &Tree,
    rel_path: &Path,
    config: &Config,
    file_exists: &dyn Fn(&Path) -> bool,
    external_exists: &dyn Fn(&Path) -> ExternalExistence,
    exceptions: &Exceptions,
) -> Vec<Diagnostic> {
    collect_with_suppressions(
        tree,
        rel_path,
        config,
        file_exists,
        external_exists,
        exceptions,
    )
    .0
}

/// Like [`collect`], but also returns the [`FileSuppressions`] ledger entry for
/// this file — what each suppression source (literal frontmatter exceptions and
/// count-keys) actually suppressed, broken out by severity (issue 036,
/// decision 012 part B).
///
/// [`collect`] is the thin wrapper that discards the ledger for the LSP, the
/// property suite, and the fuzz harness, which only consume the diagnostics; the
/// CLI lint loop calls this form and aggregates the ledger across files. The
/// emitted diagnostics are identical between the two — count-key resolution and
/// unused-exception reconciliation run regardless of whether the ledger is kept.
pub fn collect_with_suppressions(
    tree: &Tree,
    rel_path: &Path,
    config: &Config,
    file_exists: &dyn Fn(&Path) -> bool,
    external_exists: &dyn Fn(&Path) -> ExternalExistence,
    exceptions: &Exceptions,
) -> (Vec<Diagnostic>, FileSuppressions) {
    let mut diagnostics = Vec::new();
    let source = tree.source();

    // Build the reconciliation lever once and thread it through the path-shaped
    // emit sites. A lint whose policy is `Disabled` is excluded so its
    // exceptions are neither consulted nor flagged as unused — there are no live
    // diagnostics to reconcile against, and flagging them all would be a false
    // unused-exception flood (issue 031). The same `Disabled` gate makes a
    // count-key inert (issue 036).
    let lookup = ExceptionLookup::new(
        exceptions,
        &config.artifacts,
        config.policy.stale_references != StaleReferencePolicy::Disabled,
        config.policy.bare_paths != BarePathPolicy::Disabled,
    );

    emit_parser_diagnostics(tree, rel_path, &mut diagnostics);
    emit_heading_diagnostics(tree, rel_path, config, &mut diagnostics);
    emit_tree_bare_paths(
        tree,
        rel_path,
        config,
        file_exists,
        external_exists,
        &lookup,
        &mut diagnostics,
    );
    emit_bare_path_diagnostics(
        tree,
        rel_path,
        config,
        file_exists,
        external_exists,
        &lookup,
        &mut diagnostics,
    );
    emit_html_diagnostics(tree, rel_path, &mut diagnostics);
    check_markdown_in_opaque_html(tree, rel_path, &mut diagnostics);
    crate::metadata::carrier_diagnostics(tree, rel_path, &mut diagnostics);
    emit_code_block_diagnostics(tree, rel_path, config, &mut diagnostics);
    emit_image_diagnostics(tree, rel_path, config, &mut diagnostics);
    emit_trailing_whitespace_diagnostics(source, rel_path, tree, &mut diagnostics);

    // Resolve the count-keys: each lint's residual (the diagnostics buffered
    // because a count-key was active and they survived literal suppression) is
    // either suppressed wholesale (residual `M == N`) or resurfaced with a drift
    // warning anchored at the count key (`M != N`) — issue 036, decision 012.
    lookup.resolve_count_keys(rel_path, &mut diagnostics);

    // Reconcile: after every live diagnostic has had a chance to match, flag the
    // exceptions that matched nothing (issue 031, decision 011 — flag, never
    // auto-remove).
    lookup.emit_unmatched(rel_path, &mut diagnostics);

    diagnostics.sort_by(|a, b| a.file.cmp(&b.file).then(a.line.cmp(&b.line)));
    let suppressions = lookup.into_suppressions(rel_path);
    (diagnostics, suppressions)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests use expect and panic for clarity"
)]
mod tests;
