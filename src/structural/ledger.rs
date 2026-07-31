// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Exception routing and the suppression ledger.
//!
//! Two halves of one accounting question, and neither of them scans anything.
//!
//! **Routing** (issue 031, decision 011) decides which emitted diagnostic each
//! declared `exceptions` entry claims, and reports the entries that claimed
//! nothing — an exception that no longer matches is itself a finding, because a
//! stale exception is how a real defect goes quiet.
//!
//! **The ledger** (issue 036, decision 012 part B) records what the routing
//! hid: per source, by severity, so a workspace can see the shape of what it
//! has silenced rather than only the shape of what it has left. Count-key
//! suppressions and artifact suppressions are tallied separately, because they
//! answer different questions about the same document.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::fm::{CountKey, ExceptionEntry, ExceptionLint, Exceptions};
use crate::validation::{Diagnostic, Severity};

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
    pub fn new(
        entries: &'a [ExceptionEntry],
        count_key: Option<&'a CountKey>,
        active: bool,
    ) -> Self {
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
pub struct ExceptionLookup<'a> {
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
    pub fn new(
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
    pub fn route(
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
    pub fn resolve_count_keys(&self, rel_path: &Path, out: &mut Vec<Diagnostic>) {
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
    pub fn emit_unmatched(&self, rel_path: &Path, out: &mut Vec<Diagnostic>) {
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
    pub fn into_suppressions(self, rel_path: &Path) -> FileSuppressions {
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
