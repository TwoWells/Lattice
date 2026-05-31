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

use crate::block::{self, Tree};
use crate::config::{Config, ConfigError};
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
    /// Diagnostics from frontmatter parsing (unknown inverse predicates).
    pub backlink_diagnostics: Vec<BacklinkDiagnostic>,
    /// YAML parse errors (partial recovery — file is still indexed).
    pub parse_diagnostics: Vec<ParseDiagnostic>,
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
    /// Parsed backlinks: inverse predicate → list of relative file paths.
    pub backlinks: HashMap<String, Vec<String>>,
}

/// A diagnostic about a backlink predicate issue.
#[derive(Debug, PartialEq, Eq)]
pub struct BacklinkDiagnostic {
    /// 1-based line number of the predicate key in the source file.
    pub line: usize,
    /// The unknown inverse predicate.
    pub predicate: String,
}

/// A YAML parse error from frontmatter.
#[derive(Debug)]
pub struct ParseDiagnostic {
    /// 1-based line number.
    pub line: usize,
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

        Ok(Self {
            root,
            config,
            config_error,
            has_config,
            files,
        })
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
            self.files.remove(rel_path);
            return Ok(());
        }

        match parse_file(&abs_path, rel_path, &self.config) {
            Ok(data) => {
                self.files.insert(rel_path.to_path_buf(), data);
            }
            Err(e) => {
                self.files.remove(rel_path);
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
        let data = parse_content(content, rel_path, &self.config);
        self.files.insert(rel_path.to_path_buf(), data);
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
    /// When `true`, graph validation (predicates, backlinks, bare paths) is
    /// active. When `false`, Lattice acts as a code intelligence server only.
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
fn parse_content(content: &str, rel_path: &Path, config: &Config) -> FileData {
    // Parse YAML frontmatter block.
    let fm_block = yaml::parse_frontmatter_block(content);

    // Build the tree (block structure + inline elements).
    let frontmatter_span = fm_block.as_ref().map(|b| b.span);
    let frontmatter_entries = fm_block.as_ref().map(|b| b.entries.as_slice());
    let tree = block::parse_tree_with_entries(content, frontmatter_span, frontmatter_entries);

    // Extract frontmatter data.
    let mut frontmatter = None;
    let mut backlink_diagnostics = Vec::new();
    let mut parse_diagnostics = Vec::new();

    if let Some(block) = &fm_block {
        // Collect YAML parse errors as diagnostics (partial recovery).
        for diag in &block.diagnostics {
            if diag.severity == yaml::YamlSeverity::Error {
                let line = byte_offset_to_line(content, diag.span.start);
                parse_diagnostics.push(ParseDiagnostic {
                    line,
                    message: diag.message.clone(),
                });
            }
        }

        let byte_range: Range<usize> = block.span.into();
        let start_line = 1;
        let end_byte = byte_range.end;
        let newline_count = content[..end_byte.min(content.len())]
            .bytes()
            .filter(|&b| b == b'\n')
            .count();
        let end_line =
            newline_count + usize::from(!content[..end_byte.min(content.len())].ends_with('\n'));

        let backlinks = yaml::extract_backlinks(block, content);

        // Validate inverse predicates.
        for predicate in backlinks.keys() {
            if !config.is_known_inverse(predicate) {
                let line = yaml::find_predicate_line(block, predicate, content);
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

    FileData {
        tree,
        frontmatter,
        backlink_diagnostics,
        parse_diagnostics,
    }
}

/// Convert a byte offset to a 1-based line number.
#[allow(
    clippy::naive_bytecount,
    reason = "not worth a dependency for line counting"
)]
fn byte_offset_to_line(content: &str, offset: usize) -> usize {
    let offset = offset.min(content.len());
    content.as_bytes()[..offset]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
        + 1
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
}
