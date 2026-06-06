// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Link graph validation.
//!
//! Checks forward links and backlink consistency across the workspace:
//! target existence, predicate vocabulary, predicate policy compliance,
//! and frontmatter backlink reconciliation.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use crate::block::{self, HeadingId, LinkKind};
use crate::config::{Config, FragmentAlgorithm, PredicatePolicy};
use crate::span::Span;
use crate::workspace::Workspace;

/// Diagnostic severity level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Fatal issue that must be fixed.
    Error,
    /// Non-fatal issue worth addressing.
    Warning,
    /// Informational note.
    Info,
    /// Suggestion — lowest severity.
    Hint,
}

/// A diagnostic produced by validation.
#[derive(Debug)]
pub struct Diagnostic {
    /// Workspace-relative path of the file containing the issue.
    pub file: PathBuf,
    /// 1-based line number.
    pub line: usize,
    /// Severity of the diagnostic.
    pub severity: Severity,
    /// Human-readable description of the issue.
    pub message: String,
    /// Byte span of the offending text, when a precise location is known.
    ///
    /// `Some` yields a precise LSP range; `None` falls back to a whole-line
    /// range anchored on `line` (used by line-level diagnostics such as
    /// backlinks). `line` stays authoritative for sorting and the CLI's
    /// `path:line:` output regardless.
    pub span: Option<Span>,
}

/// Validate forward links across all files in the workspace.
///
/// Checks each intra-project and non-markdown link for:
/// - Target file existence.
/// - Predicate membership in the configured vocabulary.
/// - Predicate policy compliance (optional vs required).
/// - Fragment resolution against headings in the target document.
pub fn validate_forward_links(workspace: &Workspace) -> Vec<Diagnostic> {
    let config = workspace.config();
    let mut diagnostics = Vec::new();

    for (file_path, file_data) in workspace.files() {
        let links = file_data.tree.links(file_path);
        for link in &links {
            match &link.kind {
                LinkKind::External { .. } | LinkKind::IntraDocument { .. } => {}

                LinkKind::NonMarkdown { target } => {
                    check_target_exists(
                        workspace,
                        file_path,
                        link.line,
                        link.span,
                        target,
                        &mut diagnostics,
                    );
                }

                LinkKind::IntraProject {
                    target,
                    fragment,
                    predicate,
                    explicit_predicate,
                } => {
                    check_target_exists(
                        workspace,
                        file_path,
                        link.line,
                        link.span,
                        target,
                        &mut diagnostics,
                    );
                    check_predicate(
                        config,
                        file_path,
                        link.line,
                        link.span,
                        predicate,
                        *explicit_predicate,
                        &mut diagnostics,
                    );
                    if let Some(frag) = fragment {
                        check_fragment(
                            workspace,
                            config,
                            file_path,
                            link.line,
                            link.span,
                            target,
                            frag,
                            &mut diagnostics,
                        );
                    }
                }
            }
        }
    }

    diagnostics.sort_by(|a, b| a.file.cmp(&b.file).then(a.line.cmp(&b.line)));
    diagnostics
}

/// Check that a link target exists as a file in the workspace or on disk.
fn check_target_exists(
    workspace: &Workspace,
    source: &Path,
    line: usize,
    span: Span,
    target: &Path,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let is_markdown = target.extension().is_some_and(|ext| ext == "md");

    let exists = if is_markdown {
        workspace.file(target).is_some()
    } else {
        workspace.root().join(target).is_file()
    };

    if !exists {
        diagnostics.push(Diagnostic {
            file: source.to_path_buf(),
            line,
            severity: Severity::Error,
            message: format!("link target does not exist: {}", target.display()),
            span: Some(span),
        });
    }
}

/// Check predicate validity and policy compliance.
fn check_predicate(
    config: &Config,
    source: &Path,
    line: usize,
    span: Span,
    predicate: &str,
    explicit: bool,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if explicit {
        if !config.is_known_forward(predicate) {
            let known: Vec<&str> = config.predicates.keys().map(String::as_str).collect();
            diagnostics.push(Diagnostic {
                file: source.to_path_buf(),
                line,
                severity: Severity::Error,
                message: format!(
                    "unknown predicate '{predicate}': choose from [{}]",
                    known.join(", ")
                ),
                span: Some(span),
            });
        }
    } else {
        match config.policy.predicates {
            PredicatePolicy::Optional => {
                diagnostics.push(Diagnostic {
                    file: source.to_path_buf(),
                    line,
                    severity: Severity::Info,
                    message: "link has no explicit predicate (defaulting to 'references')"
                        .to_string(),
                    span: Some(span),
                });
            }
            PredicatePolicy::Required => {
                let known: Vec<&str> = config.predicates.keys().map(String::as_str).collect();
                diagnostics.push(Diagnostic {
                    file: source.to_path_buf(),
                    line,
                    severity: Severity::Error,
                    message: format!("link missing predicate: choose from [{}]", known.join(", ")),
                    span: Some(span),
                });
            }
        }
    }
}

/// Validate backlink consistency across the workspace.
///
/// Computes expected backlinks from forward links, diffs against actual
/// frontmatter, and emits warnings for missing or stale backlinks.
/// Returns an empty list when `backlinks = false` in the policy.
pub fn validate_backlinks(workspace: &Workspace) -> Vec<Diagnostic> {
    if !workspace.config().policy.backlinks {
        return Vec::new();
    }

    let expected = build_expected_backlinks(workspace);
    let mut diagnostics = Vec::new();

    check_missing_backlinks(workspace, &expected, &mut diagnostics);
    check_stale_backlinks(workspace, &expected, &mut diagnostics);

    diagnostics.sort_by(|a, b| a.file.cmp(&b.file).then(a.line.cmp(&b.line)));
    diagnostics
}

/// Location of the forward link in a source document that creates a backlink
/// expectation.
///
/// Carried so a missing-backlink diagnostic can anchor on the *source* link
/// (the present artifact) rather than on the target's absent frontmatter
/// entry. When a source links to the same target more than once with the same
/// predicate, the earliest link is kept.
#[derive(Debug, Clone, Copy)]
struct ExpectedSource {
    /// 1-based line of the forward link in the source document.
    line: usize,
    /// Byte span of the forward link in the source document.
    span: Span,
}

/// Map from a target file to its expected backlinks.
///
/// `target file → { inverse predicate → { source file → forward-link
/// location } }`. All paths are workspace-relative.
type ExpectedBacklinks = HashMap<PathBuf, HashMap<String, BTreeMap<PathBuf, ExpectedSource>>>;

/// Build expected backlinks from all forward links in the workspace.
///
/// Returns a map keyed by target file. Each source carries the position of
/// the forward link that created the expectation, so missing-backlink
/// diagnostics can anchor on the source.
fn build_expected_backlinks(workspace: &Workspace) -> ExpectedBacklinks {
    let config = workspace.config();
    let mut expected: ExpectedBacklinks = HashMap::new();

    for (source_path, file_data) in workspace.files() {
        let links = file_data.tree.links(source_path);
        for link in &links {
            if let LinkKind::IntraProject {
                target, predicate, ..
            } = &link.kind
            {
                // Skip broken targets and unknown predicates — forward validation handles those.
                if workspace.file(target).is_none() {
                    continue;
                }
                let Some(inverse) = config.inverse_of(predicate) else {
                    continue;
                };

                expected
                    .entry(target.clone())
                    .or_default()
                    .entry(inverse.to_string())
                    .or_default()
                    .entry(source_path.clone())
                    .and_modify(|loc| {
                        // Keep the earliest link when a source links a target
                        // more than once under the same predicate.
                        if link.line < loc.line {
                            *loc = ExpectedSource {
                                line: link.line,
                                span: link.span,
                            };
                        }
                    })
                    .or_insert(ExpectedSource {
                        line: link.line,
                        span: link.span,
                    });
            }
        }
    }

    expected
}

/// Resolve a backlink path (file-relative) to a workspace-relative path.
///
/// Backlink paths in frontmatter are relative to the file containing them,
/// just like forward link targets. This joins the containing file's parent
/// directory with the backlink path and normalizes the result.
fn resolve_backlink_path(containing_file: &Path, backlink_path: &str) -> PathBuf {
    let dir = containing_file.parent().unwrap_or_else(|| Path::new(""));
    block::normalize_path(&dir.join(backlink_path))
}

/// Compute the relative path from one file to another.
///
/// Both paths must be workspace-relative. Returns the path you would write
/// in `from`'s frontmatter to reference `to` — relative to `from`'s
/// parent directory.
fn file_relative(from: &Path, to: &Path) -> PathBuf {
    let from_dir = from.parent().unwrap_or_else(|| Path::new(""));
    let from_parts: Vec<_> = from_dir.components().collect();
    let to_parts: Vec<_> = to.components().collect();

    let common = from_parts
        .iter()
        .zip(&to_parts)
        .take_while(|(a, b)| a == b)
        .count();

    let mut result = PathBuf::new();
    for _ in common..from_parts.len() {
        result.push("..");
    }
    for part in &to_parts[common..] {
        result.push(part);
    }
    result
}

/// Emit warnings for expected backlinks missing from frontmatter.
///
/// The diagnostic anchors on the *source* document at the forward link — the
/// present artifact — rather than on the target's absent frontmatter entry.
/// This puts the warning on the file an agent is editing when it adds the
/// link, giving it an entry point to walk the graph to the target. The fix
/// still belongs in the target's frontmatter; the message names it.
fn check_missing_backlinks(
    workspace: &Workspace,
    expected: &ExpectedBacklinks,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for (target_path, expected_backlinks) in expected {
        let actual = workspace
            .file(target_path)
            .and_then(|f| f.frontmatter.as_ref())
            .map(|fm| &fm.backlinks);

        for (inverse_pred, expected_sources) in expected_backlinks {
            let actual_sources: BTreeSet<PathBuf> = actual
                .and_then(|a| a.get(inverse_pred.as_str()))
                .map(|paths| {
                    paths
                        .iter()
                        .map(|p| resolve_backlink_path(target_path, p))
                        .collect()
                })
                .unwrap_or_default();

            for (source, loc) in expected_sources {
                if !actual_sources.contains(source) {
                    let rel = file_relative(source, target_path);
                    diagnostics.push(Diagnostic {
                        file: source.clone(),
                        line: loc.line,
                        severity: Severity::Warning,
                        message: format!(
                            "expected backlink `{inverse_pred}` in `{}`",
                            rel.display()
                        ),
                        span: Some(loc.span),
                    });
                }
            }
        }
    }
}

/// Emit warnings for frontmatter backlinks with no corresponding forward link.
fn check_stale_backlinks(
    workspace: &Workspace,
    expected: &ExpectedBacklinks,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for (file_path, file_data) in workspace.files() {
        let Some(fm) = &file_data.frontmatter else {
            continue;
        };

        for (inverse_pred, sources) in &fm.backlinks {
            let expected_sources = expected
                .get(file_path)
                .and_then(|e| e.get(inverse_pred.as_str()));

            for source_str in sources {
                let resolved = resolve_backlink_path(file_path, source_str);
                let is_expected = expected_sources.is_some_and(|set| set.contains_key(&resolved));

                if !is_expected {
                    diagnostics.push(Diagnostic {
                        file: file_path.clone(),
                        line: fm.start_line,
                        severity: Severity::Warning,
                        message: format!(
                            "backlink `{inverse_pred}` from `{source_str}` has no corresponding forward link"
                        ),
                        span: None,
                    });
                }
            }
        }
    }
}

/// Check that a fragment resolves to a heading in the target document.
///
/// Explicit `{#id}` anchors are checked first (exact match). For computed
/// slugs, the algorithm policy determines which slugs are considered.
/// Skips the check when the target file does not exist (forward link
/// validation handles that case).
#[allow(
    clippy::too_many_arguments,
    reason = "validation context parameters are distinct concerns"
)]
fn check_fragment(
    workspace: &Workspace,
    config: &Config,
    source: &Path,
    line: usize,
    span: Span,
    target: &Path,
    fragment: &str,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let Some(target_data) = workspace.file(target) else {
        return;
    };

    let algorithm = config.policy.fragments;
    let headings = target_data.tree.headings();

    let found = headings.iter().any(|heading| match &heading.id {
        HeadingId::Explicit(id) => id == fragment,
        HeadingId::Computed {
            github,
            gitlab,
            vscode,
        } => match algorithm {
            Some(FragmentAlgorithm::Github) => github == fragment,
            Some(FragmentAlgorithm::Gitlab) => gitlab == fragment,
            Some(FragmentAlgorithm::Vscode) => vscode == fragment,
            None => github == fragment || gitlab == fragment || vscode == fragment,
        },
    });

    if !found {
        diagnostics.push(Diagnostic {
            file: source.to_path_buf(),
            line,
            severity: Severity::Error,
            message: format!(
                "fragment `#{}` not found in `{}`",
                fragment,
                target.display()
            ),
            span: Some(span),
        });
    }
}

/// Collect all diagnostics for the workspace.
///
/// Runs every validation check (forward links, backlinks, bare paths),
/// collects unknown inverse predicate errors from frontmatter, and
/// includes frontmatter parse errors. Returns diagnostics sorted by
/// file then line number.
pub fn collect_all(workspace: &Workspace) -> Vec<Diagnostic> {
    if !workspace.has_config() {
        return Vec::new();
    }

    let mut diagnostics = Vec::new();
    diagnostics.extend(validate_forward_links(workspace));
    diagnostics.extend(validate_backlinks(workspace));
    // Note: bare paths are emitted by the structural diagnostics layer
    // unconditionally — not duplicated here.

    for (path, file_data) in workspace.files() {
        for bd in &file_data.backlink_diagnostics {
            diagnostics.push(Diagnostic {
                file: path.clone(),
                line: bd.line,
                severity: Severity::Error,
                message: format!("unknown inverse predicate `{}`", bd.predicate),
                span: None,
            });
        }
        // Note: parse_diagnostics (frontmatter errors) are emitted by the
        // structural diagnostics layer unconditionally — not duplicated here.
    }

    diagnostics.sort_by(|a, b| a.file.cmp(&b.file).then(a.line.cmp(&b.line)));
    diagnostics
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests use expect and panic for clarity"
)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;
    use crate::workspace::Workspace;

    /// Create a workspace with the given files and scan it.
    fn setup_workspace(files: &[(&str, &str)]) -> (TempDir, Workspace) {
        let dir = TempDir::new().expect("create temp dir");
        // Create a .git directory so the workspace root is found.
        fs::create_dir(dir.path().join(".git")).expect("create .git");

        for (path, content) in files {
            let full = dir.path().join(path);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).expect("create parent dirs");
            }
            fs::write(&full, content).expect("write file");
        }

        let ws = Workspace::scan(dir.path()).expect("scan workspace");
        (dir, ws)
    }

    #[test]
    fn valid_link_with_known_predicate() {
        let (_dir, ws) = setup_workspace(&[
            ("index.md", r#"[other](other.md "references")"#),
            ("other.md", "# Other\n"),
        ]);

        let diags = validate_forward_links(&ws);
        assert!(
            diags.iter().all(|d| d.severity != Severity::Error),
            "no errors for valid link: {diags:?}"
        );
    }

    #[test]
    fn broken_link_target() {
        let (_dir, ws) =
            setup_workspace(&[("index.md", r#"[missing](nonexistent.md "references")"#)]);

        let diags = validate_forward_links(&ws);
        let errors: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();

        assert_eq!(errors.len(), 1, "one error for broken link");
        assert!(
            errors[0].message.contains("does not exist"),
            "message mentions non-existence: {}",
            errors[0].message
        );
        assert!(
            errors[0].message.contains("nonexistent.md"),
            "message includes target path: {}",
            errors[0].message
        );
    }

    #[test]
    fn unknown_predicate() {
        let (_dir, ws) = setup_workspace(&[
            ("index.md", r#"[other](other.md "invented_predicate")"#),
            ("other.md", "# Other\n"),
        ]);

        let diags = validate_forward_links(&ws);
        let errors: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();

        assert_eq!(errors.len(), 1, "one error for unknown predicate");
        assert!(
            errors[0].message.contains("unknown predicate"),
            "message mentions unknown predicate: {}",
            errors[0].message
        );
        assert!(
            errors[0].message.contains("invented_predicate"),
            "message includes the bad predicate: {}",
            errors[0].message
        );
    }

    #[test]
    fn missing_predicate_optional_policy() {
        let (_dir, ws) =
            setup_workspace(&[("index.md", "[other](other.md)"), ("other.md", "# Other\n")]);

        let diags = validate_forward_links(&ws);

        let infos: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Severity::Info)
            .collect();
        let has_errors = diags.iter().any(|d| d.severity == Severity::Error);

        assert_eq!(infos.len(), 1, "one info for implicit predicate");
        assert!(!has_errors, "no errors under optional policy");
        assert!(
            infos[0].message.contains("no explicit predicate"),
            "message describes missing predicate: {}",
            infos[0].message
        );
    }

    #[test]
    fn missing_predicate_required_policy() {
        let config_toml = "\
[policy]
predicates = \"required\"
";
        let (_dir, ws) = setup_workspace(&[
            (".lattice.toml", config_toml),
            ("index.md", "[other](other.md)"),
            ("other.md", "# Other\n"),
        ]);

        let diags = validate_forward_links(&ws);
        let errors: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();

        assert_eq!(errors.len(), 1, "one error for missing predicate");
        assert!(
            errors[0].message.contains("missing predicate"),
            "message describes missing predicate: {}",
            errors[0].message
        );
    }

    #[test]
    fn external_links_skipped() {
        let (_dir, ws) = setup_workspace(&[(
            "index.md",
            "[ext](https://example.com) [mail](mailto:a@b.com)",
        )]);

        let diags = validate_forward_links(&ws);
        assert!(diags.is_empty(), "no diagnostics for external links");
    }

    #[test]
    fn intra_document_links_skipped() {
        let (_dir, ws) = setup_workspace(&[("index.md", "[section](#heading)")]);

        let diags = validate_forward_links(&ws);
        assert!(diags.is_empty(), "no diagnostics for intra-document links");
    }

    #[test]
    fn non_markdown_target_exists() {
        let (_dir, ws) = setup_workspace(&[
            ("index.md", "[diagram](arch.png)"),
            ("arch.png", "fake png"),
        ]);

        let diags = validate_forward_links(&ws);
        assert!(
            diags.iter().all(|d| d.severity != Severity::Error),
            "no errors when non-markdown target exists"
        );
    }

    #[test]
    fn non_markdown_target_missing() {
        let (_dir, ws) = setup_workspace(&[("index.md", "[diagram](missing.png)")]);

        let diags = validate_forward_links(&ws);
        let errors: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();

        assert_eq!(errors.len(), 1, "one error for missing non-markdown target");
        assert!(
            errors[0].message.contains("does not exist"),
            "message mentions non-existence: {}",
            errors[0].message
        );
    }

    #[test]
    fn diagnostics_sorted_by_file_and_line() {
        let (_dir, ws) = setup_workspace(&[
            (
                "b.md",
                r#"[x](missing1.md "references")
[y](missing2.md "references")"#,
            ),
            ("a.md", r#"[z](missing3.md "references")"#),
        ]);

        let diags = validate_forward_links(&ws);
        let error_files: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .map(|d| (d.file.clone(), d.line))
            .collect();

        for window in error_files.windows(2) {
            assert!(
                window[0] <= window[1],
                "diagnostics should be sorted: {:?} should come before {:?}",
                window[0],
                window[1]
            );
        }
    }

    #[test]
    fn link_in_subdirectory() {
        let (_dir, ws) = setup_workspace(&[
            ("docs/guide.md", r#"[ref](../README.md "references")"#),
            ("README.md", "# README\n"),
        ]);

        let diags = validate_forward_links(&ws);
        assert!(
            diags.iter().all(|d| d.severity != Severity::Error),
            "no errors for valid cross-directory link: {diags:?}"
        );
    }

    // --- Backlink validation ---

    #[test]
    fn backlink_present_no_warning() {
        let target = "\
---
backlinks:
  referenced_by:
    - index.md
---
# Target
";
        let (_dir, ws) = setup_workspace(&[
            ("index.md", r#"[target](target.md "references")"#),
            ("target.md", target),
        ]);

        let diags = validate_backlinks(&ws);
        assert!(
            diags.is_empty(),
            "no warnings when backlink is present: {diags:?}"
        );
    }

    #[test]
    fn missing_backlink_warning() {
        let (_dir, ws) = setup_workspace(&[
            ("index.md", r#"[target](target.md "supersedes")"#),
            ("target.md", "# Target\n"),
        ]);

        let diags = validate_backlinks(&ws);
        let warnings: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Severity::Warning)
            .collect();

        assert_eq!(warnings.len(), 1, "one warning for missing backlink");
        assert!(
            warnings[0].message.contains("superseded_by"),
            "message names the inverse predicate: {}",
            warnings[0].message
        );
        assert!(
            warnings[0].message.contains("target.md"),
            "message names the target file the backlink belongs in: {}",
            warnings[0].message
        );
        assert_eq!(
            warnings[0].file,
            Path::new("index.md"),
            "diagnostic anchors on the source (the forward link), not the target"
        );
    }

    #[test]
    fn missing_backlink_anchors_on_source_link_line() {
        // The forward link sits on line 3 of the source. The missing-backlink
        // diagnostic must point there (with a span), not at the target.
        let (_dir, ws) = setup_workspace(&[
            (
                "index.md",
                "# Index\n\n[target](target.md \"supersedes\")\n",
            ),
            ("target.md", "# Target\n"),
        ]);

        let diags = validate_backlinks(&ws);
        let warnings: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Severity::Warning)
            .collect();

        assert_eq!(warnings.len(), 1, "one warning for missing backlink");
        assert_eq!(
            warnings[0].file,
            Path::new("index.md"),
            "diagnostic anchors on the source file"
        );
        assert_eq!(
            warnings[0].line, 3,
            "diagnostic anchors on the forward-link line, not line 1"
        );
        assert!(
            warnings[0].span.is_some(),
            "diagnostic carries the forward link's span"
        );
    }

    #[test]
    fn stale_backlink_warning() {
        let target = "\
---
backlinks:
  superseded_by:
    - ghost.md
---
# Target
";
        let (_dir, ws) = setup_workspace(&[("target.md", target)]);

        let diags = validate_backlinks(&ws);
        let warnings: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Severity::Warning)
            .collect();

        assert_eq!(warnings.len(), 1, "one warning for stale backlink");
        assert!(
            warnings[0]
                .message
                .contains("no corresponding forward link"),
            "message describes staleness: {}",
            warnings[0].message
        );
        assert!(
            warnings[0].message.contains("ghost.md"),
            "message names the stale source: {}",
            warnings[0].message
        );
    }

    #[test]
    fn default_predicate_generates_referenced_by_backlink() {
        let (_dir, ws) = setup_workspace(&[
            ("index.md", "[target](target.md)"),
            ("target.md", "# Target\n"),
        ]);

        let diags = validate_backlinks(&ws);
        let warnings: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Severity::Warning)
            .collect();

        assert_eq!(warnings.len(), 1, "one warning for missing backlink");
        assert!(
            warnings[0].message.contains("referenced_by"),
            "implicit references produces referenced_by backlink: {}",
            warnings[0].message
        );
    }

    #[test]
    fn backlinks_disabled_skips_checking() {
        let config_toml = "\
[policy]
backlinks = false
";
        let (_dir, ws) = setup_workspace(&[
            (".lattice.toml", config_toml),
            ("index.md", r#"[target](target.md "supersedes")"#),
            ("target.md", "# Target\n"),
        ]);

        let diags = validate_backlinks(&ws);
        assert!(
            diags.is_empty(),
            "no diagnostics when backlinks disabled: {diags:?}"
        );
    }

    #[test]
    fn multiple_backlinks_from_different_files() {
        let target = "\
---
backlinks:
  superseded_by:
    - a.md
---
# Target
";
        let (_dir, ws) = setup_workspace(&[
            ("a.md", r#"[target](target.md "supersedes")"#),
            ("b.md", r#"[target](target.md "supersedes")"#),
            ("target.md", target),
        ]);

        let diags = validate_backlinks(&ws);
        let warnings: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Severity::Warning)
            .collect();

        assert_eq!(
            warnings.len(),
            1,
            "one missing backlink (a.md present, b.md missing): {warnings:?}"
        );
        assert_eq!(
            warnings[0].file,
            Path::new("b.md"),
            "warning anchors on the source with the missing backlink (b.md): {warnings:?}"
        );
        assert!(
            warnings[0].message.contains("target.md"),
            "message names the target the backlink belongs in: {}",
            warnings[0].message
        );
    }

    #[test]
    fn cross_directory_backlink_to_root() {
        let target = "\
---
backlinks:
  referenced_by:
    - docs/guide.md
---
# README
";
        let (_dir, ws) = setup_workspace(&[
            ("docs/guide.md", r#"[readme](../README.md "references")"#),
            ("README.md", target),
        ]);

        let diags = validate_backlinks(&ws);
        assert!(
            diags.is_empty(),
            "no warnings for correct cross-directory backlink: {diags:?}"
        );
    }

    #[test]
    fn cross_directory_backlink_to_subdir() {
        let target = "\
---
backlinks:
  referenced_by:
    - ../index.md
---
# API
";
        let (_dir, ws) = setup_workspace(&[
            ("index.md", r#"[api](docs/api.md "references")"#),
            ("docs/api.md", target),
        ]);

        let diags = validate_backlinks(&ws);
        assert!(
            diags.is_empty(),
            "no warnings when backlink uses file-relative path: {diags:?}"
        );
    }

    #[test]
    fn same_directory_backlink() {
        let target = "\
---
backlinks:
  superseded_by:
    - guide.md
---
# API
";
        let (_dir, ws) = setup_workspace(&[
            ("docs/guide.md", r#"[api](api.md "supersedes")"#),
            ("docs/api.md", target),
        ]);

        let diags = validate_backlinks(&ws);
        assert!(
            diags.is_empty(),
            "no warnings when same-directory backlink uses bare filename: {diags:?}"
        );
    }

    #[test]
    fn missing_backlink_message_shows_file_relative_path() {
        // Source in a subdirectory links up to a root target. The message
        // (now on the source) names the target relative to the source, so the
        // file-relative `..` rendering must survive.
        let (_dir, ws) = setup_workspace(&[
            ("docs/guide.md", r#"[readme](../README.md "supersedes")"#),
            ("README.md", "# README\n"),
        ]);

        let diags = validate_backlinks(&ws);
        let warnings: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Severity::Warning)
            .collect();

        assert_eq!(warnings.len(), 1, "one warning for missing backlink");
        assert_eq!(
            warnings[0].file,
            Path::new("docs/guide.md"),
            "diagnostic anchors on the source"
        );
        assert!(
            warnings[0].message.contains("../README.md"),
            "message shows file-relative path to the target, not workspace-relative: {}",
            warnings[0].message
        );
    }

    #[test]
    fn broken_forward_link_does_not_expect_backlink() {
        let (_dir, ws) =
            setup_workspace(&[("index.md", r#"[missing](nonexistent.md "supersedes")"#)]);

        let diags = validate_backlinks(&ws);
        assert!(
            diags.is_empty(),
            "no backlink warnings for broken forward links: {diags:?}"
        );
    }

    #[test]
    fn unknown_predicate_does_not_expect_backlink() {
        let (_dir, ws) = setup_workspace(&[
            ("index.md", r#"[target](target.md "invented")"#),
            ("target.md", "# Target\n"),
        ]);

        let diags = validate_backlinks(&ws);
        assert!(
            diags.is_empty(),
            "no backlink warnings for unknown predicates: {diags:?}"
        );
    }

    // --- Fragment validation ---

    #[test]
    fn fragment_matches_explicit_anchor() {
        let (_dir, ws) = setup_workspace(&[
            ("index.md", r#"[context](target.md#my-anchor "references")"#),
            ("target.md", "## Context {#my-anchor}\n"),
        ]);

        let diags = validate_forward_links(&ws);
        assert!(
            diags.is_empty(),
            "no errors when fragment matches explicit anchor: {diags:?}"
        );
    }

    #[test]
    fn fragment_matches_computed_slug() {
        let (_dir, ws) = setup_workspace(&[
            (
                "index.md",
                r#"[gs](target.md#getting-started "references")"#,
            ),
            ("target.md", "## Getting Started\n"),
        ]);

        let diags = validate_forward_links(&ws);
        assert!(
            diags.is_empty(),
            "no errors when fragment matches computed slug: {diags:?}"
        );
    }

    #[test]
    fn fragment_not_found_produces_error() {
        let (_dir, ws) = setup_workspace(&[
            ("index.md", r#"[ref](target.md#nonexistent "references")"#),
            ("target.md", "## Introduction\n"),
        ]);

        let diags = validate_forward_links(&ws);
        let errors: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();

        assert_eq!(errors.len(), 1, "one error for unresolved fragment");
        assert!(
            errors[0].message.contains("#nonexistent"),
            "message includes the fragment: {}",
            errors[0].message
        );
        assert!(
            errors[0].message.contains("target.md"),
            "message includes the target file: {}",
            errors[0].message
        );
    }

    #[test]
    fn fragment_pinned_to_github_rejects_gitlab_only_slug() {
        // "Héllo" → github slug "héllo", gitlab slug "hllo"
        let config_toml = "\
[policy]
fragments = \"github\"
";
        let (_dir, ws) = setup_workspace(&[
            (".lattice.toml", config_toml),
            ("index.md", r#"[ref](target.md#hllo "references")"#),
            ("target.md", "## Héllo\n"),
        ]);

        let diags = validate_forward_links(&ws);
        let errors: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();

        assert_eq!(
            errors.len(),
            1,
            "gitlab-only slug rejected when pinned to github"
        );
        assert!(
            errors[0].message.contains("#hllo"),
            "message includes the fragment: {}",
            errors[0].message
        );
    }

    #[test]
    fn fragment_pinned_to_github_accepts_github_slug() {
        // "Héllo" → github slug "héllo"
        let config_toml = "\
[policy]
fragments = \"github\"
";
        let (_dir, ws) = setup_workspace(&[
            (".lattice.toml", config_toml),
            ("index.md", r#"[ref](target.md#héllo "references")"#),
            ("target.md", "## Héllo\n"),
        ]);

        let diags = validate_forward_links(&ws);
        assert!(
            diags.is_empty(),
            "github slug accepted when pinned to github: {diags:?}"
        );
    }

    #[test]
    fn fragment_pinned_to_gitlab_accepts_gitlab_slug() {
        // "Héllo" → gitlab slug "hllo"
        let config_toml = "\
[policy]
fragments = \"gitlab\"
";
        let (_dir, ws) = setup_workspace(&[
            (".lattice.toml", config_toml),
            ("index.md", r#"[ref](target.md#hllo "references")"#),
            ("target.md", "## Héllo\n"),
        ]);

        let diags = validate_forward_links(&ws);
        assert!(
            diags.is_empty(),
            "gitlab slug accepted when pinned to gitlab: {diags:?}"
        );
    }

    #[test]
    fn fragment_unpinned_accepts_any_algorithm() {
        // "Héllo" → github "héllo", gitlab "hllo", vscode "héllo"
        // With no pinned algorithm, "hllo" (gitlab-only) should still pass.
        let (_dir, ws) = setup_workspace(&[
            ("index.md", r#"[ref](target.md#hllo "references")"#),
            ("target.md", "## Héllo\n"),
        ]);

        let diags = validate_forward_links(&ws);
        assert!(
            diags.is_empty(),
            "gitlab-only slug accepted when no algorithm pinned: {diags:?}"
        );
    }

    #[test]
    fn fragment_on_broken_target_skipped() {
        let (_dir, ws) =
            setup_workspace(&[("index.md", r#"[ref](missing.md#heading "references")"#)]);

        let diags = validate_forward_links(&ws);
        let fragment_errors: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Severity::Error && d.message.contains("fragment"))
            .collect();

        assert!(
            fragment_errors.is_empty(),
            "no fragment errors for broken targets: {fragment_errors:?}"
        );
    }

    #[test]
    fn fragment_on_file_with_no_headings() {
        let (_dir, ws) = setup_workspace(&[
            ("index.md", r#"[ref](target.md#something "references")"#),
            ("target.md", "No headings here.\n"),
        ]);

        let diags = validate_forward_links(&ws);
        let errors: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();

        assert_eq!(errors.len(), 1, "error when target has no headings at all");
        assert!(
            errors[0].message.contains("#something"),
            "message includes the fragment: {}",
            errors[0].message
        );
    }

    #[test]
    fn explicit_anchor_takes_priority_over_slug() {
        // Heading has explicit anchor that differs from the computed slug.
        let (_dir, ws) = setup_workspace(&[
            ("index.md", r#"[ref](target.md#custom-id "references")"#),
            ("target.md", "## Getting Started {#custom-id}\n"),
        ]);

        let diags = validate_forward_links(&ws);
        assert!(diags.is_empty(), "explicit anchor matched: {diags:?}");
    }

    // --- Bare path validation ---

    // --- collect_all opt-in gate ---

    #[test]
    fn collect_all_empty_without_config() {
        let (_dir, ws) = setup_workspace(&[("index.md", r#"[missing](gone.md "references")"#)]);

        let diags = collect_all(&ws);
        assert!(
            diags.is_empty(),
            "collect_all should return empty without .lattice.toml: {diags:?}"
        );
    }

    #[test]
    fn collect_all_runs_with_config() {
        let (_dir, ws) = setup_workspace(&[
            (".lattice.toml", ""),
            ("index.md", r#"[missing](gone.md "references")"#),
        ]);

        let diags = collect_all(&ws);
        assert!(
            !diags.is_empty(),
            "collect_all should produce diagnostics with .lattice.toml"
        );
    }
}
