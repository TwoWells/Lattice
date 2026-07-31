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
    external_exists: &dyn Fn(&Path) -> ExternalExistence,
    lookup: &ExceptionLookup,
    out: &mut Vec<Diagnostic>,
) {
    let bare_paths = tree.bare_paths();
    for bare in &bare_paths {
        // An external-namespace reference (`{Name}/…`) is resolved existence-
        // only against its alias directory, never dir/root-joined (issue 030),
        // and regardless of extension — a cross-repo directory or non-`.md`
        // file is a real reference too.
        if route_external_reference(
            config,
            external_exists,
            config.policy.stale_references,
            rel_path,
            bare.line,
            None,
            &bare.path,
            lookup,
            out,
        )
        .is_some()
        {
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
                config,
                external_exists,
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
///
/// This is the message for a path with no known citation. When the path *does*
/// exist under a configured alias directory,
/// [`build_external_citation_steer`] replaces the generic lesson with the
/// concrete spelling (issue 073).
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

/// Emit the steering variant of the stale-reference diagnostic: the dangling
/// path exists under a configured `[external]` alias directory, so the message
/// names the exact citation to write instead of teaching the `{repo}/…` form
/// generically (issue 073).
///
/// Distinct from [`build_stale_reference`] only in wording and evidence: that
/// message says the external form *exists*, this one says *this path is one* and
/// spells it. Everything else is deliberately identical — the `stale reference:`
/// prefix keeps [`classify_028_lint`] routing it to the same ledger row and
/// exception namespace, `reference` is still the key the exception lookup
/// matches verbatim, and severity tracks [`StaleReferencePolicy`] with
/// `Disabled` returning `None`. Steering changes the message, never the tier.
fn build_external_citation_steer(
    policy: StaleReferencePolicy,
    rel_path: &Path,
    line: usize,
    span: Option<Span>,
    reference: &str,
    alias: &str,
    citation: &str,
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
            "stale reference: `{reference}` — path exists in external `{alias}` — cite it as `{citation}`"
        ),
        span,
    })
}

/// Rewrite a dark-matter reference as the workspace-relative path to probe under
/// an alias directory, or `None` when it has no external-citation form.
///
/// Collapses `.` and `..` by pure component arithmetic over the `/`-separated
/// markdown reference grammar, and drops the leading `/` of a root-relative
/// citation — which must go before the probe, because [`Path::join`] with an
/// absolute argument *discards* the alias directory and would `stat` a
/// filesystem-absolute path instead. A reference that escapes the workspace root
/// (`../sibling.md`) has no citation form under any alias and returns `None`,
/// mirroring [`candidate_exists`]'s rejection of the same shape.
///
/// The result doubles as the citation spelling's path part, so it is rebuilt
/// with `/` rather than reusing [`block::normalize_path`]'s `PathBuf`: the
/// message quotes a markdown reference, whose separator is `/` on every
/// platform.
fn external_citation_candidate(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.trim_start_matches('/').split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            normal => parts.push(normal),
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

/// Test whether a dangling reference names a path that exists under a configured
/// `[external]` alias directory, and if so return that alias and the exact
/// citation spelling to write (issue 073).
///
/// This reads the alias table in the direction nothing read it before.
/// [`route_external_reference`] consults it to validate a `{Name}/…` citation
/// someone already wrote; nothing connected a path already typed to an alias
/// already configured, so the generic message taught the escape without evidence
/// that *this* path was an instance of it — and a hand-written exception stayed
/// the cheaper local move (issue 066 accumulated nine of them, every one
/// resolvable under an alias already in `.lattice.toml`).
///
/// **Ordering is structural, not conditional.** The only caller is
/// [`route_stale_reference`], which every 028 emit site reaches solely in the
/// branch where intra-repo resolution has *already* failed. A reference that
/// resolves in this workspace can never be steered into a cross-repo citation —
/// that would be a regression, not a hint.
///
/// **Only `Present` claims a citation.** The probe reuses [`resolve_external`],
/// so the tri-state oracle degrades exactly as decision 010's tiers do: an
/// absent alias directory (partial checkout, or CI holding only this repo) and a
/// failed `stat` (issue 050) both yield no steer and fall through to the generic
/// message, rather than assert a path exists somewhere that could not be checked.
///
/// **Multiple matches: first match wins.** `config.external` is a [`BTreeMap`],
/// so the scan is alphabetical and deterministic. Every match is equally
/// correct — an external reference is existence-only and edge-free (decision
/// 010), so a path present under two alias directories genuinely has two valid
/// citations — and the message's job is to name *one* concrete spelling;
/// offering both would hand back the choice this steering exists to remove.
///
/// **Narrower than the citation machinery it borrows.** Decision 016 makes an
/// explicit `{Name}/…` reference existence-checked regardless of extension, but
/// a candidate reaching here is already `.md`-shaped by [`looks_like_path`]'s
/// intra-repo grammar, so a bare `schema.txt` sitting in an alias directory is
/// never steered — the dark-matter scan never sees it. That asymmetry *is*
/// decision 016's own rationale: the brace is the opt-in mark that removes the
/// is-this-a-reference ambiguity, and an unbraced non-`.md` path still carries
/// it.
fn steer_to_external_citation<'cfg>(
    config: &'cfg Config,
    external_exists: &dyn Fn(&Path) -> ExternalExistence,
    reference: &str,
) -> Option<(&'cfg str, String)> {
    if config.external.is_empty() {
        return None;
    }
    let (path, fragment) = split_path_fragment(reference);
    let candidate = external_citation_candidate(path)?;
    for alias in config.external.keys() {
        if resolve_external(config, external_exists, alias, &candidate) == ExternalResolution::Valid
        {
            // The `#fragment` is carried through verbatim: it is a heading
            // anchor, irrelevant to existence but part of the spelling the
            // author should write.
            let citation = fragment.map_or_else(
                || format!("{{{alias}}}/{candidate}"),
                |frag| format!("{{{alias}}}/{candidate}#{frag}"),
            );
            return Some((alias.as_str(), citation));
        }
    }
    None
}

/// Route a dangling-reference stale diagnostic through the exception lookup.
///
/// Builds the stale-reference diagnostic for `reference` and hands it to
/// [`ExceptionLookup::route`], so a literal `stale_references` exception
/// suppresses it, an active count-key buffers it, or it passes through to `out` —
/// the single seam every stale-reference emit site now shares (issue 031, issue
/// 036).
///
/// Being that single seam, it is also where the external-citation steering is
/// decided (issue 073): reaching here *is* the proof that intra-repo resolution
/// failed, so the check cannot fire on a live local reference. When the path
/// exists under a configured alias directory the message names that citation;
/// otherwise it is the generic stale message. The exception key, the ledger row,
/// and the severity are identical either way.
#[allow(
    clippy::too_many_arguments,
    reason = "routing context parameters are distinct concerns"
)]
fn route_stale_reference(
    config: &Config,
    external_exists: &dyn Fn(&Path) -> ExternalExistence,
    policy: StaleReferencePolicy,
    rel_path: &Path,
    line: usize,
    span: Option<Span>,
    reference: &str,
    lookup: &ExceptionLookup,
    out: &mut Vec<Diagnostic>,
) {
    // Both builders return `None` under `Disabled`, so this changes nothing that
    // is emitted — it keeps the steering probe's `stat`s off a path whose result
    // is discarded.
    if policy == StaleReferencePolicy::Disabled {
        return;
    }
    let diag = match steer_to_external_citation(config, external_exists, reference) {
        Some((alias, citation)) => {
            build_external_citation_steer(policy, rel_path, line, span, reference, alias, &citation)
        }
        None => build_stale_reference(policy, rel_path, line, span, reference),
    };
    if let Some(diag) = diag {
        lookup.route(ExceptionLint::StaleReferences, reference, diag, out);
    }
}

/// Build the stale-reference diagnostic for a dangling **external** `{Name}/…`
/// reference (issue 030, tier 4: a defined, present alias whose target is
/// missing under it).
///
/// Distinct from [`build_stale_reference`]: that message frames an intra-repo
/// dangle ("no such markdown file under this root") and teaches the `{repo}/…`
/// escape — both wrong here, where the reference *is* already `{repo}/…`, was
/// resolved against the alias directory rather than this root, and may name a
/// directory or non-`.md` file. This message instead names the alias and the
/// directory it resolved to, so the fix (correct the path in the aliased repo,
/// or repoint the alias) is unambiguous. Severity tracks [`StaleReferencePolicy`]
/// identically, and `Disabled` returns `None`.
fn build_external_stale_reference(
    policy: StaleReferencePolicy,
    rel_path: &Path,
    line: usize,
    span: Option<Span>,
    reference: &str,
    alias: &str,
    config: &Config,
) -> Option<Diagnostic> {
    let severity = match policy {
        StaleReferencePolicy::Disabled => return None,
        StaleReferencePolicy::Hint => Severity::Hint,
        StaleReferencePolicy::Warn => Severity::Warning,
        StaleReferencePolicy::Deny => Severity::Error,
    };
    // `Stale` is reached only for a defined alias, so the lookup is present; the
    // fallback keeps the message coherent rather than panicking if that ever
    // changes.
    let dir = config
        .external
        .get(alias)
        .map_or_else(String::new, |p| p.display().to_string());

    Some(Diagnostic {
        file: rel_path.to_path_buf(),
        line,
        severity,
        message: format!(
            "stale reference: `{reference}` — external alias `{alias}` resolves to `{dir}`, but no such file or directory exists there; fix the path in that repo (or repoint the `{alias}` alias in .lattice.toml), or except it (see `lattice help config`)"
        ),
        span,
    })
}

/// Build the cannot-verify diagnostic for an external `{Name}/…` reference
/// whose existence `stat` failed (issue 050,
/// [`ExternalResolution::Unverifiable`]).
///
/// A failed `stat` is not "absent": degrading it to the exempt tier silently
/// converted an I/O flake into exemption and misreported the reference's
/// frontmatter exception as unused. Surfacing it keeps the failure visible; a
/// `{Name}/…`-keyed `stale_references` exception still suppresses it, so an
/// excepted reference reports as suppressed, not unused. The `stale
/// reference:` prefix keeps [`classify_028_lint`] routing it to the same
/// ledger row and exception namespace as the resolution it stands in for.
/// Severity tracks [`StaleReferencePolicy`], mirroring
/// [`build_external_stale_reference`].
fn build_external_unverifiable_reference(
    policy: StaleReferencePolicy,
    rel_path: &Path,
    line: usize,
    span: Option<Span>,
    reference: &str,
    alias: &str,
    config: &Config,
) -> Option<Diagnostic> {
    let severity = match policy {
        StaleReferencePolicy::Disabled => return None,
        StaleReferencePolicy::Hint => Severity::Hint,
        StaleReferencePolicy::Warn => Severity::Warning,
        StaleReferencePolicy::Deny => Severity::Error,
    };
    // `Unverifiable` is reached only for a defined alias, so the lookup is
    // present; the fallback keeps the message coherent rather than panicking
    // if that ever changes.
    let dir = config
        .external
        .get(alias)
        .map_or_else(String::new, |p| p.display().to_string());

    Some(Diagnostic {
        file: rel_path.to_path_buf(),
        line,
        severity,
        message: format!(
            "stale reference: `{reference}` — external alias `{alias}` resolves to `{dir}`, but checking existence there failed (stat error, not \"absent\"); if this persists, check permissions on that directory"
        ),
        span,
    })
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
    external_exists: &dyn Fn(&Path) -> ExternalExistence,
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
                // existence-only against its alias directory (issue 030),
                // regardless of extension.
                if route_external_reference(
                    config,
                    external_exists,
                    stale,
                    rel_path,
                    line,
                    Some(child.span),
                    inner,
                    lookup,
                    out,
                )
                .is_some()
                {
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
                        config,
                        external_exists,
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
    external_exists: &dyn Fn(&Path) -> ExternalExistence,
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
    external_exists: &dyn Fn(&Path) -> ExternalExistence,
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
                    // existence-only against its alias directory (issue 030),
                    // regardless of extension.
                    if route_external_reference(
                        config,
                        external_exists,
                        stale,
                        rel_path,
                        line_num,
                        Some(span),
                        inner,
                        lookup,
                        out,
                    )
                    .is_some()
                    {
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
                            config,
                            external_exists,
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
/// Shapes that are not workspace paths at all are rejected outright (no
/// make-it-a-link hint, no stale-reference warning): a `~`-leading token
/// (home-relative, out of the repo, e.g. `~/Projects/Archive/AGENTS.md`); a
/// token containing `<` or `>` (a placeholder, e.g. `<name>/SKILL.md`); a token
/// containing `*` (a glob, e.g. `NN_*.md`); and a token containing an ellipsis —
/// `…` (U+2026) or `...` — which is documentation shorthand for a path shape
/// (e.g. the `{repo}/…` syntax this tool teaches), not a real file.
///
/// An external-namespace token (`{Name}/…`, issue 030) is admitted regardless
/// of extension — the `.md` scope guards against linkifying non-graph intra-repo
/// paths, but an external reference is never a local link or a graph edge
/// (decision 010), so a cross-repo directory or non-`.md` file
/// (`{Archive}/docs`) is still existence-checked against its alias directory.
/// The ellipsis exclusion still applies, so the `{Name}/…` placeholder itself
/// stays exempt while a concrete `{Name}/path` resolves.
fn looks_like_path(s: &str) -> bool {
    let path = split_path_fragment(s).0;
    !path.is_empty()
        && !path.starts_with("//")
        && !path.starts_with('~')
        && !path.contains(' ')
        && !path.contains('<')
        && !path.contains('>')
        && !path.contains('*')
        && !path.contains('…')
        && !path.contains("...")
        && (path.contains('/') || path.contains('.'))
        && (Path::new(path).extension().is_some_and(|ext| ext == "md")
            || block::external_namespace(path).is_some())
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
    /// The alias is defined but a `stat` failed while resolving, so existence
    /// is unknown — the reference is neither validated nor exempted (issue
    /// 050). Surfaced as its own diagnostic rather than degraded to
    /// [`Exempt`](Self::Exempt), which masked an I/O failure as exemption and
    /// misreported the reference's frontmatter exception as unused.
    Unverifiable,
}

/// Resolve an external-namespace reference to its tiered disposition.
///
/// `alias`/`rest` come from [`block::external_namespace`]; `external_exists`
/// `stat`s an absolute filesystem path (decision 010 — existence-only, edge-free: the
/// aliased directory is touched by `stat` alone, never read, parsed, or
/// indexed). The tiers (issue 030):
///
/// 1. alias undefined → [`Exempt`](ExternalResolution::Exempt);
/// 2. alias defined, its directory absent → [`Exempt`](ExternalResolution::Exempt);
/// 3. alias defined, directory present, file present → [`Valid`](ExternalResolution::Valid);
/// 4. alias defined, directory present, file missing → [`Stale`](ExternalResolution::Stale).
///
/// A `stat` that *fails* (as opposed to answering "absent") lands in none of
/// the four tiers: either check reporting
/// [`Unknown`](ExternalExistence::Unknown) makes the disposition
/// [`Unverifiable`](ExternalResolution::Unverifiable) (issue 050).
fn resolve_external(
    config: &Config,
    external_exists: &dyn Fn(&Path) -> ExternalExistence,
    alias: &str,
    rest: &str,
) -> ExternalResolution {
    let Some(dir) = config.external.get(alias) else {
        return ExternalResolution::Exempt;
    };
    match external_exists(dir) {
        ExternalExistence::Absent => ExternalResolution::Exempt,
        ExternalExistence::Unknown => ExternalResolution::Unverifiable,
        ExternalExistence::Present => match external_exists(&dir.join(rest)) {
            ExternalExistence::Present => ExternalResolution::Valid,
            ExternalExistence::Absent => ExternalResolution::Stale,
            ExternalExistence::Unknown => ExternalResolution::Unverifiable,
        },
    }
}

/// Resolve and route an external-namespace reference, if `reference` is one.
///
/// Returns `Some(())` when `reference` is an external `{Name}/…` token — the
/// caller has nothing further to do, because an external reference is
/// existence-only and never a local link, so it skips both the make-it-a-link
/// nudge and intra-repo resolution (issue 030, decision 010). On the stale tier
/// (a defined, present alias whose target is missing) it emits the
/// external-specific stale diagnostic, and on the unverifiable disposition (a
/// `stat` failure, issue 050) the cannot-verify one — both routed through the
/// exception lookup so a `{Name}/…`-keyed `stale_references` exception still
/// suppresses them (issue 031). Returns `None` when the token is not external,
/// so the caller falls through to ordinary `.md` resolution.
///
/// `reference` is the displayed token; any `#fragment` is stripped before
/// recognition and existence resolution (the fragment is a heading anchor and
/// does not affect whether the file exists), mirroring the intra-repo arms.
#[allow(
    clippy::too_many_arguments,
    reason = "routing context parameters are distinct concerns"
)]
fn route_external_reference(
    config: &Config,
    external_exists: &dyn Fn(&Path) -> ExternalExistence,
    policy: StaleReferencePolicy,
    rel_path: &Path,
    line: usize,
    span: Option<Span>,
    reference: &str,
    lookup: &ExceptionLookup,
    out: &mut Vec<Diagnostic>,
) -> Option<()> {
    let path = split_path_fragment(reference).0;
    let (alias, rest) = block::external_namespace(path)?;
    let diag = match resolve_external(config, external_exists, alias, rest) {
        ExternalResolution::Exempt | ExternalResolution::Valid => None,
        ExternalResolution::Stale => {
            build_external_stale_reference(policy, rel_path, line, span, reference, alias, config)
        }
        ExternalResolution::Unverifiable => build_external_unverifiable_reference(
            policy, rel_path, line, span, reference, alias, config,
        ),
    };
    if let Some(diag) = diag {
        lookup.route(ExceptionLint::StaleReferences, reference, diag, out);
    }
    Some(())
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
mod tests;
