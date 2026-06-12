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
use crate::config::{Config, ConnectivityPolicy, FragmentAlgorithm, PredicatePolicy};
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
        for link in &file_data.links {
            match &link.kind {
                LinkKind::External { .. } => {}

                LinkKind::IntraDocument { fragment } => {
                    // Same-document anchor (`[…](#heading)`): resolve against
                    // this file's own headings, exactly as a cross-file
                    // fragment resolves against its target's. A dangling
                    // in-page anchor is as broken as a dangling cross-file one
                    // (issue 021).
                    check_fragment(
                        workspace,
                        config,
                        file_path,
                        link.line,
                        link.span,
                        file_path,
                        fragment,
                        &mut diagnostics,
                    );
                }

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
        // Decision 008: a forward link may name either member of a pair —
        // a known forward predicate or a known inverse. Only a predicate in
        // neither direction is an error.
        if !config.is_known_predicate(predicate) {
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
/// `target file → { backlink label → { source file → forward-link location } }`.
/// The backlink label is the *opposite* member of the forward link's predicate
/// pair (decision 008), so an inverse-predicate link derives the forward label.
/// All paths are workspace-relative.
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
        for link in &file_data.links {
            if let LinkKind::IntraProject {
                target, predicate, ..
            } = &link.kind
            {
                // Skip broken targets and unknown predicates — forward validation handles those.
                if workspace.file(target).is_none() {
                    continue;
                }
                // Key by the opposite member of the pair (decision 008): an
                // inverse-predicate link (`"superseded_by"`) derives the
                // forward label (`"supersedes"`) on its target.
                let Some(opposite) = config.opposite_of(predicate) else {
                    continue;
                };

                expected
                    .entry(target.clone())
                    .or_default()
                    .entry(opposite.to_string())
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
/// directory with the backlink path and normalizes the result. Shared with the
/// LSP navigation handlers so that path resolution stays identical to the
/// consistency check.
pub fn resolve_backlink_path(containing_file: &Path, backlink_path: &str) -> PathBuf {
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
///
/// An obligation is satisfied by *either* a frontmatter backlink *or* a
/// reciprocal forward link from the target back to the source carrying the
/// matching paired predicate (decision 008). A warning fires only when
/// neither exists.
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

        for (backlink_key, expected_sources) in expected_backlinks {
            let actual_sources: BTreeSet<PathBuf> = actual
                .and_then(|a| a.get(backlink_key.as_str()))
                .map(|paths| {
                    paths
                        .iter()
                        .map(|p| resolve_backlink_path(target_path, p))
                        .collect()
                })
                .unwrap_or_default();

            for (source, loc) in expected_sources {
                // Satisfied by a materialized frontmatter backlink...
                if actual_sources.contains(source) {
                    continue;
                }
                // ...or by a reciprocal forward link from the target back to
                // the source under the same label (decision 008). The floor is
                // met from both ends, so no frontmatter copy is required.
                if has_reciprocal_forward_link(workspace, target_path, source, backlink_key) {
                    continue;
                }
                let rel = file_relative(source, target_path);
                diagnostics.push(Diagnostic {
                    file: source.clone(),
                    line: loc.line,
                    severity: Severity::Warning,
                    message: format!("expected backlink `{backlink_key}` in `{}`", rel.display()),
                    span: Some(loc.span),
                });
            }
        }
    }
}

/// Returns `true` if `target` has a forward link to `source` carrying
/// `predicate` — a reciprocal forward link that satisfies a backlink
/// obligation without a frontmatter entry (decision 008).
///
/// The obligation's label is already in the target's own forward direction
/// (it is `opposite_of` the source's link predicate), so the reciprocal link
/// satisfies it exactly when its predicate equals that label.
fn has_reciprocal_forward_link(
    workspace: &Workspace,
    target: &Path,
    source: &Path,
    predicate: &str,
) -> bool {
    let Some(target_data) = workspace.file(target) else {
        return false;
    };
    target_data.links.iter().any(|link| {
        matches!(
            &link.kind,
            LinkKind::IntraProject { target: t, predicate: p, .. }
                if t == source && p == predicate
        )
    })
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

        for (backlink_key, sources) in &fm.backlinks {
            let expected_sources = expected
                .get(file_path)
                .and_then(|e| e.get(backlink_key.as_str()));

            for source_str in sources {
                let resolved = resolve_backlink_path(file_path, source_str);
                let is_expected = expected_sources.is_some_and(|set| set.contains_key(&resolved));

                if !is_expected {
                    diagnostics.push(Diagnostic {
                        file: file_path.clone(),
                        line: fm.start_line,
                        severity: Severity::Warning,
                        message: format!(
                            "backlink `{backlink_key}` from `{source_str}` has no corresponding forward link"
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
/// validation handles that case). The HTML "top of document" idioms — an
/// empty fragment (`#`) and `#top` (ASCII case-insensitive) — are always
/// valid and never flagged.
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

    // `#` (empty) and `#top` (ASCII case-insensitive) are the HTML
    // "top of document" idioms: a renderer scrolls to the top regardless of
    // headings (a real `top` heading, if present, just takes precedence). Both
    // are valid, so never flag them — for same-document and cross-file links
    // alike (issue 021).
    if fragment.is_empty() || fragment.eq_ignore_ascii_case("top") {
        return;
    }

    let algorithm = config.policy.fragments;
    let headings = &target_data.headings;

    let found_heading = headings.iter().any(|heading| match &heading.id {
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

    // A fragment also resolves against an explicit raw-HTML anchor target —
    // `<a id="x"></a>` or `<a name="x">` — defined anywhere in the document.
    // Such anchors are link targets, not link sources, and `#x` resolves to
    // them exactly as it does to a heading slug (issue 025).
    let found = found_heading
        || target_data
            .anchors
            .iter()
            .any(|anchor| anchor.id == fragment);

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

/// Validate graph connectivity (topology) across the workspace.
///
/// Flags isolated documents per the configured [`ConnectivityPolicy`]:
///
/// - `no-orphans` — any non-root document with no intra-project edge.
/// - `no-islands` — any non-root document outside a root's connected
///   component (edges undirected).
/// - `reachable` — any non-root document not forward-reachable from a root.
///
/// Returns an empty list when `connectivity = "off"` (the default). The
/// diagnostic anchors on the isolated document itself (line 1), mirroring the
/// missing-backlink-on-target convention.
///
/// Edges are built from valid intra-project markdown forward links (target
/// must exist; self-loops excluded). A link counts as an edge regardless of
/// predicate validity — connectivity asks whether documents reference each
/// other, which is orthogonal to the predicate vocabulary; an unknown
/// predicate is a separate diagnostic. Roots are exempt at every level. When
/// no configured root resolves to an indexed file, `no-islands` and
/// `reachable` emit nothing (they have no anchor to traverse from), while
/// `no-orphans` still runs (it uses roots only for exemption).
pub fn validate_connectivity(workspace: &Workspace) -> Vec<Diagnostic> {
    let level = workspace.config().policy.connectivity;
    if level == ConnectivityPolicy::Off {
        return Vec::new();
    }

    let files = workspace.files();

    // Build forward (directed) and undirected adjacency over valid
    // intra-project markdown edges in a single pass.
    let mut forward: HashMap<&Path, BTreeSet<&Path>> = HashMap::new();
    let mut undirected: HashMap<&Path, BTreeSet<&Path>> = HashMap::new();
    for (source, file_data) in files {
        let src = source.as_path();
        for link in &file_data.links {
            let LinkKind::IntraProject { target, .. } = &link.kind else {
                continue;
            };
            // Skip broken targets (forward validation handles those) and
            // self-loops (a document linking to itself does not connect it to
            // any *other* document).
            let Some((target_key, _)) = files.get_key_value(target) else {
                continue;
            };
            let dst = target_key.as_path();
            if dst == src {
                continue;
            }
            forward.entry(src).or_default().insert(dst);
            undirected.entry(src).or_default().insert(dst);
            undirected.entry(dst).or_default().insert(src);
        }
    }

    // Resolve configured roots to indexed files (normalized).
    let root_set: BTreeSet<&Path> = workspace
        .config()
        .policy
        .roots
        .iter()
        .filter_map(|r| {
            files
                .get_key_value(&block::normalize_path(r))
                .map(|(k, _)| k.as_path())
        })
        .collect();

    let mut diagnostics = Vec::new();
    let flag = |node: &Path, message: &str, diags: &mut Vec<Diagnostic>| {
        diags.push(Diagnostic {
            file: node.to_path_buf(),
            line: 1,
            severity: Severity::Warning,
            message: message.to_string(),
            span: None,
        });
    };

    match level {
        ConnectivityPolicy::Off => {}
        ConnectivityPolicy::NoOrphans => {
            for path in files.keys() {
                let node = path.as_path();
                if root_set.contains(node) {
                    continue;
                }
                if undirected.get(node).is_none_or(BTreeSet::is_empty) {
                    flag(
                        node,
                        "orphaned document: no links to or from any other document",
                        &mut diagnostics,
                    );
                }
            }
        }
        ConnectivityPolicy::NoIslands | ConnectivityPolicy::Reachable => {
            if root_set.is_empty() {
                tracing::debug!(
                    "connectivity: no configured root resolves to an indexed file; skipping {level:?}"
                );
                return Vec::new();
            }
            let (adjacency, message) = if level == ConnectivityPolicy::NoIslands {
                (
                    &undirected,
                    "isolated document: not connected to the project graph",
                )
            } else {
                (
                    &forward,
                    "unreachable document: not reachable from any root by forward links",
                )
            };
            let visited = flood(&root_set, adjacency);
            for path in files.keys() {
                let node = path.as_path();
                if root_set.contains(node) {
                    continue;
                }
                if !visited.contains(node) {
                    flag(node, message, &mut diagnostics);
                }
            }
        }
    }

    diagnostics.sort_by(|a, b| a.file.cmp(&b.file).then(a.line.cmp(&b.line)));
    diagnostics
}

/// Flood-fill the set of nodes reachable from `roots` over `adjacency`.
///
/// Iterative DFS; pass undirected adjacency for `no-islands` (component
/// membership) or forward adjacency for `reachable` (directed navigability).
fn flood<'a>(
    roots: &BTreeSet<&'a Path>,
    adjacency: &HashMap<&'a Path, BTreeSet<&'a Path>>,
) -> BTreeSet<&'a Path> {
    let mut visited: BTreeSet<&Path> = BTreeSet::new();
    let mut stack: Vec<&Path> = roots.iter().copied().collect();
    while let Some(node) = stack.pop() {
        if !visited.insert(node) {
            continue;
        }
        if let Some(neighbors) = adjacency.get(node) {
            for &next in neighbors {
                if !visited.contains(next) {
                    stack.push(next);
                }
            }
        }
    }
    visited
}

/// Collect all diagnostics for the workspace.
///
/// Runs every validation check (forward links, backlinks, connectivity, bare
/// paths), collects unknown backlink predicate errors from frontmatter, and
/// includes frontmatter parse errors. Returns diagnostics sorted by
/// file then line number.
pub fn collect_all(workspace: &Workspace) -> Vec<Diagnostic> {
    if !workspace.has_config() {
        return Vec::new();
    }

    let mut diagnostics = Vec::new();
    diagnostics.extend(validate_forward_links(workspace));
    diagnostics.extend(validate_backlinks(workspace));
    diagnostics.extend(validate_connectivity(workspace));
    // Note: bare paths are emitted by the structural diagnostics layer
    // unconditionally — not duplicated here.

    for (path, file_data) in workspace.files() {
        for bd in &file_data.backlink_diagnostics {
            diagnostics.push(Diagnostic {
                file: path.clone(),
                line: bd.line,
                severity: Severity::Error,
                message: format!("unknown backlink predicate `{}`", bd.predicate),
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

    // --- Symmetric predicates (decision 008) ---

    #[test]
    fn inverse_predicate_forward_link_accepted() {
        // A forward link may use the inverse member of a pair — no
        // unknown-predicate error.
        let (_dir, ws) =
            setup_workspace(&[("a.md", r#"[b](b.md "superseded_by")"#), ("b.md", "# B\n")]);

        let diags = validate_forward_links(&ws);
        assert!(
            diags.iter().all(|d| d.severity != Severity::Error),
            "inverse-predicate forward link should not error: {diags:?}"
        );
    }

    #[test]
    fn inverse_predicate_derives_forward_label() {
        // `a` is superseded_by `b`, so `b` supersedes `a`: the derived
        // backlink on `b` is keyed by the *forward* label `supersedes`.
        let (_dir, ws) =
            setup_workspace(&[("a.md", r#"[b](b.md "superseded_by")"#), ("b.md", "# B\n")]);

        let diags = validate_backlinks(&ws);
        let warnings: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Severity::Warning)
            .collect();

        assert_eq!(warnings.len(), 1, "one missing-backlink warning: {diags:?}");
        assert!(
            warnings[0].message.contains("supersedes"),
            "derived label is the forward `supersedes`, not the inverse: {}",
            warnings[0].message
        );
    }

    #[test]
    fn reciprocal_forward_links_need_no_frontmatter() {
        // A→B `superseded_by` and B→A `supersedes` express the same edge from
        // both ends. The floor is met without any frontmatter backlink.
        let (_dir, ws) = setup_workspace(&[
            ("a.md", r#"[b](b.md "superseded_by")"#),
            ("b.md", r#"[a](a.md "supersedes")"#),
        ]);

        assert!(
            validate_backlinks(&ws).is_empty(),
            "reciprocal forward links require no backlinks: {:?}",
            validate_backlinks(&ws)
        );
        assert!(
            validate_forward_links(&ws)
                .iter()
                .all(|d| d.severity != Severity::Error),
            "reciprocal forward links produce no errors"
        );
    }

    #[test]
    fn removing_reciprocal_reverts_to_missing_warning() {
        // Drop B's reciprocal link: A→B `superseded_by` again needs a
        // `supersedes` backlink on B (now neither frontmatter nor reciprocal).
        let (_dir, ws) =
            setup_workspace(&[("a.md", r#"[b](b.md "superseded_by")"#), ("b.md", "# B\n")]);

        let warnings: Vec<_> = validate_backlinks(&ws)
            .into_iter()
            .filter(|d| d.severity == Severity::Warning)
            .collect();

        assert_eq!(warnings.len(), 1, "the obligation reverts to a warning");
        assert!(
            warnings[0].message.contains("supersedes"),
            "warning names the unmet `supersedes` backlink: {}",
            warnings[0].message
        );
    }

    #[test]
    fn forward_label_backlink_key_validates_as_known() {
        // A frontmatter backlink keyed by a forward predicate is accepted —
        // it materializes the edge that B's `superseded_by` link authored.
        let (_dir, ws) = setup_workspace(&[
            (
                "b.md",
                "---\nbacklinks:\n  supersedes:\n    - a.md\n---\n# B\n",
            ),
            ("a.md", r#"[b](b.md "superseded_by")"#),
        ]);

        let errors: Vec<_> = collect_all(&ws)
            .into_iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();

        assert!(
            errors.is_empty(),
            "forward-label backlink key is known, not an error: {errors:?}"
        );
    }

    #[test]
    fn reciprocal_link_plus_frontmatter_is_not_warned() {
        // Over-specification — both reciprocal forward links *and* both
        // frontmatter backlinks — is redundant but consistent: no warning.
        let (_dir, ws) = setup_workspace(&[
            (
                "a.md",
                "---\nbacklinks:\n  superseded_by:\n    - b.md\n---\n[b](b.md \"superseded_by\")\n",
            ),
            (
                "b.md",
                "---\nbacklinks:\n  supersedes:\n    - a.md\n---\n[a](a.md \"supersedes\")\n",
            ),
        ]);

        assert!(
            validate_backlinks(&ws).is_empty(),
            "redundant-but-consistent edge is not warned: {:?}",
            validate_backlinks(&ws)
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
    fn same_document_anchor_resolves_to_own_heading() {
        // `[…](#slug)` resolves against the source file's own headings.
        let (_dir, ws) = setup_workspace(&[(
            "index.md",
            "[top](#getting-started)\n\n## Getting Started\n",
        )]);

        let diags = validate_forward_links(&ws);
        assert!(
            diags.is_empty(),
            "no errors when same-doc anchor matches an own heading: {diags:?}"
        );
    }

    #[test]
    fn same_document_anchor_resolves_to_explicit_id() {
        let (_dir, ws) = setup_workspace(&[(
            "index.md",
            "[go](#custom-id)\n\n## Some Heading {#custom-id}\n",
        )]);

        let diags = validate_forward_links(&ws);
        assert!(
            diags.is_empty(),
            "no errors when same-doc anchor matches an explicit {{#id}}: {diags:?}"
        );
    }

    #[test]
    fn same_document_anchor_not_found_produces_error() {
        // Issue 021: a dangling in-page anchor must error, like a cross-file one.
        let (_dir, ws) =
            setup_workspace(&[("index.md", "[broken](#nonexistent)\n\n## Introduction\n")]);

        let diags = validate_forward_links(&ws);
        let errors: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();

        assert_eq!(
            errors.len(),
            1,
            "one error for an unresolved same-doc anchor: {errors:?}"
        );
        assert!(
            errors[0].message.contains("#nonexistent"),
            "message includes the fragment: {}",
            errors[0].message
        );
    }

    #[test]
    fn same_document_anchor_resolves_to_explicit_html_id() {
        // Issue 025: `[x](#a)` resolves to a preceding `<a id="a"></a>` target,
        // not just a heading slug.
        let (_dir, ws) = setup_workspace(&[(
            "index.md",
            "[go](#explicit-anchor)\n\n<a id=\"explicit-anchor\"></a>\n\n## Real Heading\n",
        )]);

        let diags = validate_forward_links(&ws);
        assert!(
            diags.is_empty(),
            "no errors when same-doc anchor matches an explicit `<a id>`: {diags:?}"
        );
    }

    #[test]
    fn same_document_anchor_resolves_to_explicit_html_name() {
        // Issue 025: `<a name="a">` is equally a valid `#a` target.
        let (_dir, ws) = setup_workspace(&[(
            "index.md",
            "[go](#name-anchor)\n\n<a name=\"name-anchor\"></a>\n\n## Real Heading\n",
        )]);

        let diags = validate_forward_links(&ws);
        assert!(
            diags.is_empty(),
            "no errors when same-doc anchor matches an explicit `<a name>`: {diags:?}"
        );
    }

    #[test]
    fn same_document_anchor_resolves_to_element_id() {
        // Issue 025 (broadened to GitHub parity): `[x](#d)` resolves against any
        // element bearing `id="d"` — `<div id>` and `<section id>`, not only
        // `<a id>`.
        let (_dir, ws) = setup_workspace(&[(
            "index.md",
            "[a](#div-anchor)\n\
             [b](#section-anchor)\n\n\
             <div id=\"div-anchor\">\n\nbody\n\n</div>\n\n\
             <section id=\"section-anchor\">\n\nmore\n\n</section>\n\n\
             ## Real Heading\n",
        )]);

        let diags = validate_forward_links(&ws);
        assert!(
            diags.is_empty(),
            "no errors when same-doc anchor matches an element `id`: {diags:?}"
        );
    }

    #[test]
    fn same_document_html_anchor_does_not_mask_missing_fragment() {
        // Issue 025 repro: explicit `<a id>`/`<a name>` and a heading slug all
        // resolve, while a genuinely missing `#does-not-exist` still errors —
        // harvesting anchors must not over-suppress the control.
        let (_dir, ws) = setup_workspace(&[(
            "index.md",
            "- [a](#explicit-anchor)\n\
             - [b](#real-heading)\n\
             - [c](#name-anchor)\n\
             - [d](#does-not-exist)\n\n\
             <a id=\"explicit-anchor\"></a>\n\n\
             ## Real Heading\n\n\
             <a name=\"name-anchor\"></a>\n\n\
             Body text.\n",
        )]);

        let diags = validate_forward_links(&ws);
        let errors: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();
        assert_eq!(
            errors.len(),
            1,
            "only the genuinely missing fragment errors: {errors:?}"
        );
        assert!(
            errors[0].message.contains("#does-not-exist"),
            "the one error names the missing fragment: {}",
            errors[0].message
        );
    }

    #[test]
    fn same_document_anchor_resolves_to_mid_paragraph_inline_id() {
        // Issue 026: `[x](#s)` resolves against a non-`<a>` `id` that appears
        // mid-paragraph as inline raw HTML — the gap issue 025 left open. The
        // block-level `<div id>` resolves as before, the mid-paragraph
        // `<span id>` now resolves too, and a genuinely missing `#z` still
        // errors with exactly one diagnostic.
        let (_dir, ws) = setup_workspace(&[(
            "index.md",
            "- [a](#block-anchor)\n\
             - [b](#inline-anchor)\n\
             - [c](#does-not-exist)\n\n\
             <div id=\"block-anchor\"></div>\n\n\
             Paragraph with an <span id=\"inline-anchor\"></span> inline target.\n",
        )]);

        let diags = validate_forward_links(&ws);
        let errors: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();
        assert_eq!(
            errors.len(),
            1,
            "only the genuinely missing fragment errors; block and mid-paragraph \
             inline ids both resolve: {errors:?}"
        );
        assert!(
            errors[0].message.contains("#does-not-exist"),
            "the one error names the missing fragment: {}",
            errors[0].message
        );
    }

    #[test]
    fn same_document_top_idioms_are_valid() {
        // `#` and `#top` (any case) are back-to-top idioms — never flagged,
        // even with no matching heading (issue 021).
        let (_dir, ws) = setup_workspace(&[(
            "index.md",
            "[a](#) [b](#top) [c](#Top) [d](#TOP)\n\n## Intro\n",
        )]);

        let diags = validate_forward_links(&ws);
        assert!(
            diags.is_empty(),
            "back-to-top idioms must not be flagged: {diags:?}"
        );
    }

    #[test]
    fn cross_file_top_idioms_are_valid() {
        // The exemption is symmetric: a cross-file `#`/`#top` is valid too.
        let (_dir, ws) = setup_workspace(&[
            ("index.md", "[a](other.md#) [b](other.md#top)\n"),
            ("other.md", "## Other Heading\n"),
        ]);

        let diags = validate_forward_links(&ws);
        let errors: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();
        assert!(
            errors.is_empty(),
            "cross-file back-to-top idioms must not error: {errors:?}"
        );
    }

    #[test]
    fn same_document_real_top_heading_still_resolves() {
        // `#top` pointing at an actual `## Top` heading is fine either way.
        let (_dir, ws) = setup_workspace(&[("index.md", "[a](#top)\n\n## Top\n")]);

        let diags = validate_forward_links(&ws);
        assert!(
            diags.is_empty(),
            "a real `top` heading resolves cleanly: {diags:?}"
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

    // --- Connectivity validation (issue 018) ---

    /// Collect the workspace-relative paths flagged by connectivity.
    fn flagged_files(ws: &Workspace) -> Vec<String> {
        let mut files: Vec<String> = validate_connectivity(ws)
            .into_iter()
            .map(|d| d.file.display().to_string())
            .collect();
        files.sort();
        files
    }

    #[test]
    fn connectivity_off_by_default() {
        // .lattice.toml present but no connectivity setting → no topology checks.
        let (_dir, ws) = setup_workspace(&[
            (".lattice.toml", "[policy]\nbacklinks = true\n"),
            ("orphan.md", "# Orphan\n"),
        ]);

        assert!(
            validate_connectivity(&ws).is_empty(),
            "connectivity defaults off: {:?}",
            validate_connectivity(&ws)
        );
    }

    #[test]
    fn no_orphans_flags_degree_zero_document() {
        let (_dir, ws) = setup_workspace(&[
            (".lattice.toml", "[policy]\nconnectivity = \"no-orphans\"\n"),
            ("index.md", r#"[a](a.md "references")"#),
            ("a.md", "# A\n"),
            ("orphan.md", "# Orphan\n"),
        ]);

        let diags = validate_connectivity(&ws);
        assert_eq!(diags.len(), 1, "only the orphan is flagged: {diags:?}");
        assert_eq!(
            diags[0].file,
            PathBuf::from("orphan.md"),
            "orphan.md flagged"
        );
        assert_eq!(diags[0].line, 1, "anchored on line 1");
        assert_eq!(
            diags[0].severity,
            Severity::Warning,
            "connectivity is a warning"
        );
        assert!(
            diags[0].message.contains("orphaned document"),
            "message names the orphan: {}",
            diags[0].message
        );
    }

    #[test]
    fn no_orphans_exempts_default_root_readme() {
        // A lone README (the default root) links nowhere but is never
        // self-flagged — a single-document workspace stays clean.
        let (_dir, ws) = setup_workspace(&[
            (".lattice.toml", "[policy]\nconnectivity = \"no-orphans\"\n"),
            ("README.md", "# Readme\n"),
        ]);

        assert!(
            validate_connectivity(&ws).is_empty(),
            "the root README is exempt: {:?}",
            validate_connectivity(&ws)
        );
    }

    #[test]
    fn no_orphans_ignores_self_loop() {
        // A document linking only to itself has no edge to any *other* document.
        let (_dir, ws) = setup_workspace(&[
            (".lattice.toml", "[policy]\nconnectivity = \"no-orphans\"\n"),
            ("selfish.md", r#"[me](selfish.md "references")"#),
        ]);

        assert_eq!(
            flagged_files(&ws),
            vec!["selfish.md".to_string()],
            "self-loop does not connect a document"
        );
    }

    #[test]
    fn no_islands_flags_disconnected_cluster() {
        let (_dir, ws) = setup_workspace(&[
            (".lattice.toml", "[policy]\nconnectivity = \"no-islands\"\n"),
            ("README.md", r#"[a](a.md "references")"#),
            ("a.md", "# A\n"),
            ("island1.md", r#"[two](island2.md "references")"#),
            ("island2.md", "# Island 2\n"),
        ]);

        assert_eq!(
            flagged_files(&ws),
            vec!["island1.md".to_string(), "island2.md".to_string()],
            "both island members flagged, root component clean"
        );
        let diags = validate_connectivity(&ws);
        assert!(
            diags[0].message.contains("isolated document"),
            "no-islands message: {}",
            diags[0].message
        );
    }

    #[test]
    fn reachable_flags_inbound_only_deadend() {
        // `lonely` links into the root component but nothing forward-navigates
        // to it from any root — connected, but unreachable.
        let (_dir, ws) = setup_workspace(&[
            (".lattice.toml", "[policy]\nconnectivity = \"reachable\"\n"),
            ("README.md", r#"[a](a.md "references")"#),
            ("a.md", "# A\n"),
            ("lonely.md", r#"[home](README.md "references")"#),
        ]);

        assert_eq!(
            flagged_files(&ws),
            vec!["lonely.md".to_string()],
            "inbound-only dead-end is unreachable"
        );
        let diags = validate_connectivity(&ws);
        assert!(
            diags[0].message.contains("unreachable document"),
            "reachable message: {}",
            diags[0].message
        );
    }

    #[test]
    fn no_islands_passes_inbound_only_deadend() {
        // The same graph as `reachable_flags_inbound_only_deadend`: `lonely`
        // shares an (undirected) edge with the root, so no-islands accepts it.
        // Demonstrates the strict superset `no-islands ⊆ reachable`.
        let (_dir, ws) = setup_workspace(&[
            (".lattice.toml", "[policy]\nconnectivity = \"no-islands\"\n"),
            ("README.md", r#"[a](a.md "references")"#),
            ("a.md", "# A\n"),
            ("lonely.md", r#"[home](README.md "references")"#),
        ]);

        assert!(
            validate_connectivity(&ws).is_empty(),
            "no-islands accepts the inbound-only dead-end: {:?}",
            validate_connectivity(&ws)
        );
    }

    #[test]
    fn reachable_uses_custom_roots() {
        let (_dir, ws) = setup_workspace(&[
            (
                ".lattice.toml",
                "[policy]\nconnectivity = \"reachable\"\nroots = [\"docs/home.md\"]\n",
            ),
            ("docs/home.md", r#"[a](../a.md "references")"#),
            ("a.md", "# A\n"),
            ("stray.md", "# Stray\n"),
        ]);

        assert_eq!(
            flagged_files(&ws),
            vec!["stray.md".to_string()],
            "traversal starts from the configured root, reaches a.md"
        );
    }

    #[test]
    fn reachable_without_resolvable_root_emits_nothing() {
        // Default root README.md is absent, so `reachable` has no anchor and
        // stays silent rather than flagging every document.
        let (_dir, ws) = setup_workspace(&[
            (".lattice.toml", "[policy]\nconnectivity = \"reachable\"\n"),
            ("a.md", "# A\n"),
            ("b.md", "# B\n"),
        ]);

        assert!(
            validate_connectivity(&ws).is_empty(),
            "no root → no reachable diagnostics: {:?}",
            validate_connectivity(&ws)
        );
    }
}
