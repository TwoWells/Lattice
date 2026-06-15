// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Structural diagnostics — document quality checks that run unconditionally.
//!
//! These diagnostics validate the document as a well-formed markdown/HTML
//! artifact, independent of Lattice's predicate graph. They run on every
//! file regardless of whether `.lattice.toml` is present.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;

use crate::block::{self, ElementKind, Syntax, Tree};
use crate::config::{
    BarePathPolicy, CodeBlockLanguagePolicy, Config, FragmentAlgorithm, StaleReferencePolicy,
};
use crate::fm::{CountKey, ExceptionEntry, ExceptionLint, Exceptions};
use crate::html;
use crate::span::Span;
use crate::validation::{Diagnostic, Severity};

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

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
/// directory — but only ever to `stat`, never to read or index.
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
    external_exists: &dyn Fn(&Path) -> bool,
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
    external_exists: &dyn Fn(&Path) -> bool,
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
// Suppression ledger (issue 036, decision 012 part B)
// ---------------------------------------------------------------------------

/// A tally of suppressed diagnostics broken out by severity.
///
/// The ledger reports what each suppression source hid, by severity; this is the
/// per-source, per-file accumulator. Only the severities a path-shaped lint
/// actually produces are tracked (errors under a `Deny` policy, warnings, and
/// hints); `Info` is included for completeness so the type is total.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SeverityCounts {
    /// Suppressed error-level diagnostics.
    pub errors: usize,
    /// Suppressed warning-level diagnostics.
    pub warnings: usize,
    /// Suppressed info-level diagnostics.
    pub info: usize,
    /// Suppressed hint-level diagnostics.
    pub hints: usize,
}

impl SeverityCounts {
    /// Record one suppressed diagnostic of `severity`.
    ///
    /// Used by the per-file exception/count-key tallies here and by the
    /// workspace subtree-override aggregate in `lint` (issue 037), which counts
    /// freeze- and `expect`-suppressed diagnostics into one of these tallies.
    pub fn record(&mut self, severity: Severity) {
        match severity {
            Severity::Error => self.errors += 1,
            Severity::Warning => self.warnings += 1,
            Severity::Info => self.info += 1,
            Severity::Hint => self.hints += 1,
        }
    }

    /// Fold another tally into this one (cross-file aggregation).
    pub fn add(&mut self, other: Self) {
        self.errors += other.errors;
        self.warnings += other.warnings;
        self.info += other.info;
        self.hints += other.hints;
    }

    /// Whether nothing was suppressed (every severity is zero).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.errors == 0 && self.warnings == 0 && self.info == 0 && self.hints == 0
    }
}

/// One count-key ledger row: a count-key that suppressed its residual.
///
/// Recorded only when the residual matched the expected count (`M == N`), so the
/// suppression actually fired. A drifted count-key suppresses nothing and so
/// produces no row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CountKeySuppression {
    /// The shared reason — the ledger row's label (decision 012's "the
    /// consolidation table").
    pub reason: String,
    /// The count-key text as written (e.g. `31`), shown as `count-key (31)`.
    pub raw: String,
    /// What the count-key suppressed, by severity.
    pub counts: SeverityCounts,
}

/// One literal-exceptions ledger row: the diagnostics a file's frontmatter
/// literal exceptions suppressed.
///
/// Aggregated per file (the row label is the file path), with the number of
/// distinct entries that actually matched at least one diagnostic — the ledger's
/// `exceptions (k)` detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExceptionSuppression {
    /// What the file's literal exceptions suppressed, by severity.
    pub counts: SeverityCounts,
    /// The number of distinct literal entries that matched ≥1 live diagnostic.
    pub matched_entries: usize,
}

/// The per-file suppression ledger entry: what each source suppressed in one
/// file (issue 036, decision 012 part B).
///
/// The CLI lint loop collects one of these per file and renders the workspace
/// ledger from them. Issue 037 added a third source (subtree overrides), and
/// issue 038 the fourth (the artifact glossary); the renderer iterates source
/// kinds rather than hard-coding them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileSuppressions {
    /// The file these suppressions belong to (workspace-relative).
    pub file: std::path::PathBuf,
    /// The literal-exceptions row for this file, if any literal exception
    /// matched.
    pub exceptions: Option<ExceptionSuppression>,
    /// The count-key rows for this file (at most one per lint namespace).
    pub count_keys: Vec<CountKeySuppression>,
    /// The artifact glossary suppressions in this file, keyed by the artifact
    /// name (issue 038, decision 013): each entry is one glossary member whose
    /// bare/backticked/quoted mentions were filtered before the 028-family
    /// machinery, tallied by severity. The CLI ledger aggregates these
    /// repo-wide into one row per artifact name.
    pub artifacts: BTreeMap<String, SeverityCounts>,
}

impl FileSuppressions {
    /// Whether this file suppressed nothing (no ledger rows).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.exceptions.is_none() && self.count_keys.is_empty() && self.artifacts.is_empty()
    }
}

/// Classify an emitted diagnostic message as one of the 028-family lints
/// (`stale_references` / `bare_paths`), or `None` if it belongs to neither.
///
/// The subtree-override expect-aggregate pass (issue 037) needs to identify the
/// live diagnostics of a given 028-family lint across the files a glob matches,
/// but [`Diagnostic`] carries no lint tag (issue 036 deliberately kept the type
/// unchanged). This is the single owner of that message → lint mapping, keyed on
/// the fixed message prefixes the emitters above produce: `stale reference: …`
/// for [`ExceptionLint::StaleReferences`], and the four `bare_paths` nudges
/// (`bare path …`, `bare URL …`, `quoted path …`, `backticked path …`) for
/// [`ExceptionLint::BarePaths`]. It is colocated with those emitters so the two
/// cannot drift, and is exercised directly by a unit test.
#[must_use]
pub fn classify_028_lint(message: &str) -> Option<ExceptionLint> {
    if message.starts_with("stale reference:") {
        Some(ExceptionLint::StaleReferences)
    } else if message.starts_with("bare path ")
        || message.starts_with("bare URL ")
        || message.starts_with("quoted path ")
        || message.starts_with("backticked path ")
    {
        Some(ExceptionLint::BarePaths)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Exception reconciliation (issue 031, decision 011) + count-key (issue 036)
// ---------------------------------------------------------------------------

/// Per-lint exception entries paired with a matched flag, plus the optional
/// count-key residual buffer and the per-source suppression tallies.
///
/// Interior mutability (`Cell` / `RefCell`) lets the emit pass record matches,
/// buffer count-key residuals, and tally suppressions behind a shared reference,
/// so the lookup can be threaded as `&self` alongside the `&mut Vec<Diagnostic>`
/// the emitters already carry.
struct LintBucket<'a> {
    /// Whether this lint is active (policy not `Disabled`). When `false` the
    /// bucket is inert: it suppresses nothing, is never flagged as unused, and
    /// its count-key neither suppresses nor flags (issue 036).
    active: bool,
    /// The declared literal entries, in source order.
    entries: &'a [ExceptionEntry],
    /// Parallel matched flags — set the first time an entry's reference matches
    /// a live diagnostic this pass.
    matched: Vec<Cell<bool>>,
    /// The count-key sentinel for this lint, if one was declared (issue 036).
    count_key: Option<&'a CountKey>,
    /// The residual buffer: live diagnostics that survived literal suppression
    /// and are deferred for the count-key decision. Only populated when a
    /// count-key is active for this lint.
    residual: RefCell<Vec<Diagnostic>>,
    /// What this lint's literal exceptions suppressed, by severity.
    literal_suppressed: RefCell<SeverityCounts>,
    /// What this lint's count-key suppressed, by severity (set only when the
    /// residual matched `N`).
    count_suppressed: RefCell<SeverityCounts>,
}

impl<'a> LintBucket<'a> {
    fn new(entries: &'a [ExceptionEntry], count_key: Option<&'a CountKey>, active: bool) -> Self {
        Self {
            active,
            entries,
            matched: entries.iter().map(|_| Cell::new(false)).collect(),
            count_key,
            residual: RefCell::new(Vec::new()),
            literal_suppressed: RefCell::new(SeverityCounts::default()),
            count_suppressed: RefCell::new(SeverityCounts::default()),
        }
    }

    /// Whether a count-key residual buffer is collecting for this lint: the lint
    /// is active and a count-key is declared.
    fn count_key_active(&self) -> bool {
        self.active && self.count_key.is_some()
    }

    /// Try to suppress a live diagnostic against an active literal entry,
    /// recording the match and tallying the suppression. Returns `true` when a
    /// literal key matched (the diagnostic is suppressed). The key is matched
    /// **verbatim** (issue 031): the full reference string, including any leading
    /// `{Name}/…` and any `#fragment`, with no normalization.
    fn suppress_literal(&self, reference: &str, severity: Severity) -> bool {
        if !self.active {
            return false;
        }
        let mut suppressed = false;
        for (entry, flag) in self.entries.iter().zip(&self.matched) {
            if entry.reference == reference {
                flag.set(true);
                suppressed = true;
            }
        }
        if suppressed {
            self.literal_suppressed.borrow_mut().record(severity);
        }
        suppressed
    }
}

/// The per-file exception reconciliation lever (issue 031, decision 011; issue
/// 036, decision 012).
///
/// Holds both lint buckets, the matched-flag state, the count-key residual
/// buffers, and the suppression tallies. The path-shaped emitters call
/// [`route`](Self::route) with each would-be diagnostic; the lookup either
/// suppresses it (a literal key matched), buffers it for the count-key decision,
/// or passes it straight through to `out`. After the emit pass,
/// [`resolve_count_keys`](Self::resolve_count_keys) decides each residual and
/// [`emit_unmatched`](Self::emit_unmatched) flags unmatched literal entries.
struct ExceptionLookup<'a> {
    stale_references: LintBucket<'a>,
    bare_paths: LintBucket<'a>,
    /// The repo-level artifact glossary (issue 038, decision 013): known
    /// external filenames whose exact dark-matter mentions are filtered before
    /// any of the 028-family machinery. Empty when no `[graph] artifacts` is
    /// configured (the common case), in which case the artifact check in
    /// [`route`](Self::route) is a single set lookup that always misses.
    artifacts: &'a BTreeSet<String>,
    /// What the glossary swallowed this file, keyed by the matched artifact name
    /// and tallied by severity — the honesty floor for the ledger (decision
    /// 013): an artifact is not reconciled, so the ledger is the only place its
    /// suppression is visible.
    artifact_suppressed: RefCell<BTreeMap<String, SeverityCounts>>,
}

impl<'a> ExceptionLookup<'a> {
    fn new(
        exceptions: &'a Exceptions,
        artifacts: &'a BTreeSet<String>,
        stale_active: bool,
        bare_active: bool,
    ) -> Self {
        Self {
            stale_references: LintBucket::new(
                exceptions.entries(ExceptionLint::StaleReferences),
                exceptions.count_key(ExceptionLint::StaleReferences),
                stale_active,
            ),
            bare_paths: LintBucket::new(
                exceptions.entries(ExceptionLint::BarePaths),
                exceptions.count_key(ExceptionLint::BarePaths),
                bare_active,
            ),
            artifacts,
            artifact_suppressed: RefCell::new(BTreeMap::new()),
        }
    }

    fn bucket(&self, lint: ExceptionLint) -> &LintBucket<'a> {
        match lint {
            ExceptionLint::StaleReferences => &self.stale_references,
            ExceptionLint::BarePaths => &self.bare_paths,
        }
    }

    /// Route a would-be `lint` diagnostic on `reference` through the lookup.
    ///
    /// Outcomes, in order: an **artifact-glossary** member is filtered first —
    /// before literal suppression and the count-key residual buffer (decision
    /// 013, issue 038): an artifact is "not a reference at all," so it is tallied
    /// by artifact name (for the ledger) and dropped, never entering an
    /// exception, a count-key residual, or an `expect` aggregate, and is not
    /// exceptable. Otherwise a matching literal key **suppresses** it (tallied,
    /// dropped — literal keys win and are carved out of the residual first,
    /// decision 012); otherwise an active count-key **buffers** it for the later
    /// residual decision; otherwise it passes straight through to `out`.
    /// `reference` is matched verbatim — against the artifact glossary and the
    /// literal keys alike — exactly as the old inline `suppress` call did.
    fn route(
        &self,
        lint: ExceptionLint,
        reference: &str,
        diag: Diagnostic,
        out: &mut Vec<Diagnostic>,
    ) {
        // Artifact glossary filters first: a bare/backticked/quoted reference
        // whose literal string is a glossary member is outside the graph
        // boundary (decision 013), so it never reaches the 028-family
        // exception / count-key / override machinery.
        if self.artifacts.contains(reference) {
            self.artifact_suppressed
                .borrow_mut()
                .entry(reference.to_string())
                .or_default()
                .record(diag.severity);
            return;
        }
        let bucket = self.bucket(lint);
        if bucket.suppress_literal(reference, diag.severity) {
            return;
        }
        if bucket.count_key_active() {
            bucket.residual.borrow_mut().push(diag);
        } else {
            out.push(diag);
        }
    }

    /// Resolve each lint's count-key against its buffered residual (issue 036).
    ///
    /// For a lint with an active count-key, let `M` be the residual size and `N`
    /// the count-key's expected value. `N` must be `>= 1` and the reason
    /// non-empty (both diagnosed at the key otherwise, with the residual
    /// resurfaced). If `M == N` the whole residual is suppressed under the shared
    /// reason (and tallied); if `M != N` the count-key is inert — every residual
    /// diagnostic resurfaces and one drift `Warning` is anchored at the key.
    fn resolve_count_keys(&self, rel_path: &Path, out: &mut Vec<Diagnostic>) {
        for (lint, bucket) in [
            (ExceptionLint::StaleReferences, &self.stale_references),
            (ExceptionLint::BarePaths, &self.bare_paths),
        ] {
            let Some(count_key) = bucket.count_key else {
                continue;
            };
            // An inactive bucket (a `Disabled` lint) is inert: no diagnostics
            // were buffered, and the count-key neither suppresses nor flags.
            if !bucket.active {
                continue;
            }
            let residual = bucket.residual.take();
            let found = residual.len();
            let expected = count_key.expected;

            // A required reason and `N >= 1` (decision 012): when either is
            // violated the count-key cannot suppress — diagnose at the key and
            // resurface the whole residual (inert).
            if count_key.reason.trim().is_empty() {
                out.push(Diagnostic {
                    file: rel_path.to_path_buf(),
                    line: count_key.line,
                    severity: Severity::Warning,
                    message: format!(
                        "count-key `{}` under `exceptions.{}` has no reason — add one explaining why these are not live references (see `lattice help config`)",
                        count_key.raw,
                        lint.key()
                    ),
                    span: Some(count_key.key_span),
                });
                out.extend(residual);
                continue;
            }
            if expected == 0 {
                out.push(Diagnostic {
                    file: rel_path.to_path_buf(),
                    line: count_key.line,
                    severity: Severity::Warning,
                    message: format!(
                        "count-key `{}` under `exceptions.{}` must be at least 1 (see `lattice help config`)",
                        count_key.raw,
                        lint.key()
                    ),
                    span: Some(count_key.key_span),
                });
                out.extend(residual);
                continue;
            }

            if found == expected {
                // Residual matches the expected count: suppress it all under the
                // shared reason, tallying each by severity for the ledger.
                let mut tally = bucket.count_suppressed.borrow_mut();
                for diag in &residual {
                    tally.record(diag.severity);
                }
            } else {
                // Drift in either direction: the sentinel is inert. Every
                // residual diagnostic resurfaces, plus one warning at the key.
                out.extend(residual);
                out.push(Diagnostic {
                    file: rel_path.to_path_buf(),
                    line: count_key.line,
                    severity: Severity::Warning,
                    message: format!(
                        "expected {expected} {} here, found {found} — update the count (and revisit the reason) or fix the drift (see `lattice help config`)",
                        lint.noun()
                    ),
                    span: Some(count_key.key_span),
                });
            }
        }
    }

    /// Flag every exception entry that matched no live diagnostic this pass.
    ///
    /// An entry with an empty or missing reason is flagged as a missing-reason
    /// defect (decision 011: the required reason is the epitaph); a non-empty
    /// entry that matched nothing is flagged as an *unused exception* whose
    /// message echoes the stored reason. Each entry yields at most one
    /// reconciliation diagnostic, anchored at the offending key. Inactive
    /// buckets (a `Disabled` lint) are skipped entirely.
    fn emit_unmatched(&self, rel_path: &Path, out: &mut Vec<Diagnostic>) {
        for (lint, bucket) in [
            (ExceptionLint::StaleReferences, &self.stale_references),
            (ExceptionLint::BarePaths, &self.bare_paths),
        ] {
            if !bucket.active {
                continue;
            }
            for (entry, flag) in bucket.entries.iter().zip(&bucket.matched) {
                if entry.reason.trim().is_empty() {
                    out.push(Diagnostic {
                        file: rel_path.to_path_buf(),
                        line: entry.line,
                        severity: Severity::Warning,
                        message: format!(
                            "exception `{}` under `exceptions.{}` has no reason — add one explaining why this is not a live reference (see `lattice help config`)",
                            entry.reference,
                            lint.key()
                        ),
                        span: Some(entry.key_span),
                    });
                } else if !flag.get() {
                    out.push(Diagnostic {
                        file: rel_path.to_path_buf(),
                        line: entry.line,
                        severity: Severity::Warning,
                        message: format!(
                            "unused exception: `{}` (reason: \"{}\") — no longer in the document. Drop the exception if its removal was intended; restore the reference if it wasn't (see `lattice help config`)",
                            entry.reference, entry.reason
                        ),
                        span: Some(entry.key_span),
                    });
                }
            }
        }
    }

    /// Consume the lookup's tallies into the file's ledger entry (issue 036,
    /// issue 038).
    ///
    /// One literal-exceptions row per file (folding both lint namespaces, since
    /// the ledger keys exceptions by file) carrying the count of distinct entries
    /// that matched, one count-key row per namespace whose residual actually
    /// suppressed (`M == N`), and the per-artifact-name glossary tally (decision
    /// 013) for the workspace-wide artifact rows.
    fn into_suppressions(self, rel_path: &Path) -> FileSuppressions {
        let mut exception_counts = SeverityCounts::default();
        let mut matched_entries = 0;
        let mut count_keys = Vec::new();

        for bucket in [&self.stale_references, &self.bare_paths] {
            exception_counts.add(*bucket.literal_suppressed.borrow());
            matched_entries += bucket.matched.iter().filter(|c| c.get()).count();

            let count_counts = *bucket.count_suppressed.borrow();
            if let Some(count_key) = bucket.count_key
                && !count_counts.is_empty()
            {
                count_keys.push(CountKeySuppression {
                    reason: count_key.reason.clone(),
                    raw: count_key.raw.clone(),
                    counts: count_counts,
                });
            }
        }

        let exceptions = (!exception_counts.is_empty()).then_some(ExceptionSuppression {
            counts: exception_counts,
            matched_entries,
        });

        FileSuppressions {
            file: rel_path.to_path_buf(),
            exceptions,
            count_keys,
            artifacts: self.artifact_suppressed.into_inner(),
        }
    }
}

// ---------------------------------------------------------------------------
// Parser diagnostics
// ---------------------------------------------------------------------------

/// Emit diagnostics that the parser already collected (unclosed fenced code
/// blocks, unclosed HTML tags, unexpected close tags, table cell mismatches,
/// unused/duplicate reference definitions).
fn emit_parser_diagnostics(tree: &Tree, rel_path: &Path, out: &mut Vec<Diagnostic>) {
    let source = tree.source();
    for diag in tree.diagnostics() {
        let line = block::byte_offset_to_line(source, diag.span.start);
        let severity = match diag.level {
            block::DiagnosticLevel::Error => Severity::Error,
            block::DiagnosticLevel::Warning => Severity::Warning,
        };
        out.push(Diagnostic {
            file: rel_path.to_path_buf(),
            line,
            severity,
            message: diag.message.clone(),
            span: Some(diag.span),
        });
    }
}

// ---------------------------------------------------------------------------
// Bare path diagnostics (from tree)
// ---------------------------------------------------------------------------

/// Emit diagnostics for bare `.md` paths detected by the tree's `bare_paths()`
/// scanner.
///
/// A resolving bare path draws the make-it-a-link nudge (gated by `bare_paths`,
/// `Deny` escalating it to an error); a dangling one draws the stale-reference
/// diagnostic instead (gated by `stale_references`, issue 028). The two policies
/// are independent, so a missing reference is still reported when `bare_paths`
/// is `Disabled`, and vice versa.
fn emit_tree_bare_paths(
    tree: &Tree,
    rel_path: &Path,
    config: &Config,
    file_exists: &dyn Fn(&Path) -> bool,
    external_exists: &dyn Fn(&Path) -> bool,
    lookup: &ExceptionLookup,
    out: &mut Vec<Diagnostic>,
) {
    let bare_paths = tree.bare_paths();
    for bare in &bare_paths {
        // An external-namespace reference (`{Name}/…`) is resolved existence-
        // only against its alias directory, never dir/root-joined (issue 030).
        if let Some(stale) = external_is_stale(config, external_exists, &bare.path) {
            if stale {
                route_stale_reference(
                    config.policy.stale_references,
                    rel_path,
                    bare.line,
                    None,
                    &bare.path,
                    lookup,
                    out,
                );
            }
            continue;
        }
        if resolves_under_any_base(rel_path, &bare.path, file_exists) {
            if config.policy.bare_paths == BarePathPolicy::Disabled {
                continue;
            }
            let diag = Diagnostic {
                file: rel_path.to_path_buf(),
                line: bare.line,
                severity: bare_path_severity(config.policy.bare_paths, Severity::Warning),
                message: format!(
                    "bare path `{}`: would moving the target update this mention? if so it's a reference — convert to a markdown link; if not it's an example — except it (see `lattice help config`)",
                    bare.path
                ),
                // `BarePath` carries only a line; fall back to a whole-line range.
                span: None,
            };
            lookup.route(ExceptionLint::BarePaths, &bare.path, diag, out);
        } else {
            route_stale_reference(
                config.policy.stale_references,
                rel_path,
                bare.line,
                None,
                &bare.path,
                lookup,
                out,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Heading diagnostics
// ---------------------------------------------------------------------------

/// Emit heading diagnostics: empty headings and duplicate slugs fire on by
/// default (both are genuine defects per decision 009). Skipped levels and
/// multiple H1 are convention checks, gated behind opt-in policy flags
/// (`config.policy.skipped_heading_level` / `config.policy.multiple_h1`).
fn emit_heading_diagnostics(
    tree: &Tree,
    rel_path: &Path,
    config: &Config,
    out: &mut Vec<Diagnostic>,
) {
    let source = tree.source();
    let mut prev_level: Option<u8> = None;
    let mut h1_count = 0u32;
    // Maps a base slug to the line of its first heading, to flag genuine slug
    // collisions (where `#slug` resolves only to the first heading).
    let mut seen_slugs: HashMap<String, usize> = HashMap::new();

    for node in tree.nodes() {
        let ElementKind::Heading { level } = &node.kind else {
            continue;
        };
        let level = *level;
        let line = block::byte_offset_to_line(source, node.span.start);

        let raw = &source[node.span.start..node.span.end];
        let text = heading_display_text(raw, node.syntax);

        if text.trim().is_empty() {
            // An empty heading produces a degenerate (empty) slug — a defect,
            // so it fires on by default.
            out.push(Diagnostic {
                file: rel_path.to_path_buf(),
                line,
                severity: Severity::Warning,
                message: "empty heading".to_string(),
                span: Some(node.span),
            });
            prev_level = Some(level);
            continue;
        }

        if config.policy.multiple_h1 && level == 1 {
            h1_count += 1;
            if h1_count == 2 {
                out.push(Diagnostic {
                    file: rel_path.to_path_buf(),
                    line,
                    severity: Severity::Warning,
                    message: "multiple H1 headings".to_string(),
                    span: Some(node.span),
                });
            }
        }

        if config.policy.skipped_heading_level
            && let Some(prev) = prev_level
            && level > prev + 1
        {
            out.push(Diagnostic {
                file: rel_path.to_path_buf(),
                line,
                severity: Severity::Warning,
                message: format!("skipped heading level: H{prev} to H{level}"),
                span: Some(node.span),
            });
        }

        prev_level = Some(level);

        // Collision is on the *base* slug, before `block::deduplicate` appends
        // a `-1`/`-2` suffix: two headings whose bases match means `#base`
        // resolves only to the first. When no fragment algorithm is configured
        // default to GitHub — the dominant renderer, and what the old
        // lowercase proxy approximated.
        let slug = match config.policy.fragments {
            Some(FragmentAlgorithm::Github) | None => block::github_slug(&text),
            Some(FragmentAlgorithm::Gitlab) => block::gitlab_slug(&text),
            Some(FragmentAlgorithm::Vscode) => block::vscode_slug(&text),
        };
        if let Some(&first_line) = seen_slugs.get(&slug) {
            out.push(Diagnostic {
                file: rel_path.to_path_buf(),
                line,
                severity: Severity::Warning,
                message: format!(
                    "duplicate heading slug `{slug}` (first at line {first_line}) — `#{slug}` resolves only to the first"
                ),
                span: Some(node.span),
            });
        } else {
            seen_slugs.insert(slug, line);
        }
    }
}

/// Extract display text from a heading node.
fn heading_display_text(raw: &str, syntax: Syntax) -> String {
    if syntax == Syntax::Html {
        return block::extract_html_heading_text(raw);
    }

    let trimmed = raw.trim_start();
    if trimmed.starts_with('#') {
        let first_line = raw.lines().next().unwrap_or("");
        let after_hashes = first_line.trim_start_matches('#');
        let content = after_hashes.trim();
        let content = content.trim_end_matches('#').trim_end();
        if let Some(brace) = content.rfind("{#")
            && content.ends_with('}')
        {
            return content[..brace].trim().to_string();
        }
        content.to_string()
    } else {
        let lines: Vec<&str> = raw.lines().collect();
        if lines.len() > 1 {
            lines[..lines.len() - 1].join(" ").trim().to_string()
        } else {
            raw.trim().to_string()
        }
    }
}

// ---------------------------------------------------------------------------
// Bare path / URL / quoted path / backticked path diagnostics
// ---------------------------------------------------------------------------

/// Resolve the severity of a prose bare-path diagnostic from the policy.
///
/// `base` is the diagnostic's default severity under `Warn`; `Deny` escalates
/// it to an error. `Disabled` is handled by an early return in the caller, so
/// it never reaches here.
const fn bare_path_severity(policy: BarePathPolicy, base: Severity) -> Severity {
    match policy {
        BarePathPolicy::Deny => Severity::Error,
        _ => base,
    }
}

/// Emit the stale-reference diagnostic for a dangling `.md`-shaped reference.
///
/// Closes the missing quadrant (issue 028): a `.md` reference — backtick or
/// bare, `#fragment` already stripped — that resolves to no file is a defect,
/// the mirror of the `link target does not exist` *error*. Both forms share one
/// severity here, governed solely by [`StaleReferencePolicy`]:
/// [`Disabled`](StaleReferencePolicy::Disabled) suppresses it (the make-it-a-
/// link resolve hint, gated by [`BarePathPolicy`], still fires); `Hint`/`Warn`/
/// `Deny` set the severity. `reference` is the displayed reference text `X`.
///
/// The message frames the choice as decision 014's move test (issue 039) — a
/// dangling mention is a reference only if moving the target would force this
/// update — and names the `{repo}/…` external-namespace escape (issue 030,
/// following suggestion 001's self-documenting-message principle), so an agent
/// learns from the diagnostic that a cross-repo reference should be written and
/// aliased rather than left to dangle.
fn build_stale_reference(
    policy: StaleReferencePolicy,
    rel_path: &Path,
    line: usize,
    span: Option<Span>,
    reference: &str,
) -> Option<Diagnostic> {
    let severity = match policy {
        StaleReferencePolicy::Disabled => return None,
        StaleReferencePolicy::Hint => Severity::Hint,
        StaleReferencePolicy::Warn => Severity::Warning,
        StaleReferencePolicy::Deny => Severity::Error,
    };

    Some(Diagnostic {
        file: rel_path.to_path_buf(),
        line,
        severity,
        message: format!(
            "stale reference: `{reference}` — no such markdown file under this root; would moving the target update this mention? if so it's a reference — fix the path (or write it as `{{repo}}/…` and alias `repo` in .lattice.toml if it's in another repo); if not it's an example — except it (see `lattice help config`)"
        ),
        span,
    })
}

/// Route a dangling-reference stale diagnostic through the exception lookup.
///
/// Builds the stale-reference diagnostic for `reference` (a no-op under a
/// `Disabled` policy) and hands it to [`ExceptionLookup::route`], so a literal
/// `stale_references` exception suppresses it, an active count-key buffers it, or
/// it passes through to `out` — the single seam every stale-reference emit site
/// now shares (issue 031, issue 036).
fn route_stale_reference(
    policy: StaleReferencePolicy,
    rel_path: &Path,
    line: usize,
    span: Option<Span>,
    reference: &str,
    lookup: &ExceptionLookup,
    out: &mut Vec<Diagnostic>,
) {
    if let Some(diag) = build_stale_reference(policy, rel_path, line, span, reference) {
        lookup.route(ExceptionLint::StaleReferences, reference, diag, out);
    }
}

/// Emit diagnostics for bare URLs, quoted paths, and backticked paths found in
/// inline-host text — paragraphs and table cells alike, matching the cells the
/// link/edge extractor already walks.
///
/// The bare-URL and make-it-a-link (resolving path) nudges honor the
/// `bare_paths` policy: `Disabled` suppresses them, `Deny` escalates them to
/// errors. A dangling `.md` reference instead draws the stale-reference
/// diagnostic, governed independently by `stale_references` (issue 028), so it
/// fires even when `bare_paths` is `Disabled`.
fn emit_bare_path_diagnostics(
    tree: &Tree,
    rel_path: &Path,
    config: &Config,
    file_exists: &dyn Fn(&Path) -> bool,
    external_exists: &dyn Fn(&Path) -> bool,
    lookup: &ExceptionLookup,
    out: &mut Vec<Diagnostic>,
) {
    let policy = config.policy.bare_paths;
    let stale = config.policy.stale_references;
    let source = tree.source();

    // Scan the same inline hosts the inline pass populates with children
    // (`Paragraph` and `TableCell`), so dark-matter detection covers table
    // cells — the very cells the link/edge extractor already walks. Without
    // the `TableCell` arm, a backticked existing-file path in a cell forms a
    // first-class graph edge once linked yet draws no "make it a link" hint.
    for node in tree.nodes() {
        if !matches!(node.kind, ElementKind::Paragraph | ElementKind::TableCell) {
            continue;
        }

        let excluded: Vec<Span> = node
            .children
            .iter()
            .map(|&child| tree.node(child).span)
            .collect();

        let text = &source[node.span.start..node.span.end];
        let base = node.span.start;

        scan_text_for_paths(
            text,
            base,
            source,
            rel_path,
            policy,
            stale,
            file_exists,
            external_exists,
            config,
            lookup,
            &excluded,
            out,
        );

        // Check InlineCode children for backticked `.md` paths.
        for &child_id in &node.children {
            let child = tree.node(child_id);
            if matches!(child.kind, ElementKind::InlineCode) {
                let code_text = &source[child.span.start..child.span.end];
                // Strip backticks to get inner content.
                let inner = strip_backtick_delimiters(code_text);
                if !looks_like_path(inner) {
                    continue;
                }
                // Resolve the path part only; the `#fragment` is the heading
                // anchor and does not affect file existence.
                let path = split_path_fragment(inner).0;
                let line = block::byte_offset_to_line(source, child.span.start);
                // An external-namespace reference (`{Name}/…`) is resolved
                // existence-only against its alias directory (issue 030).
                if let Some(is_stale) = external_is_stale(config, external_exists, path) {
                    if is_stale {
                        route_stale_reference(
                            stale,
                            rel_path,
                            line,
                            Some(child.span),
                            inner,
                            lookup,
                            out,
                        );
                    }
                    continue;
                }
                if resolves_under_any_base(rel_path, path, file_exists) {
                    if policy != BarePathPolicy::Disabled {
                        let diag = Diagnostic {
                            file: rel_path.to_path_buf(),
                            line,
                            severity: bare_path_severity(policy, Severity::Hint),
                            message: format!(
                                "backticked path `{inner}` refers to an existing file: would moving it update this mention? if so it's a reference — make it a link; if not it's an example — drop the extension (a name) or except it with a reason (see `lattice help config`)"
                            ),
                            span: Some(child.span),
                        };
                        lookup.route(ExceptionLint::BarePaths, inner, diag, out);
                    }
                } else {
                    route_stale_reference(
                        stale,
                        rel_path,
                        line,
                        Some(child.span),
                        inner,
                        lookup,
                        out,
                    );
                }
            }
        }
    }
}

/// Scan a text segment for bare URLs and quoted paths.
#[allow(
    clippy::too_many_arguments,
    reason = "scan context parameters are distinct concerns"
)]
fn scan_text_for_paths(
    text: &str,
    base: usize,
    source: &str,
    rel_path: &Path,
    policy: BarePathPolicy,
    stale: StaleReferencePolicy,
    file_exists: &dyn Fn(&Path) -> bool,
    external_exists: &dyn Fn(&Path) -> bool,
    config: &Config,
    lookup: &ExceptionLookup,
    excluded: &[Span],
    out: &mut Vec<Diagnostic>,
) {
    for (line_offset, line_text) in text.split('\n').enumerate() {
        let line_start = base
            + text
                .match_indices('\n')
                .take(line_offset)
                .last()
                .map_or(0, |(i, _)| i + 1);
        let line_num = block::byte_offset_to_line(source, line_start);

        // Bare URLs are governed solely by `bare_paths`; suppress them when it
        // is `Disabled`. Quoted `.md` paths still scan, because a dangling one
        // draws the stale-reference diagnostic (governed by `stale_references`).
        if policy != BarePathPolicy::Disabled {
            scan_line_for_bare_urls(
                line_text, line_start, line_num, rel_path, policy, excluded, out,
            );
        }
        scan_line_for_quoted_paths(
            line_text,
            line_start,
            line_num,
            rel_path,
            policy,
            stale,
            file_exists,
            external_exists,
            config,
            lookup,
            excluded,
            out,
        );
    }
}

/// Check if a byte position falls inside any excluded span.
fn is_excluded(pos: usize, excluded: &[Span]) -> bool {
    excluded.iter().any(|s| pos >= s.start && pos < s.end)
}

/// Scan a line for bare URLs (`http://` or `https://`) not inside links.
fn scan_line_for_bare_urls(
    line: &str,
    line_start: usize,
    line_num: usize,
    rel_path: &Path,
    policy: BarePathPolicy,
    excluded: &[Span],
    out: &mut Vec<Diagnostic>,
) {
    for prefix in &["https://", "http://"] {
        let mut search_start = 0;
        while let Some(idx) = line[search_start..].find(prefix) {
            let abs_pos = line_start + search_start + idx;
            search_start += idx + prefix.len();

            if is_excluded(abs_pos, excluded) {
                continue;
            }

            let rest = &line[search_start - prefix.len()..];
            let url_end = rest
                .find(|c: char| c.is_whitespace() || c == ')' || c == ']' || c == '>')
                .unwrap_or(rest.len());
            // Exclude trailing sentence punctuation, mirroring GFM autolink:
            // a trailing `.` `,` `;` `:` `!` `?` is not part of the URL.
            let url = rest[..url_end].trim_end_matches(['.', ',', ';', ':', '!', '?']);

            if url.len() <= prefix.len() {
                continue;
            }

            out.push(Diagnostic {
                file: rel_path.to_path_buf(),
                line: line_num,
                severity: bare_path_severity(policy, Severity::Warning),
                message: format!(
                    "bare URL `{url}`: wrap in angle brackets or make a markdown link"
                ),
                // `abs_pos` is the URL start; `url` is already punctuation-trimmed.
                span: Some(Span::new(abs_pos, abs_pos + url.len())),
            });
        }
    }
}

/// Whether byte offset `i` sits at a left boundary for an opening quote: the
/// char immediately before it is whitespace or an opening paren `(`, or `i` is
/// the line start.
///
/// `(` is allowed so a quoted path in a parenthetical (`('docs/x.md')`,
/// `(see 'docs/x.md')`) opens; `[` is deliberately *not* allowed — it is markdown
/// link / reference syntax and would clash. `i` must be a char boundary; the
/// look-behind decodes the preceding char from the string slice (never a raw
/// byte), so it is Unicode-correct and panic-free on multi-byte input.
fn prev_is_boundary(line: &str, i: usize) -> bool {
    line[..i]
        .chars()
        .next_back()
        .is_none_or(|c| c.is_whitespace() || c == '(')
}

/// Whether the char immediately *after* byte offset `i` is alphanumeric.
///
/// `i` must be a char boundary; the look-ahead decodes the following char from
/// the string slice (never a raw byte). The end of the line counts as a
/// non-alphanumeric boundary (no following char).
fn next_is_alphanumeric(line: &str, i: usize) -> bool {
    line[i..].chars().next().is_some_and(char::is_alphanumeric)
}

/// Whether a `'` at byte offset `i` is a quote delimiter rather than an
/// apostrophe.
///
/// `'` doubles as an apostrophe, so it is a delimiter only at a boundary: an
/// *opening* `'` requires whitespace, an opening paren `(`, or the line start
/// immediately before it; a *closing* `'` requires a non-alphanumeric char (or
/// line end) immediately after it. The opening side is the stricter of the two on
/// purpose — a `'` preceded by a letter (`it's`) or most punctuation
/// (`example_'s`) is apostrophe-ish and must not open a span. `(` is the one
/// non-whitespace opener allowed, so a parenthetical path (`('docs/x.md')`) is
/// caught; `[` is excluded because it is markdown link syntax. A closing quote
/// may be followed by punctuation (`'path'.`, `'path')`). `"` is unambiguous and
/// never takes this guard.
fn is_quote_delimiter(line: &str, i: usize, quote: u8, opening: bool) -> bool {
    if quote == b'"' {
        return true;
    }
    if opening {
        prev_is_boundary(line, i)
    } else {
        // `i` is the byte offset of the `'`; the look-ahead inspects the char
        // after it (one byte past, since `'` is ASCII and one byte wide).
        !next_is_alphanumeric(line, i + 1)
    }
}

/// Find the next closing `quote` at or after byte offset `from`, honoring the
/// word-boundary guard so an apostrophe inside a word does not close the span.
///
/// Returns the byte offset of the closing quote within `line`. The search
/// iterates char indices (never raw bytes), so it is char-boundary-safe on
/// multi-byte input.
fn find_closing_quote(line: &str, from: usize, quote: u8) -> Option<usize> {
    let quote_char = char::from(quote);
    line[from..].char_indices().find_map(|(off, c)| {
        let abs = from + off;
        (c == quote_char && is_quote_delimiter(line, abs, quote, false)).then_some(abs)
    })
}

/// Scan a line for quoted paths (`"foo.md"` and `'foo.md'`).
///
/// Both quote styles are first-class and share identical downstream handling
/// (issue 032): the external-namespace resolution (issue 030),
/// make-it-a-link / stale-reference classification, and the exception-lookup
/// suppression (issue 031). `"` pairs unconditionally; `'` is treated as a
/// delimiter only at a word boundary (see [`is_quote_delimiter`]) so an
/// apostrophe is never mistaken for a quote.
#[allow(
    clippy::too_many_arguments,
    reason = "scan context parameters are distinct concerns"
)]
fn scan_line_for_quoted_paths(
    line: &str,
    line_start: usize,
    line_num: usize,
    rel_path: &Path,
    policy: BarePathPolicy,
    stale: StaleReferencePolicy,
    file_exists: &dyn Fn(&Path) -> bool,
    external_exists: &dyn Fn(&Path) -> bool,
    config: &Config,
    lookup: &ExceptionLookup,
    excluded: &[Span],
    out: &mut Vec<Diagnostic>,
) {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let quote = bytes[i];
        // `"` and `'` are both ASCII (one byte), so byte indexing into `bytes`
        // here lands on a char boundary and the slice operations below are safe.
        if (quote == b'"' || quote == b'\'') && is_quote_delimiter(line, i, quote, true) {
            let start = i + 1;
            if let Some(end_abs) = find_closing_quote(line, start, quote) {
                let inner = &line[start..end_abs];
                let abs_pos = line_start + i;

                if !is_excluded(abs_pos, excluded) && looks_like_path(inner) {
                    // Span the whole quoted token, both quotes included.
                    let span = Span::new(abs_pos, line_start + end_abs + 1);
                    // Resolve the path part only; the `#fragment` is the
                    // heading anchor and does not affect file existence.
                    let path = split_path_fragment(inner).0;
                    // An external-namespace reference (`{Name}/…`) resolves
                    // existence-only against its alias directory (issue 030).
                    if let Some(is_stale) = external_is_stale(config, external_exists, path) {
                        if is_stale {
                            route_stale_reference(
                                stale,
                                rel_path,
                                line_num,
                                Some(span),
                                inner,
                                lookup,
                                out,
                            );
                        }
                        i = end_abs + 1;
                        continue;
                    }
                    if resolves_under_any_base(rel_path, path, file_exists) {
                        if policy != BarePathPolicy::Disabled {
                            let q = char::from(quote);
                            let diag = Diagnostic {
                                file: rel_path.to_path_buf(),
                                line: line_num,
                                severity: bare_path_severity(policy, Severity::Hint),
                                message: format!(
                                    "quoted path `{q}{inner}{q}`: would moving the target update this mention? if so it's a reference — make it a markdown link; if not it's an example — except it (see `lattice help config`)"
                                ),
                                span: Some(span),
                            };
                            lookup.route(ExceptionLint::BarePaths, inner, diag, out);
                        }
                    } else {
                        route_stale_reference(
                            stale,
                            rel_path,
                            line_num,
                            Some(span),
                            inner,
                            lookup,
                            out,
                        );
                    }
                }
                i = end_abs + 1;
            } else {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
}

/// Strip backtick delimiters from a code span (e.g. `` `foo` `` → `foo`).
fn strip_backtick_delimiters(s: &str) -> &str {
    let bytes = s.as_bytes();
    let tick_count = bytes.iter().take_while(|&&b| b == b'`').count();
    if tick_count == 0 || s.len() < tick_count * 2 {
        return s;
    }
    let end = s.len() - tick_count;
    &s[tick_count..end]
}

/// Check if a string looks like a markdown path-shaped reference.
///
/// Scoped to the markdown link-target grammar — `path[#fragment]`, ending in
/// `.md` (issue 028). `.md` is the one extension that forms a graph edge, so it
/// is the only path-shape the dark-matter scan nudges into a link; the render-
/// changing nudge on a `.rs`/`.toml`/image path fixes no graph defect (decision
/// 009). Non-`.md` *link existence* validation is separate (in `validation.rs`)
/// and unaffected.
///
/// A protocol-relative reference (`//host/path`) is a URL, not a workspace
/// path — a renderer resolves it against the current scheme and host, never
/// the repository root — so it is never path-shaped. A single leading `/` is
/// root-relative and stays path-shaped (resolved at the workspace root by
/// [`resolves_under_any_base`]).
///
/// Three shapes are not workspace paths at all, so they are rejected outright
/// (no make-it-a-link hint, no stale-reference warning): a `~`-leading token
/// (home-relative, out of the repo, e.g. `~/Projects/Catenary/AGENTS.md`); a
/// token containing `<` or `>` (a placeholder, e.g. `<name>/SKILL.md`); and a
/// token containing `*` (a glob, e.g. `NN_*.md`).
fn looks_like_path(s: &str) -> bool {
    let path = split_path_fragment(s).0;
    !path.is_empty()
        && !path.starts_with("//")
        && !path.starts_with('~')
        && !path.contains(' ')
        && !path.contains('<')
        && !path.contains('>')
        && !path.contains('*')
        && (path.contains('/') || path.contains('.'))
        && Path::new(path).extension().is_some_and(|ext| ext == "md")
}

/// Split a path-shaped token into its path and optional `#fragment`.
///
/// Mirrors the link-target classifier (issue 028): a markdown link can target
/// `path#fragment`, so the dark-matter scan strips the fragment before the
/// `.md` check and existence resolution. The fragment is the heading anchor —
/// once the reference is linked, the existing fragment check validates it; the
/// make-it-a-link hint and the stale-reference warning need only file
/// existence on the path part.
fn split_path_fragment(s: &str) -> (&str, Option<&str>) {
    match s.split_once('#') {
        Some((path, frag)) => (path, Some(frag)),
        None => (s, None),
    }
}

/// Recognize an external-namespace reference of the form `{<identifier>}/rest`.
///
/// Returns `(alias, rest)` — the bare alias name (inside the braces) and the
/// path following the `}/` — when the token is shaped as an external reference
/// (issue 030, decision 010). This is matched **before** the normal dir/root
/// resolution so the literal `{Name}` component is never dir-joined and
/// mis-flagged as a dangling intra-repo path.
///
/// An identifier is one or more of `[A-Za-z0-9_-]`; the braces must wrap a
/// non-empty identifier and be immediately followed by `/` and a non-empty
/// remainder. `{}/x`, `{ }/x`, `{a b}/x`, a bare `{Name}` with no trailing `/`,
/// and `{Name}/` with no remainder are all rejected — they are not external
/// references and fall through to ordinary handling.
fn external_namespace(s: &str) -> Option<(&str, &str)> {
    let after_brace = s.strip_prefix('{')?;
    let close = after_brace.find('}')?;
    let alias = &after_brace[..close];
    let rest = after_brace[close + 1..].strip_prefix('/')?;
    if alias.is_empty()
        || rest.is_empty()
        || !alias
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return None;
    }
    Some((alias, rest))
}

/// The disposition of an external-namespace reference under its alias.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExternalResolution {
    /// The alias is undefined, or its directory is absent — exempt, unverified
    /// (the floor and the CI / partial-checkout guard). No diagnostic.
    Exempt,
    /// The alias directory is present and the referenced file exists under it.
    Valid,
    /// The alias directory is present but the referenced file is missing — a
    /// genuinely broken cross-repo reference.
    Stale,
}

/// Resolve an external-namespace reference to its three-tier disposition.
///
/// `alias`/`rest` come from [`external_namespace`]; `external_exists` `stat`s an
/// absolute filesystem path (decision 010 — existence-only, edge-free: the
/// aliased directory is touched by `stat` alone, never read, parsed, or
/// indexed). The tiers (issue 030):
///
/// 1. alias undefined → [`Exempt`](ExternalResolution::Exempt);
/// 2. alias defined, its directory absent → [`Exempt`](ExternalResolution::Exempt);
/// 3. alias defined, directory present, file present → [`Valid`](ExternalResolution::Valid);
/// 4. alias defined, directory present, file missing → [`Stale`](ExternalResolution::Stale).
fn resolve_external(
    config: &Config,
    external_exists: &dyn Fn(&Path) -> bool,
    alias: &str,
    rest: &str,
) -> ExternalResolution {
    let Some(dir) = config.external.get(alias) else {
        return ExternalResolution::Exempt;
    };
    if !external_exists(dir) {
        return ExternalResolution::Exempt;
    }
    if external_exists(&dir.join(rest)) {
        ExternalResolution::Valid
    } else {
        ExternalResolution::Stale
    }
}

/// Classify a path-shaped reference as external and report whether it should
/// draw the stale-reference diagnostic, or `None` if it is not external.
///
/// Returns `Some(true)` for a defined-alias-but-missing-file external reference
/// (tier 4 — emit stale), `Some(false)` for an exempt or valid external
/// reference (tiers 1–3 — emit nothing), and `None` when the token is not an
/// external reference at all (the caller falls through to ordinary resolution).
/// `reference` is the path part with any `#fragment` already stripped.
fn external_is_stale(
    config: &Config,
    external_exists: &dyn Fn(&Path) -> bool,
    reference: &str,
) -> Option<bool> {
    let (alias, rest) = external_namespace(reference)?;
    Some(resolve_external(config, external_exists, alias, rest) == ExternalResolution::Stale)
}

/// Resolve a path-shaped reference against both candidate bases, normalized,
/// and report whether it exists in the workspace.
///
/// A `.md` reference written in prose can be either **dir-relative** (resolved
/// against the source file's parent, like a markdown link target) or
/// **root-relative** (a full repo-path citation, the way people cite docs in
/// prose). The dark-matter scan accepts either: the reference "resolves" if a
/// file exists under *either* base. The leading-`/` form is unambiguously
/// root-relative, so only that base is tried for it (issue 028).
///
/// Each candidate is lexically normalized (collapsing `.`/`..` by pure path-
/// component arithmetic, no filesystem access) before the existence check, so
/// a `../sibling.md` reference matches the clean workspace key. A candidate
/// that escapes the workspace root after normalization (i.e. begins with `..`)
/// is not a valid workspace path and is not checked.
///
/// This drives both branches of the same decision: the make-it-a-link hint
/// fires when it resolves under either base, and the stale-reference warning
/// fires only when it resolves under neither.
fn resolves_under_any_base(
    file_path: &Path,
    target: &str,
    file_exists: &dyn Fn(&Path) -> bool,
) -> bool {
    // A leading single `/` is unambiguously root-relative (GitHub and web
    // renderers resolve `/foo.md` against the repository root). Try only the
    // root base for it.
    if let Some(rooted) = target.strip_prefix('/') {
        return candidate_exists(Path::new(rooted), file_exists);
    }

    // Dir-relative: against the source file's parent directory.
    let dir_relative = file_path
        .parent()
        .map_or_else(|| std::path::PathBuf::from(target), |dir| dir.join(target));
    if candidate_exists(&dir_relative, file_exists) {
        return true;
    }

    // Root-relative: the target taken as a workspace-relative path.
    candidate_exists(Path::new(target), file_exists)
}

/// Lexically normalize a candidate path and check it against the workspace.
///
/// Returns `false` for a candidate that escapes the workspace root after
/// normalization (its first component is `..`): such a path is not a valid
/// workspace-relative reference, so it is never a resolution.
fn candidate_exists(candidate: &Path, file_exists: &dyn Fn(&Path) -> bool) -> bool {
    let normalized = block::normalize_path(candidate);
    if matches!(
        normalized.components().next(),
        Some(std::path::Component::ParentDir)
    ) {
        return false;
    }
    file_exists(&normalized)
}

// ---------------------------------------------------------------------------
// HTML diagnostics
// ---------------------------------------------------------------------------

/// Emit HTML-specific diagnostics from tree structure.
fn emit_html_diagnostics(tree: &Tree, rel_path: &Path, out: &mut Vec<Diagnostic>) {
    let source = tree.source();
    let mut seen_ids: HashMap<String, usize> = HashMap::new();

    for node in tree.nodes() {
        // Check both structural HTML nodes (Syntax::Html) and opaque HTML blocks.
        let is_html_node = node.syntax == Syntax::Html;
        let is_html_block = matches!(node.kind, ElementKind::HtmlBlock);
        if !is_html_node && !is_html_block {
            continue;
        }

        let raw = &source[node.span.start..node.span.end];
        let line = block::byte_offset_to_line(source, node.span.start);

        // For HtmlBlock, try the first line's tag.
        let first_line = if is_html_block {
            raw.lines().next().unwrap_or("").trim()
        } else {
            raw.trim()
        };
        let Some(tag) = html::tokenize_tag(first_line, node.span.start) else {
            continue;
        };

        match tag {
            html::HtmlTag::Open {
                ref name,
                ref attrs,
                self_closing,
                ..
            } => {
                if self_closing && !html::VOID_ELEMENTS.contains(name.as_str()) {
                    out.push(Diagnostic {
                        file: rel_path.to_path_buf(),
                        line,
                        severity: Severity::Warning,
                        message: format!("self-closing non-void tag `<{name}/>`"),
                        span: Some(node.span),
                    });
                }

                if !html::ALL_ELEMENTS.contains(name.as_str()) {
                    out.push(Diagnostic {
                        file: rel_path.to_path_buf(),
                        line,
                        severity: Severity::Info,
                        message: format!("unknown HTML element `<{name}>`"),
                        span: Some(node.span),
                    });
                }

                for attr in attrs {
                    if let Some(ref val) = attr.value
                        && attr.name == "id"
                        && !val.is_empty()
                    {
                        if let Some(&first_line) = seen_ids.get(val) {
                            out.push(Diagnostic {
                                file: rel_path.to_path_buf(),
                                line,
                                severity: Severity::Error,
                                message: format!(
                                    "duplicate `id` attribute `{val}` (first at line {first_line})",
                                ),
                                span: Some(node.span),
                            });
                        } else {
                            seen_ids.insert(val.clone(), line);
                        }
                    }
                }

                check_required_attrs(name, attrs, rel_path, line, out);
                check_block_in_inline(tree, node, name, rel_path, line, out);
                check_invalid_parent(tree, node, name, rel_path, line, out);
            }
            html::HtmlTag::Close { .. } | html::HtmlTag::Comment { .. } => {}
        }
    }
}

/// Check for markdown-like content inside opaque HTML blocks.
///
/// When HTML block content has no blank lines, markdown syntax won't be
/// parsed — headings, links, and lists render as literal text.
fn check_markdown_in_opaque_html(tree: &Tree, rel_path: &Path, out: &mut Vec<Diagnostic>) {
    let source = tree.source();

    for node in tree.nodes() {
        if !matches!(node.kind, ElementKind::HtmlBlock) {
            continue;
        }

        let raw = &source[node.span.start..node.span.end];
        let lines: Vec<&str> = raw.lines().collect();

        // Skip if there are blank lines (markdown is parsed after blank lines).
        if lines.iter().any(|l| l.trim().is_empty()) {
            continue;
        }

        // Check non-tag lines for markdown syntax.
        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            // Skip the first and last lines (likely HTML tags).
            if i == 0 || (i == lines.len() - 1 && trimmed.starts_with("</")) {
                continue;
            }

            let has_markdown = trimmed.starts_with('#')
                || trimmed.starts_with("- ")
                || trimmed.starts_with("* ")
                || trimmed.contains("](");

            if has_markdown {
                let line_start = node.span.start
                    + raw
                        .match_indices('\n')
                        .take(i)
                        .last()
                        .map_or(0, |(idx, _)| idx + 1);
                let line_num = block::byte_offset_to_line(source, line_start);
                out.push(Diagnostic {
                    file: rel_path.to_path_buf(),
                    line: line_num,
                    severity: Severity::Warning,
                    message:
                        "markdown syntax inside HTML block without blank lines will not be parsed"
                            .to_string(),
                    span: None,
                });
                // One diagnostic per HTML block is enough.
                break;
            }
        }
    }
}

/// Check for missing required attributes on HTML elements.
///
/// An `<a>` carrying `id` or `name` (and no `href`) is a valid explicit
/// anchor *target*, not a link *source* — the standard GFM idiom for a stable
/// `#fragment` (issue 025). Such a tag legitimately omits `href`, so it is not
/// flagged. An `<a>` with neither `href` nor an anchor-defining attribute is
/// still flagged.
fn check_required_attrs(
    tag: &str,
    attrs: &[html::Attribute],
    rel_path: &Path,
    line: usize,
    out: &mut Vec<Diagnostic>,
) {
    // A target `<a>` (bearing `id`/`name`) does not require `href`.
    if tag == "a" && attrs.iter().any(|a| a.name == "id" || a.name == "name") {
        return;
    }

    let required: &[&str] = match tag {
        "img" => &["alt"],
        "a" => &["href"],
        _ => return,
    };

    for &attr_name in required {
        if !attrs.iter().any(|a| a.name == attr_name) {
            out.push(Diagnostic {
                file: rel_path.to_path_buf(),
                line,
                severity: Severity::Warning,
                message: format!("`<{tag}>` missing required attribute `{attr_name}`"),
                // No node in scope here; fall back to a whole-line range.
                span: None,
            });
        }
    }
}

/// Check if a block element is nested inside an inline element context.
fn check_block_in_inline(
    tree: &Tree,
    node: &block::Node,
    tag: &str,
    rel_path: &Path,
    line: usize,
    out: &mut Vec<Diagnostic>,
) {
    if !html::BLOCK_ELEMENTS.contains(tag) {
        return;
    }

    let mut current = node.parent;
    while let Some(pid) = current {
        let parent = tree.node(pid);
        if parent.syntax == Syntax::Html {
            let parent_raw = &tree.source()[parent.span.start..parent.span.end];
            let parent_trimmed = parent_raw.trim();
            if let Some(html::HtmlTag::Open { ref name, .. }) =
                html::tokenize_tag(parent_trimmed, 0)
                && !html::BLOCK_ELEMENTS.contains(name.as_str())
                && !html::VOID_ELEMENTS.contains(name.as_str())
            {
                out.push(Diagnostic {
                    file: rel_path.to_path_buf(),
                    line,
                    severity: Severity::Error,
                    message: format!("block element `<{tag}>` inside inline element `<{name}>`"),
                    span: Some(node.span),
                });
                return;
            }
        }
        current = parent.parent;
    }
}

/// Check if an element has a valid parent (e.g., `<tr>` must be inside `<table>`).
fn check_invalid_parent(
    tree: &Tree,
    node: &block::Node,
    tag: &str,
    rel_path: &Path,
    line: usize,
    out: &mut Vec<Diagnostic>,
) {
    let required_parents: &[&str] = match tag {
        "tr" | "thead" | "tbody" | "tfoot" | "caption" | "colgroup" | "col" => &["table"],
        "td" | "th" => &["table", "tr"],
        "li" => &["ul", "ol", "menu"],
        "summary" => &["details"],
        "option" | "optgroup" => &["select", "datalist"],
        _ => return,
    };

    let mut current = node.parent;
    while let Some(pid) = current {
        let parent = tree.node(pid);
        if parent.syntax == Syntax::Html {
            let parent_raw = &tree.source()[parent.span.start..parent.span.end];
            let parent_trimmed = parent_raw.trim();
            if let Some(html::HtmlTag::Open { ref name, .. }) =
                html::tokenize_tag(parent_trimmed, 0)
                && required_parents.contains(&name.as_str())
            {
                return;
            }
        }
        match &parent.kind {
            ElementKind::Table { .. } if required_parents.contains(&"table") => return,
            ElementKind::List { ordered: true, .. } if required_parents.contains(&"ol") => return,
            ElementKind::List { ordered: false, .. } if required_parents.contains(&"ul") => return,
            ElementKind::Details if required_parents.contains(&"details") => return,
            _ => {}
        }
        current = parent.parent;
    }

    out.push(Diagnostic {
        file: rel_path.to_path_buf(),
        line,
        severity: Severity::Error,
        message: format!(
            "`<{tag}>` requires parent {}",
            required_parents
                .iter()
                .map(|p| format!("`<{p}>`"))
                .collect::<Vec<_>>()
                .join(" or ")
        ),
        span: Some(node.span),
    });
}

// ---------------------------------------------------------------------------
// Code block diagnostics
// ---------------------------------------------------------------------------

/// Emit code block language tag diagnostics.
fn emit_code_block_diagnostics(
    tree: &Tree,
    rel_path: &Path,
    config: &Config,
    out: &mut Vec<Diagnostic>,
) {
    let severity = match config.policy.code_block_language {
        CodeBlockLanguagePolicy::Disabled => return,
        CodeBlockLanguagePolicy::Hint => Severity::Hint,
        CodeBlockLanguagePolicy::Warn => Severity::Warning,
        CodeBlockLanguagePolicy::Deny => Severity::Error,
    };

    let source = tree.source();

    for node in tree.nodes() {
        if !matches!(node.kind, ElementKind::CodeBlock) || node.syntax == Syntax::Html {
            continue;
        }

        let raw = &source[node.span.start..node.span.end];
        let first_line = raw.lines().next().unwrap_or("");
        let trimmed = first_line.trim();

        let is_fenced = trimmed.starts_with("```") || trimmed.starts_with("~~~");
        if !is_fenced {
            continue;
        }

        let fence_end = trimmed
            .find(|c: char| c != '`' && c != '~')
            .unwrap_or(trimmed.len());
        let info = trimmed[fence_end..].trim();

        if info.is_empty() {
            let line = block::byte_offset_to_line(source, node.span.start);
            out.push(Diagnostic {
                file: rel_path.to_path_buf(),
                line,
                severity,
                message:
                    "code block without a language tag — add one (use `text` for non-code output)"
                        .to_string(),
                span: Some(node.span),
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Image diagnostics
// ---------------------------------------------------------------------------

/// Emit diagnostics for images with empty alt text.
///
/// A convention check, not a defect (empty alt is the correct choice for
/// decorative images), so per decision 009 it is gated behind the opt-in
/// `config.policy.image_empty_alt` flag and off by default.
fn emit_image_diagnostics(
    tree: &Tree,
    rel_path: &Path,
    config: &Config,
    out: &mut Vec<Diagnostic>,
) {
    if !config.policy.image_empty_alt {
        return;
    }

    let source = tree.source();

    for node in tree.nodes() {
        if !matches!(
            &node.kind,
            ElementKind::Image { .. } | ElementKind::Video { .. } | ElementKind::Audio { .. }
        ) {
            continue;
        }

        let raw = &source[node.span.start..node.span.end];
        if node.syntax == Syntax::Markdown
            && raw.starts_with("![")
            && let Some(close) = raw.find("](")
        {
            let alt = &raw[2..close];
            if alt.trim().is_empty() {
                let line = block::byte_offset_to_line(source, node.span.start);
                out.push(Diagnostic {
                    file: rel_path.to_path_buf(),
                    line,
                    severity: Severity::Warning,
                    message: "image with empty alt text".to_string(),
                    span: Some(node.span),
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Trailing whitespace diagnostics
// ---------------------------------------------------------------------------

/// Emit diagnostics for invalid trailing whitespace (1 or 3+ trailing spaces).
///
/// Two trailing spaces is a valid hard line break in `CommonMark`.
/// Lines inside fenced code blocks and HTML blocks are excluded.
fn emit_trailing_whitespace_diagnostics(
    source: &str,
    rel_path: &Path,
    tree: &Tree,
    out: &mut Vec<Diagnostic>,
) {
    let excluded: Vec<Span> = tree
        .nodes()
        .iter()
        .filter(|n| {
            matches!(
                n.kind,
                ElementKind::CodeBlock | ElementKind::HtmlBlock | ElementKind::Math
            )
        })
        .map(|n| n.span)
        .collect();

    for (line_idx, line) in source.lines().enumerate() {
        let line_num = line_idx + 1;
        let line_start = source
            .match_indices('\n')
            .take(line_idx)
            .last()
            .map_or(0, |(i, _)| i + 1);

        if excluded
            .iter()
            .any(|s| line_start >= s.start && line_start < s.end)
        {
            continue;
        }

        let trailing = line.len() - line.trim_end_matches(' ').len();
        if trailing == 1 || trailing >= 3 {
            let line_end = line_start + line.len();
            out.push(Diagnostic {
                file: rel_path.to_path_buf(),
                line: line_num,
                severity: Severity::Warning,
                message: format!(
                    "invalid trailing whitespace ({trailing} spaces): use 2 for hard break or 0"
                ),
                // Underline only the offending trailing spaces.
                span: Some(Span::new(line_end - trailing, line_end)),
            });
        }
    }
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
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::block;
    use crate::config::Config;
    use crate::fm;
    use crate::yaml;

    fn diagnose(content: &str) -> Vec<Diagnostic> {
        let config = Config::default();
        diagnose_with_config(content, &config)
    }

    /// Parse `content`'s frontmatter and extract its `exceptions` block (issue
    /// 031). Returns the empty default when there is no frontmatter.
    fn exceptions_of(content: &str) -> Exceptions {
        yaml::parse_frontmatter_block(content)
            .map(|block| fm::extract_exceptions(&block, content))
            .unwrap_or_default()
    }

    fn diagnose_with_config(content: &str, config: &Config) -> Vec<Diagnostic> {
        let fm = yaml::parse_frontmatter_block(content);
        let fm_span = fm.as_ref().map(|b| b.span);
        let tree = block::parse_tree(content, fm_span);
        let rel_path = std::path::Path::new("test.md");
        let exceptions = exceptions_of(content);
        collect(&tree, rel_path, config, &|_| false, &|_| false, &exceptions)
    }

    /// Like [`diagnose_with_config`], but with an explicit external-existence
    /// oracle: `external_present` lists the absolute filesystem paths
    /// (alias directories and their joined files) that `stat` finds, backing the
    /// three-tier `{Name}/…` resolution (issue 030).
    fn diagnose_with_external(
        content: &str,
        config: &Config,
        external_present: &[&str],
    ) -> Vec<Diagnostic> {
        let fm = yaml::parse_frontmatter_block(content);
        let fm_span = fm.as_ref().map(|b| b.span);
        let tree = block::parse_tree(content, fm_span);
        let rel_path = std::path::Path::new("test.md");
        let present: HashSet<&str> = external_present.iter().copied().collect();
        let exceptions = exceptions_of(content);
        collect(
            &tree,
            rel_path,
            config,
            &|_| false,
            &|p| present.contains(p.to_str().unwrap_or("")),
            &exceptions,
        )
    }

    fn diagnose_with_files(content: &str, existing: &[&str]) -> Vec<Diagnostic> {
        diagnose_at_path_with_files("test.md", content, existing)
    }

    /// Like `diagnose_with_files`, but treats the document as living at
    /// `rel_path` (a workspace-relative path), so path-shaped references
    /// resolve relative to that location — and root-relative `/` references
    /// resolve at the workspace root regardless of `rel_path`'s depth.
    /// `existing` lists workspace-relative paths that exist.
    fn diagnose_at_path_with_files(
        rel_path: &str,
        content: &str,
        existing: &[&str],
    ) -> Vec<Diagnostic> {
        let fm = yaml::parse_frontmatter_block(content);
        let fm_span = fm.as_ref().map(|b| b.span);
        let tree = block::parse_tree(content, fm_span);
        let config = Config::default();
        let rel_path = std::path::Path::new(rel_path);
        let existing_set: HashSet<&str> = existing.iter().copied().collect();
        let exceptions = exceptions_of(content);
        collect(
            &tree,
            rel_path,
            &config,
            &|p| existing_set.contains(p.to_str().unwrap_or("")),
            &|_| false,
            &exceptions,
        )
    }

    fn count_matching(diags: &[Diagnostic], severity: Severity, substr: &str) -> usize {
        diags
            .iter()
            .filter(|d| d.severity == severity && d.message.contains(substr))
            .count()
    }

    fn has_matching(diags: &[Diagnostic], severity: Severity, substr: &str) -> bool {
        diags
            .iter()
            .any(|d| d.severity == severity && d.message.contains(substr))
    }

    fn has_any(diags: &[Diagnostic], substr: &str) -> bool {
        diags.iter().any(|d| d.message.contains(substr))
    }

    // -- Parser diagnostics --

    #[test]
    fn unclosed_fenced_code_block() {
        let diags = diagnose("```rust\nfn main() {}\n");
        assert_eq!(
            count_matching(&diags, Severity::Error, "unclosed fenced code block"),
            1,
            "one error for unclosed code block: {diags:?}"
        );
    }

    #[test]
    fn closed_code_block_no_error() {
        let diags = diagnose("```rust\nfn main() {}\n```\n");
        assert!(
            !has_matching(&diags, Severity::Error, "unclosed"),
            "no errors for closed code block: {diags:?}"
        );
    }

    #[test]
    fn unclosed_html_tag() {
        let diags = diagnose("<div>\n\nSome content\n");
        assert_eq!(
            count_matching(&diags, Severity::Error, "unclosed"),
            1,
            "one error for unclosed div: {diags:?}"
        );
    }

    #[test]
    fn unexpected_close_tag() {
        let diags = diagnose("</div>\n");
        assert_eq!(
            count_matching(&diags, Severity::Error, "unexpected closing tag"),
            1,
            "one error for unexpected close: {diags:?}"
        );
    }

    // -- Heading diagnostics --

    #[test]
    fn skipped_heading_level_silent_by_default() {
        // Decision 009: a skipped level is a convention check, not a defect, so
        // it does not fire by default.
        let diags = diagnose("# H1\n\n### H3\n");
        assert!(
            !has_any(&diags, "skipped heading level"),
            "no skipped-level warning by default: {diags:?}"
        );
    }

    #[test]
    fn skipped_heading_level_fires_when_enabled() {
        let mut config = Config::default();
        config.policy.skipped_heading_level = true;
        let diags = diagnose_with_config("# H1\n\n### H3\n", &config);
        assert_eq!(
            count_matching(&diags, Severity::Warning, "skipped heading level"),
            1,
            "one warning for skipped heading when enabled: {diags:?}"
        );
        assert!(
            has_any(&diags, "H1 to H3"),
            "message mentions levels: {diags:?}"
        );
    }

    #[test]
    fn multiple_h1_silent_by_default() {
        // Decision 009: multiple H1 is a convention check, not a defect.
        let diags = diagnose("# First\n\n# Second\n");
        assert!(
            !has_any(&diags, "multiple H1"),
            "no multiple-H1 warning by default: {diags:?}"
        );
    }

    #[test]
    fn multiple_h1_fires_when_enabled() {
        let mut config = Config::default();
        config.policy.multiple_h1 = true;
        let diags = diagnose_with_config("# First\n\n# Second\n", &config);
        assert_eq!(
            count_matching(&diags, Severity::Warning, "multiple H1"),
            1,
            "one warning for multiple H1 when enabled: {diags:?}"
        );
    }

    #[test]
    fn duplicate_heading_exact() {
        // An exact-duplicate heading slugs identically — a real collision that
        // fires on by default.
        let diags = diagnose("## Overview\n\n## Overview\n");
        assert_eq!(
            count_matching(
                &diags,
                Severity::Warning,
                "duplicate heading slug `overview`"
            ),
            1,
            "one warning for exact duplicate heading: {diags:?}"
        );
    }

    #[test]
    fn duplicate_heading_punctuation_collision() {
        // `Hello, World` and `Hello World` both slug to `hello-world`, so
        // `#hello-world` resolves only to the first — a genuine collision the
        // old lowercase proxy missed.
        let diags = diagnose("# Hello, World\n\n# Hello World\n");
        assert_eq!(
            count_matching(
                &diags,
                Severity::Warning,
                "duplicate heading slug `hello-world`"
            ),
            1,
            "one warning for punctuation/spacing slug collision: {diags:?}"
        );
    }

    #[test]
    fn distinct_heading_slugs_no_duplicate() {
        // Two headings with distinct slugs do not collide.
        let diags = diagnose("## Overview\n\n## Details\n");
        assert!(
            !has_any(&diags, "duplicate heading slug"),
            "no duplicate warning for distinct slugs: {diags:?}"
        );
    }

    #[test]
    fn empty_heading() {
        let diags = diagnose("# \n");
        assert_eq!(
            count_matching(&diags, Severity::Warning, "empty heading"),
            1,
            "one warning for empty heading: {diags:?}"
        );
    }

    #[test]
    fn sequential_headings_no_warning() {
        // Even with the opt-in skipped-level check on, sequential headings
        // (H1→H2→H3) draw no warning.
        let mut config = Config::default();
        config.policy.skipped_heading_level = true;
        let diags = diagnose_with_config("# H1\n\n## H2\n\n### H3\n", &config);
        assert!(
            !has_matching(&diags, Severity::Warning, "skipped"),
            "no warnings for sequential headings: {diags:?}"
        );
    }

    // -- Code block language --

    #[test]
    fn code_block_without_language_silent_by_default() {
        // Decision 009: an untagged fence is valid CommonMark with a
        // render-neutral non-fix, so `code_block_language` defaults to
        // Disabled and produces no diagnostic by default.
        let diags = diagnose("```\ncode\n```\n");
        assert!(
            !has_any(&diags, "language tag"),
            "no missing-language diagnostic by default: {diags:?}"
        );
    }

    #[test]
    fn code_block_without_language_fires_when_enabled() {
        // When opted in to `hint`, the untagged fence draws a hint that names
        // the `text` escape hatch (issue 020). `warn`/`deny` are covered by
        // their own tests below.
        for (policy, severity) in [
            (CodeBlockLanguagePolicy::Hint, Severity::Hint),
            (CodeBlockLanguagePolicy::Warn, Severity::Warning),
            (CodeBlockLanguagePolicy::Deny, Severity::Error),
        ] {
            let mut config = Config::default();
            config.policy.code_block_language = policy;
            let diags = diagnose_with_config("```\ncode\n```\n", &config);
            assert_eq!(
                count_matching(&diags, severity, "without a language tag"),
                1,
                "one {policy:?} diagnostic for missing language: {diags:?}"
            );
        }

        // The hint variant must name the `text` escape hatch so authors of
        // non-code blocks (output, diagrams, trees) tag them deliberately
        // instead of guessing a language.
        let mut config = Config::default();
        config.policy.code_block_language = CodeBlockLanguagePolicy::Hint;
        let diags = diagnose_with_config("```\ncode\n```\n", &config);
        assert!(
            has_matching(&diags, Severity::Hint, "`text`"),
            "missing-language hint should point at the `text` escape hatch: {diags:?}"
        );
    }

    #[test]
    fn code_block_with_language_no_diagnostic() {
        let diags = diagnose("```rust\ncode\n```\n");
        assert!(
            !has_any(&diags, "language tag"),
            "no hint for code block with language: {diags:?}"
        );
    }

    // -- Image --

    #[test]
    fn image_empty_alt_text_silent_by_default() {
        // Decision 009: empty alt text is a convention check, not a defect (it
        // is the correct choice for decorative images), so it is off by
        // default.
        let diags = diagnose("![](image.png)\n");
        assert!(
            !has_any(&diags, "empty alt text"),
            "no empty-alt warning by default: {diags:?}"
        );
    }

    #[test]
    fn image_empty_alt_text_fires_when_enabled() {
        let mut config = Config::default();
        config.policy.image_empty_alt = true;
        let diags = diagnose_with_config("![](image.png)\n", &config);
        assert_eq!(
            count_matching(&diags, Severity::Warning, "empty alt text"),
            1,
            "one warning for empty alt when enabled: {diags:?}"
        );
    }

    #[test]
    fn image_with_alt_text_no_diagnostic() {
        // Even with the opt-in flag on, a non-empty alt draws no warning.
        let mut config = Config::default();
        config.policy.image_empty_alt = true;
        let diags = diagnose_with_config("![a logo](image.png)\n", &config);
        assert!(
            !has_any(&diags, "empty alt text"),
            "no warning for image with alt: {diags:?}"
        );
    }

    // -- Anchor `<a>` href requirement (issue 025) --

    #[test]
    fn anchor_with_id_no_href_no_warning() {
        // `<a id="a"></a>` is an explicit anchor target, not a link source;
        // it legitimately carries no `href` and must not be flagged.
        let diags = diagnose("<a id=\"a\"></a>\n");
        assert!(
            !has_any(&diags, "missing required attribute `href`"),
            "no missing-href warning for an `<a id>` anchor target: {diags:?}"
        );
    }

    #[test]
    fn anchor_with_name_no_href_no_warning() {
        // `<a name="a">` is the legacy anchor-target form — also exempt.
        let diags = diagnose("<a name=\"a\"></a>\n");
        assert!(
            !has_any(&diags, "missing required attribute `href`"),
            "no missing-href warning for an `<a name>` anchor target: {diags:?}"
        );
    }

    #[test]
    fn anchor_without_href_or_anchor_attr_still_warns() {
        // The relaxation must not over-suppress: an `<a>` with neither `href`
        // nor an anchor-defining attribute is still flagged.
        let diags = diagnose("<a class=\"x\"></a>\n");
        assert_eq!(
            count_matching(
                &diags,
                Severity::Warning,
                "missing required attribute `href`"
            ),
            1,
            "an `<a>` with no href and no id/name still warns: {diags:?}"
        );
    }

    #[test]
    fn anchor_with_href_no_warning() {
        // A normal linking `<a href>` is unaffected by the relaxation.
        let diags = diagnose("<a href=\"https://example.com\">x</a>\n");
        assert!(
            !has_any(&diags, "missing required attribute `href`"),
            "no missing-href warning for a normal linking `<a href>`: {diags:?}"
        );
    }

    // -- Trailing whitespace --

    #[test]
    fn single_trailing_space() {
        let diags = diagnose("hello \n");
        assert_eq!(
            count_matching(&diags, Severity::Warning, "trailing whitespace"),
            1,
            "one warning for 1 trailing space: {diags:?}"
        );
    }

    #[test]
    fn two_trailing_spaces_ok() {
        let diags = diagnose("hello  \n");
        assert!(
            !has_any(&diags, "trailing whitespace"),
            "no warning for 2 trailing spaces: {diags:?}"
        );
    }

    #[test]
    fn three_trailing_spaces() {
        let diags = diagnose("hello   \n");
        assert_eq!(
            count_matching(&diags, Severity::Warning, "trailing whitespace"),
            1,
            "one warning for 3 trailing spaces: {diags:?}"
        );
    }

    #[test]
    fn trailing_whitespace_in_code_block_excluded() {
        let diags = diagnose("```\nhello   \n```\n");
        assert!(
            !has_any(&diags, "trailing whitespace"),
            "no warning for trailing spaces inside code: {diags:?}"
        );
    }

    // -- Bare URL --

    #[test]
    fn bare_url_in_paragraph() {
        let diags = diagnose("Visit https://example.com for info.\n");
        assert_eq!(
            count_matching(&diags, Severity::Warning, "bare URL"),
            1,
            "one warning for bare URL: {diags:?}"
        );
    }

    // Regression: issue 012 — a URL written mid-sentence had its trailing
    // punctuation folded into the reported URL (`https://example.com,`). GFM
    // autolink excludes trailing `.,;:!?`, and so must the bare-URL hint.
    #[test]
    fn bare_url_trailing_punctuation_excluded() {
        let diags = diagnose("See https://example.com, then continue.\n");
        assert!(
            has_matching(&diags, Severity::Warning, "bare URL `https://example.com`"),
            "trailing comma excluded from the reported URL: {diags:?}"
        );
        assert!(
            !has_any(&diags, "https://example.com,"),
            "reported URL must not include the trailing comma: {diags:?}"
        );
    }

    // Regression: issue 006 — a bare URL past the midpoint of its line drove
    // `scan_line_for_bare_urls` to slice at `2*idx`, an out-of-bounds byte
    // index that aborted the LSP. It must warn, not panic.
    #[test]
    fn bare_url_past_line_midpoint_no_panic() {
        let diags =
            diagnose("A long line of filler text before the link, then https://example.com\n");
        assert_eq!(
            count_matching(&diags, Severity::Warning, "bare URL"),
            1,
            "one warning for bare URL past line midpoint: {diags:?}"
        );
    }

    // Issue 011: producers must carry a precise byte span, not just a line.
    #[test]
    fn bare_url_diagnostic_has_precise_span() {
        let content = "Visit https://example.com for info.\n";
        let diags = diagnose(content);
        let d = diags
            .iter()
            .find(|d| d.message.contains("bare URL"))
            .expect("a bare URL diagnostic");
        let span = d.span.expect("bare URL diagnostic carries a span");
        assert_eq!(
            &content[span.start..span.end],
            "https://example.com",
            "span underlines exactly the URL: {diags:?}"
        );
    }

    #[test]
    fn trailing_whitespace_diagnostic_spans_the_spaces() {
        // Three trailing spaces after "hello"; the span must cover only them.
        let content = "hello   \nworld\n";
        let diags = diagnose(content);
        let d = diags
            .iter()
            .find(|d| d.message.contains("trailing whitespace"))
            .expect("a trailing whitespace diagnostic");
        let span = d
            .span
            .expect("trailing whitespace diagnostic carries a span");
        assert_eq!(
            &content[span.start..span.end],
            "   ",
            "span covers exactly the three trailing spaces: {diags:?}"
        );
    }

    // -- Error recovery --

    #[test]
    fn unclosed_html_no_cascade_to_valid_content() {
        let diags = diagnose("<div>\n\n# Valid Heading\n\nSome paragraph.\n");
        let errors: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();
        assert_eq!(errors.len(), 1, "only one error, no cascading: {diags:?}");
        assert!(
            errors[0].message.contains("unclosed"),
            "the error is about unclosed tag: {}",
            errors[0].message
        );
    }

    // -- Quoted path --

    #[test]
    fn quoted_path_with_existing_file() {
        let diags = diagnose_with_files("See \"other.md\" for details.\n", &["other.md"]);
        assert_eq!(
            count_matching(&diags, Severity::Hint, "quoted path"),
            1,
            "one hint for quoted path: {diags:?}"
        );
    }

    // -- Backticked path --

    #[test]
    fn backticked_path_with_existing_file() {
        let diags = diagnose_with_files("See `other.md` for details.\n", &["other.md"]);
        assert_eq!(
            count_matching(&diags, Severity::Hint, "backticked path"),
            1,
            "one hint for backticked path: {diags:?}"
        );
        // The hint teaches both honest resolutions (suggestion 001): make it a
        // link if it's a reference, or drop the extension if it's only a name.
        assert!(
            has_matching(&diags, Severity::Hint, "make it a link"),
            "the hint offers the make-it-a-link resolution: {diags:?}"
        );
        assert!(
            has_matching(&diags, Severity::Hint, "drop the extension"),
            "the hint offers the drop-the-extension resolution for a name: {diags:?}"
        );
    }

    #[test]
    fn backticked_path_no_file() {
        // A dangling backtick `.md` draws no make-it-a-link hint, but does
        // draw the stale-reference warning (issue 028, default `warn`).
        let diags = diagnose("See `other.md` for details.\n");
        assert!(
            !has_any(&diags, "backticked path"),
            "no make-it-a-link hint when file doesn't exist: {diags:?}"
        );
        assert_eq!(
            count_matching(&diags, Severity::Warning, "stale reference"),
            1,
            "a dangling backtick `.md` draws the stale-reference warning: {diags:?}"
        );
    }

    // -- Path-shaped reference detection: `.md`-scope, fragments, missing
    //    quadrant (issue 028) --

    #[test]
    fn quoted_path_no_file_is_stale_reference() {
        // The quoted form mirrors the backtick form: a dangling `.md` draws
        // the stale-reference warning, not the make-it-a-link hint.
        let diags = diagnose("See \"other.md\" for details.\n");
        assert!(
            !has_any(&diags, "quoted path"),
            "no make-it-a-link hint for a dangling quoted path: {diags:?}"
        );
        assert_eq!(
            count_matching(&diags, Severity::Warning, "stale reference"),
            1,
            "a dangling quoted `.md` draws the stale-reference warning: {diags:?}"
        );
    }

    // -- Quoted dir-bearing path: single owner (issue 032) --

    #[test]
    fn quoted_dir_path_dangling_emits_one_stale() {
        // A quoted token carrying a directory component is seen by the quoted
        // scanner and — before issue 032 — also by the bare-path scanner, which
        // trimmed the surrounding quotes. The bare scanner now leaves quoted
        // content to its single owner, so a dangling `"docs/gone.md"` draws
        // exactly one stale-reference diagnostic.
        let diags = diagnose("See \"docs/gone.md\" for details.\n");
        assert_eq!(
            count_matching(&diags, Severity::Warning, "stale reference"),
            1,
            "a dangling quoted dir-bearing `.md` is stale exactly once: {diags:?}"
        );
    }

    #[test]
    fn quoted_external_dir_path_dangling_emits_one_stale() {
        // The `{Name}/…` quoted form (present alias dir, missing file) is the
        // external-namespace variant of the same shape: exactly one stale
        // diagnostic, not two.
        let config = config_with_catenary_alias();
        let diags = diagnose_with_external(
            "See \"{Catenary}/gone.md\" for details.\n",
            &config,
            &["/ext/Catenary"],
        );
        assert_eq!(
            count_matching(&diags, Severity::Warning, "stale reference"),
            1,
            "a quoted external dir-bearing `.md` is stale exactly once: {diags:?}"
        );
    }

    #[test]
    fn quoted_dir_path_resolving_emits_one_make_it_a_link() {
        // The other double-emit variant: a quoted dir-bearing token that
        // *resolves* drew both the quoted scanner's make-it-a-link hint and the
        // bare scanner's "convert to a markdown link" nudge. With quoted spans
        // single-owned, only the quoted-path hint fires, and no bare-path nudge.
        let diags = diagnose_with_files("See \"docs/other.md\" for details.\n", &["docs/other.md"]);
        assert_eq!(
            count_matching(&diags, Severity::Hint, "quoted path"),
            1,
            "a resolving quoted dir-bearing path draws one make-it-a-link hint: {diags:?}"
        );
        assert!(
            !has_any(&diags, "convert to a markdown link"),
            "the bare-path nudge does not also fire on quoted content: {diags:?}"
        );
    }

    #[test]
    fn two_distinct_quoted_dir_paths_each_emit_once() {
        // Single-ownership must not over-suppress: two *different* quoted
        // dir-bearing dangling paths on one line still each emit once.
        let diags = diagnose("See \"docs/a.md\" and \"docs/b.md\" for details.\n");
        assert_eq!(
            count_matching(&diags, Severity::Warning, "stale reference"),
            2,
            "two distinct quoted dir-bearing paths each emit one stale: {diags:?}"
        );
    }

    // -- Single-quoted paths: first-class, identical to double quotes
    //    (issue 032, Option C) --

    #[test]
    fn single_quoted_dangling_path_emits_one_stale() {
        // A single-quoted dangling `.md` is a first-class quoted path: exactly
        // one stale-reference diagnostic, mirroring the double-quote form.
        let diags = diagnose("See 'docs/gone.md' for details.\n");
        assert_eq!(
            count_matching(&diags, Severity::Warning, "stale reference"),
            1,
            "a dangling single-quoted `.md` is stale exactly once: {diags:?}"
        );
    }

    #[test]
    fn single_quoted_resolving_path_emits_one_make_it_a_link() {
        // A single-quoted resolving path draws exactly one make-it-a-link hint,
        // with the message reflecting the actual quote character.
        let diags = diagnose_with_files("See 'docs/other.md' for details.\n", &["docs/other.md"]);
        assert_eq!(
            count_matching(&diags, Severity::Hint, "quoted path"),
            1,
            "a resolving single-quoted path draws one make-it-a-link hint: {diags:?}"
        );
        assert!(
            has_any(&diags, "`'docs/other.md'`"),
            "the hint reflects the single-quote character: {diags:?}"
        );
    }

    #[test]
    fn double_quoted_dangling_path_still_one_stale_no_regression() {
        // The double-quote form is unchanged by adding single-quote support.
        let diags = diagnose("See \"docs/gone.md\" for details.\n");
        assert_eq!(
            count_matching(&diags, Severity::Warning, "stale reference"),
            1,
            "a dangling double-quoted `.md` is still stale exactly once: {diags:?}"
        );
    }

    #[test]
    fn apostrophe_not_treated_as_quote() {
        // A `'` flanked by alphanumerics is an apostrophe, never a quote
        // delimiter: contractions, possessives, and `'n'` draw no path
        // diagnostic. (There is no `.md`-shaped path here, but the guard must
        // also not pair the apostrophes into a span at all.)
        for content in ["it's a test\n", "the dogs' bowls\n", "rock 'n' roll\n"] {
            let diags = diagnose(content);
            assert!(
                !has_any(&diags, "quoted path") && !has_any(&diags, "stale reference"),
                "an apostrophe is not a quote delimiter in {content:?}: {diags:?}"
            );
        }
    }

    #[test]
    fn opening_single_quote_requires_whitespace_before() {
        // The opening `'` must have whitespace (or line start, or `(`) before
        // it, not merely a non-alphanumeric char — `_`/`-` are non-alphanumeric
        // but not boundaries. In `set value_'docs/gone.md' now`, the `_`-preceded
        // `'` is apostrophe-ish (cf. `example_'s`) and must not open a span, even
        // though the bytes after it look like a path.
        let glued = diagnose("set value_'docs/gone.md' now\n");
        assert!(
            !has_any(&glued, "stale reference") && !has_any(&glued, "quoted path"),
            "a non-whitespace-preceded `'` must not open a quoted span: {glued:?}"
        );
        // The user's pathological prose: underscores and possessives make
        // several apostrophe-`'`s; none open.
        let prose = diagnose("the function example_'s parameters' types are typed\n");
        assert!(
            !has_any(&prose, "stale reference") && !has_any(&prose, "quoted path"),
            "apostrophe-heavy prose opens no quoted span: {prose:?}"
        );
    }

    #[test]
    fn paren_opens_single_quote_but_bracket_does_not() {
        // `(` is allowed before an opening `'` so a quoted path in a
        // parenthetical is caught; `[` is not, because it is markdown link
        // syntax and would clash.
        let paren = diagnose("see the example ('docs/gone.md') here\n");
        assert_eq!(
            count_matching(&paren, Severity::Warning, "stale reference"),
            1,
            "a `(`-preceded `'` opens a quoted path: {paren:?}"
        );
        let bracket = diagnose("see the example ['docs/gone.md'] here\n");
        assert!(
            !has_any(&bracket, "stale reference") && !has_any(&bracket, "quoted path"),
            "a `[`-preceded `'` does not open (markdown link clash): {bracket:?}"
        );
    }

    #[test]
    fn contraction_before_single_quoted_path_is_caught() {
        // The whole reason the closing-search must also skip apostrophe
        // candidates: in `it's in 'docs/gone.md' today`, the apostrophe of
        // `it's` must not be consumed as an opening quote, and the real
        // single-quoted path is still found exactly once.
        let diags = diagnose("it's in 'docs/gone.md' today\n");
        assert_eq!(
            count_matching(&diags, Severity::Warning, "stale reference"),
            1,
            "a contraction before a single-quoted path does not hide it: {diags:?}"
        );
    }

    #[test]
    fn multibyte_before_single_quote_is_caught_no_panic() {
        // A multi-byte char immediately before the opening `'` must not panic
        // (the look-behind decodes a char, never a raw byte) and the path is
        // still caught: `é` is alphanumeric, but a space separates it from the
        // quote, so the quote is at a word boundary.
        let diags = diagnose("café 'docs/gone.md'\n");
        assert_eq!(
            count_matching(&diags, Severity::Warning, "stale reference"),
            1,
            "a multibyte char before a single-quoted path: caught, no panic: {diags:?}"
        );
    }

    #[test]
    fn single_quoted_external_dir_path_dangling_emits_one_stale() {
        // The `{Name}/…` external form in single quotes (defined alias, missing
        // file) is stale exactly once.
        let config = config_with_catenary_alias();
        let diags = diagnose_with_external(
            "See '{Catenary}/gone.md' for details.\n",
            &config,
            &["/ext/Catenary"],
        );
        assert_eq!(
            count_matching(&diags, Severity::Warning, "stale reference"),
            1,
            "a single-quoted external dir-bearing `.md` is stale exactly once: {diags:?}"
        );
    }

    #[test]
    fn two_distinct_single_quoted_paths_each_emit_once() {
        // Single-quote support must not over-suppress: two *different*
        // single-quoted dangling paths on one line still each emit once.
        let diags = diagnose("See 'docs/a.md' and 'docs/b.md' for details.\n");
        assert_eq!(
            count_matching(&diags, Severity::Warning, "stale reference"),
            2,
            "two distinct single-quoted dir-bearing paths each emit one stale: {diags:?}"
        );
    }

    #[test]
    fn mixed_quote_styles_with_multibyte_each_emit_once() {
        // A double- and a single-quoted path on one line, with multibyte
        // content, each emit exactly one stale — no double-emit, no panic.
        let diags = diagnose("See \"docs/other.md\" and 'docs/外部.md' for café details.\n");
        assert_eq!(
            count_matching(&diags, Severity::Warning, "stale reference"),
            2,
            "one double- and one single-quoted path each emit one stale: {diags:?}"
        );
    }

    #[test]
    fn bare_path_no_file_is_stale_reference() {
        // The bare (unbackticked, unquoted) form, with a directory component,
        // draws the stale-reference warning when its target is missing.
        let diags = diagnose("See docs/other.md for details.\n");
        assert!(
            !has_any(&diags, "convert to a markdown link"),
            "no make-it-a-link nudge for a dangling bare path: {diags:?}"
        );
        assert_eq!(
            count_matching(&diags, Severity::Warning, "stale reference"),
            1,
            "a dangling bare `.md` draws the stale-reference warning: {diags:?}"
        );
    }

    #[test]
    fn bare_path_existing_file_is_make_it_a_link() {
        // A resolving bare path keeps the make-it-a-link nudge and draws no
        // stale-reference warning.
        let diags = diagnose_with_files("See docs/other.md for details.\n", &["docs/other.md"]);
        assert_eq!(
            count_matching(&diags, Severity::Warning, "convert to a markdown link"),
            1,
            "a resolving bare path keeps the make-it-a-link nudge: {diags:?}"
        );
        assert!(
            !has_any(&diags, "stale reference"),
            "a resolving bare path draws no stale-reference warning: {diags:?}"
        );
    }

    #[test]
    fn backticked_fragment_existing_file_make_it_a_link() {
        // `` `foo.md#section` `` with `foo.md` present: the fragment is
        // stripped and the make-it-a-link hint fires on the file.
        let diags = diagnose_with_files("See `other.md#intro` for details.\n", &["other.md"]);
        assert_eq!(
            count_matching(&diags, Severity::Hint, "backticked path"),
            1,
            "an anchored backtick path resolves the file (fragment stripped): {diags:?}"
        );
    }

    #[test]
    fn backticked_fragment_missing_file_is_stale_reference() {
        // `` `foo.md#section` `` with `foo.md` absent draws the stale-reference
        // warning (fragment stripped, path part resolved).
        let diags = diagnose("See `other.md#intro` for details.\n");
        assert_eq!(
            count_matching(&diags, Severity::Warning, "stale reference"),
            1,
            "an anchored backtick to a missing file is stale: {diags:?}"
        );
    }

    #[test]
    fn quoted_fragment_existing_file_make_it_a_link() {
        let diags = diagnose_with_files("See \"other.md#intro\" for details.\n", &["other.md"]);
        assert_eq!(
            count_matching(&diags, Severity::Hint, "quoted path"),
            1,
            "an anchored quoted path resolves the file (fragment stripped): {diags:?}"
        );
    }

    #[test]
    fn non_md_extensions_draw_no_dark_matter() {
        // `.rs`/`.toml`/image paths are not `.md`, so they form no graph edge:
        // neither a resolving nor a dangling one draws any dark-matter
        // diagnostic (decision 009). Link-existence validation is separate and
        // untouched (see `validation.rs`).
        for path in ["src/main.rs", "Cargo.toml", "docs/logo.png"] {
            let backtick = format!("See `{path}` for details.\n");
            let resolving = diagnose_with_files(&backtick, &[path]);
            let dangling = diagnose(&backtick);
            for diags in [&resolving, &dangling] {
                assert!(
                    !has_any(diags, "backticked path")
                        && !has_any(diags, "stale reference")
                        && !has_any(diags, "convert to a markdown link"),
                    "non-`.md` path `{path}` draws no dark-matter diagnostic: {diags:?}"
                );
            }
        }
    }

    #[test]
    fn stem_without_extension_is_silent() {
        // A stem (`README`, `docs/README`) has no recognized extension, so it
        // is plain prose — out of the graph, no diagnostic either way.
        for stem in ["README", "docs/README"] {
            let diags = diagnose_with_files(&format!("See `{stem}` for details.\n"), &[stem]);
            assert!(
                !has_any(&diags, "backticked path")
                    && !has_any(&diags, "stale reference")
                    && !has_any(&diags, "convert to a markdown link"),
                "a bare stem `{stem}` is silent: {diags:?}"
            );
        }
    }

    #[test]
    fn file_line_syntax_is_silent() {
        // `foo.md:102` is editor `file:line` syntax, not a markdown reference
        // form — it is never recognized.
        let diags = diagnose("See docs/foo.md:102 for details.\n");
        assert!(
            !has_any(&diags, "stale reference")
                && !has_any(&diags, "convert to a markdown link")
                && !has_any(&diags, "backticked path"),
            "`file:line` syntax is not a reference form: {diags:?}"
        );
    }

    #[test]
    fn root_relative_existing_file_make_it_a_link() {
        // `/README.md` from a nested file with `<root>/README.md` present draws
        // the make-it-a-link hint (resolved at the workspace root).
        let diags = diagnose_at_path_with_files("a/b/c.md", "See `/README.md`.\n", &["README.md"]);
        assert_eq!(
            count_matching(&diags, Severity::Hint, "backticked path"),
            1,
            "root-relative `.md` resolves at the workspace root: {diags:?}"
        );
        assert!(
            !has_any(&diags, "stale reference"),
            "a resolving root-relative path draws no stale-reference: {diags:?}"
        );
    }

    // -- stale_references policy (issue 028) --

    fn diagnose_with_stale_policy(
        content: &str,
        existing: &[&str],
        stale: StaleReferencePolicy,
    ) -> Vec<Diagnostic> {
        let fm = yaml::parse_frontmatter_block(content);
        let fm_span = fm.as_ref().map(|b| b.span);
        let tree = block::parse_tree(content, fm_span);
        let mut config = Config::default();
        config.policy.stale_references = stale;
        let rel_path = std::path::Path::new("test.md");
        let existing_set: HashSet<&str> = existing.iter().copied().collect();
        let exceptions = exceptions_of(content);
        collect(
            &tree,
            rel_path,
            &config,
            &|p| existing_set.contains(p.to_str().unwrap_or("")),
            &|_| false,
            &exceptions,
        )
    }

    #[test]
    fn stale_references_disabled_silences_only_the_stale_warning() {
        // `disabled` silences the stale-reference warning but leaves the
        // make-it-a-link hint intact for resolving references.
        let dangling =
            diagnose_with_stale_policy("See `gone.md`.\n", &[], StaleReferencePolicy::Disabled);
        assert!(
            !has_any(&dangling, "stale reference"),
            "disabled silences the stale-reference warning: {dangling:?}"
        );

        let resolving = diagnose_with_stale_policy(
            "See `other.md`.\n",
            &["other.md"],
            StaleReferencePolicy::Disabled,
        );
        assert_eq!(
            count_matching(&resolving, Severity::Hint, "backticked path"),
            1,
            "disabling stale_references leaves the make-it-a-link hint intact: {resolving:?}"
        );
    }

    #[test]
    fn stale_references_deny_is_error() {
        let diags = diagnose_with_stale_policy("See `gone.md`.\n", &[], StaleReferencePolicy::Deny);
        assert_eq!(
            count_matching(&diags, Severity::Error, "stale reference"),
            1,
            "deny escalates the stale-reference to an error: {diags:?}"
        );
    }

    #[test]
    fn stale_references_hint_is_hint() {
        let diags = diagnose_with_stale_policy("See `gone.md`.\n", &[], StaleReferencePolicy::Hint);
        assert_eq!(
            count_matching(&diags, Severity::Hint, "stale reference"),
            1,
            "hint downgrades the stale-reference to a hint: {diags:?}"
        );
    }

    #[test]
    fn stale_reference_fires_even_when_bare_paths_disabled() {
        // The two policies are decoupled: disabling `bare_paths` (the
        // make-it-a-link nudge) must not silence the stale-reference warning.
        let fm = yaml::parse_frontmatter_block("See `gone.md`.\n");
        let fm_span = fm.as_ref().map(|b| b.span);
        let tree = block::parse_tree("See `gone.md`.\n", fm_span);
        let mut config = Config::default();
        config.policy.bare_paths = BarePathPolicy::Disabled;
        let rel_path = std::path::Path::new("test.md");
        let diags = collect(
            &tree,
            rel_path,
            &config,
            &|_| false,
            &|_| false,
            &Exceptions::default(),
        );
        assert_eq!(
            count_matching(&diags, Severity::Warning, "stale reference"),
            1,
            "stale_references is independent of bare_paths: {diags:?}"
        );
    }

    // -- Root-relative `/` dark-matter resolution (issue 028) --

    #[test]
    fn backticked_root_relative_path_resolves_at_workspace_root() {
        // From a nested file, `` `/README.md` `` resolves at the workspace
        // root, so an existing `<root>/README.md` draws the make-it-a-link
        // hint — not silence (the path was previously read as filesystem
        // absolute and missed).
        let diags = diagnose_at_path_with_files(
            "a/b/c.md",
            "See `/README.md` for details.\n",
            &["README.md"],
        );
        assert_eq!(
            count_matching(&diags, Severity::Hint, "backticked path"),
            1,
            "root-relative backticked path resolves at the workspace root: {diags:?}"
        );
    }

    #[test]
    fn backticked_root_relative_resolution_independent_of_depth() {
        // The same `/README.md` reference resolves identically from the root
        // and from a deep subdirectory.
        let root = diagnose_at_path_with_files("root.md", "See `/README.md`.\n", &["README.md"]);
        let deep =
            diagnose_at_path_with_files("a/b/c/d/deep.md", "See `/README.md`.\n", &["README.md"]);
        assert_eq!(
            count_matching(&root, Severity::Hint, "backticked path"),
            count_matching(&deep, Severity::Hint, "backticked path"),
            "root-relative resolution is depth-independent: root={root:?} deep={deep:?}"
        );
        assert_eq!(
            count_matching(&deep, Severity::Hint, "backticked path"),
            1,
            "the deep reference still resolves at the workspace root: {deep:?}"
        );
    }

    #[test]
    fn backticked_root_relative_missing_file_no_hint() {
        // A root-relative reference whose target does not exist draws no
        // make-it-a-link hint, but does draw the stale-reference warning
        // (issue 028, the missing-quadrant default).
        let diags = diagnose_at_path_with_files(
            "a/b/c.md",
            "See `/nope.md` for details.\n",
            &["README.md"],
        );
        assert!(
            !has_any(&diags, "backticked path"),
            "no make-it-a-link hint for a missing root-relative target: {diags:?}"
        );
        assert_eq!(
            count_matching(&diags, Severity::Warning, "stale reference"),
            1,
            "a missing root-relative `.md` draws the stale-reference warning: {diags:?}"
        );
    }

    #[test]
    fn protocol_relative_backticked_path_not_treated_as_workspace_path() {
        // `//host/lib.md` is a protocol-relative URL, not a workspace path:
        // even if a same-named file existed it must not draw a path hint.
        let diags = diagnose_at_path_with_files(
            "a/b/c.md",
            "See `//cdn.example.com/lib.md` for details.\n",
            &["cdn.example.com/lib.md", "lib.md"],
        );
        assert!(
            !has_any(&diags, "backticked path"),
            "protocol-relative `//host` is external, not a workspace path: {diags:?}"
        );
    }

    // -- Both-bases resolution + `..` normalization + shape exclusions
    //    (issue 028 false-positive flood) --

    #[test]
    fn dir_relative_dotdot_is_normalized_no_stale() {
        // Bug 2 repro: a backtick `../claude_code/PostToolUse.md` in
        // `architecture/catenary/Hook.md` joins to
        // `architecture/catenary/../claude_code/PostToolUse.md`, which must
        // normalize (collapse `..`) to the clean workspace key
        // `architecture/claude_code/PostToolUse.md` — so the reference resolves
        // and draws the make-it-a-link hint, not a stale-reference warning.
        let diags = diagnose_at_path_with_files(
            "architecture/catenary/Hook.md",
            "See `../claude_code/PostToolUse.md` for details.\n",
            &["architecture/claude_code/PostToolUse.md"],
        );
        assert!(
            !has_any(&diags, "stale reference"),
            "a `..`-relative reference that resolves after normalization is not stale: {diags:?}"
        );
        assert_eq!(
            count_matching(&diags, Severity::Hint, "backticked path"),
            1,
            "the normalized dir-relative reference draws the make-it-a-link hint: {diags:?}"
        );
    }

    #[test]
    fn repo_root_relative_citation_resolves_at_root_no_stale() {
        // Bug 1 repro: a full repo-path citation `tickets/acquire/DESIGN.md`
        // inside `tickets/acquire/v2_01_cleanup.md` must resolve at the
        // workspace root (where the file lives), not at the source file's
        // parent (which would yield `tickets/acquire/tickets/acquire/...`).
        let diags = diagnose_at_path_with_files(
            "tickets/acquire/v2_01_cleanup.md",
            "See `tickets/acquire/DESIGN.md` for details.\n",
            &["tickets/acquire/DESIGN.md"],
        );
        assert!(
            !has_any(&diags, "stale reference"),
            "a repo-root-relative citation that exists at root is not stale: {diags:?}"
        );
        assert_eq!(
            count_matching(&diags, Severity::Hint, "backticked path"),
            1,
            "the root-resolved citation draws the make-it-a-link hint: {diags:?}"
        );
    }

    #[test]
    fn genuine_dangling_under_neither_base_is_stale() {
        // A reference that exists under neither the dir base nor the root base
        // is a genuine dangling reference and still draws the stale warning.
        let diags = diagnose_at_path_with_files(
            "tickets/x/note.md",
            "See `tickets/correlation/missing.md` for details.\n",
            &["tickets/acquire/DESIGN.md"],
        );
        assert!(
            !has_any(&diags, "backticked path"),
            "a reference resolving under no base draws no make-it-a-link hint: {diags:?}"
        );
        assert_eq!(
            count_matching(&diags, Severity::Warning, "stale reference"),
            1,
            "a reference resolving under neither base is a genuine stale reference: {diags:?}"
        );
    }

    #[test]
    fn excluded_path_shapes_draw_no_diagnostic() {
        // `~`-leading (home/out-of-repo), `<>`-bearing (placeholder), and
        // `*`-bearing (glob) tokens are not workspace paths at all: no
        // make-it-a-link hint and no stale-reference warning, whether or not a
        // same-named file exists.
        for token in [
            "~/Projects/Catenary/AGENTS.md",
            "<name>/SKILL.md",
            "NN_*.md",
        ] {
            let backtick = format!("See `{token}` for details.\n");
            // Once with nothing present, once with the literal token present.
            let dangling = diagnose(&backtick);
            let with_file = diagnose_with_files(&backtick, &[token]);
            for diags in [&dangling, &with_file] {
                assert!(
                    !has_any(diags, "backticked path")
                        && !has_any(diags, "stale reference")
                        && !has_any(diags, "convert to a markdown link"),
                    "excluded shape `{token}` draws no dark-matter diagnostic: {diags:?}"
                );
            }
        }
    }

    #[test]
    fn excluded_glob_bare_path_draws_no_diagnostic() {
        // A bare (unbackticked) glob path with a directory component must also
        // be excluded by the tree-level scanner (`is_bare_path`).
        let diags = diagnose("See docs/NN_*.md for details.\n");
        assert!(
            !has_any(&diags, "stale reference") && !has_any(&diags, "convert to a markdown link"),
            "a bare glob path draws no dark-matter diagnostic: {diags:?}"
        );
    }

    #[test]
    fn plain_in_dir_dangling_still_warns() {
        // Regression: a plain in-dir `.md` that exists under neither base is
        // still a genuine stale reference (the both-bases change must not
        // suppress real dangles).
        let diags = diagnose_at_path_with_files("docs/note.md", "See `gone.md`.\n", &[]);
        assert_eq!(
            count_matching(&diags, Severity::Warning, "stale reference"),
            1,
            "a plain in-dir dangling `.md` still warns: {diags:?}"
        );
    }

    #[test]
    fn root_file_still_resolves_via_root_base() {
        // Regression: `/README.md` with `<root>/README.md` present still
        // resolves at the root (no stale warning, make-it-a-link hint fires).
        let diags = diagnose_at_path_with_files("a/b/c.md", "See `/README.md`.\n", &["README.md"]);
        assert_eq!(
            count_matching(&diags, Severity::Hint, "backticked path"),
            1,
            "a root-relative `/README.md` with the root file present still resolves: {diags:?}"
        );
        assert!(
            !has_any(&diags, "stale reference"),
            "the resolving root file draws no stale warning: {diags:?}"
        );
    }

    #[test]
    fn dotdot_escaping_root_is_not_a_resolution() {
        // A `..` chain that escapes the workspace root after normalization is
        // not a valid workspace candidate, so an existing same-stem key must
        // not falsely resolve it; from a top-level file it is a genuine dangle.
        let diags = diagnose_at_path_with_files(
            "note.md",
            "See `../outside.md` for details.\n",
            &["outside.md"],
        );
        assert!(
            !has_any(&diags, "backticked path"),
            "an escaping `..` reference draws no make-it-a-link hint: {diags:?}"
        );
        assert_eq!(
            count_matching(&diags, Severity::Warning, "stale reference"),
            1,
            "an escaping `..` reference is a genuine stale reference: {diags:?}"
        );
    }

    // -- External-namespace `{Name}/…` references (issue 030, decision 010) --

    /// A config with `Catenary` aliased to a fixed (test-only) directory.
    fn config_with_catenary_alias() -> Config {
        let mut config = Config::default();
        config.external.insert(
            "Catenary".to_string(),
            std::path::PathBuf::from("/ext/Catenary"),
        );
        config
    }

    #[test]
    fn external_namespace_recognizer() {
        assert_eq!(
            external_namespace("{Catenary}/docs/x.md"),
            Some(("Catenary", "docs/x.md")),
            "a leading `{{ident}}/` is recognized, splitting alias from the remainder"
        );
        assert_eq!(
            external_namespace("{my_repo-2}/x.md"),
            Some(("my_repo-2", "x.md")),
            "alphanumerics, `_` and `-` are valid identifier characters"
        );
        // Not external references — these fall through to ordinary handling.
        for token in [
            "{Catenary}",         // no trailing `/`
            "{Catenary}/",        // empty remainder
            "{}/x.md",            // empty identifier
            "{a b}/x.md",         // space (not an identifier)
            "docs/{Catenary}.md", // brace not at the start
            "Catenary/x.md",      // no braces
        ] {
            assert_eq!(
                external_namespace(token),
                None,
                "`{token}` is not an external-namespace reference"
            );
        }
    }

    #[test]
    fn external_undefined_alias_is_exempt() {
        // Tier 1 (the exempt floor): with no `[external]` table, a `{Name}/…`
        // citation is external and unverified — no diagnostic, no config needed.
        let diags = diagnose("See `{Catenary}/docs/configuration.md` for details.\n");
        assert!(
            !has_any(&diags, "stale reference")
                && !has_any(&diags, "backticked path")
                && !has_any(&diags, "convert to a markdown link"),
            "an undefined `{{Name}}/…` alias draws no diagnostic (exempt floor): {diags:?}"
        );
    }

    #[test]
    fn external_alias_dir_absent_is_exempt() {
        // Tier 2 (the CI / partial-checkout guard): the alias is defined but its
        // directory is not present on disk — exempt, never a false break.
        let config = config_with_catenary_alias();
        let diags = diagnose_with_external(
            "See `{Catenary}/docs/configuration.md` for details.\n",
            &config,
            // Nothing present: not even the alias directory.
            &[],
        );
        assert!(
            !has_any(&diags, "stale reference"),
            "a defined alias whose directory is absent is exempt: {diags:?}"
        );
    }

    #[test]
    fn external_alias_dir_present_file_present_is_valid() {
        // Tier 3: directory present and the referenced file exists under it —
        // valid, no diagnostic. Notably no make-it-a-link nudge either: an
        // external reference is never a local link.
        let config = config_with_catenary_alias();
        let diags = diagnose_with_external(
            "See `{Catenary}/docs/configuration.md` for details.\n",
            &config,
            &["/ext/Catenary", "/ext/Catenary/docs/configuration.md"],
        );
        assert!(
            !has_any(&diags, "stale reference")
                && !has_any(&diags, "backticked path")
                && !has_any(&diags, "convert to a markdown link"),
            "a present external file is valid and draws no diagnostic: {diags:?}"
        );
    }

    #[test]
    fn external_alias_dir_present_file_missing_is_stale() {
        // Tier 4: directory present but the referenced file is missing — a
        // genuinely broken cross-repo reference draws the stale-reference
        // warning.
        let config = config_with_catenary_alias();
        let diags = diagnose_with_external(
            "See `{Catenary}/docs/configuration.md` for details.\n",
            &config,
            // The alias directory exists; the file under it does not.
            &["/ext/Catenary"],
        );
        assert_eq!(
            count_matching(&diags, Severity::Warning, "stale reference"),
            1,
            "a missing file under a present alias directory is stale: {diags:?}"
        );
    }

    #[test]
    fn external_reference_quoted_and_bare_forms() {
        // The `{Name}/…` shape is recognized on every citation surface 028
        // covers: quoted and bare-with-dir, not only backtick. The quoted
        // dir-bearing token now yields exactly one stale diagnostic — issue 032
        // gave quoted spans a single owner (the structural quoted scanner), so
        // the bare-path surface no longer claims the inner string. Both surfaces
        // therefore assert "exactly one."
        let config = config_with_catenary_alias();
        for content in [
            "See \"{Catenary}/docs/configuration.md\" for details.\n",
            "See {Catenary}/docs/configuration.md for details.\n",
        ] {
            // Present dir, missing file → stale (tier 4).
            let stale = diagnose_with_external(content, &config, &["/ext/Catenary"]);
            assert_eq!(
                count_matching(&stale, Severity::Warning, "stale reference"),
                1,
                "missing external file is stale exactly once on this surface: {stale:?}"
            );
            // Undefined alias → exempt (tier 1) on the same surface.
            let exempt = diagnose(content);
            assert!(
                !has_any(&exempt, "stale reference"),
                "undefined alias is exempt on this surface: {exempt:?}"
            );
        }
    }

    #[test]
    fn external_reference_message_teaches_the_escape() {
        // The stale-reference message names the `{repo}/…` escape (suggestion
        // 001's self-documenting-message principle).
        let diags = diagnose("See `gone/missing.md` for details.\n");
        assert!(
            has_matching(&diags, Severity::Warning, "{repo}/") && has_any(&diags, ".lattice.toml"),
            "the stale message teaches the `{{repo}}/…` external escape: {diags:?}"
        );
    }

    #[test]
    fn external_reference_is_never_a_graph_edge() {
        // Decision 010: a `{Name}/…` citation imposes no graph obligation. It is
        // a backtick/quoted/bare citation, not a markdown link, so it never
        // appears in link extraction — assert nothing comes out of `links()`.
        let tree = block::parse_tree(
            "See `{Catenary}/docs/configuration.md` and {Catenary}/x.md.\n",
            None,
        );
        let links = tree.links(std::path::Path::new("test.md"));
        assert!(
            links.is_empty(),
            "an external `{{Name}}/…` citation forms no graph edge: {links:?}"
        );
    }

    // -- Table-cell dark-matter coverage (issue 023) --

    // A backticked existing-file path inside a GFM table cell must emit the
    // same "make it a link" hint as the identical path in prose, anchored at
    // the cell's row — the link/edge extractor already walks these cells.
    #[test]
    fn backticked_path_in_table_cell_emits_hint() {
        let content = "| # | Tracker |\n|---|---------|\n| 1 | `tickets/foo/README.md` |\n";
        let diags = diagnose_with_files(content, &["tickets/foo/README.md"]);

        let hits: Vec<&Diagnostic> = diags
            .iter()
            .filter(|d| d.severity == Severity::Hint && d.message.contains("backticked path"))
            .collect();
        assert_eq!(
            hits.len(),
            1,
            "exactly one backticked-path hint for the cell: {diags:?}"
        );
        // The cell sits on the third line of the document (1-based).
        assert_eq!(
            hits[0].line, 3,
            "hint is anchored at the table cell's row (line 3): {diags:?}"
        );
    }

    // The hint must agree with prose: a path that exists only in a cell is
    // surfaced; one that does not exist is not.
    #[test]
    fn backticked_path_in_table_cell_no_file() {
        let content = "| # | Tracker |\n|---|---------|\n| 1 | `tickets/foo/README.md` |\n";
        let diags = diagnose(content);
        assert!(
            !has_any(&diags, "backticked path"),
            "no hint for a non-existent cell path: {diags:?}"
        );
    }

    // Sibling dark-matter surfaces extended for parity with the edge extractor
    // (issue 023, fix point 4): bare URL, quoted path, and tree-level bare path
    // inside a table cell must each surface just as they do in prose.
    #[test]
    fn bare_url_in_table_cell_emits_warning() {
        let content = "| Site |\n|------|\n| https://example.com/page |\n";
        let diags = diagnose(content);
        assert_eq!(
            count_matching(&diags, Severity::Warning, "bare URL"),
            1,
            "one bare-URL warning for the cell: {diags:?}"
        );
    }

    #[test]
    fn quoted_path_in_table_cell_emits_hint() {
        let content = "| Ref |\n|-----|\n| \"other.md\" |\n";
        let diags = diagnose_with_files(content, &["other.md"]);
        assert_eq!(
            count_matching(&diags, Severity::Hint, "quoted path"),
            1,
            "one quoted-path hint for the cell: {diags:?}"
        );
    }

    #[test]
    fn bare_path_in_table_cell_emits_diagnostic() {
        let content = "| Ref |\n|-----|\n| docs/page.md |\n";
        let diags = diagnose_with_files(content, &["docs/page.md"]);
        assert_eq!(
            count_matching(&diags, Severity::Warning, "convert to a markdown link"),
            1,
            "one bare-path diagnostic for the cell: {diags:?}"
        );
    }

    // -- Self-closing non-void --

    #[test]
    fn self_closing_div() {
        let diags = diagnose("<div/>\n");
        assert_eq!(
            count_matching(&diags, Severity::Warning, "self-closing non-void"),
            1,
            "one warning for self-closing div: {diags:?}"
        );
    }

    #[test]
    fn self_closing_void_ok() {
        let diags = diagnose("<br/>\n");
        assert!(
            !has_any(&diags, "self-closing non-void"),
            "no warning for self-closing void: {diags:?}"
        );
    }

    // -- Unknown element --

    #[test]
    fn unknown_element() {
        let diags = diagnose("<foo>\n</foo>\n");
        assert_eq!(
            count_matching(&diags, Severity::Info, "unknown HTML element"),
            1,
            "one info for unknown element: {diags:?}"
        );
    }

    // -- Duplicate id (inline + block, issue 026) --

    #[test]
    fn duplicate_id_across_block_and_mid_paragraph_inline() {
        // Issue 026: harvesting mid-paragraph id-bearing inline tags as
        // `InlineHtml` nodes puts them on the same `Syntax::Html` surface the
        // duplicate-id pass walks, so a block `<div id>` and a mid-paragraph
        // `<span id>` sharing the same id now collide (invalid HTML — GitHub
        // anchors only the first).
        let diags = diagnose(
            "<div id=\"shared\"></div>\n\n\
             Paragraph with an <span id=\"shared\"></span> inline target.\n",
        );
        assert_eq!(
            count_matching(&diags, Severity::Error, "duplicate `id` attribute `shared`"),
            1,
            "one error for the inline id duplicating the block id: {diags:?}"
        );
    }

    #[test]
    fn distinct_mid_paragraph_inline_id_no_duplicate() {
        // A mid-paragraph inline id distinct from every other id is not flagged.
        let diags = diagnose(
            "<div id=\"block\"></div>\n\n\
             Paragraph with an <span id=\"inline\"></span> inline target.\n",
        );
        assert!(
            !has_any(&diags, "duplicate `id`"),
            "distinct ids do not collide: {diags:?}"
        );
    }

    // -- Config: code_block_language --

    #[test]
    fn code_block_language_disabled() {
        let fm = yaml::parse_frontmatter_block("```\ncode\n```\n");
        let fm_span = fm.as_ref().map(|b| b.span);
        let tree = block::parse_tree("```\ncode\n```\n", fm_span);
        let mut config = Config::default();
        config.policy.code_block_language = CodeBlockLanguagePolicy::Disabled;
        let rel_path = std::path::Path::new("test.md");
        let diags = collect(
            &tree,
            rel_path,
            &config,
            &|_| false,
            &|_| false,
            &Exceptions::default(),
        );
        assert!(
            !has_any(&diags, "language tag"),
            "no diagnostic when disabled: {diags:?}"
        );
    }

    #[test]
    fn code_block_language_deny_is_error() {
        let fm = yaml::parse_frontmatter_block("```\ncode\n```\n");
        let fm_span = fm.as_ref().map(|b| b.span);
        let tree = block::parse_tree("```\ncode\n```\n", fm_span);
        let mut config = Config::default();
        config.policy.code_block_language = CodeBlockLanguagePolicy::Deny;
        let rel_path = std::path::Path::new("test.md");
        let diags = collect(
            &tree,
            rel_path,
            &config,
            &|_| false,
            &|_| false,
            &Exceptions::default(),
        );
        assert_eq!(
            count_matching(&diags, Severity::Error, "without a language tag"),
            1,
            "one error when deny: {diags:?}"
        );
    }

    // -- Config: bare_paths policy governs both emitters (issue 007) --

    fn diagnose_with_policy(
        content: &str,
        existing: &[&str],
        policy: BarePathPolicy,
    ) -> Vec<Diagnostic> {
        let fm = yaml::parse_frontmatter_block(content);
        let fm_span = fm.as_ref().map(|b| b.span);
        let tree = block::parse_tree(content, fm_span);
        let mut config = Config::default();
        config.policy.bare_paths = policy;
        let rel_path = std::path::Path::new("test.md");
        let existing_set: HashSet<&str> = existing.iter().copied().collect();
        let exceptions = exceptions_of(content);
        collect(
            &tree,
            rel_path,
            &config,
            &|p| existing_set.contains(p.to_str().unwrap_or("")),
            &|_| false,
            &exceptions,
        )
    }

    // One paragraph exercising every bare-path emitter: a tree-level bare path
    // (`docs/page.md`), a prose bare URL, a quoted path, and a backticked path.
    const BARE_PATH_SAMPLE: &str =
        "Visit https://example.com and see \"other.md\" or `other.md` in docs/page.md here.\n";

    const BARE_PATH_NEEDLES: [&str; 4] = [
        "convert to a markdown link",
        "bare URL",
        "quoted path",
        "backticked path",
    ];

    #[test]
    fn bare_paths_disabled_silences_both_emitters() {
        let diags = diagnose_with_policy(
            BARE_PATH_SAMPLE,
            &["other.md", "docs/page.md"],
            BarePathPolicy::Disabled,
        );
        for needle in BARE_PATH_NEEDLES {
            assert!(
                !has_any(&diags, needle),
                "disabled should silence `{needle}`: {diags:?}"
            );
        }
    }

    #[test]
    fn bare_paths_deny_escalates_both_emitters() {
        let diags = diagnose_with_policy(
            BARE_PATH_SAMPLE,
            &["other.md", "docs/page.md"],
            BarePathPolicy::Deny,
        );
        for needle in BARE_PATH_NEEDLES {
            assert!(
                has_matching(&diags, Severity::Error, needle),
                "deny should escalate `{needle}` to error: {diags:?}"
            );
        }
    }

    // -- close_block_quotes HTML scope desync --

    #[test]
    fn html_in_blockquote_closed_on_blank_line() {
        // An HTML container inside a block quote followed by a blank line
        // should produce exactly one unclosed-tag diagnostic, not desync
        // the scope stacks and cascade errors.
        let diags = diagnose("> <div>\n>\n> text\n\nparagraph\n");
        assert_eq!(
            count_matching(&diags, Severity::Error, "unclosed"),
            1,
            "one unclosed div error, no cascading: {diags:?}"
        );
    }

    // -- Malformed link --

    #[test]
    fn malformed_link_destination() {
        let diags = diagnose("[text](\n");
        assert_eq!(
            count_matching(&diags, Severity::Error, "malformed link"),
            1,
            "one error for malformed link: {diags:?}"
        );
    }

    // -- Unused/duplicate ref defs are Warning, not Error --

    #[test]
    fn unused_ref_def_is_warning() {
        let diags = diagnose("[label]: https://example.com\n\nSome text.\n");
        assert_eq!(
            count_matching(&diags, Severity::Warning, "unused reference definition"),
            1,
            "unused ref def should be warning: {diags:?}"
        );
        assert!(
            !has_matching(&diags, Severity::Error, "unused reference definition"),
            "unused ref def should not be error: {diags:?}"
        );
    }

    #[test]
    fn duplicate_ref_def_is_warning() {
        let diags = diagnose("[label]: https://a.com\n[label]: https://b.com\n\n[text][label]\n");
        assert_eq!(
            count_matching(&diags, Severity::Warning, "duplicate reference definition"),
            1,
            "duplicate ref def should be warning: {diags:?}"
        );
    }

    // -- Markdown in opaque HTML --

    #[test]
    fn markdown_in_opaque_html_warns() {
        // <center> is a type 6 block tag with no structural mapping,
        // so it falls through to HtmlBlock. Content without blank
        // lines won't be parsed as markdown.
        let diags = diagnose("<center>\n# Heading\n</center>\n");
        assert_eq!(
            count_matching(
                &diags,
                Severity::Warning,
                "markdown syntax inside HTML block"
            ),
            1,
            "one warning for markdown in opaque HTML: {diags:?}"
        );
    }

    // -- Frontmatter `exceptions` (issue 031, decision 011) --

    #[test]
    fn exception_suppresses_unresolved_stale_reference() {
        // An exception keyed by the still-unresolved reference suppresses its
        // stale-reference diagnostic — and, having matched, is not flagged as
        // unused.
        let content = "---\n\
            exceptions:\n  \
              stale_references:\n    \
                \"gone.md\": \"hypothetical path in the worked example\"\n\
            ---\n\
            See `gone.md` for details.\n";
        let diags = diagnose_with_files(content, &[]);
        assert!(
            !has_any(&diags, "stale reference"),
            "the exception suppresses the stale-reference diagnostic: {diags:?}"
        );
        assert!(
            !has_any(&diags, "unused exception"),
            "a matched exception is not flagged as unused: {diags:?}"
        );
    }

    #[test]
    fn exception_with_no_live_diagnostic_is_unused_and_echoes_reason() {
        // The reference is gone from the body, so the exception matches nothing
        // — flagged as unused, echoing the stored reason (the epitaph).
        let content = "---\n\
            exceptions:\n  \
              stale_references:\n    \
                \"gone.md\": \"hypothetical path in the worked example\"\n\
            ---\n\
            Nothing references it now.\n";
        let diags = diagnose_with_files(content, &[]);
        assert_eq!(
            count_matching(&diags, Severity::Warning, "unused exception: `gone.md`"),
            1,
            "an exception matching no live diagnostic is flagged as unused: {diags:?}"
        );
        assert!(
            has_any(&diags, "hypothetical path in the worked example"),
            "the unused-exception message echoes the stored reason: {diags:?}"
        );
    }

    #[test]
    fn exception_with_empty_reason_is_a_diagnostic() {
        // A required reason: an empty reason is itself a defect, anchored at the
        // key — even though the suppression still applies.
        let content = "---\n\
            exceptions:\n  \
              stale_references:\n    \
                \"gone.md\": \"\"\n\
            ---\n\
            See `gone.md` here.\n";
        let diags = diagnose_with_files(content, &[]);
        assert_eq!(
            count_matching(&diags, Severity::Warning, "has no reason"),
            1,
            "an empty-reason exception is a diagnostic: {diags:?}"
        );
        // An empty-reason entry that matched a live diagnostic does not *also*
        // get flagged as unused — exactly one reconciliation diagnostic.
        assert!(
            !has_any(&diags, "unused exception"),
            "a matched empty-reason entry is not also flagged unused: {diags:?}"
        );
    }

    #[test]
    fn external_alias_keyed_exception_suppresses_present_missing_stale() {
        // A `{Name}/…`-keyed exception flows through identically: a defined,
        // present alias whose target file is missing is a stale reference, and
        // the literal `{Name}/…` key suppresses it (decision 011).
        let config = config_with_catenary_alias();
        let content = "---\n\
            exceptions:\n  \
              stale_references:\n    \
                \"{Catenary}/old/layout.md\": \"pre-refactor path, kept for the changelog note\"\n\
            ---\n\
            See `{Catenary}/old/layout.md` for the old shape.\n";
        // Alias directory present, file under it missing → tier 4 (stale).
        let diags = diagnose_with_external(content, &config, &["/ext/Catenary"]);
        assert!(
            !has_any(&diags, "stale reference"),
            "a `{{Name}}/…`-keyed exception suppresses the present-missing stale: {diags:?}"
        );
        assert!(
            !has_any(&diags, "unused exception"),
            "the matched alias-keyed exception is not flagged unused: {diags:?}"
        );
    }

    #[test]
    fn exception_scope_is_per_reference() {
        // An exception for one reference does not suppress a *different*
        // unresolved one.
        let content = "---\n\
            exceptions:\n  \
              stale_references:\n    \
                \"excepted.md\": \"deliberately not a live reference\"\n\
            ---\n\
            See `excepted.md` and also `other.md`.\n";
        let diags = diagnose_with_files(content, &[]);
        assert_eq!(
            count_matching(&diags, Severity::Warning, "stale reference: `other.md`"),
            1,
            "the unexcepted reference still fires: {diags:?}"
        );
        assert!(
            !has_any(&diags, "stale reference: `excepted.md`"),
            "the excepted reference is suppressed: {diags:?}"
        );
    }

    #[test]
    fn exception_is_never_a_graph_edge_or_backlink_obligation() {
        // An `exceptions` block is a path-shaped-lint lever only: it must never
        // appear in link/graph extraction (decision 011 — no edge, no backlink).
        let content = "---\n\
            exceptions:\n  \
              stale_references:\n    \
                \"gone.md\": \"deliberately dead\"\n\
            ---\n\
            Body text.\n";
        let fm = yaml::parse_frontmatter_block(content);
        let fm_span = fm.as_ref().map(|b| b.span);
        let tree = block::parse_tree(content, fm_span);
        let links = tree.links(std::path::Path::new("test.md"));
        assert!(
            links.is_empty(),
            "an exception forms no graph edge: {links:?}"
        );
    }

    #[test]
    fn rename_flags_old_key_unused_while_new_name_fires() {
        // On a rename the old exception key matches nothing (unused) while the
        // renamed reference, lacking an exception, fires fresh — both present.
        let content = "---\n\
            exceptions:\n  \
              stale_references:\n    \
                \"old-name.md\": \"the design doc, since renamed\"\n\
            ---\n\
            See `new-name.md` for the design.\n";
        let diags = diagnose_with_files(content, &[]);
        assert_eq!(
            count_matching(&diags, Severity::Warning, "unused exception: `old-name.md`"),
            1,
            "the renamed-away old key is flagged unused: {diags:?}"
        );
        assert_eq!(
            count_matching(&diags, Severity::Warning, "stale reference: `new-name.md`"),
            1,
            "the new name fires a fresh stale reference: {diags:?}"
        );
    }

    #[test]
    fn bare_paths_exception_suppresses_resolve_hint() {
        // The `bare_paths` namespace suppresses the make-it-a-link nudge on a
        // *resolving* path (the lint fires on resolution).
        let content = "---\n\
            exceptions:\n  \
              bare_paths:\n    \
                \"README.md\": \"naming the file, deliberately not a link\"\n\
            ---\n\
            See `README.md` for the overview.\n";
        let diags = diagnose_with_files(content, &["README.md"]);
        assert!(
            !has_any(&diags, "backticked path"),
            "the bare_paths exception suppresses the resolve hint: {diags:?}"
        );
        assert!(
            !has_any(&diags, "unused exception"),
            "the matched bare_paths exception is not flagged unused: {diags:?}"
        );
    }

    #[test]
    fn exception_round_trips_both_namespaces_and_alias_keys() {
        // A frontmatter exercising both namespaces, the map form, and a
        // `{Name}/…` key parses into the two buckets with reasons retained.
        let content = "---\n\
            exceptions:\n  \
              stale_references:\n    \
                \"tickets/acquire/DESIGN.md\": \"hypothetical path in the worked example\"\n    \
                \"{Catenary}/old/layout.md\": \"pre-refactor path\"\n  \
              bare_paths:\n    \
                \"README\": \"naming the file, deliberately not a link\"\n\
            ---\n\
            Body.\n";
        let exceptions = exceptions_of(content);
        assert_eq!(
            exceptions.stale_references.len(),
            2,
            "two stale_references exceptions parsed: {exceptions:?}"
        );
        assert_eq!(
            exceptions.bare_paths.len(),
            1,
            "one bare_paths exception parsed: {exceptions:?}"
        );
        assert_eq!(
            exceptions.stale_references[0].reference, "tickets/acquire/DESIGN.md",
            "the first stale key is the literal reference: {exceptions:?}"
        );
        assert_eq!(
            exceptions.stale_references[1].reference, "{Catenary}/old/layout.md",
            "the `{{Name}}/…` key is retained verbatim: {exceptions:?}"
        );
        assert_eq!(
            exceptions.bare_paths[0].reference, "README",
            "the bare_paths key is the literal reference: {exceptions:?}"
        );
        assert_eq!(
            exceptions.stale_references[0].reason, "hypothetical path in the worked example",
            "the reason is the map value: {exceptions:?}"
        );
    }

    #[test]
    fn exception_for_disabled_lint_is_not_flagged_unused() {
        // When `stale_references` is `Disabled`, its exceptions are inert: no
        // suppression is needed and no unused-exception flood is produced.
        let content = "---\n\
            exceptions:\n  \
              stale_references:\n    \
                \"gone.md\": \"deliberately dead\"\n\
            ---\n\
            Nothing references it.\n";
        let diags = diagnose_with_stale_policy(content, &[], StaleReferencePolicy::Disabled);
        assert!(
            !has_any(&diags, "unused exception"),
            "a disabled lint's exceptions are not flagged unused: {diags:?}"
        );
    }

    // -- In-tool config pointer: messages close the loop in-context (issue 035) --

    #[test]
    fn stale_reference_message_points_at_config_help() {
        // The stale-reference message routes the agent back to the config
        // grammar from the diagnostic itself (issue 035).
        let diags = diagnose("See `gone/missing.md` for details.\n");
        assert!(
            has_matching(&diags, Severity::Warning, "lattice help config"),
            "the stale-reference message names `lattice help config`: {diags:?}"
        );
    }

    #[test]
    fn make_it_a_link_message_names_both_escapes_and_config_help() {
        // FU2 (issue 031, folded into 035; reframed by 039): the make-it-a-link
        // hint names BOTH example escapes — drop the extension, OR except it with
        // a reason — under the move-test framing, pointing to `lattice help
        // config` (the literal `exceptions.bare_paths` namespace lives in the
        // config reference now, not in the per-occurrence message).
        let diags = diagnose_with_files("See `other.md` for details.\n", &["other.md"]);
        assert!(
            has_matching(&diags, Severity::Hint, "drop the extension"),
            "the hint still offers drop-the-extension: {diags:?}"
        );
        assert!(
            has_matching(&diags, Severity::Hint, "except it with a reason"),
            "the hint names the frontmatter exception escape with its required reason (FU2): {diags:?}"
        );
        assert!(
            has_matching(&diags, Severity::Hint, "lattice help config"),
            "the hint points at `lattice help config`: {diags:?}"
        );
    }

    #[test]
    fn stale_reference_message_frames_the_move_test() {
        // Issue 039 / decision 014: the stale-reference message states the choice
        // as the move test ("would a move update this?"), not just a flat list of
        // knobs, while keeping the `lattice help config` pointer.
        let diags = diagnose("See `gone.md` here.\n");
        assert!(
            has_matching(&diags, Severity::Warning, "stale reference: `gone.md`"),
            "the stale-reference message still fires: {diags:?}"
        );
        assert!(
            has_matching(
                &diags,
                Severity::Warning,
                "would moving the target update this"
            ),
            "the stale-reference message frames the choice as the move test: {diags:?}"
        );
        assert!(
            has_matching(&diags, Severity::Warning, "lattice help config"),
            "the stale-reference message keeps the config pointer: {diags:?}"
        );
    }

    #[test]
    fn make_it_a_link_message_frames_the_move_test() {
        // The resolving backticked-path (make-it-a-link) message frames link-vs-
        // example as the move test, keeps the make-it-a-link resolution, and the
        // config pointer.
        let diags = diagnose_with_files("See `other.md` here.\n", &["other.md"]);
        assert!(
            has_matching(&diags, Severity::Hint, "backticked path `other.md`"),
            "the make-it-a-link hint still fires: {diags:?}"
        );
        assert!(
            has_matching(&diags, Severity::Hint, "would moving it update this"),
            "the make-it-a-link hint frames the choice as the move test: {diags:?}"
        );
        assert!(
            has_matching(&diags, Severity::Hint, "make it a link"),
            "the make-it-a-link hint keeps the link resolution: {diags:?}"
        );
        assert!(
            has_matching(&diags, Severity::Hint, "lattice help config"),
            "the make-it-a-link hint keeps the config pointer: {diags:?}"
        );
    }

    #[test]
    fn bare_path_make_it_a_link_message_points_at_config_help() {
        // Every bare_paths-gated nudge routes to the config grammar (issue 035):
        // the unbacticked resolving-path "convert to a markdown link" warning
        // carries the `lattice help config` pointer too.
        let diags = diagnose_with_files("See docs/other.md for details.\n", &["docs/other.md"]);
        assert!(
            has_matching(&diags, Severity::Warning, "convert to a markdown link"),
            "the bare-path nudge still fires: {diags:?}"
        );
        assert!(
            has_matching(&diags, Severity::Warning, "lattice help config"),
            "the bare-path nudge points at `lattice help config`: {diags:?}"
        );
    }

    #[test]
    fn quoted_path_message_points_at_config_help() {
        // The quoted-path resolving hint is the third bare_paths-gated nudge; it
        // carries the `lattice help config` pointer too (issue 035).
        let diags = diagnose_with_files("See \"docs/other.md\" for details.\n", &["docs/other.md"]);
        assert!(
            has_matching(&diags, Severity::Hint, "quoted path"),
            "the quoted-path hint still fires: {diags:?}"
        );
        assert!(
            has_matching(&diags, Severity::Hint, "lattice help config"),
            "the quoted-path hint points at `lattice help config`: {diags:?}"
        );
    }

    #[test]
    fn unused_exception_message_points_at_config_help() {
        let content = "---\n\
            exceptions:\n  \
              stale_references:\n    \
                \"gone.md\": \"hypothetical path in the worked example\"\n\
            ---\n\
            Nothing references it now.\n";
        let diags = diagnose_with_files(content, &[]);
        assert!(
            has_matching(&diags, Severity::Warning, "unused exception: `gone.md`",),
            "the unused-exception message still fires: {diags:?}"
        );
        assert!(
            has_matching(&diags, Severity::Warning, "lattice help config"),
            "the unused-exception message points at `lattice help config`: {diags:?}"
        );
    }

    #[test]
    fn empty_reason_message_points_at_config_help() {
        let content = "---\n\
            exceptions:\n  \
              stale_references:\n    \
                \"gone.md\": \"\"\n\
            ---\n\
            See `gone.md` here.\n";
        let diags = diagnose_with_files(content, &[]);
        assert!(
            has_matching(&diags, Severity::Warning, "has no reason"),
            "the empty-reason message still fires: {diags:?}"
        );
        assert!(
            has_matching(&diags, Severity::Warning, "lattice help config"),
            "the empty-reason message points at `lattice help config`: {diags:?}"
        );
    }

    // -- Count-key + suppression ledger (issue 036, decision 012) --

    /// Like [`diagnose_with_files`], but returns both the diagnostics and the
    /// [`FileSuppressions`] ledger entry, with an explicit config so the
    /// count-key tests can flip a lint to `Disabled`.
    fn diagnose_full(
        content: &str,
        config: &Config,
        existing: &[&str],
    ) -> (Vec<Diagnostic>, FileSuppressions) {
        let fm = yaml::parse_frontmatter_block(content);
        let fm_span = fm.as_ref().map(|b| b.span);
        let tree = block::parse_tree(content, fm_span);
        let rel_path = std::path::Path::new("test.md");
        let existing_set: HashSet<&str> = existing.iter().copied().collect();
        let exceptions = exceptions_of(content);
        collect_with_suppressions(
            &tree,
            rel_path,
            config,
            &|p| existing_set.contains(p.to_str().unwrap_or("")),
            &|_| false,
            &exceptions,
        )
    }

    /// A document with three dangling stale references in the body, under a
    /// `stale_references` count-key of `count`, with a non-empty shared reason.
    fn three_stale_with_count(count: &str) -> String {
        format!(
            "---\n\
             exceptions:\n  \
               stale_references:\n    \
                 \"{count}\": \"migration table — every path is a record, not a live reference\"\n\
             ---\n\
             See `a.md`, `b.md`, and `c.md`.\n"
        )
    }

    #[test]
    fn count_key_suppresses_iff_residual_equals_n() {
        // Three dangling references, N = 3: the whole residual is suppressed
        // under the single shared reason, nothing resurfaces.
        let config = Config::default();
        let (diags, sup) = diagnose_full(&three_stale_with_count("3"), &config, &[]);
        assert!(
            !has_any(&diags, "stale reference"),
            "a count-key of N == M suppresses the whole residual: {diags:?}"
        );
        assert!(
            !has_any(&diags, "expected"),
            "no drift warning when the count matches: {diags:?}"
        );
        // The ledger records the count-key suppression by severity (the default
        // stale_references policy is `warn`).
        let count_key = &sup.count_keys;
        assert_eq!(
            count_key.len(),
            1,
            "the matched count-key produces one ledger row: {sup:?}"
        );
        assert_eq!(
            count_key[0].counts.warnings, 3,
            "the ledger tallies the three suppressed warnings: {sup:?}"
        );
        assert_eq!(
            count_key[0].raw, "3",
            "the row carries the raw key: {sup:?}"
        );
    }

    #[test]
    fn count_key_one_too_many_resurfaces_and_flags() {
        // Three dangling references, N = 2 (one too few expected → drift): the
        // sentinel is inert, every residual resurfaces, and a drift warning is
        // anchored on the key with the `expected N, found M` message.
        let config = Config::default();
        let (diags, sup) = diagnose_full(&three_stale_with_count("2"), &config, &[]);
        assert_eq!(
            count_matching(&diags, Severity::Warning, "stale reference: `"),
            3,
            "every residual diagnostic resurfaces on drift: {diags:?}"
        );
        assert!(
            has_matching(
                &diags,
                Severity::Warning,
                "expected 2 stale references here, found 3"
            ),
            "the drift warning names N and M: {diags:?}"
        );
        assert!(
            sup.count_keys.is_empty(),
            "a drifted count-key suppresses nothing, so no ledger row: {sup:?}"
        );
    }

    #[test]
    fn count_key_one_too_few_resurfaces_and_flags() {
        // Three dangling references, N = 4 (one too many expected → drift).
        let config = Config::default();
        let (diags, sup) = diagnose_full(&three_stale_with_count("4"), &config, &[]);
        assert_eq!(
            count_matching(&diags, Severity::Warning, "stale reference: `"),
            3,
            "every residual diagnostic resurfaces on drift: {diags:?}"
        );
        assert!(
            has_matching(
                &diags,
                Severity::Warning,
                "expected 4 stale references here, found 3"
            ),
            "the drift warning names N and M: {diags:?}"
        );
        assert!(
            sup.count_keys.is_empty(),
            "a drifted count-key suppresses nothing: {sup:?}"
        );
    }

    #[test]
    fn count_key_and_literal_compose() {
        // A literal key carves its own diagnostic out of the residual first; the
        // count-key then claims the remaining two. N = 2 over the residual.
        let content = "---\n\
            exceptions:\n  \
              stale_references:\n    \
                \"a.md\": \"the worked example path\"\n    \
                \"2\": \"the rest of the migration table\"\n\
            ---\n\
            See `a.md`, `b.md`, and `c.md`.\n";
        let config = Config::default();
        let (diags, sup) = diagnose_full(content, &config, &[]);
        assert!(
            !has_any(&diags, "stale reference"),
            "the literal carves one out and the count covers the rest: {diags:?}"
        );
        assert!(
            !has_any(&diags, "expected"),
            "no drift: the residual after the literal is exactly N: {diags:?}"
        );
        let ex = sup
            .exceptions
            .as_ref()
            .expect("the literal exception suppressed one");
        assert_eq!(
            ex.counts.warnings, 1,
            "the literal row tallies its one suppression: {sup:?}"
        );
        assert_eq!(ex.matched_entries, 1, "one literal entry matched: {sup:?}");
        assert_eq!(
            sup.count_keys.first().map(|c| c.counts.warnings),
            Some(2),
            "the count-key row tallies the residual of two: {sup:?}"
        );
    }

    #[test]
    fn count_key_with_empty_reason_is_diagnosed() {
        // An empty reason is a defect (the shared epitaph is required), anchored
        // at the key; the residual resurfaces (the sentinel cannot suppress).
        let content = "---\n\
            exceptions:\n  \
              stale_references:\n    \
                \"3\": \"\"\n\
            ---\n\
            See `a.md`, `b.md`, and `c.md`.\n";
        let config = Config::default();
        let (diags, sup) = diagnose_full(content, &config, &[]);
        assert!(
            has_matching(&diags, Severity::Warning, "count-key `3`")
                && has_matching(&diags, Severity::Warning, "has no reason"),
            "an empty-reason count-key is diagnosed at the key: {diags:?}"
        );
        assert_eq!(
            count_matching(&diags, Severity::Warning, "stale reference: `"),
            3,
            "the residual resurfaces under an empty-reason count-key: {diags:?}"
        );
        assert!(
            sup.count_keys.is_empty(),
            "an empty-reason count-key suppresses nothing: {sup:?}"
        );
    }

    #[test]
    fn count_key_of_zero_is_diagnosed() {
        // `N >= 1`: a `0` count-key is invalid — diagnosed at the key, residual
        // resurfaces.
        let content = "---\n\
            exceptions:\n  \
              stale_references:\n    \
                \"0\": \"a reason\"\n\
            ---\n\
            See `a.md`.\n";
        let config = Config::default();
        let (diags, _sup) = diagnose_full(content, &config, &[]);
        assert!(
            has_matching(&diags, Severity::Warning, "must be at least 1"),
            "a zero count-key is diagnosed: {diags:?}"
        );
        assert_eq!(
            count_matching(&diags, Severity::Warning, "stale reference: `"),
            1,
            "the residual resurfaces under a zero count-key: {diags:?}"
        );
    }

    #[test]
    fn count_key_under_disabled_lint_is_inert() {
        // A `Disabled` stale_references lint makes the count-key inert: no
        // suppression, no drift flag, no empty-reason flag — and no residual to
        // resurface (the lint emits nothing).
        let mut config = Config::default();
        config.policy.stale_references = StaleReferencePolicy::Disabled;
        // N deliberately mismatches the body, which would drift if active.
        let (diags, sup) = diagnose_full(&three_stale_with_count("99"), &config, &[]);
        assert!(
            !has_any(&diags, "stale reference"),
            "a disabled lint emits no stale references: {diags:?}"
        );
        assert!(
            !has_any(&diags, "expected"),
            "a disabled lint's count-key raises no drift flag: {diags:?}"
        );
        assert!(
            sup.is_empty(),
            "a disabled-lint count-key suppresses nothing: {sup:?}"
        );
    }

    #[test]
    fn count_key_shape_discrimination() {
        // `31` is a sentinel (claims the residual); `31.md` and `a/31.md` are
        // literal references (each suppresses only its own diagnostic). Here the
        // two literal keys carve their own out and the `31` sentinel claims the
        // single remaining dangling reference, so N = 1 suppresses cleanly.
        let content = "---\n\
            exceptions:\n  \
              stale_references:\n    \
                \"31.md\": \"a literal path-shaped key\"\n    \
                \"a/31.md\": \"another literal path-shaped key\"\n    \
                \"1\": \"the residual count sentinel\"\n\
            ---\n\
            See `31.md`, `a/31.md`, and `loose.md`.\n";
        let config = Config::default();
        let (diags, sup) = diagnose_full(content, &config, &[]);
        assert!(
            !has_any(&diags, "stale reference"),
            "the two literals carve out, the sentinel claims the rest: {diags:?}"
        );
        let ex = sup
            .exceptions
            .as_ref()
            .expect("the two path-shaped literals suppressed");
        assert_eq!(
            ex.matched_entries, 2,
            "`31.md` and `a/31.md` are literal entries, both matched: {sup:?}"
        );
        assert_eq!(
            sup.count_keys.first().map(|c| c.counts.warnings),
            Some(1),
            "the `1` sentinel claims the single residual: {sup:?}"
        );
    }

    // -- 028-family lint classifier (issue 037) --

    #[test]
    fn classify_028_lint_maps_each_message_family() {
        // The exact production message prefixes the emitters above produce.
        assert_eq!(
            classify_028_lint("stale reference: `gone.md` — no such markdown file"),
            Some(ExceptionLint::StaleReferences),
            "the stale-reference message maps to StaleReferences"
        );
        for bare in [
            "bare path `docs/x.md`: convert to a markdown link",
            "bare URL `https://x` : wrap in angle brackets",
            "quoted path `\"x.md\"`: use backticks",
            "backticked path `x.md` refers to an existing file",
        ] {
            assert_eq!(
                classify_028_lint(bare),
                Some(ExceptionLint::BarePaths),
                "a bare_paths-family message maps to BarePaths: {bare}"
            );
        }
        assert_eq!(
            classify_028_lint("empty heading"),
            None,
            "a non-028 message maps to neither lint"
        );
        assert_eq!(
            classify_028_lint("duplicate heading slug `x`"),
            None,
            "another non-028 message maps to neither lint"
        );
    }

    // -- Artifact glossary (issue 038, decision 013) --

    /// A [`Config`] whose `[graph] artifacts` glossary lists `names`.
    fn config_with_artifacts(names: &[&str]) -> Config {
        Config {
            artifacts: names.iter().map(|s| (*s).to_string()).collect(),
            ..Config::default()
        }
    }

    #[test]
    fn artifact_name_resolving_draws_no_make_it_a_link_hint() {
        // The bare artifact name coincides with this repo's own root file, so it
        // would normally draw the make-it-a-link hint — the glossary swallows it.
        let config = config_with_artifacts(&["AGENTS.md"]);
        let (diags, sup) =
            diagnose_full("See `AGENTS.md` for the hooks.\n", &config, &["AGENTS.md"]);
        assert!(
            !has_any(&diags, "make it a link"),
            "a glossary artifact draws no make-it-a-link hint even when it resolves: {diags:?}"
        );
        assert!(
            !has_any(&diags, "AGENTS.md"),
            "no diagnostic mentions the artifact at all: {diags:?}"
        );
        assert_eq!(
            sup.artifacts.get("AGENTS.md").map(|c| c.hints),
            Some(1),
            "the swallowed hint is recorded in the ledger tally: {sup:?}"
        );
    }

    #[test]
    fn artifact_name_dangling_draws_no_stale_reference() {
        // The bare artifact name resolves to no file in this repo, so it would
        // normally draw a stale_references warning — the glossary swallows it.
        let config = config_with_artifacts(&["GEMINI.md"]);
        let (diags, sup) = diagnose_full("Put hooks in `GEMINI.md`.\n", &config, &[]);
        assert!(
            !has_any(&diags, "stale reference"),
            "a glossary artifact draws no stale-reference diagnostic when it dangles: {diags:?}"
        );
        assert_eq!(
            sup.artifacts.get("GEMINI.md").map(|c| c.warnings),
            Some(1),
            "the swallowed stale-reference warning is recorded in the ledger tally: {sup:?}"
        );
    }

    #[test]
    fn artifact_exact_match_only_path_qualified_still_flags() {
        // `AGENTS.md` is a glossary member; `dir/AGENTS.md` is a DIFFERENT
        // reference and is not matched — it still draws its normal diagnostic.
        let config = config_with_artifacts(&["AGENTS.md"]);
        let (diags, sup) = diagnose_full("See `dir/AGENTS.md`.\n", &config, &[]);
        assert!(
            has_matching(
                &diags,
                Severity::Warning,
                "stale reference: `dir/AGENTS.md`"
            ),
            "a path-qualified reference is not the bare artifact and still flags: {diags:?}"
        );
        assert!(
            sup.artifacts.is_empty(),
            "the path-qualified reference produced no artifact suppression: {sup:?}"
        );
    }

    #[test]
    fn artifact_quoted_and_backticked_both_filtered() {
        // Both dark-matter shapes — a quoted path and a backticked path — are
        // filtered by the glossary.
        let config = config_with_artifacts(&["CLAUDE.md"]);
        let (diags, sup) =
            diagnose_full("Edit \"CLAUDE.md\" and also `CLAUDE.md`.\n", &config, &[]);
        assert!(
            !has_any(&diags, "CLAUDE.md"),
            "neither the quoted nor the backticked artifact mention is flagged: {diags:?}"
        );
        assert_eq!(
            sup.artifacts.get("CLAUDE.md").map(|c| c.warnings),
            Some(2),
            "both dark-matter mentions are tallied: {sup:?}"
        );
    }

    #[test]
    fn artifact_filtered_before_count_key_residual() {
        // The artifact is removed before the count-key sees it: a count-key of
        // N = 2 over the two genuine dangling references suppresses cleanly, with
        // no drift — the artifact never entered the residual.
        let content = "---\n\
            exceptions:\n  \
              stale_references:\n    \
                \"2\": \"the two genuine dangling references\"\n\
            ---\n\
            See `AGENTS.md`, `a.md`, and `b.md`.\n";
        let config = config_with_artifacts(&["AGENTS.md"]);
        let (diags, sup) = diagnose_full(content, &config, &[]);
        assert!(
            !has_any(&diags, "stale reference"),
            "the count-key of 2 covers the two genuine refs; the artifact was filtered first: {diags:?}"
        );
        assert!(
            !has_any(&diags, "expected"),
            "no drift — the artifact never entered the residual, so the residual is exactly 2: {diags:?}"
        );
        assert_eq!(
            sup.count_keys.first().map(|c| c.counts.warnings),
            Some(2),
            "the count-key residual is the two genuine refs, not three: {sup:?}"
        );
        assert_eq!(
            sup.artifacts.get("AGENTS.md").map(|c| c.warnings),
            Some(1),
            "the artifact is tallied as its own source, not folded into the count-key: {sup:?}"
        );
    }

    #[test]
    fn artifact_is_not_exceptable() {
        // An artifact filters before the exception machinery, so a frontmatter
        // `stale_references` exception keyed on the artifact name matches
        // nothing live and is flagged as unused — it is not the lever.
        let content = "---\n\
            exceptions:\n  \
              stale_references:\n    \
                \"SKILL.md\": \"trying (wrongly) to except the artifact here\"\n\
            ---\n\
            See `SKILL.md`.\n";
        let config = config_with_artifacts(&["SKILL.md"]);
        let (diags, sup) = diagnose_full(content, &config, &[]);
        assert!(
            has_matching(&diags, Severity::Warning, "unused exception: `SKILL.md`"),
            "the exception keyed on the artifact matches nothing live (the glossary filtered it first): {diags:?}"
        );
        assert!(
            sup.exceptions.is_none(),
            "the artifact was not suppressed by the exception: {sup:?}"
        );
        assert_eq!(
            sup.artifacts.get("SKILL.md").map(|c| c.warnings),
            Some(1),
            "the artifact suppression is recorded under the artifact source: {sup:?}"
        );
    }

    #[test]
    fn no_glossary_keeps_current_behaviour() {
        // With an empty glossary the artifact name flags exactly as before.
        let config = Config::default();
        let (diags, sup) = diagnose_full("See `AGENTS.md`.\n", &config, &[]);
        assert!(
            has_matching(&diags, Severity::Warning, "stale reference: `AGENTS.md`"),
            "an empty glossary leaves the name to flag normally: {diags:?}"
        );
        assert!(
            sup.artifacts.is_empty(),
            "an empty glossary records no artifact suppression: {sup:?}"
        );
    }
}
