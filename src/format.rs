// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Document formatting engine and the `lattice format` CLI surface.
//!
//! The engine ([`format_source`]) is the single source of formatting semantics
//! shared by the LSP `textDocument/formatting` handler and the CLI: it sorts
//! backlink frontmatter (predicate keys alphabetical, paths within each
//! predicate sorted, whitespace normalized) and, when a `[format] command` is
//! configured, pipes the whole document through it (ticket integration 12).
//!
//! The CLI runner ([`run`]) applies that engine to every file in a workspace,
//! scoped exactly like `lattice lint` (issue 024): it discovers the workspace
//! root by walking up from the start path, scans every file so scoping stays
//! consistent, and formats only the files at or under the scope. In write mode
//! it rewrites changed files in place and reports each changed path; in
//! `--check` mode it writes nothing and exits non-zero listing the files whose
//! formatted form differs (ticket integration 17).
//!
//! Formatting is a **graph no-op**: it only reorders and re-whitespaces the
//! frontmatter Lattice owns (and delegates the body to the external formatter),
//! so the diagnostic set a `lattice lint` produces is identical before and
//! after a format pass. The CLI acceptance tests assert this on both a clean
//! and a drifted fixture.

use std::fmt::Write as _;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::workspace::{Frontmatter, Workspace};

/// Compute the formatted form of a document, or `None` when nothing applies.
///
/// The two formatting inputs are the parsed `frontmatter` (whose `backlinks`
/// are sorted and re-emitted over its `byte_range`) and the optional external
/// `format_command` (which receives the whole post-sort document on stdin and
/// returns the formatted document on stdout). When the document has no backlinks
/// to sort and no formatter is configured, there is nothing to do and this
/// returns `None`.
///
/// The returned string is the full formatted document. It may still be
/// byte-identical to `source` (e.g. backlinks already sorted, or a formatter
/// that is a no-op on this input); the caller decides "changed" by comparing
/// bytes. This keeps the change decision — and the exit-code / write decision
/// that rides on it — in one place.
///
/// This is the single source of formatting semantics: the LSP formatting
/// handler and the [`run`] CLI both call it, so the two cannot drift.
#[must_use]
pub fn format_source(
    source: &str,
    frontmatter: Option<&Frontmatter>,
    format_command: Option<&str>,
) -> Option<String> {
    let has_backlinks = frontmatter.is_some_and(|fm| !fm.backlinks.is_empty());

    // Nothing to do if there are no backlinks to sort and no external formatter.
    if !has_backlinks && format_command.is_none() {
        return None;
    }

    // Step 1: sort frontmatter backlinks in place over the carrier's byte range.
    let mut document = source.to_string();
    if let Some(fm) = frontmatter
        && !fm.backlinks.is_empty()
    {
        // The carrier's `byte_range` includes the line ending that follows the
        // closing `---` delimiter, so the rebuilt block must reproduce that same
        // trailing terminator — otherwise an already-sorted file loses the
        // newline after `---` and re-formatting is no longer a byte-for-byte
        // no-op (which the graph-no-op contract requires).
        let original = &source[fm.byte_range.clone()];
        let replacement = sorted_backlinks_block(fm, trailing_line_ending(original));
        document.replace_range(fm.byte_range.clone(), &replacement);
    }

    // Step 2: pipe the whole document through the external formatter, if any.
    if let Some(cmd) = format_command
        && let Some(formatted) = run_formatter(cmd, &document)
    {
        document = formatted;
    }

    Some(document)
}

/// Render a `Frontmatter`'s backlinks as a normalized `---`-delimited YAML
/// block: predicate keys alphabetical, paths within each predicate sorted,
/// two-space indentation. This is the exact text that replaces the carrier's
/// `byte_range`.
///
/// `trailing` is the line ending that followed the closing `---` in the original
/// block (the carrier's `byte_range` includes it), re-appended so an
/// already-sorted document round-trips to identical bytes.
fn sorted_backlinks_block(fm: &Frontmatter, trailing: &str) -> String {
    let mut sorted: std::collections::BTreeMap<&str, Vec<&str>> = std::collections::BTreeMap::new();
    for (pred, paths) in &fm.backlinks {
        let mut path_refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        path_refs.sort_unstable();
        sorted.insert(pred.as_str(), path_refs);
    }

    let mut yaml = String::from("---\nbacklinks:\n");
    for (pred, paths) in &sorted {
        let _ = writeln!(yaml, "  {pred}:");
        for path in paths {
            let _ = writeln!(yaml, "    - {path}");
        }
    }
    yaml.push_str("---");
    yaml.push_str(trailing);
    yaml
}

/// The trailing line ending of a frontmatter block's original text.
///
/// The carrier's `byte_range` includes the terminator after the closing `---`
/// (`\n`, `\r\n`, or a bare `\r`), or nothing when the block ends at EOF with no
/// newline. Returns that exact terminator so the rebuilt block preserves it.
fn trailing_line_ending(original: &str) -> &str {
    if original.ends_with("\r\n") {
        &original[original.len() - 2..]
    } else if original.ends_with(['\n', '\r']) {
        &original[original.len() - 1..]
    } else {
        ""
    }
}

/// Run an external formatter command, piping `content` through stdin/stdout.
///
/// The command is passed to `sh -c` so shell features (pipes, quoted args,
/// environment variables) work as expected. Returns `None` — leaving the
/// pre-formatter document unchanged — when the command fails to spawn, exits
/// non-zero, or emits non-UTF-8, so a broken formatter never corrupts a file.
fn run_formatter(command: &str, content: &str) -> Option<String> {
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    let mut child = Command::new("sh")
        .args(["-c", command])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(content.as_bytes());
    }

    let output = child.wait_with_output().ok()?;
    if output.status.success() {
        String::from_utf8(output.stdout).ok()
    } else {
        tracing::warn!(
            "formatter exited with status {}: {}",
            output.status,
            command
        );
        None
    }
}

/// Format every file in the workspace scoped to `start`, writing changes in
/// place (or, under `check`, only reporting the files that would change).
///
/// `start` is both the discovery hint and the format scope, mirroring
/// `lattice lint` (issue 024): the workspace root (and `.lattice.toml`) is
/// discovered by walking up from `start`, the whole workspace is scanned so the
/// scope filter is consistent, and only files at or under `start` are
/// considered. Every path spelling (`archive`, `archive/`, `./archive/`, the
/// absolute form, a single file) normalizes to one scope.
///
/// A file "changes" when [`format_source`] yields text that differs from its
/// current bytes. In write mode each changed file is rewritten and its path
/// reported to `out`; in `check` mode nothing is written and each would-change
/// path is reported. Returns `true` when at least one file changed (the caller
/// maps that to a non-zero exit code under `--check`, mirroring the lint
/// exit-code contract); the write mode returns the same flag so a caller can
/// tell whether anything was rewritten, but a successful write is not itself a
/// failure.
///
/// # Errors
///
/// Returns an error if the workspace cannot be scanned, a file cannot be
/// rewritten, or output cannot be written.
pub fn run(start: &Path, check: bool, out: &mut impl Write) -> Result<bool> {
    let workspace = Workspace::scan(start).context("failed to scan workspace")?;
    let scope = scope_relative_to_root(start, workspace.root());

    let mut changed_any = false;
    // Iterate in the workspace's deterministic key order (a `BTreeMap`), so the
    // reported paths are stable across runs.
    for (rel_path, file_data) in workspace.files() {
        if !in_scope(rel_path, scope.as_deref()) {
            continue;
        }

        let source = file_data.tree.source();
        let Some(formatted) = format_source(
            source,
            file_data.frontmatter.as_ref(),
            workspace.config().format_command.as_deref(),
        ) else {
            continue;
        };

        if formatted == source {
            continue;
        }

        changed_any = true;
        if check {
            writeln!(out, "{}", rel_path.display())?;
        } else {
            let abs_path = workspace.root().join(rel_path);
            std::fs::write(&abs_path, &formatted)
                .with_context(|| format!("failed to write {}", abs_path.display()))?;
            writeln!(out, "formatted {}", rel_path.display())?;
        }
    }

    Ok(changed_any)
}

/// Express `start` as a path relative to the workspace `root`, for scoping.
///
/// Returns `None` when the format pass should cover the whole workspace:
/// `start` resolves to the root itself, or it cannot be normalized against the
/// root (in which case scoping is skipped rather than risk silently dropping
/// files). Otherwise returns the workspace-relative scope — a directory prefix
/// or a single file — with a leading `./`, a trailing slash, and the
/// relative-vs-absolute distinction all erased by canonicalization. This is the
/// exact scoping `lattice lint` uses (issue 024), kept identical so the two
/// commands agree on where a scope begins.
fn scope_relative_to_root(start: &Path, root: &Path) -> Option<PathBuf> {
    let abs_start = std::fs::canonicalize(start).ok()?;
    let abs_root = std::fs::canonicalize(root).ok()?;
    let rel = abs_start.strip_prefix(&abs_root).ok()?;
    if rel.as_os_str().is_empty() {
        None
    } else {
        Some(rel.to_path_buf())
    }
}

/// Whether a file at workspace-relative `file` is within `scope`.
///
/// `scope` is `None` for a whole-workspace pass (everything is in scope). A
/// `Some` scope matches when `file` equals it (a single-file scope) or is
/// nested under it (a directory scope) — component-wise, so `archive` matches
/// `archive/x.md` but never `archived/y.md`.
fn in_scope(file: &Path, scope: Option<&Path>) -> bool {
    scope.is_none_or(|scope| file.starts_with(scope))
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

    use super::{in_scope, run};
    use crate::lint;

    /// Create a workspace with the given files and return the temp dir. Mirrors
    /// the lint-suite fixture helper: a `.git` marker makes the temp dir a
    /// discoverable root.
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

    /// Run `lattice format` on a temp dir. Returns (`changed`, reported output).
    fn run_format(dir: &TempDir, check: bool) -> (bool, String) {
        let mut buf = Vec::new();
        let changed = run(dir.path(), check, &mut buf).expect("format run should succeed");
        let output = String::from_utf8(buf).expect("output should be utf-8");
        (changed, output)
    }

    /// Run `lattice lint` on a temp dir with the ledger suppressed, returning
    /// the diagnostic output — the graph observable a format pass must not move.
    fn lint_output(dir: &TempDir) -> String {
        let mut buf = Vec::new();
        lint::run(dir.path(), false, true, false, &mut buf).expect("lint run should succeed");
        String::from_utf8(buf).expect("lint output should be utf-8")
    }

    /// Read a file's bytes as a string.
    fn read(dir: &TempDir, rel: &str) -> String {
        fs::read_to_string(dir.path().join(rel)).expect("read file back")
    }

    #[test]
    fn unsorted_backlinks_fail_check_then_pass_after_format() {
        // Acceptance: a file with unsorted backlinks fails `--check`, is
        // rewritten by `lattice format`, then passes `--check`.
        let dir = setup(&[(
            "a.md",
            "---\nbacklinks:\n  referenced_by:\n    - z.md\n    - a.md\n  amended_by:\n    - b.md\n---\n\n# A\n",
        )]);

        let (changed, output) = run_format(&dir, true);
        assert!(
            changed,
            "unsorted backlinks must fail --check (report a change): {output}"
        );
        assert!(
            output.contains("a.md"),
            "the drifted file must be named in --check output: {output}"
        );

        let (rewrote, _) = run_format(&dir, false);
        assert!(rewrote, "the write pass must rewrite the drifted file");

        let formatted = read(&dir, "a.md");
        let amended = formatted.find("amended_by").expect("amended_by present");
        let referenced = formatted
            .find("referenced_by")
            .expect("referenced_by present");
        assert!(
            amended < referenced,
            "predicates must sort alphabetically after format: {formatted}"
        );
        let a_pos = formatted.find("- a.md").expect("a.md path present");
        let z_pos = formatted.find("- z.md").expect("z.md path present");
        assert!(
            a_pos < z_pos,
            "paths within a predicate must sort after format: {formatted}"
        );

        let (still_changed, output) = run_format(&dir, true);
        assert!(
            !still_changed,
            "a formatted file must pass --check with no reported change: {output}"
        );
    }

    #[test]
    fn no_backlinks_no_config_is_byte_identical_and_check_passes() {
        // Acceptance: with no backlinks and no `[format]` config, files are
        // byte-identical and `--check` passes with exit 0.
        let original = "# Title\n\nA plain document with no frontmatter.\n";
        let dir = setup(&[("plain.md", original)]);

        let (changed, output) = run_format(&dir, true);
        assert!(
            !changed,
            "a file with no backlinks and no formatter must not change: {output}"
        );
        assert!(
            output.is_empty(),
            "--check on an already-clean tree prints nothing: {output}"
        );

        // A write pass must leave the bytes untouched.
        let (rewrote, _) = run_format(&dir, false);
        assert!(!rewrote, "the write pass must not report a change");
        assert_eq!(
            read(&dir, "plain.md"),
            original,
            "the file must be byte-identical after a format pass"
        );
    }

    #[test]
    fn format_is_a_graph_no_op_on_a_drifted_fixture() {
        // Acceptance: `lattice lint` output is unchanged by a format pass on a
        // drifted fixture (graph no-op). The fixture links reciprocally so the
        // graph carries real backlink structure; the frontmatter is drifted
        // (unsorted) so a format pass genuinely rewrites bytes.
        let dir = setup(&[
            (".lattice.toml", ""),
            (
                "index.md",
                "---\nbacklinks:\n  referenced_by:\n    - other.md\n---\n\n[other](other.md \"references\")\n",
            ),
            (
                "other.md",
                "---\nbacklinks:\n  referenced_by:\n    - index.md\n  amended_by:\n    - z.md\n---\n\n[index](index.md \"references\")\n",
            ),
        ]);

        let before = lint_output(&dir);

        let (changed, _) = run_format(&dir, false);
        assert!(
            changed,
            "the drifted fixture must actually be rewritten (bytes change)"
        );

        let after = lint_output(&dir);
        assert_eq!(
            before, after,
            "the diagnostic set must be identical before and after a format pass (graph no-op)"
        );
    }

    #[test]
    fn format_is_a_graph_no_op_on_a_clean_fixture() {
        // Acceptance: the graph no-op also holds on a clean fixture — a format
        // pass that changes nothing still cannot move the diagnostics.
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

        let before = lint_output(&dir);
        let index_before = read(&dir, "index.md");

        let (changed, _) = run_format(&dir, false);
        assert!(
            !changed,
            "an already-sorted fixture must not be rewritten by a format pass"
        );
        assert_eq!(
            read(&dir, "index.md"),
            index_before,
            "a clean file must be byte-identical after a format pass"
        );

        let after = lint_output(&dir);
        assert_eq!(
            before, after,
            "a no-op format pass must leave the diagnostic set identical (graph no-op)"
        );
    }

    #[test]
    fn scoped_format_touches_only_in_scope_files() {
        // Path scoping mirrors `lattice lint` (issue 024): a scoped format pass
        // rewrites only files at or under the scope, leaving out-of-scope
        // drifted files untouched.
        let dir = setup(&[
            (
                "sub/a.md",
                "---\nbacklinks:\n  referenced_by:\n    - z.md\n    - a.md\n---\n\n# A\n",
            ),
            (
                "other/b.md",
                "---\nbacklinks:\n  referenced_by:\n    - z.md\n    - a.md\n---\n\n# B\n",
            ),
        ]);
        let other_before = read(&dir, "other/b.md");

        let mut buf = Vec::new();
        let changed = run(&dir.path().join("sub"), false, &mut buf).expect("scoped format run");
        let output = String::from_utf8(buf).expect("output should be utf-8");

        assert!(changed, "the in-scope drifted file must be rewritten");
        assert!(
            output.contains("sub/a.md") || output.contains("sub\\a.md") || output.contains("a.md"),
            "the in-scope file must be reported: {output}"
        );
        assert!(
            !output.contains("b.md"),
            "the out-of-scope file must not be reported: {output}"
        );
        assert_eq!(
            read(&dir, "other/b.md"),
            other_before,
            "the out-of-scope file must be left byte-identical"
        );
    }

    #[test]
    fn in_scope_matches_directory_and_file_but_not_sibling_prefix() {
        use std::path::{Path, PathBuf};

        let scope = PathBuf::from("archive");
        assert!(
            in_scope(Path::new("archive/x.md"), Some(&scope)),
            "a file under the scoped directory is in scope"
        );
        assert!(
            !in_scope(Path::new("archived/y.md"), Some(&scope)),
            "a sibling sharing a name prefix must not be in scope (component-wise)"
        );
        assert!(
            in_scope(Path::new("anything/at/all.md"), None),
            "a None scope means the whole workspace is in scope"
        );
    }
}
