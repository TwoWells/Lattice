// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Workspace scanning and file indexing.
//!
//! Discovers all markdown files under the workspace root, parses them into
//! an in-memory index backed by the unified parse tree, and supports
//! incremental updates when individual files change.

use std::collections::{BTreeMap, HashMap};
use std::ops::Range;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::block::{self, Syntax, Tree};
use crate::config::{Config, ConfigError};
use crate::fm;
use crate::json;
use crate::line_index::LineIndex;
use crate::structural;
use crate::toml;
use crate::validation::Diagnostic;
use crate::yaml;

/// Errors that can occur during workspace operations.
#[derive(Debug, Error)]
pub enum WorkspaceError {
    /// Failed to read a markdown file.
    #[error("failed to read {path}: {source}")]
    Read {
        /// Path that could not be read.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },

    /// Failed to determine the workspace root.
    #[error("could not determine workspace root from {start}")]
    NoRoot {
        /// The starting path used for the search.
        start: PathBuf,
    },
}

/// Parsed data for a single markdown file.
#[derive(Debug)]
pub struct FileData {
    /// The unified parse tree.
    pub tree: Tree,
    /// Parsed frontmatter, if present.
    pub frontmatter: Option<Frontmatter>,
    /// Diagnostics from frontmatter parsing (unknown backlink predicates).
    pub backlink_diagnostics: Vec<BacklinkDiagnostic>,
    /// Frontmatter parse diagnostics (partial recovery — file is still indexed).
    pub parse_diagnostics: Vec<ParseDiagnostic>,
    /// Cached structural diagnostics (`structural::collect` output) for this
    /// file.
    ///
    /// Structural diagnostics are file-local: they depend only on this file's
    /// tree plus workspace *membership* (the bare-path "refers to an existing
    /// file" check reads the file set). The cache is refreshed when this file
    /// is reparsed and, on a membership change, for every file — so the
    /// diagnostic collectors read it directly instead of re-walking every
    /// cached tree on each sync (issue 013 — stage 2).
    pub structural: Vec<Diagnostic>,
    /// Cached extracted headings (`Tree::headings()` output, with precomputed
    /// github/gitlab/vscode slugs) for this file.
    ///
    /// Unlike `structural`, which also reads workspace membership, this is a
    /// pure function of this file's own tree — so it is built directly in the
    /// parse path and refreshed exactly when the file reparses. Fragment
    /// validation reads it instead of re-deriving a linked document's headings
    /// once per `file.md#heading` reference (issue 013 — ticket perf 06).
    pub headings: Vec<block::Heading>,
    /// Cached extracted links (`Tree::links()` output) for this file, classified
    /// against its own workspace-relative path.
    ///
    /// Like `headings`, a pure function of this file's tree and path, rebuilt
    /// only on reparse. The forward-link, backlink, connectivity, and
    /// reciprocal-link validators read it instead of re-walking and
    /// re-classifying every link on each sync (ticket perf 06).
    pub links: Vec<block::Link>,
    /// Cached byte-offset ↔ LSP-position map for this file's source.
    ///
    /// Built from `tree.source()` once per parse, so it refreshes exactly when
    /// the file reparses — like `headings`/`links`, a pure function of this
    /// file's own text. Diagnostic materialization routes its byte→UTF-16
    /// position conversion through it instead of re-walking the source per
    /// diagnostic, and the inverse direction feeds the future incremental
    /// text-sync path (ticket perf 01).
    pub line_index: LineIndex,
}

/// Parsed frontmatter from a markdown document.
#[derive(Debug)]
pub struct Frontmatter {
    /// Byte range of the entire frontmatter block (including `---` delimiters).
    pub byte_range: Range<usize>,
    /// 1-based line of the opening `---`.
    pub start_line: usize,
    /// 1-based line of the closing `---`.
    pub end_line: usize,
    /// Parsed backlinks: backlink label → list of relative file paths. The
    /// label is any known predicate — an inverse value or a forward label
    /// (decision 008).
    pub backlinks: HashMap<String, Vec<String>>,
}

/// A diagnostic about a backlink predicate issue.
#[derive(Debug, PartialEq, Eq)]
pub struct BacklinkDiagnostic {
    /// 1-based line number of the predicate key in the source file.
    pub line: usize,
    /// The unknown backlink predicate (known in neither direction).
    pub predicate: String,
}

/// A parse diagnostic from frontmatter.
#[derive(Debug)]
pub struct ParseDiagnostic {
    /// 1-based line number.
    pub line: usize,
    /// Severity level.
    pub severity: fm::FmSeverity,
    /// Human-readable message.
    pub message: String,
}

/// In-memory index of all markdown files in a workspace.
#[derive(Debug)]
pub struct Workspace {
    /// Absolute path to the workspace root directory.
    root: PathBuf,
    /// Configuration loaded from the workspace.
    config: Config,
    /// Error from loading `.lattice.toml`, if any. When set, defaults were used.
    config_error: Option<ConfigError>,
    /// Whether a `.lattice.toml` file was found in the workspace root.
    has_config: bool,
    /// Parsed file data, keyed by workspace-relative path.
    files: BTreeMap<PathBuf, FileData>,
}

impl Workspace {
    /// Scan a workspace starting from `start`, discovering and parsing all
    /// markdown files.
    ///
    /// The workspace root is determined by walking up from `start` looking for
    /// `.lattice.toml`, then `.git`. Falls back to `start` itself (or its
    /// parent directory if `start` is a file).
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceError::NoRoot`] if the starting path cannot be
    /// resolved to a directory. Individual file read errors are collected
    /// but do not abort the scan.
    pub fn scan(start: &Path) -> Result<Self, WorkspaceError> {
        let root = find_workspace_root(start).ok_or_else(|| WorkspaceError::NoRoot {
            start: start.to_path_buf(),
        })?;

        let has_config = root.join(".lattice.toml").is_file();

        let (config, config_error) = match Config::load(&root) {
            Ok(c) => (c, None),
            Err(e) => {
                tracing::warn!(root = %root.display(), "config error, using defaults: {e}");
                (Config::default(), Some(e))
            }
        };

        let md_paths = discover_markdown_files(&root);

        let mut files = BTreeMap::new();

        for abs_path in md_paths {
            let rel_path = abs_path
                .strip_prefix(&root)
                .unwrap_or(&abs_path)
                .to_path_buf();

            match parse_file(&abs_path, &rel_path, &config) {
                Ok(data) => {
                    files.insert(rel_path, data);
                }
                Err(e) => {
                    tracing::warn!(path = %rel_path.display(), "failed to read file: {e}");
                }
            }
        }

        let mut workspace = Self {
            root,
            config,
            config_error,
            has_config,
            files,
        };
        // Membership is final after the scan loop, so structural caches can be
        // computed for every file now (bare-path existence sees the full set).
        workspace.recompute_all_structural();

        Ok(workspace)
    }

    /// Re-parse a single file and update the workspace index.
    ///
    /// `rel_path` must be relative to the workspace root. If the file no
    /// longer exists, it is removed from the index.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceError::Read`] if the file exists but cannot be read.
    pub fn update(&mut self, rel_path: &Path) -> Result<(), WorkspaceError> {
        let abs_path = self.root.join(rel_path);

        if !abs_path.is_file() {
            if self.files.remove(rel_path).is_some() {
                self.recompute_all_structural();
            }
            return Ok(());
        }

        let membership_changed = !self.files.contains_key(rel_path);
        match parse_file(&abs_path, rel_path, &self.config) {
            Ok(data) => {
                self.files.insert(rel_path.to_path_buf(), data);
                self.refresh_structural_after_update(rel_path, membership_changed);
            }
            Err(e) => {
                if self.files.remove(rel_path).is_some() {
                    self.recompute_all_structural();
                }
                return Err(WorkspaceError::Read {
                    path: rel_path.to_path_buf(),
                    source: e,
                });
            }
        }

        Ok(())
    }

    /// Update the index for a file using in-memory content.
    ///
    /// `rel_path` must be relative to the workspace root. The content is
    /// parsed directly without reading from disk, which is used by the LSP
    /// server for unsaved editor buffers.
    pub fn update_content(&mut self, rel_path: &Path, content: &str) {
        let membership_changed = !self.files.contains_key(rel_path);
        let data = parse_content(content, rel_path, &self.config);
        self.files.insert(rel_path.to_path_buf(), data);
        self.refresh_structural_after_update(rel_path, membership_changed);
    }

    /// Refresh the structural cache after `rel_path` was (re)parsed.
    ///
    /// An edit that does not change membership only invalidates the edited
    /// file's cache. A membership change (a file added or removed) can flip a
    /// bare-path existence answer in *any* file, so it forces a full recompute.
    fn refresh_structural_after_update(&mut self, rel_path: &Path, membership_changed: bool) {
        if membership_changed {
            self.recompute_all_structural();
        } else {
            self.recompute_structural(rel_path);
        }
    }

    /// Recompute and cache the structural diagnostics for a single indexed
    /// file from its cached tree and the current workspace membership. No-op if
    /// the path is not indexed.
    fn recompute_structural(&mut self, rel_path: &Path) {
        let Some(file_data) = self.files.get(rel_path) else {
            return;
        };
        let file_exists = |target: &Path| self.files.contains_key(target);
        let diagnostics =
            structural::collect(&file_data.tree, rel_path, &self.config, &file_exists);
        if let Some(file_data) = self.files.get_mut(rel_path) {
            file_data.structural = diagnostics;
        }
    }

    /// Recompute the structural cache for every indexed file.
    ///
    /// Required on a membership change: the bare-path "refers to an existing
    /// file" check reads the full file set, so adding or removing one file can
    /// change structural diagnostics on any other file.
    fn recompute_all_structural(&mut self) {
        let paths: Vec<PathBuf> = self.files.keys().cloned().collect();
        for path in &paths {
            self.recompute_structural(path);
        }
    }

    /// The absolute path to the workspace root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The workspace configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Error from loading `.lattice.toml`, if any.
    ///
    /// When this is `Some`, the workspace is using default configuration.
    /// The LSP should publish this as a diagnostic on the config file;
    /// the CLI should treat it as a hard error.
    pub fn config_error(&self) -> Option<&ConfigError> {
        self.config_error.as_ref()
    }

    /// Whether a `.lattice.toml` file was found in the workspace root.
    ///
    /// This gates only the **graph** diagnostic tier — forward-link
    /// existence, backlink reconciliation, and unknown predicates — which is
    /// active only when this returns `true`. The **structural** tier (heading
    /// hierarchy, trailing whitespace, HTML and code-block well-formedness,
    /// bare paths, etc.) always runs via `structural::collect`, so
    /// `has_config() == false` does not mean Lattice is silent.
    pub fn has_config(&self) -> bool {
        self.has_config
    }

    /// Parsed file data for all successfully parsed files.
    pub fn files(&self) -> &BTreeMap<PathBuf, FileData> {
        &self.files
    }

    /// Get parsed data for a specific file by its workspace-relative path.
    pub fn file(&self, rel_path: &Path) -> Option<&FileData> {
        self.files.get(rel_path)
    }
}

// --- Internal helpers ---

/// Parse a single markdown file from disk into [`FileData`].
fn parse_file(
    abs_path: &Path,
    rel_path: &Path,
    config: &Config,
) -> Result<FileData, std::io::Error> {
    let content = std::fs::read_to_string(abs_path)?;
    Ok(parse_content(&content, rel_path, config))
}

/// Parse markdown content into [`FileData`].
///
/// Always succeeds — YAML parse errors become diagnostics instead of
/// hard failures, enabling partial frontmatter recovery.
#[must_use]
pub fn parse_content(content: &str, rel_path: &Path, config: &Config) -> FileData {
    // Try YAML (`---`), then TOML (`+++`), then JSON (`{`) frontmatter.
    let (fm_block, fm_syntax) = yaml::parse_frontmatter_block(content).map_or_else(
        || {
            toml::parse_frontmatter_block(content).map_or_else(
                || {
                    json::parse_frontmatter_block(content)
                        .map_or((None, Syntax::Yaml), |block| (Some(block), Syntax::Json))
                },
                |block| (Some(block), Syntax::Toml),
            )
        },
        |block| (Some(block), Syntax::Yaml),
    );

    // Build the tree (block structure + inline elements).
    let frontmatter_span = fm_block.as_ref().map(|b| b.span);
    let frontmatter_entries = fm_block.as_ref().map(|b| b.entries.as_slice());
    let tree =
        block::parse_tree_with_entries(content, frontmatter_span, fm_syntax, frontmatter_entries);

    // Extract frontmatter data.
    let mut frontmatter = None;
    let mut backlink_diagnostics = Vec::new();
    let mut parse_diagnostics = Vec::new();

    if let Some(block) = &fm_block {
        // Collect parse diagnostics (partial recovery).
        for diag in &block.diagnostics {
            let line = byte_offset_to_line(content, diag.span.start);
            parse_diagnostics.push(ParseDiagnostic {
                line,
                severity: diag.severity,
                message: diag.message.clone(),
            });
        }

        let byte_range: Range<usize> = block.span.into();
        let start_line = 1;
        let end_byte = byte_range.end.min(content.len());
        // Step back one byte off the span end (which includes the closing
        // delimiter's line ending) so we land on the delimiter line itself
        // rather than the line after it. Recognizes all line-ending styles.
        let end_line = byte_offset_to_line(content, end_byte.saturating_sub(1));

        let backlinks = fm::extract_backlinks(block, content);

        // Validate backlink keys. A key may be any known predicate — an
        // inverse value or a forward label (decision 008) — since a forward
        // link may now derive a forward-labelled backlink on its target.
        for predicate in backlinks.keys() {
            if !config.is_known_predicate(predicate) {
                let line = fm::find_predicate_line(block, predicate, content);
                backlink_diagnostics.push(BacklinkDiagnostic {
                    line,
                    predicate: predicate.clone(),
                });
            }
        }

        frontmatter = Some(Frontmatter {
            byte_range,
            start_line,
            end_line,
            backlinks,
        });
    }

    // Collect parse diagnostics from the tree itself.
    for diag in tree.diagnostics() {
        let _ = &rel_path; // reserved for future per-file filtering
        let _ = diag;
    }

    // Extract this file's headings (with precomputed slugs) and links once, here
    // in the parse path, so the graph validators read a cached vector instead of
    // re-deriving it — `headings()` per fragment-link, `links()` per file per
    // sync (ticket perf 06). Both are pure functions of this file's own tree and
    // path, so the cache refreshes exactly when the file reparses; no
    // post-insertion workspace step is needed, unlike `structural`.
    let headings = tree.headings();
    let links = tree.links(rel_path);

    // Build the byte↔position index from the same source the tree carries, so it
    // refreshes exactly when the file reparses (ticket perf 01).
    let line_index = LineIndex::new(content);

    FileData {
        tree,
        frontmatter,
        backlink_diagnostics,
        parse_diagnostics,
        // Left empty here — `structural::collect` needs workspace membership
        // (for bare-path existence) that a standalone parse cannot know. The
        // workspace fills it via `recompute_structural` after insertion.
        structural: Vec::new(),
        headings,
        links,
        line_index,
    }
}

/// Convert a byte offset to a 1-based line number.
///
/// Recognizes `\n`, `\r\n`, and bare `\r` line endings (delegates to the
/// crate-wide counter in [`crate::fm`]).
fn byte_offset_to_line(content: &str, offset: usize) -> usize {
    fm::byte_offset_to_line(content, offset)
}

/// Walk up from `start` looking for `.lattice.toml` or `.git`.
///
/// Returns the directory containing the first marker found. Falls back to the
/// starting directory itself.
fn find_workspace_root(start: &Path) -> Option<PathBuf> {
    let dir = if start.is_file() {
        start.parent()?.to_path_buf()
    } else if start.is_dir() {
        start.to_path_buf()
    } else {
        return None;
    };

    let mut current = dir.as_path();
    loop {
        if current.join(".lattice.toml").is_file() || current.join(".git").exists() {
            return Some(current.to_path_buf());
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent,
            _ => break,
        }
    }

    // Fall back to the starting directory.
    Some(dir)
}

/// Discover all `.md` files under `root`, respecting `.gitignore`.
fn discover_markdown_files(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();

    let walker = ignore::WalkBuilder::new(root)
        .standard_filters(true)
        .build();

    for entry in walker {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.is_file()
            && path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
        {
            paths.push(path.to_path_buf());
        }
    }

    paths
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for clarity")]
mod tests {
    use std::fs;

    use super::*;

    /// Create a temp directory with `.git` marker and optional files.
    fn workspace_with_files(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("create temp dir");
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

    #[test]
    fn discovers_markdown_files() {
        let dir = workspace_with_files(&[
            ("README.md", "# Root"),
            ("docs/guide.md", "# Guide"),
            ("src/main.rs", "fn main() {}"),
        ]);

        let ws = Workspace::scan(dir.path()).expect("scan should succeed");
        assert_eq!(ws.files().len(), 2, "should find two .md files");
        assert!(
            ws.file(Path::new("README.md")).is_some(),
            "should find README.md"
        );
        assert!(
            ws.file(Path::new("docs/guide.md")).is_some(),
            "should find docs/guide.md"
        );
    }

    #[test]
    fn respects_gitignore() {
        let dir = workspace_with_files(&[
            ("README.md", "# Root"),
            (".gitignore", "build/\n"),
            ("build/output.md", "# Should be ignored"),
        ]);

        let ws = Workspace::scan(dir.path()).expect("scan should succeed");
        assert_eq!(
            ws.files().len(),
            1,
            "should find only README.md, not build/"
        );
        assert!(
            ws.file(Path::new("README.md")).is_some(),
            "should find README.md"
        );
    }

    #[test]
    fn workspace_root_from_git() {
        let dir = workspace_with_files(&[("README.md", "# Root")]);

        let ws = Workspace::scan(dir.path()).expect("scan should succeed");
        assert_eq!(
            ws.root(),
            dir.path(),
            "root should be the directory with .git"
        );
    }

    #[test]
    fn workspace_root_from_lattice_toml() {
        let dir = workspace_with_files(&[(".lattice.toml", ""), ("README.md", "# Root")]);
        // Remove .git so .lattice.toml is the marker.
        fs::remove_dir(dir.path().join(".git")).expect("remove .git");

        let ws = Workspace::scan(dir.path()).expect("scan should succeed");
        assert_eq!(
            ws.root(),
            dir.path(),
            "root should be the directory with .lattice.toml"
        );
    }

    #[test]
    fn workspace_root_from_subdirectory() {
        let dir = workspace_with_files(&[("README.md", "# Root"), ("docs/guide.md", "# Guide")]);

        let ws = Workspace::scan(&dir.path().join("docs")).expect("scan should succeed");
        assert_eq!(
            ws.root(),
            dir.path(),
            "root should be found by walking up to .git"
        );
    }

    #[test]
    fn parses_links_and_headings() {
        let dir =
            workspace_with_files(&[("doc.md", "# Title\n\n[link](other.md \"references\")\n")]);

        let ws = Workspace::scan(dir.path()).expect("scan should succeed");
        let data = ws.file(Path::new("doc.md")).expect("should find doc.md");
        let headings = data.tree.headings();
        let links = data.tree.links(Path::new("doc.md"));
        assert_eq!(headings.len(), 1, "should have one heading");
        assert_eq!(links.len(), 1, "should have one link");
    }

    #[test]
    fn parses_frontmatter_backlinks() {
        let dir = workspace_with_files(&[(
            "target.md",
            "---\nbacklinks:\n  superseded_by:\n    - source.md\n---\n# Target\n",
        )]);

        let ws = Workspace::scan(dir.path()).expect("scan should succeed");
        let data = ws
            .file(Path::new("target.md"))
            .expect("should find target.md");
        let fm = data.frontmatter.as_ref().expect("should have frontmatter");
        assert_eq!(
            fm.backlinks.get("superseded_by"),
            Some(&vec!["source.md".to_string()]),
            "should parse backlinks"
        );
        assert!(
            data.backlink_diagnostics.is_empty(),
            "known predicate should produce no diagnostics"
        );
    }

    #[test]
    fn frontmatter_error_partial_recovery() {
        let dir = workspace_with_files(&[("bad.md", "---\n: broken: yaml: [[\n---\n# Bad\n")]);

        let ws = Workspace::scan(dir.path()).expect("scan should succeed");
        // With partial recovery, the file should still be indexed.
        let data = ws
            .file(Path::new("bad.md"))
            .expect("file should be indexed");
        assert!(
            !data.parse_diagnostics.is_empty(),
            "should have parse diagnostics"
        );
    }

    #[test]
    fn incremental_update_adds_file() {
        let dir = workspace_with_files(&[("README.md", "# Root")]);

        let mut ws = Workspace::scan(dir.path()).expect("scan should succeed");
        assert_eq!(ws.files().len(), 1, "should start with one file");

        fs::write(dir.path().join("new.md"), "# New").expect("write new file");
        ws.update(Path::new("new.md"))
            .expect("update should succeed");

        assert_eq!(ws.files().len(), 2, "should have two files after update");
        assert!(ws.file(Path::new("new.md")).is_some(), "should find new.md");
    }

    #[test]
    fn incremental_update_modifies_file() {
        let dir = workspace_with_files(&[("doc.md", "# Original")]);

        let mut ws = Workspace::scan(dir.path()).expect("scan should succeed");
        let headings = ws
            .file(Path::new("doc.md"))
            .expect("should find doc.md")
            .tree
            .headings();
        assert_eq!(headings.len(), 1, "should have one heading");
        assert_eq!(headings[0].text, "Original", "heading should be Original");

        fs::write(dir.path().join("doc.md"), "# Updated\n\n## Section\n")
            .expect("overwrite doc.md");
        ws.update(Path::new("doc.md"))
            .expect("update should succeed");

        let headings = ws
            .file(Path::new("doc.md"))
            .expect("should find doc.md")
            .tree
            .headings();
        assert_eq!(headings.len(), 2, "should have two headings after update");
        assert_eq!(
            headings[0].text, "Updated",
            "first heading should be Updated"
        );
    }

    #[test]
    fn incremental_update_removes_deleted_file() {
        let dir = workspace_with_files(&[("a.md", "# A"), ("b.md", "# B")]);

        let mut ws = Workspace::scan(dir.path()).expect("scan should succeed");
        assert_eq!(ws.files().len(), 2, "should start with two files");

        fs::remove_file(dir.path().join("b.md")).expect("delete b.md");
        ws.update(Path::new("b.md")).expect("update should succeed");

        assert_eq!(ws.files().len(), 1, "should have one file after deletion");
        assert!(ws.file(Path::new("b.md")).is_none(), "b.md should be gone");
    }

    #[test]
    fn incremental_update_clears_previous_error() {
        let dir = workspace_with_files(&[("doc.md", "---\n: broken: yaml\n---\n# Bad\n")]);

        let mut ws = Workspace::scan(dir.path()).expect("scan should succeed");
        // With partial recovery, the file is now indexed with parse diagnostics.
        let data = ws.file(Path::new("doc.md")).expect("file should exist");
        assert!(
            !data.parse_diagnostics.is_empty(),
            "should have parse diagnostics initially"
        );

        fs::write(dir.path().join("doc.md"), "# Fixed\n").expect("fix doc.md");
        ws.update(Path::new("doc.md"))
            .expect("update should succeed");

        let data = ws.file(Path::new("doc.md")).expect("file should be parsed");
        assert!(
            data.parse_diagnostics.is_empty(),
            "parse diagnostics should be cleared"
        );
    }

    #[test]
    fn empty_workspace() {
        let dir = workspace_with_files(&[]);

        let ws = Workspace::scan(dir.path()).expect("scan should succeed");
        assert!(ws.files().is_empty(), "should have no files");
    }

    #[test]
    fn case_insensitive_md_extension() {
        let dir = workspace_with_files(&[("lower.md", "# Lower"), ("upper.MD", "# Upper")]);

        let ws = Workspace::scan(dir.path()).expect("scan should succeed");
        assert_eq!(ws.files().len(), 2, "should find both .md and .MD files");
    }

    #[test]
    fn update_content_replaces_file_data() {
        let dir = workspace_with_files(&[("doc.md", "[link](other.md)\n")]);
        let mut ws = Workspace::scan(dir.path()).expect("scan should succeed");

        let links = ws
            .file(Path::new("doc.md"))
            .expect("file should exist")
            .tree
            .links(Path::new("doc.md"));
        assert_eq!(links.len(), 1, "initial parse should find one link");

        ws.update_content(Path::new("doc.md"), "# No links here\n");
        let links = ws
            .file(Path::new("doc.md"))
            .expect("file should still exist")
            .tree
            .links(Path::new("doc.md"));
        assert!(links.is_empty(), "updated content should have no links");
    }

    #[test]
    fn update_content_adds_new_file() {
        let dir = workspace_with_files(&[("a.md", "# A\n")]);
        let mut ws = Workspace::scan(dir.path()).expect("scan should succeed");
        assert_eq!(ws.files().len(), 1, "should start with one file");

        ws.update_content(Path::new("b.md"), "# B\n");
        assert_eq!(ws.files().len(), 2, "should have two files after adding");
        assert!(
            ws.file(Path::new("b.md")).is_some(),
            "new file should be indexed"
        );
    }

    #[test]
    fn broken_config_surfaces_error() {
        let dir = workspace_with_files(&[
            (".lattice.toml", "not valid toml {{{}}}"),
            ("README.md", "# Root"),
        ]);

        let ws = Workspace::scan(dir.path()).expect("scan should still succeed");
        assert!(
            ws.config_error().is_some(),
            "broken config should be surfaced"
        );
        assert!(
            ws.file(Path::new("README.md")).is_some(),
            "files should still be parsed with defaults"
        );
    }

    #[test]
    fn has_config_true_when_lattice_toml_present() {
        let dir = workspace_with_files(&[(".lattice.toml", ""), ("README.md", "# Root")]);

        let ws = Workspace::scan(dir.path()).expect("scan should succeed");
        assert!(ws.has_config(), "should detect .lattice.toml");
    }

    #[test]
    fn has_config_false_when_no_lattice_toml() {
        let dir = workspace_with_files(&[("README.md", "# Root")]);

        let ws = Workspace::scan(dir.path()).expect("scan should succeed");
        assert!(!ws.has_config(), "should detect absence of .lattice.toml");
    }

    #[test]
    fn has_config_true_when_config_is_broken() {
        let dir = workspace_with_files(&[
            (".lattice.toml", "not valid toml {{{}}}"),
            ("README.md", "# Root"),
        ]);

        let ws = Workspace::scan(dir.path()).expect("scan should succeed");
        assert!(ws.has_config(), "broken config still means user opted in");
    }

    // -- Stage-2 structural cache (issue 013) --

    /// Stage-2 differential invariant: every file's cached `structural` vector
    /// must equal a from-scratch `structural::collect` for that file. A drift
    /// here is the "silent stale diagnostic" failure mode the cache risks.
    fn assert_cache_matches_recompute(ws: &Workspace) {
        for (path, file_data) in ws.files() {
            let file_exists = |target: &Path| ws.file(target).is_some();
            let fresh = structural::collect(&file_data.tree, path, ws.config(), &file_exists);
            assert_eq!(
                file_data.structural,
                fresh,
                "cached structural for {} drifted from a fresh collect",
                path.display()
            );
        }
    }

    /// Severity of the bare-path diagnostic on `path`, if any.
    fn bare_path_severity(ws: &Workspace, path: &Path) -> Option<crate::validation::Severity> {
        ws.file(path)?
            .structural
            .iter()
            .find(|d| d.message.contains("convert to a markdown link"))
            .map(|d| d.severity)
    }

    #[test]
    fn structural_cache_matches_recompute_across_mutations() {
        let dir = workspace_with_files(&[
            ("a.md", "See docs/page.md for details.\ntrailing \n"),
            ("docs/page.md", "# Page\n"),
        ]);
        let mut ws = Workspace::scan(dir.path()).expect("scan should succeed");
        assert_cache_matches_recompute(&ws);
        assert!(
            !ws.file(Path::new("a.md"))
                .expect("a.md indexed")
                .structural
                .is_empty(),
            "fixture should exercise real structural diagnostics"
        );

        // Content edit, membership unchanged.
        ws.update_content(
            Path::new("a.md"),
            "# Clean\n\nstill referencing docs/page.md.\n",
        );
        assert_cache_matches_recompute(&ws);

        // Add a file: membership grows.
        ws.update_content(Path::new("docs/extra.md"), "# Extra\n");
        assert_cache_matches_recompute(&ws);

        // Remove a file from disk: membership shrinks.
        fs::remove_file(dir.path().join("docs/page.md")).expect("delete page.md");
        ws.update(Path::new("docs/page.md"))
            .expect("update should succeed");
        assert_cache_matches_recompute(&ws);
    }

    #[test]
    fn bare_path_severity_flips_when_target_added() {
        // a.md references docs/page.md as a bare path. With the target absent
        // it is a hint; adding the target must flip a.md's cached diagnostic to
        // a warning even though a.md itself never changed — the membership
        // recompute path.
        let dir = workspace_with_files(&[("a.md", "See docs/page.md for details.\n")]);
        let mut ws = Workspace::scan(dir.path()).expect("scan should succeed");
        assert_eq!(
            bare_path_severity(&ws, Path::new("a.md")),
            Some(crate::validation::Severity::Hint),
            "absent target should be a hint"
        );

        ws.update_content(Path::new("docs/page.md"), "# Page\n");
        assert_eq!(
            bare_path_severity(&ws, Path::new("a.md")),
            Some(crate::validation::Severity::Warning),
            "adding the target should flip the source's bare-path to a warning"
        );
    }

    // -- Parsed-extraction cache (ticket perf 06) --

    /// Differential invariant: every file's cached `headings`/`links` vector
    /// must equal a fresh `Tree::headings()`/`Tree::links()` extraction. A drift
    /// here is the "silent stale extraction" failure mode the cache risks — the
    /// stage-2 spirit applied to fragment and forward-link inputs.
    fn assert_extraction_cache_matches_recompute(ws: &Workspace) {
        for (path, file_data) in ws.files() {
            assert_eq!(
                file_data.headings,
                file_data.tree.headings(),
                "cached headings for {} drifted from a fresh extraction",
                path.display()
            );
            assert_eq!(
                file_data.links,
                file_data.tree.links(path),
                "cached links for {} drifted from a fresh extraction",
                path.display()
            );
        }
    }

    #[test]
    fn extraction_cache_matches_recompute_across_mutations() {
        let dir = workspace_with_files(&[
            ("a.md", "# A\n\n[to b](b.md \"references\")\n\n## Section\n"),
            ("b.md", "# B\n"),
        ]);
        let mut ws = Workspace::scan(dir.path()).expect("scan should succeed");
        assert_extraction_cache_matches_recompute(&ws);
        assert!(
            !ws.file(Path::new("a.md"))
                .expect("a.md indexed")
                .links
                .is_empty(),
            "fixture should exercise real cached links"
        );
        assert!(
            !ws.file(Path::new("a.md"))
                .expect("a.md indexed")
                .headings
                .is_empty(),
            "fixture should exercise real cached headings"
        );

        // Content edit, membership unchanged.
        ws.update_content(
            Path::new("a.md"),
            "# A renamed\n\n[to b](b.md#b \"references\")\n",
        );
        assert_extraction_cache_matches_recompute(&ws);

        // Add a file: membership grows.
        ws.update_content(Path::new("c.md"), "# C\n\n[to a](a.md \"references\")\n");
        assert_extraction_cache_matches_recompute(&ws);

        // Remove a file from disk: membership shrinks.
        fs::remove_file(dir.path().join("b.md")).expect("delete b.md");
        ws.update(Path::new("b.md")).expect("update should succeed");
        assert_extraction_cache_matches_recompute(&ws);
    }

    // -- Line index cache (ticket perf 01) --

    /// Differential invariant: every file's cached `line_index` must equal a
    /// fresh index built from the same source the tree carries — the index is a
    /// pure function of the file's text, so any drift is a stale cache.
    fn assert_line_index_cache_matches_recompute(ws: &Workspace) {
        for (path, file_data) in ws.files() {
            assert_eq!(
                file_data.line_index,
                LineIndex::new(file_data.tree.source()),
                "cached line index for {} drifted from a fresh build",
                path.display()
            );
        }
    }

    #[test]
    fn line_index_rebuilt_only_on_reparse() {
        // Two files with deliberately different line shapes (CRLF vs LF, multi-
        // byte content) so an index swap would be observable.
        let dir = workspace_with_files(&[
            ("a.md", "# A\r\n\r\nfirst café line\r\n"),
            ("b.md", "# B\n\nsecond λ line\n"),
        ]);
        let mut ws = Workspace::scan(dir.path()).expect("scan should succeed");
        assert_line_index_cache_matches_recompute(&ws);

        let a_before = ws
            .file(Path::new("a.md"))
            .expect("a.md indexed")
            .line_index
            .clone();
        let b_before = ws
            .file(Path::new("b.md"))
            .expect("b.md indexed")
            .line_index
            .clone();

        // Edit only a.md, changing its line structure.
        ws.update_content(Path::new("a.md"), "# A renamed\n\nshorter\n");

        assert_ne!(
            ws.file(Path::new("a.md"))
                .expect("a.md still indexed")
                .line_index,
            a_before,
            "the edited file's index must be rebuilt from its new source"
        );
        assert_eq!(
            ws.file(Path::new("b.md"))
                .expect("b.md still indexed")
                .line_index,
            b_before,
            "an unrelated file's index must be untouched by another file's edit"
        );
        assert_line_index_cache_matches_recompute(&ws);
    }

    #[test]
    fn extraction_cache_rebuilt_only_on_reparse() {
        // target.md owns headings; three sources each reference a fragment in
        // it, so a from-scratch fragment pass would re-derive its headings three
        // times. The cache must serve all three from one parse-time extraction.
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            ("target.md", "# Alpha\n\n## Beta\n"),
            ("a.md", "[x](target.md#alpha \"references\")\n"),
            ("b.md", "[y](target.md#beta \"references\")\n"),
            ("c.md", "[z](target.md#alpha \"references\")\n"),
        ]);
        let mut ws = Workspace::scan(dir.path()).expect("scan should succeed");

        // A full forward-link/fragment validation reads the cache: it re-extracts
        // nothing, no matter how many fragment-links point at target.md.
        block::reset_extract_counts();
        let _ = crate::validation::validate_forward_links(&ws);
        assert_eq!(
            block::headings_extract_count(),
            0,
            "fragment validation must read cached headings, not re-extract once per fragment-link"
        );
        assert_eq!(
            block::links_extract_count(),
            0,
            "forward-link validation must read cached links, not re-extract once per file"
        );

        // Editing one file re-extracts exactly that file's headings/links once,
        // leaving every other file's cache untouched.
        let a_links_before = format!(
            "{:?}",
            ws.file(Path::new("a.md")).expect("a.md indexed").links
        );
        block::reset_extract_counts();
        ws.update_content(Path::new("target.md"), "# Alpha\n\n## Gamma\n");
        assert_eq!(
            block::headings_extract_count(),
            1,
            "one reparse re-extracts headings exactly once, not once per other file"
        );
        assert_eq!(
            block::links_extract_count(),
            1,
            "one reparse re-extracts links exactly once, not once per other file"
        );
        let a_links_after = format!(
            "{:?}",
            ws.file(Path::new("a.md")).expect("a.md indexed").links
        );
        assert_eq!(
            a_links_before, a_links_after,
            "editing target.md must not rebuild another file's link cache"
        );
    }
}
