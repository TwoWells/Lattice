// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Workspace scanning and file indexing.
//!
//! Discovers all markdown files under the workspace root, parses them into
//! an in-memory index of links, headings, and backlinks, and supports
//! incremental updates when individual files change.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::config::{Config, ConfigError};
use crate::frontmatter::{self, BacklinkDiagnostic, Frontmatter, FrontmatterError};
use crate::markdown::{self, BarePath, Heading, Link, ParsedDocument};

/// Errors that can occur during workspace operations.
#[derive(Debug, Error)]
pub enum WorkspaceError {
    /// Failed to read a markdown file.
    #[error("failed to read {path}: {source}")]
    #[allow(
        dead_code,
        reason = "constructed in Workspace::update, used by LSP server"
    )]
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
    /// Links extracted from the document body.
    pub links: Vec<Link>,
    /// Headings extracted from the document body.
    pub headings: Vec<Heading>,
    /// Bare file paths found in prose text.
    pub bare_paths: Vec<BarePath>,
    /// Parsed frontmatter, if present.
    pub frontmatter: Option<Frontmatter>,
    /// Diagnostics from frontmatter parsing (unknown inverse predicates).
    pub backlink_diagnostics: Vec<BacklinkDiagnostic>,
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
    /// Parsed file data, keyed by workspace-relative path.
    files: BTreeMap<PathBuf, FileData>,
    /// Files that had parse errors (frontmatter), keyed by relative path.
    errors: BTreeMap<PathBuf, FrontmatterError>,
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

        let (config, config_error) = match Config::load(&root) {
            Ok(c) => (c, None),
            Err(e) => {
                tracing::warn!(root = %root.display(), "config error, using defaults: {e}");
                (Config::default(), Some(e))
            }
        };

        let md_paths = discover_markdown_files(&root);

        let mut files = BTreeMap::new();
        let mut errors = BTreeMap::new();

        for abs_path in md_paths {
            let rel_path = abs_path
                .strip_prefix(&root)
                .unwrap_or(&abs_path)
                .to_path_buf();

            match parse_file(&abs_path, &rel_path, &config) {
                Ok(data) => {
                    files.insert(rel_path, data);
                }
                Err(ParseFileError::Read(e)) => {
                    tracing::warn!(path = %rel_path.display(), "failed to read file: {e}");
                }
                Err(ParseFileError::Frontmatter(e)) => {
                    errors.insert(rel_path, e);
                }
            }
        }

        Ok(Self {
            root,
            config,
            config_error,
            files,
            errors,
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
    #[allow(dead_code, reason = "used by LSP server for incremental re-indexing")]
    pub fn update(&mut self, rel_path: &Path) -> Result<(), WorkspaceError> {
        let abs_path = self.root.join(rel_path);

        if !abs_path.is_file() {
            self.files.remove(rel_path);
            self.errors.remove(rel_path);
            return Ok(());
        }

        self.errors.remove(rel_path);

        match parse_file(&abs_path, rel_path, &self.config) {
            Ok(data) => {
                self.files.insert(rel_path.to_path_buf(), data);
            }
            Err(ParseFileError::Read(e)) => {
                self.files.remove(rel_path);
                return Err(WorkspaceError::Read {
                    path: rel_path.to_path_buf(),
                    source: e,
                });
            }
            Err(ParseFileError::Frontmatter(e)) => {
                self.files.remove(rel_path);
                self.errors.insert(rel_path.to_path_buf(), e);
            }
        }

        Ok(())
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

    /// Parsed file data for all successfully parsed files.
    pub fn files(&self) -> &BTreeMap<PathBuf, FileData> {
        &self.files
    }

    /// Get parsed data for a specific file by its workspace-relative path.
    pub fn file(&self, rel_path: &Path) -> Option<&FileData> {
        self.files.get(rel_path)
    }

    /// Frontmatter parse errors, keyed by workspace-relative path.
    pub fn errors(&self) -> &BTreeMap<PathBuf, FrontmatterError> {
        &self.errors
    }
}

// --- Internal helpers ---

/// Errors from parsing a single file.
enum ParseFileError {
    Read(std::io::Error),
    Frontmatter(FrontmatterError),
}

/// Parse a single markdown file into [`FileData`].
fn parse_file(
    abs_path: &Path,
    rel_path: &Path,
    config: &Config,
) -> Result<FileData, ParseFileError> {
    let content = std::fs::read_to_string(abs_path).map_err(ParseFileError::Read)?;

    let ParsedDocument {
        links,
        headings,
        bare_paths,
    } = markdown::parse_document(&content, rel_path);
    let fm_result =
        frontmatter::parse_frontmatter(&content, config).map_err(ParseFileError::Frontmatter)?;

    Ok(FileData {
        links,
        headings,
        bare_paths,
        frontmatter: fm_result.frontmatter,
        backlink_diagnostics: fm_result.diagnostics,
    })
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
        assert_eq!(data.headings.len(), 1, "should have one heading");
        assert_eq!(data.links.len(), 1, "should have one link");
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
    fn frontmatter_error_collected() {
        let dir = workspace_with_files(&[("bad.md", "---\n: broken: yaml: [[\n---\n# Bad\n")]);

        let ws = Workspace::scan(dir.path()).expect("scan should succeed");
        assert!(
            ws.file(Path::new("bad.md")).is_none(),
            "bad file should not be in files"
        );
        assert!(
            ws.errors().contains_key(Path::new("bad.md")),
            "bad file should be in errors"
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
        let data = ws.file(Path::new("doc.md")).expect("should find doc.md");
        assert_eq!(data.headings.len(), 1, "should have one heading");
        assert_eq!(
            data.headings[0].text, "Original",
            "heading should be Original"
        );

        fs::write(dir.path().join("doc.md"), "# Updated\n\n## Section\n")
            .expect("overwrite doc.md");
        ws.update(Path::new("doc.md"))
            .expect("update should succeed");

        let data = ws.file(Path::new("doc.md")).expect("should find doc.md");
        assert_eq!(
            data.headings.len(),
            2,
            "should have two headings after update"
        );
        assert_eq!(
            data.headings[0].text, "Updated",
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
        assert!(
            ws.errors().contains_key(Path::new("doc.md")),
            "should have error initially"
        );

        fs::write(dir.path().join("doc.md"), "# Fixed\n").expect("fix doc.md");
        ws.update(Path::new("doc.md"))
            .expect("update should succeed");

        assert!(
            !ws.errors().contains_key(Path::new("doc.md")),
            "error should be cleared"
        );
        assert!(
            ws.file(Path::new("doc.md")).is_some(),
            "file should be parsed"
        );
    }

    #[test]
    fn empty_workspace() {
        let dir = workspace_with_files(&[]);

        let ws = Workspace::scan(dir.path()).expect("scan should succeed");
        assert!(ws.files().is_empty(), "should have no files");
        assert!(ws.errors().is_empty(), "should have no errors");
    }

    #[test]
    fn case_insensitive_md_extension() {
        let dir = workspace_with_files(&[("lower.md", "# Lower"), ("upper.MD", "# Upper")]);

        let ws = Workspace::scan(dir.path()).expect("scan should succeed");
        assert_eq!(ws.files().len(), 2, "should find both .md and .MD files");
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
}
