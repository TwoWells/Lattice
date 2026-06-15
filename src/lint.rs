// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Batch lint runner for the CLI.
//!
//! Scans a workspace, runs all validation checks, and writes diagnostics
//! in compiler-compatible format.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::config::{BarePathOverride, Override, StaleReferenceOverride};
use crate::fm::{ExceptionLint, Exceptions};
use crate::structural::{self, SeverityCounts};
use crate::validation::{self, Diagnostic, Severity};
use crate::workspace::Workspace;

/// Run all validation checks on the workspace, scoped to `start`.
///
/// `start` is both the discovery hint and the lint scope. The workspace root
/// (and `.lattice.toml`) is discovered by walking up from `start`, and the
/// whole workspace is scanned so the cross-file graph the backlink and
/// connectivity checks need is built over every file. Only the *emitted*
/// diagnostics — and the exit code derived from them — are restricted to files
/// at or under `start`; in-scope diagnostics that depend on out-of-scope files
/// (e.g. a missing backlink anchored on an in-scope source whose reciprocal
/// lives elsewhere) therefore stay correct.
///
/// `start` is normalized before filtering: a leading `./`, a trailing slash,
/// and the relative-vs-absolute distinction are erased by canonicalizing
/// against the discovered root, so `archive`, `archive/`, `./archive/`, and the
/// absolute form all scope identically. `.` (or any path that resolves to the
/// root) lints the whole workspace.
///
/// Structural diagnostics always run. Graph diagnostics require
/// `.lattice.toml`. Writes diagnostics to `out` and returns `true` if the run
/// should fail the exit code: any in-scope error-level diagnostic, or — when
/// `strict` is set — any in-scope warning-level diagnostic. Info/hint
/// diagnostics never fail the exit code.
///
/// After the diagnostics, unless `quiet` is set, prints the **suppression
/// ledger** (issue 036, decision 012): a summary of what each suppression source
/// (frontmatter literal exceptions, count-keys) hid in the in-scope files,
/// broken out by severity. A turned-off blanket is never silent; `--quiet` drops
/// it for machine-readable CI output.
///
/// # Errors
///
/// Returns an error if the workspace cannot be scanned or output cannot
/// be written.
pub fn run(start: &Path, strict: bool, quiet: bool, out: &mut impl Write) -> Result<bool> {
    let workspace = Workspace::scan(start).context("failed to scan workspace")?;

    // The lint scope is `start` expressed relative to the discovered root.
    // `None` means "the whole workspace" (`start` resolves to the root itself,
    // or could not be normalized — in which case we never silently drop
    // diagnostics). The cross-file graph above is always built over every file;
    // only the emitted set below is restricted to this scope.
    let scope = scope_relative_to_root(start, workspace.root());

    let mut failed = false;
    let mut diagnostics = Vec::new();

    // Structural diagnostics: always run. Read from the per-file cache the
    // workspace scan populated (issue 013 — stage 2).
    for (path, file_data) in workspace.files() {
        diagnostics.extend(file_data.structural.iter().cloned());

        // Frontmatter parse diagnostics are structural (unconditional).
        for pd in &file_data.parse_diagnostics {
            let severity = match pd.severity {
                crate::fm::FmSeverity::Error => Severity::Error,
                crate::fm::FmSeverity::Warning => Severity::Warning,
            };
            diagnostics.push(Diagnostic {
                file: path.clone(),
                line: pd.line,
                severity,
                message: format!("frontmatter: {}", pd.message),
                span: None,
            });
        }
    }

    // Graph diagnostics: gated by .lattice.toml.
    if workspace.has_config() {
        // Config errors are hard failures in CLI mode.
        if let Some(config_err) = workspace.config_error() {
            let _ = writeln!(out, ".lattice.toml: error: {config_err}");
            failed = true;
        }
        diagnostics.extend(validation::collect_all(&workspace));
    } else {
        writeln!(
            out,
            "note: no .lattice.toml found, graph validation disabled"
        )?;
    }

    // Subtree-override pass (issue 037, decision 012 part 2). The level-override
    // (per-file) path already ran inside the workspace structural collect via
    // `Config::effective_policy`; this is the *workspace-aggregate* half — the
    // `{ expect = N }` tripwires and the freeze ledger rows. It consumes the
    // assembled diagnostics plus the override globs and returns: the override
    // ledger rows (freeze / matched-expect), and the workspace-level messages
    // (expect drift, unused-override). Aggregates are computed over the whole
    // workspace (the glob's full match set), independent of the lint scope; the
    // scope filter below still governs what prints.
    let override_outcome = apply_subtree_overrides(&workspace, &mut diagnostics);

    diagnostics.sort_by(|a, b| a.file.cmp(&b.file).then(a.line.cmp(&b.line)));

    for diag in &diagnostics {
        // Filter by the file the diagnostic is anchored on (issue 024). A
        // missing-backlink diagnostic is anchored on its source file (issue
        // 001), so this scopes to "diagnostics about in-scope files" — not
        // "diagnostics whose every dependency is in scope".
        if !in_scope(&diag.file, scope.as_deref()) {
            continue;
        }
        let gates =
            diag.severity == Severity::Error || (strict && diag.severity == Severity::Warning);
        if gates {
            failed = true;
        }
        writeln!(out, "{}", format_diagnostic(diag))?;
    }

    // Workspace-level override messages (issue 037): expect-drift and
    // unused-override flags. They have no document line, so they print as
    // `.lattice.toml:` warnings naming the override entry (CLI-first; the LSP
    // publish path is a deferred follow-up — these do not map to one markdown
    // file). They gate like warnings under `--strict`.
    for message in &override_outcome.messages {
        if strict {
            failed = true;
        }
        writeln!(out, ".lattice.toml: warning: {message}")?;
    }

    // The suppression ledger (issue 036, decision 012): summarize what was
    // suppressed in the in-scope files, by source and severity. `--quiet` drops
    // it. Per-file rows are scoped identically to the diagnostics above; the
    // workspace-level subtree-override rows (issue 037) are appended after them
    // — an override is a central, workspace-wide policy statement, not a
    // per-file suppression, so it is shown whenever it suppressed anything.
    if !quiet {
        let mut rows = collect_ledger_rows(&workspace, scope.as_deref());
        rows.extend(override_outcome.rows);
        write_ledger(&rows, out)?;
    }

    Ok(failed)
}

/// Express `start` as a path relative to the workspace `root`, for scoping.
///
/// Returns `None` when the lint should cover the whole workspace: `start`
/// resolves to the root itself, or it cannot be normalized against the root (in
/// which case scoping is skipped rather than risk silently dropping
/// diagnostics). Otherwise returns the workspace-relative scope — a directory
/// prefix or a single file — with a leading `./`, a trailing slash, and the
/// relative-vs-absolute distinction all erased by canonicalization.
fn scope_relative_to_root(start: &Path, root: &Path) -> Option<PathBuf> {
    // Canonicalize both sides so `.`, `./`, trailing slashes, symlinks, and the
    // relative/absolute distinction resolve to one comparable absolute form.
    // The scan already succeeded from `start`, so it exists on disk and this
    // resolves; on the unexpected failure path we return `None` (whole
    // workspace) so a normalization gap never reads as a false-clean.
    let abs_start = std::fs::canonicalize(start).ok()?;
    let abs_root = std::fs::canonicalize(root).ok()?;
    let rel = abs_start.strip_prefix(&abs_root).ok()?;
    if rel.as_os_str().is_empty() {
        None
    } else {
        Some(rel.to_path_buf())
    }
}

/// Whether a diagnostic anchored on workspace-relative `file` is within `scope`.
///
/// `scope` is `None` for a whole-workspace lint (everything is in scope). A
/// `Some` scope matches when `file` equals it (a single-file scope) or is
/// nested under it (a directory scope) — component-wise, so `archive` matches
/// `archive/cli-design.md` but never `archived/x.md`.
fn in_scope(file: &Path, scope: Option<&Path>) -> bool {
    scope.is_none_or(|scope| file.starts_with(scope))
}

/// Format a diagnostic in `path:line: severity: message` format.
fn format_diagnostic(diag: &Diagnostic) -> String {
    let severity = match diag.severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Info => "info",
        Severity::Hint => "hint",
    };
    format!(
        "{}:{}: {}: {}",
        diag.file.display(),
        diag.line,
        severity,
        diag.message
    )
}

// ---------------------------------------------------------------------------
// Suppression ledger (issue 036, decision 012 part B)
// ---------------------------------------------------------------------------

/// The suppression source a ledger row reports.
///
/// Issue 037 adds the subtree-override variant and issue 038 the artifact
/// variant; the renderer iterates rows by kind, so each source slots in without
/// restructuring the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LedgerSource {
    /// A file's frontmatter literal exceptions.
    Exceptions,
    /// A per-document count-key.
    CountKey,
    /// A `[[override]]` subtree entry (freeze or matched `expect = N`).
    Override,
    /// A `[graph] artifacts` glossary member (decision 013, issue 038),
    /// aggregated repo-wide by artifact name.
    Artifact,
}

impl LedgerSource {
    /// The header label naming this source's row count (`N <name>`).
    const fn header_name(self) -> &'static str {
        match self {
            Self::Exceptions => "exceptions",
            Self::CountKey => "count-key",
            Self::Override => "overrides",
            Self::Artifact => "artifacts",
        }
    }
}

/// One row of the suppression ledger: a label, what it suppressed by severity,
/// and a source-specific detail suffix.
#[derive(Debug, Clone)]
struct LedgerRow {
    /// The source kind, for the header tally and ordering.
    source: LedgerSource,
    /// The row label — a file path (exceptions) or the shared reason (count-key).
    label: String,
    /// What this row suppressed, by severity.
    counts: SeverityCounts,
    /// The trailing detail — `count-key (31)` or `exceptions (2)`.
    detail: String,
}

/// Gather the suppression ledger rows from the workspace, scoped like the
/// diagnostics.
///
/// One row per in-scope file with matched literal exceptions (labeled by path)
/// and one row per in-scope count-key that suppressed its residual (labeled by
/// its shared reason). The artifact-glossary suppressions (decision 013, issue
/// 038) are **aggregated repo-wide** into one row per artifact name (labeled by
/// the name), since artifact names scatter across folders rather than cluster in
/// one file. Files in source order (the workspace is a `BTreeMap`), so the ledger
/// is deterministic.
fn collect_ledger_rows(workspace: &Workspace, scope: Option<&Path>) -> Vec<LedgerRow> {
    let mut rows = Vec::new();
    // Artifact suppressions are not per-file rows: a glossary name recurs across
    // dozens of files, so the ledger shows one row per name folding every
    // in-scope file's tally for it (decision 013's grain).
    let mut artifact_totals: std::collections::BTreeMap<String, SeverityCounts> =
        std::collections::BTreeMap::new();
    for (path, file_data) in workspace.files() {
        if !in_scope(path, scope) {
            continue;
        }
        let sup = &file_data.suppressions;
        if sup.is_empty() {
            continue;
        }
        if let Some(ex) = &sup.exceptions {
            rows.push(LedgerRow {
                source: LedgerSource::Exceptions,
                label: path.display().to_string(),
                counts: ex.counts,
                detail: format!("exceptions ({})", ex.matched_entries),
            });
        }
        for ck in &sup.count_keys {
            rows.push(LedgerRow {
                source: LedgerSource::CountKey,
                label: ck.reason.clone(),
                counts: ck.counts,
                detail: format!("count-key ({})", ck.raw),
            });
        }
        for (name, counts) in &sup.artifacts {
            artifact_totals
                .entry(name.clone())
                .or_default()
                .add(*counts);
        }
    }
    // Emit the aggregated artifact rows after the per-file rows, in name order
    // (the `BTreeMap` is sorted) so the ledger stays deterministic.
    for (name, counts) in artifact_totals {
        rows.push(LedgerRow {
            source: LedgerSource::Artifact,
            label: name,
            counts,
            detail: "artifact".to_string(),
        });
    }
    rows
}

// ---------------------------------------------------------------------------
// Subtree-override workspace pass (issue 037, decision 012 part 2)
// ---------------------------------------------------------------------------

/// What the subtree-override workspace pass produced: ledger rows to merge and
/// workspace-level messages to print.
///
/// The level-override (per-file) half ran earlier inside the structural collect
/// (`Config::effective_policy`); this carries the workspace-aggregate half — the
/// `freeze` and matched-`expect` ledger rows, and the expect-drift /
/// unused-override flags.
struct OverrideOutcome {
    /// Override ledger rows (freeze + matched-expect), appended after the
    /// per-file rows.
    rows: Vec<LedgerRow>,
    /// Workspace-level messages (expect drift, unused-override), printed as
    /// `.lattice.toml:` warnings.
    messages: Vec<String>,
}

/// Run the workspace-aggregate half of the subtree-override feature (issue 037).
///
/// Mutates `diagnostics` in place: a matched `{ expect = N }` aggregate has its
/// live diagnostics **removed** (they become a ledger row); a drifted one leaves
/// them in place and adds a drift message. Returns the freeze / matched-expect
/// ledger rows and the workspace-level messages (drift, unused-override).
///
/// The two override mechanisms stay strictly separated: the per-file level path
/// already happened (it shaped what is in `diagnostics`); this function reads
/// those diagnostics but never re-levels them — it only counts, suppresses, or
/// flags as a workspace aggregate.
fn apply_subtree_overrides(
    workspace: &Workspace,
    diagnostics: &mut Vec<Diagnostic>,
) -> OverrideOutcome {
    let overrides = &workspace.config().overrides;
    let mut outcome = OverrideOutcome {
        rows: Vec::new(),
        messages: Vec::new(),
    };
    if overrides.is_empty() {
        return outcome;
    }

    // Unused-override: a glob set matching zero workspace files is stale (a tree
    // was renamed or removed). Flagged regardless of mode, the config analogue
    // of the unused-exception (decision 012).
    for ov in overrides {
        let matches_any = workspace.files().keys().any(|p| ov.matches(p));
        if !matches_any {
            outcome.messages.push(format!(
                "unused override: `{}` matches no files — remove it, or restore the path if the tree moved (see `lattice help config`){}",
                ov.label(),
                ov.hint_suffix()
            ));
        }
    }

    // The two 028-family lints are handled independently.
    for lint in [ExceptionLint::StaleReferences, ExceptionLint::BarePaths] {
        apply_override_lint(workspace, overrides, lint, diagnostics, &mut outcome);
    }

    outcome
}

/// Apply the subtree overrides for one 028-family `lint` across the workspace.
///
/// Resolves, per file, the last matching override entry that names this lint
/// (last-match-wins). A `Level` resolution was already applied per-file in the
/// structural collect — here it only contributes the **freeze** ledger row when
/// it is `disabled`. An `Expect(N)` resolution makes the file a member of that
/// entry's aggregate; once every member is gathered, the aggregate is decided:
/// total `== N` suppresses the lint's live diagnostics in those files (and adds
/// one ledger row), total `!= N` leaves them and adds one drift message.
fn apply_override_lint(
    workspace: &Workspace,
    overrides: &[Override],
    lint: ExceptionLint,
    diagnostics: &mut Vec<Diagnostic>,
    outcome: &mut OverrideOutcome,
) {
    // Per expect-entry: the member files attributed to it (those whose last
    // matching entry for this lint is that entry's `Expect`).
    let mut expect_members: Vec<Vec<PathBuf>> = overrides.iter().map(|_| Vec::new()).collect();
    // Per freeze-entry: the member files whose last match is that entry's
    // `disabled` level — for the freeze ledger row's base-level counts.
    let mut freeze_members: Vec<Vec<PathBuf>> = overrides.iter().map(|_| Vec::new()).collect();

    for path in workspace.files().keys() {
        match resolve_last_match(overrides, lint, path) {
            Some((idx, OverrideResolution::Expect)) => expect_members[idx].push(path.clone()),
            Some((idx, OverrideResolution::Freeze)) => freeze_members[idx].push(path.clone()),
            Some((_, OverrideResolution::OtherLevel)) | None => {}
        }
    }

    // Freeze rows: the lint is off for these files, so the suppressed count is
    // what it WOULD have emitted at the repo-wide level (after frontmatter
    // carve-outs). Compute that by a base-level re-collect for frozen files.
    for (idx, members) in freeze_members.iter().enumerate() {
        if members.is_empty() {
            continue;
        }
        let mut counts = SeverityCounts::default();
        for path in members {
            for diag in base_level_lint_diagnostics(workspace, path, lint) {
                counts.record(diag.severity);
            }
        }
        if !counts.is_empty() {
            outcome.rows.push(LedgerRow {
                source: LedgerSource::Override,
                label: overrides[idx].label(),
                counts,
                detail: format!("override (freeze){}", overrides[idx].hint_suffix()),
            });
        }
    }

    // Expect aggregates: count the lint's live diagnostics across the member
    // files (already in `diagnostics`, after frontmatter carve-outs), then
    // suppress or flag.
    for (idx, members) in expect_members.iter().enumerate() {
        if members.is_empty() {
            continue;
        }
        let Some(expect) = expect_count(&overrides[idx], lint) else {
            continue;
        };

        let member_set: std::collections::HashSet<&Path> =
            members.iter().map(PathBuf::as_path).collect();
        let is_member_lint = |d: &Diagnostic| {
            member_set.contains(d.file.as_path())
                && structural::classify_028_lint(&d.message) == Some(lint)
        };
        let found = diagnostics.iter().filter(|d| is_member_lint(d)).count();

        if found == expect {
            // Aggregate matches: suppress the lint's diagnostics in these files,
            // tallying them as one override ledger row.
            let mut counts = SeverityCounts::default();
            diagnostics.retain(|d| {
                if is_member_lint(d) {
                    counts.record(d.severity);
                    false
                } else {
                    true
                }
            });
            outcome.rows.push(LedgerRow {
                source: LedgerSource::Override,
                label: overrides[idx].label(),
                counts,
                detail: format!("override (expect={expect}){}", overrides[idx].hint_suffix()),
            });
        } else {
            // Drift: the override is inert (diagnostics stay), plus one flag.
            outcome.messages.push(format!(
                "override `{}` expects {expect} {} but found {found} — update the count or fix the drift (see `lattice help config`){}",
                overrides[idx].label(),
                lint.noun(),
                overrides[idx].hint_suffix()
            ));
        }
    }
}

/// The resolution of a single 028-family `lint` for one file under the overrides.
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

/// Re-collect a file's structural diagnostics at the **repo-wide** policy and
/// return only those of `lint`.
///
/// A `disabled` freeze emits nothing for the frozen lint, so its ledger row's
/// suppressed count cannot be read from the diagnostics — it is "what the lint
/// would have said at the base level". This recomputes that, with the file's
/// frontmatter applied (frontmatter wins, decision 012), using the same oracles
/// the workspace loader uses. Called only for frozen files (rare, opt-in, CLI
/// path), so the extra collect is not a hot-path cost.
fn base_level_lint_diagnostics(
    workspace: &Workspace,
    rel_path: &Path,
    lint: ExceptionLint,
) -> Vec<Diagnostic> {
    let Some(file_data) = workspace.file(rel_path) else {
        return Vec::new();
    };
    let file_exists = |target: &Path| workspace.file(target).is_some();
    let external_exists = |path: &Path| path.exists();
    let empty_exceptions = Exceptions::default();
    let exceptions = file_data
        .frontmatter
        .as_ref()
        .map_or(&empty_exceptions, |fm| &fm.exceptions);
    let (diagnostics, _) = structural::collect_with_suppressions(
        &file_data.tree,
        rel_path,
        workspace.config(),
        &file_exists,
        &external_exists,
        exceptions,
    );
    diagnostics
        .into_iter()
        .filter(|d| structural::classify_028_lint(&d.message) == Some(lint))
        .collect()
}

/// Render a `SeverityCounts` as the comma-joined `N warnings, M hints` phrase the
/// ledger uses. Zero-severity buckets are omitted; the plural `s` is dropped for
/// a count of one.
fn format_counts(counts: &SeverityCounts) -> String {
    let mut parts = Vec::new();
    for (n, singular) in [
        (counts.errors, "error"),
        (counts.warnings, "warning"),
        (counts.info, "info"),
        (counts.hints, "hint"),
    ] {
        if n == 0 {
            continue;
        }
        // `info` has no distinct plural; the others take a trailing `s`.
        if n == 1 || singular == "info" {
            parts.push(format!("{n} {singular}"));
        } else {
            parts.push(format!("{n} {singular}s"));
        }
    }
    parts.join(", ")
}

/// Write the suppression ledger (issue 036). No-op when nothing was suppressed,
/// so a clean run prints no ledger at all.
fn write_ledger(rows: &[LedgerRow], out: &mut impl Write) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }

    // Header: total suppressed by severity, then the row count per source kind.
    let mut totals = SeverityCounts::default();
    for row in rows {
        totals.add(row.counts);
    }
    let mut source_counts = Vec::new();
    for source in [
        LedgerSource::Override,
        LedgerSource::CountKey,
        LedgerSource::Exceptions,
        LedgerSource::Artifact,
    ] {
        let n = rows.iter().filter(|r| r.source == source).count();
        if n > 0 {
            source_counts.push(format!("{n} {}", source.header_name()));
        }
    }
    writeln!(
        out,
        "suppressed: {}  ({})",
        format_counts(&totals),
        source_counts.join(", ")
    )?;

    // Rows: label, padded, then the per-row counts and the source detail. The
    // label column is padded to the widest label so the counts align.
    let label_width = rows.iter().map(|r| r.label.len()).max().unwrap_or(0);
    let counts_width = rows
        .iter()
        .map(|r| format_counts(&r.counts).len())
        .max()
        .unwrap_or(0);
    for row in rows {
        let counts = format_counts(&row.counts);
        writeln!(
            out,
            "  {label:<label_width$}  {counts:<counts_width$}  {detail}",
            label = row.label,
            detail = row.detail,
        )?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests use expect and panic for clarity"
)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Mutex;

    use tempfile::TempDir;

    use super::*;

    /// Serializes tests that mutate the process-global current working
    /// directory. `std::env::set_current_dir` affects the whole process, so two
    /// CWD-mutating tests running concurrently (plain `cargo test` shares the
    /// process) would race. Poison-tolerant: a panic in one CWD test must not
    /// wedge the others.
    static CWD_LOCK: Mutex<()> = Mutex::new(());

    /// Create a workspace with the given files and return the temp dir.
    fn setup(files: &[(&str, &str)]) -> TempDir {
        let dir = TempDir::new().expect("create temp dir");
        fs::create_dir(dir.path().join(".git")).expect("create .git");
        for (path, content) in files {
            let full = dir.path().join(path);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).expect("create parent dirs");
            }
            fs::write(&full, content).expect("write file");
        }
        dir
    }

    /// Run lint on a temp dir and return (`failed`, output). The ledger is
    /// suppressed (`quiet`) so diagnostic-output assertions stay focused on the
    /// diagnostics; the ledger has its own tests.
    fn run_lint(dir: &TempDir) -> (bool, String) {
        run_lint_with(dir, false)
    }

    /// Run lint on a temp dir with an explicit `strict` flag, ledger suppressed.
    fn run_lint_with(dir: &TempDir, strict: bool) -> (bool, String) {
        let mut buf = Vec::new();
        let failed = run(dir.path(), strict, true, &mut buf).expect("run should succeed");
        let output = String::from_utf8(buf).expect("output should be utf-8");
        (failed, output)
    }

    /// Run lint on a temp dir with the suppression ledger enabled (the default
    /// user experience) and return (`failed`, output).
    fn run_lint_with_ledger(dir: &TempDir) -> (bool, String) {
        let mut buf = Vec::new();
        let failed = run(dir.path(), false, false, &mut buf).expect("run should succeed");
        let output = String::from_utf8(buf).expect("output should be utf-8");
        (failed, output)
    }

    #[test]
    fn format_error_diagnostic() {
        let diag = Diagnostic {
            file: PathBuf::from("docs/foo.md"),
            line: 5,
            severity: Severity::Error,
            message: "target does not exist".to_string(),
            span: None,
        };
        assert_eq!(
            format_diagnostic(&diag),
            "docs/foo.md:5: error: target does not exist",
            "error diagnostic should format as path:line: error: message"
        );
    }

    #[test]
    fn format_warning_diagnostic() {
        let diag = Diagnostic {
            file: PathBuf::from("index.md"),
            line: 1,
            severity: Severity::Warning,
            message: "expected backlink".to_string(),
            span: None,
        };
        assert_eq!(
            format_diagnostic(&diag),
            "index.md:1: warning: expected backlink",
            "warning diagnostic should format as path:line: warning: message"
        );
    }

    #[test]
    fn format_info_diagnostic() {
        let diag = Diagnostic {
            file: PathBuf::from("notes.md"),
            line: 12,
            severity: Severity::Info,
            message: "no explicit predicate".to_string(),
            span: None,
        };
        assert_eq!(
            format_diagnostic(&diag),
            "notes.md:12: info: no explicit predicate",
            "info diagnostic should format as path:line: info: message"
        );
    }

    #[test]
    fn clean_workspace_no_output() {
        let dir = setup(&[
            (".lattice.toml", ""),
            (
                "index.md",
                "---\nbacklinks:\n  referenced_by:\n    - other.md\n---\n\n[other](other.md \"references\")\n",
            ),
            (
                "other.md",
                "---\nbacklinks:\n  referenced_by:\n    - index.md\n---\n\n[index](index.md \"references\")\n",
            ),
        ]);
        let (has_errors, output) = run_lint(&dir);
        assert!(!has_errors, "clean workspace should have no errors");
        assert!(
            output.is_empty(),
            "clean workspace should produce no output"
        );
    }

    #[test]
    fn broken_link_reports_error() {
        let dir = setup(&[
            (".lattice.toml", ""),
            ("index.md", "[missing](gone.md \"references\")\n"),
        ]);
        let (has_errors, output) = run_lint(&dir);
        assert!(has_errors, "broken link should produce errors");
        assert!(
            output.contains("error:"),
            "output should contain an error diagnostic: {output}"
        );
        assert!(
            output.contains("index.md:1:"),
            "output should reference the source file and line: {output}"
        );
    }

    #[test]
    fn unknown_predicate_reports_error() {
        let dir = setup(&[
            (".lattice.toml", ""),
            ("a.md", "[b](b.md \"invented\")\n"),
            ("b.md", "# B\n"),
        ]);
        let (has_errors, output) = run_lint(&dir);
        assert!(has_errors, "unknown predicate should produce errors");
        assert!(
            output.contains("error:"),
            "output should contain an error diagnostic: {output}"
        );
    }

    #[test]
    fn warnings_only_exit_zero() {
        // Missing backlink produces a warning, not an error.
        let dir = setup(&[
            (".lattice.toml", ""),
            ("a.md", "[b](b.md \"references\")\n"),
            ("b.md", "# B\n"),
        ]);
        let (has_errors, output) = run_lint(&dir);
        assert!(
            !has_errors,
            "warnings-only workspace should not have errors"
        );
        assert!(
            output.contains("warning:"),
            "output should contain a warning diagnostic: {output}"
        );
        assert!(
            !output.contains("error:"),
            "output should not contain error diagnostics: {output}"
        );
    }

    #[test]
    fn strict_gates_warnings() {
        // Same fixture as `warnings_only_exit_zero`: a missing backlink is a
        // warning. Under --strict it must fail the exit code while staying a
        // warning-level diagnostic in the output.
        let dir = setup(&[
            (".lattice.toml", ""),
            ("a.md", "[b](b.md \"references\")\n"),
            ("b.md", "# B\n"),
        ]);
        let (failed, output) = run_lint_with(&dir, true);
        assert!(
            failed,
            "strict mode should fail the exit code on warnings: {output}"
        );
        assert!(
            output.contains("warning:"),
            "the gated diagnostic should still print as a warning: {output}"
        );
        assert!(
            !output.contains("error:"),
            "strict gating must not relabel warnings as errors: {output}"
        );
    }

    #[test]
    fn invalid_config_reports_error() {
        let dir = setup(&[
            (".lattice.toml", "[policy]\npredicates = \"bogus\"\n"),
            ("index.md", "# Hello\n"),
        ]);
        let (has_errors, output) = run_lint(&dir);
        assert!(has_errors, "invalid config should produce errors");
        assert!(
            output.contains(".lattice.toml: error:"),
            "output should reference the config file: {output}"
        );
    }

    #[test]
    fn unknown_backlink_predicate_reports_error() {
        let dir = setup(&[
            (".lattice.toml", ""),
            (
                "a.md",
                "---\nbacklinks:\n  invented_by:\n    - b.md\n---\n\n# A\n",
            ),
        ]);
        let (has_errors, output) = run_lint(&dir);
        assert!(
            has_errors,
            "predicate known in neither direction should produce errors"
        );
        assert!(
            output.contains("unknown backlink predicate"),
            "output should mention the unknown predicate: {output}"
        );
    }

    #[test]
    fn forward_label_backlink_key_is_known() {
        // A backlink keyed by a forward predicate (decision 008) is valid —
        // it derives from a reciprocal `superseded_by` forward link.
        let dir = setup(&[
            (".lattice.toml", ""),
            (
                "a.md",
                "---\nbacklinks:\n  supersedes:\n    - b.md\n---\n\n# A\n",
            ),
            ("b.md", "[a](a.md \"superseded_by\")\n"),
        ]);
        let (has_errors, output) = run_lint(&dir);
        assert!(
            !has_errors,
            "forward-label backlink key should not error: {output}"
        );
    }

    #[test]
    fn no_config_prints_note() {
        let dir = setup(&[("a.md", "[b](b.md \"references\")\n"), ("b.md", "# B\n")]);
        let (has_errors, output) = run_lint(&dir);
        assert!(!has_errors, "no config should not produce errors");
        assert!(
            output.contains("no .lattice.toml found"),
            "output should note that graph validation is disabled: {output}"
        );
    }

    // -- Path-scoped lint (issue 024) --

    /// Run lint with an explicit `start` path (not the workspace root), so a
    /// scoped invocation can be exercised. Returns (`failed`, output).
    fn run_lint_start(start: &Path) -> (bool, String) {
        let mut buf = Vec::new();
        let failed = run(start, false, true, &mut buf).expect("run should succeed");
        let output = String::from_utf8(buf).expect("output should be utf-8");
        (failed, output)
    }

    /// A fixture with an error in `sub/dir/file.md` (a broken link) and an
    /// independent error in the out-of-scope sibling `other/sibling.md`. The
    /// `.lattice.toml` enables the graph tier, mirroring the dogfood repro.
    fn scoped_fixture() -> TempDir {
        setup(&[
            (".lattice.toml", ""),
            ("sub/dir/file.md", "[broken](nope.md \"references\")\n"),
            ("other/sibling.md", "[gone](missing.md \"references\")\n"),
        ])
    }

    #[test]
    fn scoped_lint_reports_in_scope_error_under_every_path_form() {
        // The ticket pins this: a known error in sub/dir/file.md must report
        // that diagnostic and exit 1 under every spelling of the scope, and must
        // never leak the out-of-scope sibling's error.
        let dir = scoped_fixture();
        let forms: [PathBuf; 5] = [
            PathBuf::from("sub"),
            PathBuf::from("sub/"),
            PathBuf::from("./sub/"),
            PathBuf::from("sub/dir/file.md"),
            dir.path().join("sub"),
        ];

        for form in &forms {
            // Forms are relative to the workspace root, so run from there.
            let start = if form.is_absolute() {
                form.clone()
            } else {
                dir.path().join(form)
            };
            let (failed, output) = run_lint_start(&start);
            assert!(
                failed,
                "scope `{}` contains an error and must exit non-zero: {output}",
                form.display()
            );
            assert!(
                output.contains("error:"),
                "scope `{}` must surface the in-scope error: {output}",
                form.display()
            );
            assert!(
                output.contains("file.md:1:"),
                "scope `{}` must anchor the diagnostic on sub/dir/file.md: {output}",
                form.display()
            );
            assert!(
                !output.contains("sibling.md"),
                "scope `{}` must not leak the out-of-scope sibling's diagnostic: {output}",
                form.display()
            );
        }
    }

    #[test]
    fn scoped_lint_bare_relative_from_cwd_reports_and_fails() {
        // Issue 024 (reopened): the dangerous half. A path genuinely relative to
        // the process CWD with no leading `./` (`sub`, `sub/`, `sub/dir/file.md`)
        // used to make root discovery walk up to the empty path and lint zero
        // files — exit 0, no output, a silent false-clean. With the scan root
        // absolutized, each bare-relative spelling must report the in-scope error
        // and exit 1.
        //
        // The pre-existing `scoped_lint_reports_in_scope_error_under_every_path_form`
        // does `dir.path().join(form)`, making every form absolute — so this
        // genuinely-relative-from-CWD branch was never exercised. This test sets
        // the CWD to the fixture root and lints relative spellings directly.
        let dir = setup(&[
            (".lattice.toml", ""),
            ("sub/dir/file.md", "[broken](nope.md \"references\")\n"),
            ("other/sibling.md", "[gone](missing.md \"references\")\n"),
            ("clean/ok.md", "# All good\n"),
        ]);

        let _guard = CWD_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let original = std::env::current_dir().expect("read original cwd");
        std::env::set_current_dir(dir.path()).expect("chdir to fixture root");

        // Bare-relative directory and single-file spellings — none carry `./`.
        let forms = ["sub", "sub/", "sub/dir/file.md"];
        let mut results = Vec::new();
        for form in forms {
            results.push((form, run_lint_start(Path::new(form))));
        }
        // A clean bare-relative scope must still exit 0 (the empty-path branch
        // must never read as either a false-clean *or* a spurious failure).
        let clean = run_lint_start(Path::new("clean"));

        std::env::set_current_dir(&original).expect("restore original cwd");

        for (form, (failed, output)) in results {
            assert!(
                failed,
                "bare-relative scope `{form}` contains an error and must exit non-zero: {output}"
            );
            assert!(
                output.contains("file.md:1:"),
                "bare-relative scope `{form}` must surface the in-scope error: {output}"
            );
            assert!(
                !output.contains("sibling.md"),
                "bare-relative scope `{form}` must not leak the out-of-scope sibling: {output}"
            );
        }

        let (clean_failed, clean_output) = clean;
        assert!(
            !clean_failed,
            "a clean bare-relative scope must exit zero, not false-clean nor spurious-fail: {clean_output}"
        );
        assert!(
            !clean_output.contains("sibling.md"),
            "a clean bare-relative scope must not leak the out-of-scope sibling: {clean_output}"
        );
    }

    #[test]
    fn scoped_lint_single_file_form_isolates_the_file() {
        // The single-file scope must report only that file, excluding a sibling
        // error in the same directory.
        let dir = setup(&[
            (".lattice.toml", ""),
            ("sub/dir/file.md", "[broken](nope.md \"references\")\n"),
            ("sub/dir/neighbor.md", "[also](absent.md \"references\")\n"),
        ]);
        let (failed, output) = run_lint_start(&dir.path().join("sub/dir/file.md"));
        assert!(
            failed,
            "single-file scope with an error must exit non-zero: {output}"
        );
        assert!(
            output.contains("file.md:1:"),
            "single-file scope must report the targeted file: {output}"
        );
        assert!(
            !output.contains("neighbor.md"),
            "single-file scope must not report a sibling in the same dir: {output}"
        );
    }

    #[test]
    fn scoped_lint_clean_subtree_exits_zero() {
        // Exit-code parity: a clean scope exits 0 even though an out-of-scope
        // sibling has an error that `lattice lint .` would report.
        let dir = setup(&[
            (".lattice.toml", ""),
            ("clean/ok.md", "# All good\n"),
            ("other/sibling.md", "[gone](missing.md \"references\")\n"),
        ]);
        let (failed, output) = run_lint_start(&dir.path().join("clean"));
        assert!(
            !failed,
            "a clean scope must exit zero regardless of out-of-scope errors: {output}"
        );
        assert!(
            !output.contains("error:"),
            "a clean scope must not emit error diagnostics: {output}"
        );
        assert!(
            !output.contains("sibling.md"),
            "a clean scope must not leak the out-of-scope error: {output}"
        );
    }

    #[test]
    fn whole_workspace_still_reports_every_error() {
        // The root scope (`.` resolved to the root) must keep reporting every
        // file's diagnostics — scoping must not narrow the default lint.
        let dir = scoped_fixture();
        let (failed, output) = run_lint(&dir);
        assert!(
            failed,
            "whole-workspace lint with errors must exit non-zero"
        );
        assert!(
            output.contains("file.md:1:"),
            "whole-workspace lint must report the sub-tree error: {output}"
        );
        assert!(
            output.contains("sibling.md:1:"),
            "whole-workspace lint must report the sibling error too: {output}"
        );
    }

    #[test]
    fn scoped_lint_in_scope_error_depends_on_out_of_scope_file() {
        // The cross-file graph must still be built over the whole workspace: a
        // missing-backlink warning is anchored on the in-scope source but is
        // only computed because the out-of-scope target exists and is reachable.
        // source.md (in scope) links to target.md (out of scope) with a
        // reciprocal predicate but target.md has no backlink, so source.md gets
        // a stale/missing-backlink warning that a sub-tree-only scan could not
        // produce.
        let dir = setup(&[
            (".lattice.toml", ""),
            ("sub/source.md", "[t](../target.md \"references\")\n"),
            ("target.md", "# Target\n"),
        ]);
        let (_failed, output) = run_lint_start(&dir.path().join("sub"));
        assert!(
            output.contains("source.md:1:"),
            "the in-scope source's graph diagnostic (computed from the whole-workspace graph) must survive scoping: {output}"
        );
        assert!(
            !output.contains("target.md:"),
            "the out-of-scope target must not appear in a scoped lint: {output}"
        );
    }

    #[test]
    fn scope_relative_to_root_normalizes_path_forms() {
        // Every spelling of the same sub-tree must normalize to one scope, and
        // the root itself (and `.`) must mean "whole workspace" (`None`).
        let dir = setup(&[("sub/dir/file.md", "# X\n")]);
        let root = dir.path();

        let bare = scope_relative_to_root(&root.join("sub"), root);
        let trailing = scope_relative_to_root(&root.join("sub/"), root);
        let dotted = scope_relative_to_root(&root.join("./sub/"), root);
        assert_eq!(
            bare, trailing,
            "`sub` and `sub/` must normalize to the same scope"
        );
        assert_eq!(
            bare, dotted,
            "`sub` and `./sub/` must normalize to the same scope"
        );
        assert_eq!(
            bare,
            Some(PathBuf::from("sub")),
            "the scope must be the workspace-relative sub-tree"
        );

        assert_eq!(
            scope_relative_to_root(root, root),
            None,
            "the root itself must mean whole-workspace (no scope)"
        );
        assert_eq!(
            scope_relative_to_root(&root.join("."), root),
            None,
            "`.` must mean whole-workspace (no scope)"
        );
    }

    #[test]
    fn in_scope_matches_directory_and_file_but_not_sibling_prefix() {
        let scope = PathBuf::from("archive");
        assert!(
            in_scope(Path::new("archive/cli-design.md"), Some(&scope)),
            "a file under the scoped directory is in scope"
        );
        assert!(
            in_scope(Path::new("archive"), Some(&scope)),
            "the scoped directory itself is in scope"
        );
        assert!(
            !in_scope(Path::new("archived/x.md"), Some(&scope)),
            "a sibling sharing a name prefix must not be in scope (component-wise match)"
        );
        assert!(
            !in_scope(Path::new("other/x.md"), Some(&scope)),
            "an unrelated file must not be in scope"
        );
        assert!(
            in_scope(Path::new("anything/at/all.md"), None),
            "a None scope means the whole workspace is in scope"
        );
    }

    // -- Suppression ledger (issue 036, decision 012 part B) --

    #[test]
    fn format_counts_omits_zero_buckets_and_pluralizes() {
        let one = SeverityCounts {
            warnings: 1,
            ..SeverityCounts::default()
        };
        assert_eq!(
            format_counts(&one),
            "1 warning",
            "a count of one is singular and only the warning bucket shows"
        );
        let many = SeverityCounts {
            warnings: 84,
            hints: 12,
            ..SeverityCounts::default()
        };
        assert_eq!(
            format_counts(&many),
            "84 warnings, 12 hints",
            "multiple non-zero buckets are comma-joined and pluralized"
        );
    }

    #[test]
    fn ledger_reports_exception_counts_by_severity() {
        // A file whose frontmatter excepts a dangling stale reference: the ledger
        // reports one exception row, one suppressed warning, labeled by path.
        let dir = setup(&[(
            "intro.md",
            "---\n\
             exceptions:\n  \
               stale_references:\n    \
                 \"gone.md\": \"a worked example path, not a live reference\"\n\
             ---\n\
             See `gone.md` for details.\n",
        )]);
        let (_failed, output) = run_lint_with_ledger(&dir);
        assert!(
            output.contains("suppressed: 1 warning  (1 exceptions)"),
            "the ledger header tallies one suppressed warning from one exception source: {output}"
        );
        assert!(
            output.contains("intro.md") && output.contains("exceptions (1)"),
            "the exception row is labeled by path with the matched-entry detail: {output}"
        );
    }

    #[test]
    fn ledger_reports_count_key_row_by_reason() {
        // A count-key whose residual matches: the ledger row is labeled by the
        // shared reason and detailed as `count-key (N)`.
        let dir = setup(&[(
            "table.md",
            "---\n\
             exceptions:\n  \
               stale_references:\n    \
                 \"2\": \"the consolidation migration table\"\n\
             ---\n\
             See `a.md` and `b.md`.\n",
        )]);
        let (_failed, output) = run_lint_with_ledger(&dir);
        assert!(
            output.contains("suppressed: 2 warnings  (1 count-key)"),
            "the header tallies the count-key's two suppressed warnings: {output}"
        );
        assert!(
            output.contains("the consolidation migration table")
                && output.contains("count-key (2)"),
            "the count-key row is labeled by reason, detailed by raw key: {output}"
        );
    }

    #[test]
    fn quiet_drops_the_ledger() {
        let dir = setup(&[(
            "intro.md",
            "---\n\
             exceptions:\n  \
               stale_references:\n    \
                 \"gone.md\": \"a worked example path, not a live reference\"\n\
             ---\n\
             See `gone.md` for details.\n",
        )]);
        // Default (ledger on) shows it; `--quiet` (the `run_lint` helper) drops it.
        let (_f1, with_ledger) = run_lint_with_ledger(&dir);
        assert!(
            with_ledger.contains("suppressed:"),
            "the ledger prints by default: {with_ledger}"
        );
        let (_f2, quiet) = run_lint(&dir);
        assert!(
            !quiet.contains("suppressed:"),
            "--quiet drops the ledger: {quiet}"
        );
    }

    #[test]
    fn ledger_absent_when_nothing_suppressed() {
        let dir = setup(&[("clean.md", "# Title\n\nNo suppressions here.\n")]);
        let (_failed, output) = run_lint_with_ledger(&dir);
        assert!(
            !output.contains("suppressed:"),
            "a clean run prints no ledger: {output}"
        );
    }

    // -- Subtree overrides (issue 037, decision 012 part 2) --

    #[test]
    fn override_disables_lint_for_matching_files_only() {
        // archive/old.md and live/cur.md both quote a dead `.md` path (a stale
        // reference, warning by default). A freeze override on archive/** must
        // silence archive/old.md's stale reference while live/cur.md's stays.
        let dir = setup(&[
            (
                ".lattice.toml",
                "[[override]]\npaths = [\"archive/**\"]\nstale_references = \"disabled\"\nhint = \"frozen docs\"\n",
            ),
            ("archive/old.md", "See `gone.md` here.\n"),
            ("live/cur.md", "See `missing.md` here.\n"),
        ]);
        let (_failed, output) = run_lint(&dir);
        assert!(
            !output.contains("archive/old.md"),
            "the freeze override silences the matching file's stale reference: {output}"
        );
        assert!(
            output.contains("live/cur.md") && output.contains("stale reference"),
            "a non-matching file's stale reference is unaffected: {output}"
        );
    }

    #[test]
    fn override_raise_escalates_for_matching_files() {
        // stale_references default is warn; a deny override on strict/** must
        // escalate the matching file's stale reference to an error (and exit 1),
        // while a non-matching file stays a warning.
        let dir = setup(&[
            (
                ".lattice.toml",
                "[[override]]\npaths = [\"strict/**\"]\nstale_references = \"deny\"\n",
            ),
            ("strict/a.md", "See `gone.md` here.\n"),
            ("lax/b.md", "See `gone.md` here.\n"),
        ]);
        let (failed, output) = run_lint(&dir);
        assert!(
            failed,
            "a raise to deny must fail the exit code on the matching file: {output}"
        );
        assert!(
            output.contains("strict/a.md:1: error: stale reference"),
            "the matching file's stale reference is escalated to an error: {output}"
        );
        assert!(
            output.contains("lax/b.md:1: warning: stale reference"),
            "a non-matching file keeps the repo-wide warning level: {output}"
        );
    }

    #[test]
    fn override_expect_match_suppresses_all() {
        // Two matching files with one stale reference each: expect = 2 matches
        // the aggregate, so both are suppressed and no stale-reference
        // diagnostic prints.
        let dir = setup(&[
            (
                ".lattice.toml",
                "[[override]]\npaths = [\"sweep/**\"]\nstale_references = { expect = 2 }\n",
            ),
            ("sweep/a.md", "See `gone.md` here.\n"),
            ("sweep/b.md", "See `missing.md` here.\n"),
        ]);
        let (_failed, output) = run_lint(&dir);
        assert!(
            !output.contains("stale reference"),
            "a matched expect aggregate suppresses every member's stale reference: {output}"
        );
    }

    #[test]
    fn override_expect_drift_resurfaces_and_flags() {
        // expect = 5 but only two stale references exist: the override is inert,
        // so both diagnostics resurface AND one drift flag names the override.
        let dir = setup(&[
            (
                ".lattice.toml",
                "[[override]]\npaths = [\"sweep/**\"]\nstale_references = { expect = 5 }\n",
            ),
            ("sweep/a.md", "See `gone.md` here.\n"),
            ("sweep/b.md", "See `missing.md` here.\n"),
        ]);
        let (_failed, output) = run_lint(&dir);
        assert!(
            output.contains("sweep/a.md") && output.contains("sweep/b.md"),
            "on drift, every member diagnostic resurfaces: {output}"
        );
        assert!(
            output.contains("expects 5 stale references but found 2")
                && output.contains("sweep/**"),
            "the drift flag names the override and the expected/found counts: {output}"
        );
    }

    #[test]
    fn unused_override_flags() {
        // A glob matching zero files is flagged as an unused override.
        let dir = setup(&[
            (
                ".lattice.toml",
                "[[override]]\npaths = [\"archive/**\"]\nstale_references = \"disabled\"\n",
            ),
            ("live/doc.md", "# Live\n"),
        ]);
        let (_failed, output) = run_lint(&dir);
        assert!(
            output.contains("unused override") && output.contains("archive/**"),
            "a zero-match override is flagged, naming the glob: {output}"
        );
    }

    #[test]
    fn frontmatter_wins_over_override() {
        // Two files match an expect = 1 override. sweep/a.md's own frontmatter
        // excepts its dead reference — frontmatter wins, so that reference is
        // carved out FIRST and is never counted in the override aggregate. Only
        // sweep/b.md's live stale reference reaches the aggregate, so expect = 1
        // matches and suppresses it. If the override (not frontmatter) had
        // claimed a.md's reference, the aggregate would have seen 2 and drifted.
        let dir = setup(&[
            (
                ".lattice.toml",
                "[[override]]\npaths = [\"sweep/**\"]\nstale_references = { expect = 1 }\n",
            ),
            (
                "sweep/a.md",
                "---\nexceptions:\n  stale_references:\n    \"gone.md\": \"frontmatter wins\"\n---\nSee `gone.md` here.\n",
            ),
            ("sweep/b.md", "See `missing.md` here.\n"),
        ]);
        let (_failed, output) = run_lint(&dir);
        assert!(
            !output.contains("stale reference"),
            "frontmatter carves a.md's reference out first, then the expect = 1 aggregate suppresses b.md's: {output}"
        );
        assert!(
            !output.contains("expects 1"),
            "the aggregate counts only the frontmatter survivor (1), so it matches and does not drift: {output}"
        );
    }

    #[test]
    fn override_expect_drifts_when_frontmatter_carves_all() {
        // The mirror of `frontmatter_wins_over_override`: when frontmatter carves
        // out the only reference, the aggregate honestly sees 0. An expect = 5
        // there is genuinely stale (the subtree no longer has 5 dead refs), so it
        // drifts — frontmatter winning does not silence the tripwire, it just
        // changes what the tripwire counts.
        let dir = setup(&[
            (
                ".lattice.toml",
                "[[override]]\npaths = [\"sweep/**\"]\nstale_references = { expect = 5 }\n",
            ),
            (
                "sweep/a.md",
                "---\nexceptions:\n  stale_references:\n    \"gone.md\": \"frontmatter wins\"\n---\nSee `gone.md` here.\n",
            ),
        ]);
        let (_failed, output) = run_lint(&dir);
        assert!(
            output.contains("expects 5 stale references but found 0"),
            "with every reference carved out by frontmatter, the expect = 5 aggregate honestly drifts to 0: {output}"
        );
    }

    #[test]
    fn ledger_includes_override_rows() {
        // A freeze override and a matched expect override each contribute one
        // ledger row, labelled by glob with the freeze / expect=N detail.
        let dir = setup(&[
            (
                ".lattice.toml",
                "[[override]]\npaths = [\"archive/**\"]\nstale_references = \"disabled\"\n\n[[override]]\npaths = [\"sweep/**\"]\nstale_references = { expect = 1 }\n",
            ),
            ("archive/old.md", "See `gone.md` here.\n"),
            ("sweep/a.md", "See `missing.md` here.\n"),
        ]);
        let (_failed, output) = run_lint_with_ledger(&dir);
        assert!(
            output.contains("suppressed:") && output.contains("2 overrides"),
            "the ledger header tallies both override rows: {output}"
        );
        assert!(
            output.contains("archive/**") && output.contains("override (freeze)"),
            "the freeze override contributes a labelled ledger row: {output}"
        );
        assert!(
            output.contains("sweep/**") && output.contains("override (expect=1)"),
            "the matched expect override contributes a labelled ledger row: {output}"
        );
    }

    // -- Artifact glossary ledger (issue 038, decision 013) --

    #[test]
    fn ledger_reports_artifact_suppressions_by_severity() {
        // Two files each mention `AGENTS.md` (a glossary member that dangles in
        // this repo, so a stale-reference warning each). The ledger aggregates
        // them repo-wide into one artifact row by name, tallying two warnings.
        let dir = setup(&[
            (
                ".lattice.toml",
                "[graph]\nartifacts = [\"AGENTS.md\", \"CLAUDE.md\"]\n",
            ),
            ("intro.md", "Put hooks in `AGENTS.md`.\n"),
            ("docs/guide.md", "Also see `AGENTS.md`.\n"),
        ]);
        let (_failed, output) = run_lint_with_ledger(&dir);
        assert!(
            output.contains("suppressed: 2 warnings  (1 artifacts)"),
            "the header tallies the two repo-wide artifact suppressions as one artifact source: {output}"
        );
        assert!(
            output.contains("AGENTS.md") && output.contains("artifact"),
            "the artifact row is labelled by the name with the `artifact` detail: {output}"
        );
        // The make-it-a-link / stale-reference diagnostics themselves are gone.
        assert!(
            !output.contains("stale reference"),
            "no artifact mention surfaces as a diagnostic: {output}"
        );
    }

    #[test]
    fn quiet_drops_the_artifact_ledger() {
        let dir = setup(&[
            (".lattice.toml", "[graph]\nartifacts = [\"AGENTS.md\"]\n"),
            ("intro.md", "Put hooks in `AGENTS.md`.\n"),
        ]);
        let (_f1, with_ledger) = run_lint_with_ledger(&dir);
        assert!(
            with_ledger.contains("suppressed:") && with_ledger.contains("artifact"),
            "the artifact ledger prints by default: {with_ledger}"
        );
        let (_f2, quiet) = run_lint(&dir);
        assert!(
            !quiet.contains("suppressed:"),
            "--quiet drops the artifact ledger: {quiet}"
        );
    }

    #[test]
    fn override_last_match_wins_freeze_then_raise() {
        // x/** freezes bare_paths; x/strict/** raises it to deny. A file under
        // x/strict resolving a make-it-a-link bare path must error (the later
        // entry wins); a file only under x/** is silenced.
        let dir = setup(&[
            (
                ".lattice.toml",
                "[[override]]\npaths = [\"x/**\"]\nbare_paths = \"disabled\"\n\n[[override]]\npaths = [\"x/strict/**\"]\nbare_paths = \"deny\"\n",
            ),
            ("x/strict/a.md", "See \"target.md\" for details.\n"),
            ("x/other.md", "See \"target.md\" for details.\n"),
            ("target.md", "# Target\n"),
        ]);
        let (failed, output) = run_lint(&dir);
        assert!(
            failed,
            "the later deny entry wins for the overlapping file and fails the exit code: {output}"
        );
        assert!(
            output.contains("x/strict/a.md:1: error:"),
            "the file under the last-matching deny entry escalates to an error: {output}"
        );
        assert!(
            !output.contains("x/other.md"),
            "a file matched only by the first freeze entry is silenced: {output}"
        );
    }

    #[test]
    fn override_drift_gates_under_strict() {
        // An expect-drift flag is a warning; under --strict it must fail the exit
        // code like any other warning.
        let dir = setup(&[
            (
                ".lattice.toml",
                "[[override]]\npaths = [\"sweep/**\"]\nstale_references = { expect = 5 }\n",
            ),
            ("sweep/a.md", "See `gone.md` here.\n"),
        ]);
        let (failed, output) = run_lint_with(&dir, true);
        assert!(failed, "the drift flag gates under --strict: {output}");
        assert!(
            output.contains("expects 5 stale references but found 1"),
            "the drift flag is present: {output}"
        );
    }
}
