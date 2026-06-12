// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Batch lint runner for the CLI.
//!
//! Scans a workspace, runs all validation checks, and writes diagnostics
//! in compiler-compatible format.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

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
/// # Errors
///
/// Returns an error if the workspace cannot be scanned or output cannot
/// be written.
pub fn run(start: &Path, strict: bool, out: &mut impl Write) -> Result<bool> {
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

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests use expect and panic for clarity"
)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use tempfile::TempDir;

    use super::*;

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

    /// Run lint on a temp dir and return (`failed`, output).
    fn run_lint(dir: &TempDir) -> (bool, String) {
        run_lint_with(dir, false)
    }

    /// Run lint on a temp dir with an explicit `strict` flag.
    fn run_lint_with(dir: &TempDir, strict: bool) -> (bool, String) {
        let mut buf = Vec::new();
        let failed = run(dir.path(), strict, &mut buf).expect("run should succeed");
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
        let failed = run(start, false, &mut buf).expect("run should succeed");
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
}
