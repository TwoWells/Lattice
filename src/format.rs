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
//!
//! "The frontmatter Lattice owns" is exactly the `backlinks` entry of a YAML
//! carrier — the span [`crate::fm::backlinks_region`] reports (issue 079).
//! Everything else in the carrier is somebody else's and is byte-preserved: the
//! `exceptions` block (decisions 011/012), arbitrary user fields, the
//! delimiters, and the surrounding blank and comment lines. A TOML or JSON
//! carrier has no owned region at all, since Lattice canonicalizes backlinks
//! only in YAML; such a document is left byte-identical rather than converted.

use std::fmt::Write as _;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::workspace::{Frontmatter, Workspace};

/// Compute the formatted form of a document, or `None` when nothing applies.
///
/// The two formatting inputs are the parsed `frontmatter` (whose `backlinks`
/// are sorted and re-emitted over its
/// [`backlinks_region`](Frontmatter::backlinks_region) — and *only* over that
/// region, so every other frontmatter byte survives verbatim) and the optional
/// external `format_command` (which receives the whole post-sort document on
/// stdin and returns the formatted document on stdout). When the document has no
/// backlinks to sort and no formatter is configured, there is nothing to do and
/// this returns `None`.
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
    // The bytes this pass owns: the `backlinks` entry of a YAML carrier that
    // actually declares backlinks. A carrier with no owned region (no
    // `backlinks` key, or a TOML/JSON block) contributes no rewrite at all.
    let owned = frontmatter.and_then(|fm| {
        if fm.backlinks.is_empty() {
            None
        } else {
            fm.backlinks_region.clone()
        }
    });

    // Nothing to do if there are no backlinks to sort and no external formatter.
    if owned.is_none() && format_command.is_none() {
        return None;
    }

    // Step 1: sort the backlinks entry in place, over its own span only. The
    // span stops before its last line's terminator, so the replacement carries
    // no trailing newline and the bytes on either side — sibling keys such as
    // `exceptions`, arbitrary user fields, the `---` delimiters — are untouched
    // (issue 079).
    let mut document = source.to_string();
    if let (Some(fm), Some(region)) = (frontmatter, owned) {
        let replacement = sorted_backlinks_block(fm, line_ending_after(source, region.end));
        document.replace_range(region, &replacement);
    }

    // Step 2: pipe the whole document through the external formatter, if any.
    if let Some(cmd) = format_command
        && let Some(formatted) = run_formatter(cmd, &document)
    {
        document = formatted;
    }

    Some(document)
}

/// Render a `Frontmatter`'s backlinks as the normalized `backlinks:` entry:
/// predicate keys alphabetical, paths within each predicate sorted, two-space
/// indentation. This is the exact text that replaces the entry's
/// [`backlinks_region`](Frontmatter::backlinks_region).
///
/// No delimiters and no trailing terminator are emitted: the region covers the
/// entry alone and stops before its last line ending, so the block it is spliced
/// into keeps its own `---` (or fence) and its own line structure.
///
/// `newline` is the document's line-ending style, so a CRLF file keeps CRLF
/// endings inside the rebuilt entry instead of acquiring mixed ones.
fn sorted_backlinks_block(fm: &Frontmatter, newline: &str) -> String {
    let mut sorted: std::collections::BTreeMap<&str, Vec<&str>> = std::collections::BTreeMap::new();
    for (pred, paths) in &fm.backlinks {
        let mut path_refs: Vec<&str> = paths.iter().map(String::as_str).collect();
        path_refs.sort_unstable();
        sorted.insert(pred.as_str(), path_refs);
    }

    let mut yaml = String::from("backlinks:");
    for (pred, paths) in &sorted {
        let _ = write!(yaml, "{newline}  {pred}:");
        for path in paths {
            let _ = write!(yaml, "{newline}    - {path}");
        }
    }
    yaml
}

/// The line-ending style to re-render the backlinks entry with: the terminator
/// that immediately follows its region (`\r\n`, a bare `\r`, or `\n`).
///
/// The region ends just before its last line's terminator, so the byte at `end`
/// is that terminator — which is the document's own style. Defaulting to `\n`
/// covers the region ending at EOF with no terminator at all.
fn line_ending_after(source: &str, end: usize) -> &'static str {
    let rest = &source[end..];
    if rest.starts_with("\r\n") {
        "\r\n"
    } else if rest.starts_with('\r') {
        "\r"
    } else {
        "\n"
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

    /// Run `lattice lint` with the full suppression ledger (issue 036), so the
    /// compared observable includes *what the exceptions hid* — the part a
    /// carrier-destroying format pass moves even when the live diagnostics
    /// happen to match.
    fn lint_output_with_ledger(dir: &TempDir) -> String {
        let mut buf = Vec::new();
        lint::run(dir.path(), false, false, true, &mut buf).expect("lint run should succeed");
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
    fn broken_config_refuses_the_format_run() {
        // Decision 023, issue 065: `format` must not run with its `[format]`
        // table silently gone — a present-but-unreadable `.lattice.toml`
        // refuses at the scan layer (exit 2 via the CLI's chain mapping),
        // writing nothing.
        let original = "---\nbacklinks:\n  referenced_by:\n    - z.md\n    - a.md\n---\n\n# A\n";
        let dir = setup(&[(".lattice.toml", "[[override\n"), ("a.md", original)]);

        let mut buf = Vec::new();
        let err = run(dir.path(), false, &mut buf).expect_err("a broken config refuses the run");
        assert!(
            err.chain().any(|cause| matches!(
                cause.downcast_ref::<crate::workspace::WorkspaceError>(),
                Some(crate::workspace::WorkspaceError::Config { .. })
            )),
            "the refusal carries the config error for the exit-2 mapping: {err:#}"
        );
        assert_eq!(
            read(&dir, "a.md"),
            original,
            "the refused run rewrites nothing, not even sortable backlinks"
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

    // -- Issue 079: format owns the `backlinks` entry and nothing else -------

    #[test]
    fn literal_exception_keys_and_reasons_survive_format() {
        // Issue 079: a format pass rebuilt the whole carrier out of the parsed
        // backlinks, so every key it did not own was deleted with it —
        // `exceptions` first among them. The reason is an epitaph (decision
        // 011): the only surviving record of a vanished reference's intent, so
        // losing it is unrecoverable data loss, not a reformat.
        let dir = setup(&[(
            "a.md",
            "---\nbacklinks:\n  referenced_by:\n    - z.md\n    - b.md\nexceptions:\n  stale_references:\n    \"gone.md\": \"deleted in the 2025 archive sweep\"\n  bare_paths:\n    \"docs/notes.md\": \"prose, deliberately not a link\"\n---\n\n# A\n",
        )]);

        let (changed, _) = run_format(&dir, false);
        assert!(changed, "the drifted backlinks must actually be rewritten");

        let formatted = read(&dir, "a.md");
        assert_eq!(
            formatted,
            "---\nbacklinks:\n  referenced_by:\n    - b.md\n    - z.md\nexceptions:\n  stale_references:\n    \"gone.md\": \"deleted in the 2025 archive sweep\"\n  bare_paths:\n    \"docs/notes.md\": \"prose, deliberately not a link\"\n---\n\n# A\n",
            "only the backlinks entry may be re-rendered; the exceptions block, its keys, and its reasons are byte-preserved"
        );
    }

    #[test]
    fn count_key_exceptions_survive_format() {
        // Issue 079 for the other half of the suppression mechanism: a
        // count-key (decision 012, issue 036) is an all-digits sentinel
        // claiming a document's residual. Deleting it silently un-suppresses
        // the whole residual.
        let dir = setup(&[(
            "a.md",
            "---\nbacklinks:\n  referenced_by:\n    - z.md\n    - b.md\nexceptions:\n  stale_references:\n    3: \"the whole 2025 archive sweep\"\n  bare_paths:\n    \"2\": \"two prose paths, deliberate\"\n---\n\n# A\n",
        )]);

        let (changed, _) = run_format(&dir, false);
        assert!(changed, "the drifted backlinks must actually be rewritten");

        let formatted = read(&dir, "a.md");
        assert_eq!(
            formatted,
            "---\nbacklinks:\n  referenced_by:\n    - b.md\n    - z.md\nexceptions:\n  stale_references:\n    3: \"the whole 2025 archive sweep\"\n  bare_paths:\n    \"2\": \"two prose paths, deliberate\"\n---\n\n# A\n",
            "count keys and their shared reasons survive a format pass verbatim"
        );

        // And they survive as *sentinels*, not just as text.
        let workspace =
            crate::workspace::Workspace::scan(dir.path()).expect("rescan the formatted workspace");
        let fm = workspace
            .file(std::path::Path::new("a.md"))
            .and_then(|file| file.frontmatter.as_ref())
            .expect("the formatted file still parses frontmatter");
        assert_eq!(
            fm.exceptions
                .stale_references_count
                .as_ref()
                .map(|count| count.expected),
            Some(3),
            "the stale_references count-key still reconciles after format"
        );
        assert_eq!(
            fm.exceptions
                .bare_paths_count
                .as_ref()
                .map(|count| count.expected),
            Some(2),
            "the quoted bare_paths count-key still reconciles after format"
        );
    }

    #[test]
    fn unknown_user_frontmatter_fields_survive_format() {
        // The contract is "only the bytes Lattice owns": arbitrary user
        // frontmatter — scalars, nested maps, sequences — sits either side of
        // the backlinks entry and must come through byte-identical, with only
        // the backlinks entry canonicalized (issue 079).
        let dir = setup(&[(
            "a.md",
            "---\ntitle: A Document\nbacklinks:\n  referenced_by:\n    - z.md\n    - b.md\n  amended_by:\n    - c.md\nmeta:\n  owner: mark\n  tags:\n    - alpha\n    - beta\nexceptions:\n  stale_references:\n    \"gone.md\": \"deleted\"\n---\n\n# A\n",
        )]);

        let (changed, _) = run_format(&dir, false);
        assert!(changed, "the drifted backlinks must actually be rewritten");

        assert_eq!(
            read(&dir, "a.md"),
            "---\ntitle: A Document\nbacklinks:\n  amended_by:\n    - c.md\n  referenced_by:\n    - b.md\n    - z.md\nmeta:\n  owner: mark\n  tags:\n    - alpha\n    - beta\nexceptions:\n  stale_references:\n    \"gone.md\": \"deleted\"\n---\n\n# A\n",
            "predicates and paths sort, and every other frontmatter field — before and after the entry, scalar and nested — is untouched"
        );
    }

    #[test]
    fn format_is_a_graph_no_op_on_a_suppressing_fixture() {
        // The contract, stated directly: `lattice lint` produces the same
        // diagnostics before and after a format pass — *including* what the
        // suppression ledger reports as hidden. Issue 079 broke exactly this:
        // deleting the exceptions block turned suppressed hints back into live
        // diagnostics, so a format pass silently changed lint's verdict.
        let dir = setup(&[
            (".lattice.toml", ""),
            (
                "a.md",
                "---\nbacklinks:\n  referenced_by:\n    - z.md\n    - index.md\nexceptions:\n  stale_references:\n    \"missing.md\": \"the target was deleted in 2025\"\n---\n\n# A\n\nSee `missing.md` for the history.\n",
            ),
            ("index.md", "# Index\n\n[a](a.md \"references\")\n"),
            ("z.md", "# Z\n\n[a](a.md \"references\")\n"),
        ]);

        let before = lint_output_with_ledger(&dir);
        assert!(
            before.contains("suppressed: 1 warning"),
            "the fixture must actually exercise a live suppression: {before}"
        );

        let (changed, _) = run_format(&dir, false);
        assert!(
            changed,
            "the drifted fixture must actually be rewritten (bytes change)"
        );

        let after = lint_output_with_ledger(&dir);
        assert_eq!(
            before, after,
            "diagnostics and suppression counts must be identical before and after a format pass"
        );
    }

    #[test]
    fn canonical_file_with_exceptions_is_stable_under_check() {
        // `--check` is advertised as a CI gate, so it must agree with the write
        // pass: an already-canonical file carrying exceptions is a fixed point.
        // Issue 079 made it perpetually dirty — every run "found" the same
        // deletion to make.
        let original = "---\nbacklinks:\n  referenced_by:\n    - b.md\n    - z.md\nexceptions:\n  stale_references:\n    \"gone.md\": \"deleted in the 2025 archive sweep\"\n    4: \"the rest of the sweep\"\n---\n\n# A\n";
        let dir = setup(&[("a.md", original)]);

        let (changed, output) = run_format(&dir, true);
        assert!(
            !changed,
            "a canonical file with exceptions must pass --check: {output}"
        );
        assert!(
            output.is_empty(),
            "--check on a clean tree prints nothing: {output}"
        );

        let (rewrote, _) = run_format(&dir, false);
        assert!(!rewrote, "the write pass must agree with --check");
        assert_eq!(
            read(&dir, "a.md"),
            original,
            "format(x) == x for an already-canonical file with exceptions"
        );

        // And formatting is idempotent from a drifted start: format(format(x))
        // == format(x).
        let dir = setup(&[(
            "a.md",
            "---\nbacklinks:\n  referenced_by:\n    - z.md\n    - b.md\nexceptions:\n  stale_references:\n    \"gone.md\": \"deleted in the 2025 archive sweep\"\n    4: \"the rest of the sweep\"\n---\n\n# A\n",
        )]);
        let (changed, _) = run_format(&dir, false);
        assert!(changed, "the drifted file must be rewritten once");
        let once = read(&dir, "a.md");
        let (changed_again, output) = run_format(&dir, false);
        assert!(!changed_again, "the second pass must be a no-op: {output}");
        assert_eq!(read(&dir, "a.md"), once, "format(format(x)) == format(x)");
        assert_eq!(
            once, original,
            "the drifted file converges on the canonical form"
        );
    }

    #[test]
    fn fenced_carrier_backlinks_sort_without_gaining_delimiters() {
        // A `yaml lattice` carrier's byte range is the *in-fence body*
        // (decision 015), so rebuilding the block as a `---`-delimited document
        // wrote YAML document markers inside the fence and deleted the rest of
        // the body (issue 079). Sorting the entry in place cannot do either.
        let dir = setup(&[(
            "a.md",
            "# A\n\nbody\n\n<details><summary>lattice</summary>\n\n```yaml lattice\nbacklinks:\n  referenced_by:\n    - z.md\n    - b.md\nexceptions:\n  bare_paths:\n    \"docs/notes.md\": \"prose\"\n```\n\n</details>\n",
        )]);

        let (changed, _) = run_format(&dir, false);
        assert!(changed, "the drifted carrier must be rewritten");

        assert_eq!(
            read(&dir, "a.md"),
            "# A\n\nbody\n\n<details><summary>lattice</summary>\n\n```yaml lattice\nbacklinks:\n  referenced_by:\n    - b.md\n    - z.md\nexceptions:\n  bare_paths:\n    \"docs/notes.md\": \"prose\"\n```\n\n</details>\n",
            "the carrier body sorts in place: no `---` delimiters injected, no sibling keys lost"
        );
    }

    #[test]
    fn toml_and_json_carriers_are_left_byte_identical() {
        // Lattice canonicalizes backlinks only in YAML, so a `+++` or `{`
        // carrier has no owned region: formatting it as a `---` block would
        // have converted the syntax and dropped every other key (issue 079).
        // Leaving it alone is the honest no-op.
        let toml = "+++\ntitle = \"Toml Doc\"\n[backlinks]\nreferenced_by = [\"z.md\", \"b.md\"]\n+++\n\n# Toml\n";
        let json = "{\n  \"title\": \"Json Doc\",\n  \"backlinks\": { \"referenced_by\": [\"z.md\", \"b.md\"] }\n}\n\n# Json\n";
        let dir = setup(&[("t.md", toml), ("j.md", json)]);

        let (changed, output) = run_format(&dir, true);
        assert!(
            !changed,
            "a non-YAML carrier reports no change under --check: {output}"
        );

        let (rewrote, _) = run_format(&dir, false);
        assert!(!rewrote, "the write pass must not touch a non-YAML carrier");
        assert_eq!(
            read(&dir, "t.md"),
            toml,
            "TOML frontmatter is byte-identical"
        );
        assert_eq!(
            read(&dir, "j.md"),
            json,
            "JSON frontmatter is byte-identical"
        );
    }

    #[test]
    fn crlf_backlinks_keep_their_line_endings() {
        // The rebuilt entry inherits the document's line-ending style, so a
        // CRLF file does not come back with a mixed-ending frontmatter block.
        let dir = setup(&[(
            "a.md",
            "---\r\nbacklinks:\r\n  referenced_by:\r\n    - z.md\r\n    - b.md\r\ntitle: A\r\n---\r\n\r\n# A\r\n",
        )]);

        let (changed, _) = run_format(&dir, false);
        assert!(changed, "the drifted backlinks must be rewritten");

        assert_eq!(
            read(&dir, "a.md"),
            "---\r\nbacklinks:\r\n  referenced_by:\r\n    - b.md\r\n    - z.md\r\ntitle: A\r\n---\r\n\r\n# A\r\n",
            "the re-rendered entry keeps CRLF endings and leaves the rest of the block alone"
        );
    }

    #[test]
    fn comments_and_blank_lines_around_the_entry_survive_format() {
        // A comment between `backlinks` and the next top-level key belongs to
        // neither; the owned region stops before it. A blank line does too.
        let dir = setup(&[(
            "a.md",
            "---\n# top note\nbacklinks:\n  referenced_by:\n    - z.md\n    - b.md\n\n# a note about the exceptions\nexceptions:\n  stale_references:\n    \"gone.md\": \"deleted\"\n---\n\n# A\n",
        )]);

        let (changed, _) = run_format(&dir, false);
        assert!(changed, "the drifted backlinks must be rewritten");

        assert_eq!(
            read(&dir, "a.md"),
            "---\n# top note\nbacklinks:\n  referenced_by:\n    - b.md\n    - z.md\n\n# a note about the exceptions\nexceptions:\n  stale_references:\n    \"gone.md\": \"deleted\"\n---\n\n# A\n",
            "comments and blank lines outside the backlinks entry are not part of it"
        );
    }
}
