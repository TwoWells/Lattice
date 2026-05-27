// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Batch lint runner for the CLI.
//!
//! Scans a workspace, runs all validation checks, and writes diagnostics
//! in compiler-compatible format.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};

use crate::validation::{self, Diagnostic, Severity};
use crate::workspace::Workspace;

/// Run all validation checks on the workspace rooted at `start`.
///
/// Writes diagnostics to `out` and returns `true` if any error-level
/// diagnostics were emitted.
///
/// # Errors
///
/// Returns an error if the workspace cannot be scanned or output cannot
/// be written.
pub fn run(start: &Path, out: &mut impl Write) -> Result<bool> {
    let workspace = Workspace::scan(start).context("failed to scan workspace")?;

    if !workspace.has_config() {
        writeln!(
            out,
            "note: no .lattice.toml found, graph validation disabled"
        )?;
        return Ok(false);
    }

    // Config errors are hard failures in CLI mode.
    let mut has_errors = workspace.config_error().is_some_and(|config_err| {
        let _ = writeln!(out, ".lattice.toml: error: {config_err}");
        true
    });

    let diagnostics = validation::collect_all(&workspace);

    for diag in &diagnostics {
        if diag.severity == Severity::Error {
            has_errors = true;
        }
        writeln!(out, "{}", format_diagnostic(diag))?;
    }

    Ok(has_errors)
}

/// Format a diagnostic in `path:line: severity: message` format.
fn format_diagnostic(diag: &Diagnostic) -> String {
    let severity = match diag.severity {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Info => "info",
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

    /// Run lint on a temp dir and return (`has_errors`, output).
    fn run_lint(dir: &TempDir) -> (bool, String) {
        let mut buf = Vec::new();
        let has_errors = run(dir.path(), &mut buf).expect("run should succeed");
        let output = String::from_utf8(buf).expect("output should be utf-8");
        (has_errors, output)
    }

    #[test]
    fn format_error_diagnostic() {
        let diag = Diagnostic {
            file: PathBuf::from("docs/foo.md"),
            line: 5,
            severity: Severity::Error,
            message: "target does not exist".to_string(),
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
    fn unknown_inverse_predicate_reports_error() {
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
            "unknown inverse predicate should produce errors"
        );
        assert!(
            output.contains("unknown inverse predicate"),
            "output should mention the unknown predicate: {output}"
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
}
