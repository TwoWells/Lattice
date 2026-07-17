// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! The workspace-aggregate half of the `[[override]]` machinery (issue 037,
//! decision 012 part 2), shared by both diagnostic surfaces (decision 023).
//!
//! The per-file *level* half of an override lives in
//! [`Config::effective_policy`](crate::config::Config::effective_policy) and is
//! applied inside the shared structural collect — it reaches every surface by
//! construction. This module owns the other half: the `{ expect = N }`
//! aggregate tripwires and the `disabled` freeze attribution. It computes
//! **verdicts as data** — glob attribution ([`resolve_last_match`]), member
//! counting ([`crate::structural::classify_028_lint`] over live diagnostics),
//! and the per-entry matched / drifted / freeze decision — so the two consumers
//! cannot drift (issue 064):
//!
//! - `lint::run` renders verdicts as ledger rows and `.lattice.toml:` warning
//!   strings (the CLI presentation);
//! - the LSP server holds one adjudicated [`OverrideVerdicts`] per root,
//!   recomputed at save-point commitments, and filters every published set
//!   through it via [`suppress_matched`].

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::config::{BarePathOverride, Override, StaleReferenceOverride};
use crate::fm::ExceptionLint;
use crate::structural::{self, SeverityCounts};
use crate::validation::Diagnostic;

/// The adjudicated state of every override entry over one workspace's live
/// diagnostic set — the workspace-aggregate pass, as data.
///
/// Produced by [`adjudicate`] at a commitment point (a `lint` run reads disk
/// once, so its whole run is one commitment; the server re-adjudicates at
/// `didOpen` / `didSave` / watched-files batches per decision 023) and consumed
/// by [`suppress_matched`] plus each surface's renderer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OverrideVerdicts {
    /// Per-entry-per-lint verdicts, ordered lint-major (`stale_references`
    /// first, then `bare_paths`), freezes before expects within a lint, entry
    /// order within each group — the CLI ledger's row order.
    pub entries: Vec<EntryVerdict>,
    /// Indices (into the config's `overrides`) of entries whose glob set
    /// matches no workspace file — stale config, flagged regardless of mode
    /// (the config analogue of the unused exception, decision 012).
    pub unused: Vec<usize>,
}

/// One override entry's verdict for one 028-family lint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryVerdict {
    /// Index of the entry in the config's `overrides` slice.
    pub entry: usize,
    /// The lint this verdict adjudicates.
    pub lint: ExceptionLint,
    /// Matched / drifted / freeze.
    pub kind: VerdictKind,
    /// The workspace-relative member files attributed to this entry for this
    /// lint (those whose last matching entry naming the lint is this one).
    pub members: Vec<PathBuf>,
}

/// The decision an aggregate reached for one entry and lint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerdictKind {
    /// `{ expect = N }` whose live member count equals `N`: the members' live
    /// diagnostics of this lint suppress (they become ledger rows).
    Matched {
        /// The declared count.
        expect: usize,
    },
    /// `{ expect = N }` whose live member count differs: the override is inert
    /// (diagnostics stay live) and the drift is flagged.
    Drifted {
        /// The declared count.
        expect: usize,
        /// The live count the aggregate actually saw.
        found: usize,
    },
    /// The `disabled` freeze level: the per-file policy already silenced the
    /// lint for the members, so there is nothing to suppress here — carried so
    /// the CLI ledger can report what the freeze hid.
    Freeze,
}

/// Run the workspace-aggregate override pass over one workspace's live
/// diagnostics, returning the per-entry verdicts as data.
///
/// `files` is the workspace's full membership (workspace-relative paths) —
/// aggregates are computed over the glob's full match set, independent of any
/// display scope. `diagnostics` is the live set the per-file collect produced
/// (after frontmatter carve-outs); it is only read — suppression is the
/// separate [`suppress_matched`] step, so a holder of verdicts can filter later
/// sets through them without re-deciding (decision 023's held verdict).
#[must_use]
pub fn adjudicate<'p>(
    overrides: &[Override],
    files: impl IntoIterator<Item = &'p Path>,
    diagnostics: &[Diagnostic],
) -> OverrideVerdicts {
    let mut verdicts = OverrideVerdicts::default();
    if overrides.is_empty() {
        return verdicts;
    }
    let files: Vec<&Path> = files.into_iter().collect();

    // Unused-override: a glob set matching zero workspace files is stale (a
    // tree was renamed or removed).
    for (idx, ov) in overrides.iter().enumerate() {
        if !files.iter().any(|p| ov.matches(p)) {
            verdicts.unused.push(idx);
        }
    }

    // The two 028-family lints are adjudicated independently.
    for lint in [ExceptionLint::StaleReferences, ExceptionLint::BarePaths] {
        // Per entry: the member files attributed to it for this lint
        // (last-match-wins, decision 012), split by resolution kind.
        let mut expect_members: Vec<Vec<PathBuf>> = overrides.iter().map(|_| Vec::new()).collect();
        let mut freeze_members: Vec<Vec<PathBuf>> = overrides.iter().map(|_| Vec::new()).collect();
        for path in &files {
            match resolve_last_match(overrides, lint, path) {
                Some((idx, OverrideResolution::Expect)) => {
                    expect_members[idx].push(path.to_path_buf());
                }
                Some((idx, OverrideResolution::Freeze)) => {
                    freeze_members[idx].push(path.to_path_buf());
                }
                Some((_, OverrideResolution::OtherLevel)) | None => {}
            }
        }

        // Freeze verdicts first, then expect verdicts, mirroring the CLI
        // ledger's row order so the renderer is a plain iteration.
        for (idx, members) in freeze_members.into_iter().enumerate() {
            if members.is_empty() {
                continue;
            }
            verdicts.entries.push(EntryVerdict {
                entry: idx,
                lint,
                kind: VerdictKind::Freeze,
                members,
            });
        }
        for (idx, members) in expect_members.into_iter().enumerate() {
            if members.is_empty() {
                continue;
            }
            let Some(expect) = expect_count(&overrides[idx], lint) else {
                continue;
            };
            let member_set: HashSet<&Path> = members.iter().map(PathBuf::as_path).collect();
            let found = diagnostics
                .iter()
                .filter(|d| {
                    member_set.contains(d.file.as_path())
                        && structural::classify_028_lint(&d.message) == Some(lint)
                })
                .count();
            let kind = if found == expect {
                VerdictKind::Matched { expect }
            } else {
                VerdictKind::Drifted { expect, found }
            };
            verdicts.entries.push(EntryVerdict {
                entry: idx,
                lint,
                kind,
                members,
            });
        }
    }

    verdicts
}

/// Remove from `diagnostics` every member finding a [`VerdictKind::Matched`]
/// verdict suppresses, returning the per-verdict suppressed tallies (parallel
/// to `verdicts.entries`; non-matched entries stay zero).
///
/// This is the one suppression decision, applied by both surfaces: `lint`
/// applies it to the set it just adjudicated (and renders the tallies as ledger
/// rows); the server applies a **held** verdict to each freshly computed set at
/// the publish seam — counts move mid-edit, the decision does not
/// (decision 023).
pub fn suppress_matched(
    verdicts: &OverrideVerdicts,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<SeverityCounts> {
    let mut tallies = vec![SeverityCounts::default(); verdicts.entries.len()];
    for (verdict, tally) in verdicts.entries.iter().zip(tallies.iter_mut()) {
        let VerdictKind::Matched { .. } = verdict.kind else {
            continue;
        };
        let member_set: HashSet<&Path> = verdict.members.iter().map(PathBuf::as_path).collect();
        diagnostics.retain(|d| {
            if member_set.contains(d.file.as_path())
                && structural::classify_028_lint(&d.message) == Some(verdict.lint)
            {
                tally.record(d.severity);
                false
            } else {
                true
            }
        });
    }
    tallies
}

/// The resolution of a single 028-family `lint` for one file under the
/// overrides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverrideResolution {
    /// The winning entry sets this lint to `{ expect = N }`.
    Expect,
    /// The winning entry sets this lint to the `disabled` freeze level.
    Freeze,
    /// The winning entry sets this lint to a non-freeze level (warn/deny/hint).
    /// Already applied per-file; no workspace-aggregate action.
    OtherLevel,
}

/// Find the last override entry that names `lint` and matches `path`, with its
/// resolution kind (last-match-wins, decision 012). `None` when no entry both
/// matches and names this lint.
fn resolve_last_match(
    overrides: &[Override],
    lint: ExceptionLint,
    path: &Path,
) -> Option<(usize, OverrideResolution)> {
    let mut winner = None;
    for (idx, ov) in overrides.iter().enumerate() {
        if !ov.matches(path) {
            continue;
        }
        let resolution = match lint {
            ExceptionLint::StaleReferences => ov.stale_references.map(|m| match m {
                StaleReferenceOverride::Expect(_) => OverrideResolution::Expect,
                StaleReferenceOverride::Level(crate::config::StaleReferencePolicy::Disabled) => {
                    OverrideResolution::Freeze
                }
                StaleReferenceOverride::Level(_) => OverrideResolution::OtherLevel,
            }),
            ExceptionLint::BarePaths => ov.bare_paths.map(|m| match m {
                BarePathOverride::Expect(_) => OverrideResolution::Expect,
                BarePathOverride::Level(crate::config::BarePathPolicy::Disabled) => {
                    OverrideResolution::Freeze
                }
                BarePathOverride::Level(_) => OverrideResolution::OtherLevel,
            }),
        };
        if let Some(resolution) = resolution {
            winner = Some((idx, resolution));
        }
    }
    winner
}

/// The `expect = N` count an override sets for `lint`, if any.
fn expect_count(ov: &Override, lint: ExceptionLint) -> Option<usize> {
    match lint {
        ExceptionLint::StaleReferences => match ov.stale_references {
            Some(StaleReferenceOverride::Expect(n)) => Some(n),
            _ => None,
        },
        ExceptionLint::BarePaths => match ov.bare_paths {
            Some(BarePathOverride::Expect(n)) => Some(n),
            _ => None,
        },
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests use expect and panic for clarity"
)]
mod tests {
    use crate::config::StaleReferencePolicy;
    use crate::validation::Severity;

    use super::*;

    /// Build an `[[override]]` entry from globs and per-lint modes (no disk).
    fn entry(globs: &[&str], stale_references: Option<StaleReferenceOverride>) -> Override {
        Override {
            paths: globs
                .iter()
                .map(|g| glob::Pattern::new(g).expect("test glob compiles"))
                .collect(),
            raw_paths: globs.iter().map(ToString::to_string).collect(),
            stale_references,
            bare_paths: None,
            hint: None,
        }
    }

    /// A stale-reference diagnostic anchored on `file`.
    fn stale(file: &str) -> Diagnostic {
        Diagnostic {
            file: PathBuf::from(file),
            line: 1,
            severity: Severity::Warning,
            message: "stale reference: `gone.md` — no such markdown file".to_string(),
            span: None,
        }
    }

    #[test]
    fn adjudicate_matched_expect_yields_matched_verdict() {
        let overrides = vec![entry(
            &["sweep/**"],
            Some(StaleReferenceOverride::Expect(2)),
        )];
        let files = [Path::new("sweep/a.md"), Path::new("sweep/b.md")];
        let diagnostics = vec![stale("sweep/a.md"), stale("sweep/b.md")];
        let verdicts = adjudicate(&overrides, files, &diagnostics);
        assert_eq!(
            verdicts.entries.len(),
            1,
            "one expect entry yields one verdict: {verdicts:?}"
        );
        assert_eq!(
            verdicts.entries[0].kind,
            VerdictKind::Matched { expect: 2 },
            "a live count equal to the declared count is a match"
        );
        assert_eq!(
            verdicts.entries[0].members,
            vec![PathBuf::from("sweep/a.md"), PathBuf::from("sweep/b.md")],
            "both glob-matched files are members"
        );
    }

    #[test]
    fn adjudicate_count_mismatch_yields_drifted_verdict() {
        let overrides = vec![entry(
            &["sweep/**"],
            Some(StaleReferenceOverride::Expect(5)),
        )];
        let files = [Path::new("sweep/a.md")];
        let diagnostics = vec![stale("sweep/a.md")];
        let verdicts = adjudicate(&overrides, files, &diagnostics);
        assert_eq!(
            verdicts.entries[0].kind,
            VerdictKind::Drifted {
                expect: 5,
                found: 1
            },
            "a live count differing from the declared count is drift"
        );
    }

    #[test]
    fn adjudicate_flags_unused_entry() {
        let overrides = vec![entry(
            &["archive/**"],
            Some(StaleReferenceOverride::Level(
                StaleReferencePolicy::Disabled,
            )),
        )];
        let files = [Path::new("live/doc.md")];
        let verdicts = adjudicate(&overrides, files, &[]);
        assert_eq!(
            verdicts.unused,
            vec![0],
            "a glob matching no workspace file is flagged unused"
        );
        assert!(
            verdicts.entries.is_empty(),
            "an unused entry attributes no members: {verdicts:?}"
        );
    }

    #[test]
    fn adjudicate_expect_shadows_disabled_attributes_to_the_expect() {
        // The expect-resets-level shape (issues 064, decision 023): an outer
        // freeze and a nested later expect. Files under the nested glob resolve
        // to the expect (last-match-wins) and are counted; files under only the
        // outer glob stay frozen.
        let overrides = vec![
            entry(
                &["archive/**"],
                Some(StaleReferenceOverride::Level(
                    StaleReferencePolicy::Disabled,
                )),
            ),
            entry(
                &["archive/sweep/**"],
                Some(StaleReferenceOverride::Expect(1)),
            ),
        ];
        let files = [
            Path::new("archive/notes.md"),
            Path::new("archive/sweep/readme.md"),
        ];
        // The frozen file emits nothing (per-file level); the shadowed file
        // emits at base level and its finding is counted by the aggregate.
        let diagnostics = vec![stale("archive/sweep/readme.md")];
        let verdicts = adjudicate(&overrides, files, &diagnostics);
        let freeze = verdicts
            .entries
            .iter()
            .find(|v| v.kind == VerdictKind::Freeze)
            .expect("the outer entry keeps its freeze verdict");
        assert_eq!(
            freeze.members,
            vec![PathBuf::from("archive/notes.md")],
            "only the un-shadowed file stays attributed to the freeze"
        );
        let expect = verdicts
            .entries
            .iter()
            .find(|v| matches!(v.kind, VerdictKind::Matched { .. }))
            .expect("the nested expect matches its live count");
        assert_eq!(
            expect.members,
            vec![PathBuf::from("archive/sweep/readme.md")],
            "the shadowed file is the expect's member"
        );
    }

    #[test]
    fn suppress_matched_removes_only_member_findings_of_the_lint() {
        let overrides = vec![entry(
            &["sweep/**"],
            Some(StaleReferenceOverride::Expect(1)),
        )];
        let files = [Path::new("sweep/a.md"), Path::new("other.md")];
        let mut diagnostics = vec![
            stale("sweep/a.md"),
            stale("other.md"),
            Diagnostic {
                file: PathBuf::from("sweep/a.md"),
                line: 2,
                severity: Severity::Warning,
                message: "empty heading".to_string(),
                span: None,
            },
        ];
        let verdicts = adjudicate(&overrides, files, &diagnostics);
        let tallies = suppress_matched(&verdicts, &mut diagnostics);
        assert_eq!(
            tallies[0].warnings, 1,
            "the matched member finding is tallied"
        );
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.file == Path::new("sweep/a.md")
                    && d.message.starts_with("stale reference")),
            "the member's lint finding is suppressed: {diagnostics:?}"
        );
        assert!(
            diagnostics.iter().any(|d| d.file == Path::new("other.md")),
            "a non-member file's finding survives: {diagnostics:?}"
        );
        assert!(
            diagnostics.iter().any(|d| d.message == "empty heading"),
            "a member file's non-028 finding survives: {diagnostics:?}"
        );
    }

    #[test]
    fn suppress_matched_leaves_drifted_findings_live() {
        let overrides = vec![entry(
            &["sweep/**"],
            Some(StaleReferenceOverride::Expect(5)),
        )];
        let files = [Path::new("sweep/a.md")];
        let mut diagnostics = vec![stale("sweep/a.md")];
        let verdicts = adjudicate(&overrides, files, &diagnostics);
        let tallies = suppress_matched(&verdicts, &mut diagnostics);
        assert_eq!(diagnostics.len(), 1, "a drifted verdict suppresses nothing");
        assert!(
            tallies[0].is_empty(),
            "a drifted verdict tallies nothing: {tallies:?}"
        );
    }

    #[test]
    fn held_verdict_suppresses_a_grown_live_set() {
        // Decision 023: the held verdict filters by membership, not by count. A
        // mid-edit set that crossed the tripwire (2 -> 3) still suppresses in
        // full under the verdict held from the last commitment.
        let overrides = vec![entry(
            &["sweep/**"],
            Some(StaleReferenceOverride::Expect(2)),
        )];
        let files = [Path::new("sweep/a.md")];
        let committed = vec![stale("sweep/a.md"), stale("sweep/a.md")];
        let verdicts = adjudicate(&overrides, files, &committed);
        assert_eq!(
            verdicts.entries[0].kind,
            VerdictKind::Matched { expect: 2 },
            "the commitment adjudicated a match"
        );
        let mut mid_edit = vec![
            stale("sweep/a.md"),
            stale("sweep/a.md"),
            stale("sweep/a.md"),
        ];
        suppress_matched(&verdicts, &mut mid_edit);
        assert!(
            mid_edit.is_empty(),
            "the held matched verdict suppresses every member finding, count moved or not: {mid_edit:?}"
        );
    }
}
