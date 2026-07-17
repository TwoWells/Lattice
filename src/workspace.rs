// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Workspace scanning and file indexing.
//!
//! Discovers all markdown files under a scope root — stopping at every
//! strictly-deeper scope boundary (decision 019) — and parses them into an
//! in-memory index backed by the unified parse tree. The owning [`Workspace`]
//! is the CLI's single-scan engine; the LSP server drives incremental sync
//! through its own flat document store (ticket server 10) and consumes the
//! shared pipeline through [`WorkspaceLike`] views.

use std::borrow::Cow;
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
    /// Failed to read a markdown file. Only produced by the test-only
    /// incremental [`Workspace::update`]; the LSP server drives incremental
    /// sync through its flat document store (ticket server 10).
    #[cfg(test)]
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

    /// `.lattice.toml` is present but unreadable — a **failed commitment**,
    /// not an absent config (decision 023, issue 065). Refused at the loading
    /// layer so no one-shot subcommand evaluates under a fabricated default
    /// config: defaults are the semantics of a repo that declared nothing.
    /// The CLI maps this to exit 2 ("could not evaluate").
    #[error(transparent)]
    Config {
        /// The load failure, naming the config path.
        source: ConfigError,
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
    /// Cached suppression ledger entry for this file (issue 036, decision 012):
    /// what each suppression source (literal frontmatter exceptions, count-keys)
    /// actually suppressed, by severity. Refreshed alongside `structural` from
    /// the same `structural::collect_with_suppressions` pass; the CLI lint loop
    /// aggregates it into the workspace ledger. The LSP never reads it.
    pub suppressions: structural::FileSuppressions,
    /// Cached extracted headings (`Tree::headings()` output, with precomputed
    /// github/gitlab/vscode slugs) for this file.
    ///
    /// Unlike `structural`, which also reads workspace membership, this is a
    /// pure function of this file's own tree — so it is built directly in the
    /// parse path and refreshed exactly when the file reparses. Fragment
    /// validation reads it instead of re-deriving a linked document's headings
    /// once per `file.md#heading` reference (issue 013 — ticket perf 06).
    pub headings: Vec<block::Heading>,
    /// Cached extracted links (`Tree::links()` output) for this file, resolved
    /// against its own **absolute** path (decision 019 clause 8): a
    /// document-relative target is absolute, a root-relative (`/x`) one the
    /// relative remainder. Both are root-free, so this cache survives a
    /// `primary_root` change unchanged — placement becomes a metadata flip, not
    /// a reparse (ticket server 11).
    ///
    /// Like `headings`, a pure function of this file's tree and path, rebuilt
    /// only on reparse. The forward-link, backlink, connectivity, and
    /// reciprocal-link validators read it — mapping each target onto a stored
    /// key via [`WorkspaceLike`] — instead of re-walking and re-classifying
    /// every link on each sync (ticket perf 06).
    pub links: Vec<block::Link>,
    /// Cached explicit in-page anchor targets (`Tree::anchors()` output) —
    /// `id`/`name` values harvested from this file's raw-HTML `<a>` tags.
    ///
    /// Like `headings`/`links`, a pure function of this file's own tree, rebuilt
    /// only on reparse. Same-document fragment validation resolves `[…](#x)`
    /// against this set in addition to heading slugs, so an explicit
    /// `<a id="x"></a>` / `<a name="x">` anchor is a valid `#x` target (issue
    /// 025).
    pub anchors: Vec<block::Anchor>,
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
///
/// Populated from a leading `---` / `+++` / `{` block or, when none is present,
/// from a fenced `yaml lattice` metadata carrier (decision 015). The two carry
/// identical backlink/exception data; the spans differ only in what they cover
/// (delimiter-bounded block vs. in-fence body).
#[derive(Debug)]
pub struct Frontmatter {
    /// Byte range of the metadata carrier — the entire frontmatter block
    /// (including `---` delimiters) or the in-fence body of a `yaml lattice`
    /// carrier.
    pub byte_range: Range<usize>,
    /// 1-based line of the carrier's start (the opening `---`, or the carrier's
    /// first body line).
    pub start_line: usize,
    /// 1-based line of the carrier's last line (the closing `---`, or the
    /// carrier body's last line).
    pub end_line: usize,
    /// Parsed backlinks: backlink label → list of relative file paths. The
    /// label is any known predicate — an inverse value or a forward label
    /// (decision 008).
    pub backlinks: HashMap<String, Vec<String>>,
    /// Parsed `exceptions` block (issue 031, decision 011): per-reference,
    /// reconciled suppressions over the path-shaped lints. Consumed by the
    /// structural pass, which suppresses a matching live diagnostic and flags an
    /// exception that matches none as unused.
    pub exceptions: fm::Exceptions,
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
    /// Absolute paths of strictly-deeper scope boundaries pruned from this
    /// scope's scan (decision 019 clause 1): each is a nested `.lattice.toml`
    /// (another scope) or a nested `.git` (a non-root environment). A link or
    /// path-shaped mention resolving into any of these — or escaping above
    /// `root` — crosses a scope boundary and steers to an `[external]` alias
    /// ([`WorkspaceLike::crosses_boundary`], decision 019 clause 3).
    boundaries: Vec<PathBuf>,
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
    /// resolved to a directory, and [`WorkspaceError::Config`] when a
    /// `.lattice.toml` is present but unreadable — the refusal at the loading
    /// layer (decision 023, issue 065): defaults are the semantics of an
    /// *absent* config, never a substitute for a broken one, so no one-shot
    /// surface may evaluate under them. Individual file read errors are
    /// collected but do not abort the scan.
    pub fn scan(start: &Path) -> Result<Self, WorkspaceError> {
        let mut workspace = Self::scan_recording_config_error(start)?;
        if let Some(source) = workspace.config_error.take() {
            return Err(WorkspaceError::Config { source });
        }
        Ok(workspace)
    }

    /// Scan like [`Workspace::scan`], but **hold** a broken `.lattice.toml`
    /// instead of refusing: the index is built under default config with the
    /// load error recorded in `config_error`, for the one caller that can
    /// express "a failed commitment changes nothing" in a running process —
    /// the LSP server, which serves config-independent features and publishes
    /// the error on the config's URI rather than gating (decision 023
    /// addendum). Every one-shot surface goes through [`Workspace::scan`] and
    /// refuses.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceError::NoRoot`] if the starting path cannot be
    /// resolved to a directory.
    pub fn scan_recording_config_error(start: &Path) -> Result<Self, WorkspaceError> {
        // Absolutize `start` before root discovery. A bare single-component
        // relative path (`archive`) has `Path::parent() == Some("")` — an empty
        // path — so the walk-up loop in `find_workspace_root` would step to `""`,
        // match `.lattice.toml`/`.git` relative to the process CWD, and return an
        // empty root that `discover_markdown_files` walks to zero files (a silent
        // false-clean — issue 024). Canonicalizing here makes every spelling
        // (`archive`, `archive/`, `tickets/misc`, `./archive/`, the absolute
        // form) resolve to the same absolute root, and matches the canonicalized
        // form `lint::scope_relative_to_root` strips the scope against, so
        // discovery and scoping stay consistent. The scan path must exist on disk
        // for the lint to be meaningful, so canonicalize is safe; on failure we
        // fall back to `start` unchanged so behavior never regresses below the
        // pre-fix state.
        let start = std::fs::canonicalize(start).unwrap_or_else(|_| start.to_path_buf());
        let start = start.as_path();
        let root = find_workspace_root(start).ok_or_else(|| WorkspaceError::NoRoot {
            start: start.to_path_buf(),
        })?;

        let has_config = root.join(".lattice.toml").is_file();

        let (config, config_error) = match Config::load(&root) {
            Ok(c) => (c, None),
            Err(e) => {
                tracing::warn!(root = %root.display(), "config error recorded, index built under defaults: {e}");
                (Config::default(), Some(e))
            }
        };

        // Scan the scope, pruning at every strictly-deeper boundary (a nested
        // `.lattice.toml` or `.git`): those subtrees belong to their own graph
        // (decision 019 clause 1). The pruned boundary directories are captured
        // so cross-boundary references can steer to an alias rather than dangle.
        let (md_paths, boundaries) = discover_markdown_files_and_boundaries(&root);

        let mut files = BTreeMap::new();

        for abs_path in md_paths {
            let rel_path = abs_path
                .strip_prefix(&root)
                .unwrap_or(&abs_path)
                .to_path_buf();

            match parse_file(&abs_path, &config) {
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
            boundaries,
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
    /// Retained as the owning-workspace incremental engine that the shared
    /// parse/structural helpers are regression-tested through; the LSP server
    /// now drives incremental sync through its flat document store
    /// (ticket server 10), so this is exercised by tests only.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceError::Read`] if the file exists but cannot be read.
    #[cfg(test)]
    pub fn update(&mut self, rel_path: &Path) -> Result<(), WorkspaceError> {
        let abs_path = self.root.join(rel_path);

        if !abs_path.is_file() {
            if self.files.remove(rel_path).is_some() {
                self.recompute_all_structural();
            }
            return Ok(());
        }

        let membership_changed = !self.files.contains_key(rel_path);
        match parse_file(&abs_path, &self.config) {
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
    /// parsed directly without reading from disk. Test-only (see
    /// [`Workspace::update`]).
    #[cfg(test)]
    pub fn update_content(&mut self, rel_path: &Path, content: &str) {
        let membership_changed = !self.files.contains_key(rel_path);
        let data = parse_content(content, &self.root.join(rel_path), &self.config);
        self.files.insert(rel_path.to_path_buf(), data);
        self.refresh_structural_after_update(rel_path, membership_changed);
    }

    /// Reload `.lattice.toml` and rebuild the index against the new config.
    ///
    /// Re-runs [`Config::load`] from the workspace root and re-parses every
    /// indexed file with the fresh config, then recomputes the structural
    /// caches. Config feeds both `parse_content` (backlink-predicate
    /// validation) and the structural collectors (artifacts, overrides,
    /// external aliases), so a marker change invalidates the *whole* workspace,
    /// not a single file (decision 017). This is effectively a config-only
    /// re-scan: membership is preserved and each file is reparsed from its
    /// in-memory source (the editor buffer, never re-read from disk), so an
    /// unsaved buffer is not clobbered by a marker edit.
    ///
    /// Mirrors the hot-reload the LSP server now performs per root over its
    /// flat document store (issue 044, ticket server 08 / 10); retained as a
    /// regression test of the shared config-reload semantics, so test-only. A
    /// parse/read error is a failed commitment (decision 023, issue 065): the
    /// previous valid config is held with the error recorded — never a swap
    /// to fabricated defaults.
    #[cfg(test)]
    pub fn reload_config(&mut self) {
        self.has_config = self.root.join(".lattice.toml").is_file();
        match Config::load(&self.root) {
            Ok(config) => {
                self.config = config;
                self.config_error = None;
            }
            Err(e) => {
                tracing::warn!(root = %self.root.display(), "config reload error, holding last-good: {e}");
                self.config_error = Some(e);
                return;
            }
        }

        // Re-parse every file from its cached source so config-dependent parse
        // output (backlink-predicate diagnostics) is refreshed. The source is
        // the in-memory buffer, so unsaved editor edits survive the reload.
        let rel_paths: Vec<PathBuf> = self.files.keys().cloned().collect();
        for rel_path in &rel_paths {
            let Some(file_data) = self.files.get(rel_path) else {
                continue;
            };
            let content = file_data.tree.source().to_string();
            let data = parse_content(&content, &self.root.join(rel_path), &self.config);
            self.files.insert(rel_path.clone(), data);
        }

        // Config changed for every file, so the structural caches must be
        // recomputed workspace-wide (membership is unchanged).
        self.recompute_all_structural();
    }

    /// Refresh the structural cache after `rel_path` was (re)parsed.
    ///
    /// An edit that does not change membership only invalidates the edited
    /// file's cache. A membership change (a file added or removed) can flip a
    /// bare-path existence answer in *any* file, so it forces a full recompute.
    /// Only reached from the test-only incremental methods.
    #[cfg(test)]
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
        let (diagnostics, suppressions) =
            compute_structural(file_data, rel_path, &self.config, &file_exists);
        if let Some(file_data) = self.files.get_mut(rel_path) {
            file_data.structural = diagnostics;
            file_data.suppressions = suppressions;
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

    /// Get parsed data for a specific file by a link target or a stored key —
    /// see [`target_to_key`]. A workspace-relative key passes through unchanged.
    pub fn file(&self, target: &Path) -> Option<&FileData> {
        self.files.get(&*target_to_key(&self.root, target))
    }

    /// Consume this workspace into its scanned parts.
    ///
    /// Lets the LSP server's flat document store (ticket server 10) take
    /// ownership of the parsed files a folder scan produced without re-parsing
    /// them: the store reuses [`Workspace::scan`]'s canonicalization, root
    /// discovery, gitignore-aware walk, and parse loop, then folds each file
    /// into its single-owner `Document` keyed by absolute path.
    #[must_use]
    pub fn into_parts(self) -> WorkspaceParts {
        WorkspaceParts {
            root: self.root,
            config: self.config,
            config_error: self.config_error,
            has_config: self.has_config,
            files: self.files,
        }
    }
}

/// The owned parts of a scanned [`Workspace`], produced by
/// [`Workspace::into_parts`]. The `root` is the canonical scan root; `files` is
/// keyed by path relative to that root.
#[derive(Debug)]
pub struct WorkspaceParts {
    /// Canonical scan root (the directory `.lattice.toml`/`.git` was found in).
    pub root: PathBuf,
    /// Configuration loaded from the root.
    pub config: Config,
    /// Error from loading `.lattice.toml`, if any.
    pub config_error: Option<ConfigError>,
    /// Whether a `.lattice.toml` was found at the root.
    pub has_config: bool,
    /// Parsed file data keyed by root-relative path.
    pub files: BTreeMap<PathBuf, FileData>,
}

/// Map a link `target` onto the view-relative key its file is stored under.
///
/// A document-relative link resolves to an absolute path at parse time
/// (root-free, decision 019 clause 8); stripping `root` yields its key — and in
/// a nested view an ancestor and a descendant root strip to *their own*
/// key, so both agree on the same file (the ticket-10 divergence fix). A
/// root-relative (`/x`) target is stored as the relative remainder `x`, already
/// the key. A caller passing a stored key back in (already relative) is returned
/// unchanged. An absolute target outside `root` fails the strip and is returned
/// as-is, so the lookup simply misses (it is not a member of this view).
#[must_use]
pub fn target_to_key<'a>(root: &Path, target: &'a Path) -> Cow<'a, Path> {
    if target.is_absolute() {
        target.strip_prefix(root).map_or_else(
            |_| Cow::Borrowed(target),
            |key| Cow::Owned(key.to_path_buf()),
        )
    } else {
        Cow::Borrowed(target)
    }
}

/// The read-only surface the graph and structural diagnostic pipeline consumes
/// from a workspace: its config, membership, and per-file parsed data.
///
/// Implemented by both the owning [`Workspace`] (the CLI's single-root index)
/// and the borrowed [`WorkspaceView`] the LSP server derives per root from its
/// flat document store by range scan (ticket server 10). Making the validators
/// generic over this trait lets the two storage models share one pipeline
/// without either one owning the file map twice.
pub trait WorkspaceLike {
    /// The workspace root the file map's keys are relative to.
    fn root(&self) -> &Path;
    /// The effective configuration.
    fn config(&self) -> &Config;
    /// Whether the graph diagnostic tier is enabled (a `.lattice.toml` exists).
    fn has_config(&self) -> bool;
    /// Parsed data for one file by a link `target` (absolute for a
    /// document-relative link, the root-relative remainder otherwise) or by a
    /// stored key directly — [`target_to_key`] reconciles them.
    fn file(&self, target: &Path) -> Option<&FileData>;
    /// Iterate every file as `(view-relative key, parsed data)`.
    fn files_iter(&self) -> impl Iterator<Item = (&PathBuf, &FileData)>;
    /// Resolve a link `target` to the stored key it matches (existence probe
    /// that also yields a borrow living as long as the workspace), or `None`.
    /// The argument follows the same absolute/relative convention as
    /// [`WorkspaceLike::file`].
    fn resolve_key(&self, target: &Path) -> Option<&Path>;

    /// The absolute paths of the strictly-deeper scope boundaries inside this
    /// scope (nested `.lattice.toml` / `.git` directories — decision 019). Used
    /// by [`crosses_boundary`](WorkspaceLike::crosses_boundary).
    fn boundaries(&self) -> &[PathBuf];

    /// Whether a link `target` resolves *across* a scope boundary — into a
    /// strictly-deeper nested scope, or above this scope's root (decision 019
    /// clause 3). Such a target encodes the host layout in every referring
    /// document and fails the move rule wholesale: it is a defect that must be
    /// written as an `[external]` alias, not a plain relative path.
    ///
    /// `target` is the root-free link target: absolute for a document-relative
    /// link (decision 019 clause 8), the root-relative remainder otherwise. It
    /// is absolutized against the root, then tested against the two boundary
    /// directions — a target not under the root has climbed out of the scope; a
    /// target under one of the pruned nested boundaries has crossed into a
    /// deeper one. A plain (non-crossing) miss is an ordinary broken link, not a
    /// boundary crossing, so this returns `false` for it.
    fn crosses_boundary(&self, target: &Path) -> bool {
        let target_abs = if target.is_absolute() {
            target.to_path_buf()
        } else {
            self.root().join(target)
        };
        !target_abs.starts_with(self.root())
            || self
                .boundaries()
                .iter()
                .any(|boundary| target_abs.starts_with(boundary))
    }
}

impl WorkspaceLike for Workspace {
    fn root(&self) -> &Path {
        &self.root
    }
    fn config(&self) -> &Config {
        &self.config
    }
    fn has_config(&self) -> bool {
        self.has_config
    }
    fn file(&self, target: &Path) -> Option<&FileData> {
        self.files.get(&*target_to_key(&self.root, target))
    }
    fn files_iter(&self) -> impl Iterator<Item = (&PathBuf, &FileData)> {
        self.files.iter()
    }
    fn resolve_key(&self, target: &Path) -> Option<&Path> {
        self.files
            .get_key_value(&*target_to_key(&self.root, target))
            .map(|(k, _)| k.as_path())
    }
    fn boundaries(&self) -> &[PathBuf] {
        &self.boundaries
    }
}

/// A borrowed, per-root view over a set of parsed documents.
///
/// The LSP server's flat document store builds one of these per root by range
/// scan over its membership (ticket server 10), presenting the same read API
/// the owning [`Workspace`] does so the diagnostic tier, navigation,
/// completion, and structural existence oracle port mechanically. It borrows
/// each file's [`FileData`]; the map is keyed by the path each file was parsed
/// relative to — the view's root for a rooted view, or the file name for a
/// single-file (rootless) view.
#[derive(Debug)]
pub struct WorkspaceView<'a> {
    root: PathBuf,
    config: &'a Config,
    has_config: bool,
    files: BTreeMap<PathBuf, &'a FileData>,
    boundaries: Vec<PathBuf>,
}

impl<'a> WorkspaceView<'a> {
    /// Construct a view from its parts. `files` maps each document's
    /// view-relative path to its parsed data; `boundaries` names the
    /// strictly-deeper scope roots inside this one (the server derives them from
    /// its registered roots — decision 019).
    #[must_use]
    pub fn new(
        root: PathBuf,
        config: &'a Config,
        has_config: bool,
        files: BTreeMap<PathBuf, &'a FileData>,
        boundaries: Vec<PathBuf>,
    ) -> Self {
        Self {
            root,
            config,
            has_config,
            files,
            boundaries,
        }
    }

    /// The view's root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
    /// The effective configuration.
    #[must_use]
    pub fn config(&self) -> &Config {
        self.config
    }
    /// Whether the graph diagnostic tier is enabled for this root.
    #[must_use]
    pub fn has_config(&self) -> bool {
        self.has_config
    }
    /// The borrowed file map, keyed by view-relative path.
    #[must_use]
    pub fn files(&self) -> &BTreeMap<PathBuf, &'a FileData> {
        &self.files
    }
    /// Parsed data for one file by a link target or a view-relative key — see
    /// [`target_to_key`].
    #[must_use]
    pub fn file(&self, target: &Path) -> Option<&FileData> {
        self.files.get(&*target_to_key(&self.root, target)).copied()
    }
    /// Resolve a link target to the view-relative key it matches, or `None`.
    /// The borrow lives as long as the view (the stored key, not the argument).
    #[must_use]
    pub fn resolve_key(&self, target: &Path) -> Option<&Path> {
        self.files
            .get_key_value(&*target_to_key(&self.root, target))
            .map(|(k, _)| k.as_path())
    }
}

impl WorkspaceLike for WorkspaceView<'_> {
    fn root(&self) -> &Path {
        &self.root
    }
    fn config(&self) -> &Config {
        self.config
    }
    fn has_config(&self) -> bool {
        self.has_config
    }
    fn file(&self, target: &Path) -> Option<&FileData> {
        self.files.get(&*target_to_key(&self.root, target)).copied()
    }
    fn files_iter(&self) -> impl Iterator<Item = (&PathBuf, &FileData)> {
        self.files.iter().map(|(path, data)| (path, *data))
    }
    fn resolve_key(&self, target: &Path) -> Option<&Path> {
        self.files
            .get_key_value(&*target_to_key(&self.root, target))
            .map(|(k, _)| k.as_path())
    }
    fn boundaries(&self) -> &[PathBuf] {
        &self.boundaries
    }
}

/// Compute the structural diagnostics and suppression ledger for one parsed
/// file against a membership oracle.
///
/// Factored out of [`Workspace::recompute_structural`] so the LSP server's flat
/// document store (ticket server 10) computes structural caches identically:
/// the same external-existence stat oracle, the same per-file `exceptions`
/// wiring, and the same `[[override]]` effective-policy resolution.
/// `file_exists` answers workspace membership for the bare-path existence
/// check; the caller supplies it from whatever membership representation it
/// owns (a rel-path map for [`Workspace`], a range scan for the server).
#[must_use]
pub fn compute_structural(
    file_data: &FileData,
    rel_path: &Path,
    config: &Config,
    file_exists: &dyn Fn(&Path) -> bool,
) -> (Vec<Diagnostic>, structural::FileSuppressions) {
    // External-namespace (`{Name}/…`) references resolve existence-only against
    // the configured alias directories, which live *outside* the workspace
    // index — so this oracle `stat`s the real filesystem rather than consulting
    // workspace membership (issue 030, decision 010). It only ever `stat`s; the
    // aliased repository is never read or indexed. The verdict is tri-state: a
    // *failed* stat answers `Unknown`, not "absent", so an I/O flake surfaces
    // instead of silently exempting the reference (issue 050).
    let external_exists = |path: &Path| structural::ExternalExistence::stat(path);
    // The `exceptions` frontmatter block (issue 031) is per-file and lives in
    // this file's own frontmatter; an empty default applies when there is no
    // frontmatter. The structural pass suppresses a matching live diagnostic
    // and reconciles the rest as unused.
    let empty_exceptions = fm::Exceptions::default();
    let exceptions = file_data
        .frontmatter
        .as_ref()
        .map_or(&empty_exceptions, |fm| &fm.exceptions);
    // Resolve this file's effective 028-family policy by applying any matching
    // `[[override]]` level entries (issue 037, decision 012). Only an override
    // that sets `stale_references` / `bare_paths` as a *level* changes the
    // per-file collect (a `disabled` freeze, or a raise such as `warn` →
    // `deny`); an `{ expect = N }` aggregate leaves the per-file level alone and
    // is reconciled later by the lint loop's expect pass. The clone is taken
    // only when an override actually moves this file's policy — the common
    // no-override file reuses the base config directly.
    let effective_policy = config.effective_policy(rel_path);
    let effective_config;
    let config: &Config = if effective_policy == config.policy {
        config
    } else {
        effective_config = Config {
            policy: effective_policy,
            ..config.clone()
        };
        &effective_config
    };
    structural::collect_with_suppressions(
        &file_data.tree,
        rel_path,
        config,
        file_exists,
        &external_exists,
        exceptions,
    )
}

// --- Internal helpers ---

/// Parse a single markdown file from disk into [`FileData`].
fn parse_file(abs_path: &Path, config: &Config) -> Result<FileData, std::io::Error> {
    let content = std::fs::read_to_string(abs_path)?;
    Ok(parse_content(&content, abs_path, config))
}

/// Parse markdown content into [`FileData`].
///
/// Always succeeds — YAML parse errors become diagnostics instead of
/// hard failures, enabling partial frontmatter recovery.
///
/// `abs_path` is the document's absolute path in production; it is threaded into
/// link classification only ([`Tree::links`]), so cached link targets are
/// root-free (decision 019 clause 8). The parse output carries no workspace
/// root: structural diagnostics (which anchor on the root-relative path) are
/// filled separately after insertion, once membership is known.
#[must_use]
pub fn parse_content(content: &str, abs_path: &Path, config: &Config) -> FileData {
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

    // A leading `---` / `+++` / `{` block is the primary carrier; when absent,
    // a fenced `yaml lattice` metadata carrier (decision 015) populates the same
    // `frontmatter` fields. The block parsers detect frontmatter by leading
    // delimiter, so the two never both match `fm_block`; the carrier is consulted
    // only when no leading block was found. Its body is parsed via the tree-based
    // recognition in `metadata`, so a `yaml lattice` fence nested in an outer
    // documentation fence stays inert.
    let carrier_block = if fm_block.is_some() {
        None
    } else {
        crate::metadata::parse_carrier_block(&tree)
    };
    let effective_block = fm_block.as_ref().or(carrier_block.as_ref());

    if let Some(block) = effective_block {
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
        let start_line = byte_offset_to_line(content, byte_range.start);
        let end_byte = byte_range.end.min(content.len());
        // Step back one byte off the span end (which includes the closing
        // delimiter's line ending) so we land on the delimiter line itself
        // rather than the line after it. Recognizes all line-ending styles.
        let end_line = byte_offset_to_line(content, end_byte.saturating_sub(1));

        let backlinks = fm::extract_backlinks(block, content);
        let exceptions = fm::extract_exceptions(block, content);

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
            exceptions,
        });
    }

    // Collect parse diagnostics from the tree itself.
    for diag in tree.diagnostics() {
        let _ = &abs_path; // reserved for future per-file filtering
        let _ = diag;
    }

    // Extract this file's headings (with precomputed slugs) and links once, here
    // in the parse path, so the graph validators read a cached vector instead of
    // re-deriving it — `headings()` per fragment-link, `links()` per file per
    // sync (ticket perf 06). Both are pure functions of this file's own tree and
    // path, so the cache refreshes exactly when the file reparses; no
    // post-insertion workspace step is needed, unlike `structural`.
    let headings = tree.headings();
    let links = tree.links(abs_path);
    // Explicit in-page anchors (`<a id>` / `<a name>`) — cached like
    // headings/links so same-document fragment validation resolves `[…](#x)`
    // against explicit anchors as well as heading slugs (issue 025).
    let anchors = tree.anchors();

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
        // workspace fills it (and the suppression ledger) via
        // `recompute_structural` after insertion.
        structural: Vec::new(),
        suppressions: structural::FileSuppressions::default(),
        headings,
        links,
        anchors,
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

/// Discover the scope root covering `start`: the nearest ancestor `.lattice.toml`
/// or `.git`, or the starting directory itself when none is found (decision 019
/// clause 7 — markers declare structure, so a folder without an ancestor marker
/// is its own fallback scope).
///
/// Client-spelling: `start` is walked (and markers are `stat`ed) as given, so
/// the returned root is in the same spelling the caller's document keys use. The
/// flat document store (ticket server 10) roots each opened folder at the scope
/// this discovers.
#[must_use]
pub fn find_scope_root(start: &Path) -> Option<PathBuf> {
    find_workspace_root(start)
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

/// Discover all `.md` files under `root`, respecting `.gitignore`, together with
/// the strictly-deeper scope boundaries pruned from the walk (decision 019).
///
/// Descent stops at any directory (other than `root`) that carries its own
/// marker — a nested `.lattice.toml` (another scope) or a nested `.git` (a
/// non-root environment: a submodule or vendored repo). That subtree belongs to
/// its own graph, so neither its `.md` files (they are not members of this
/// scope) nor its contents are scanned here; the boundary directory itself is
/// recorded so a link resolving into it can steer to an `[external]` alias
/// instead of dangling. Returns `(markdown files, boundary directories)`, both
/// absolute.
fn discover_markdown_files_and_boundaries(root: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    use std::sync::{Arc, Mutex};

    // Captured in the walker's `filter_entry` closure, which prunes each nested
    // boundary from the traversal and records it here. `Arc<Mutex<…>>` satisfies
    // the closure's `Send + Sync + 'static` bound (a `RefCell` would not).
    let boundaries: Arc<Mutex<Vec<PathBuf>>> = Arc::new(Mutex::new(Vec::new()));
    let boundaries_sink = Arc::clone(&boundaries);
    let root_owned = root.to_path_buf();

    let walker = ignore::WalkBuilder::new(root)
        .standard_filters(true)
        .filter_entry(move |entry| {
            let path = entry.path();
            // The root itself is never a boundary of its own scan.
            if path == root_owned {
                return true;
            }
            let is_dir = entry.file_type().is_some_and(|ft| ft.is_dir());
            if is_dir && is_scope_boundary(path) {
                if let Ok(mut sink) = boundaries_sink.lock() {
                    sink.push(path.to_path_buf());
                }
                // Prune the boundary directory and everything beneath it.
                return false;
            }
            true
        })
        .build();

    let mut paths = Vec::new();
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

    let mut boundaries = Arc::try_unwrap(boundaries)
        .ok()
        .and_then(|mutex| mutex.into_inner().ok())
        .unwrap_or_default();
    boundaries.sort();
    boundaries.dedup();
    (paths, boundaries)
}

/// Whether `dir` is a strictly-deeper scope boundary: it carries its own
/// `.lattice.toml` (a nested scope) or `.git` (a non-root environment — a
/// submodule or vendored repo). Decision 019 resolution 2.
fn is_scope_boundary(dir: &Path) -> bool {
    dir.join(".lattice.toml").is_file() || dir.join(".git").exists()
}

/// The strictly-deeper scope boundaries directly inside `root` (client-spelling,
/// gitignore-aware), not descending into any of them — decision 019.
///
/// The flat document store (ticket server 10) uses this to register each nested
/// marker as its own scope root, and to recompute the active scope set when a
/// folder is removed. Only the shallowest boundary in each branch is returned;
/// a scope nested inside a nested scope is that scope's own boundary, not this
/// root's.
#[must_use]
pub fn discover_scope_boundaries(root: &Path) -> Vec<PathBuf> {
    let (_, boundaries) = discover_markdown_files_and_boundaries(root);
    boundaries
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for clarity")]
mod tests {
    use std::fs;
    use std::sync::Mutex;

    use super::*;

    /// Serializes tests that mutate the process-global current working
    /// directory. `std::env::set_current_dir` affects the whole process, so two
    /// CWD-mutating tests running concurrently (plain `cargo test` shares the
    /// process; `cargo nextest` does not) would race. The lock is intentionally
    /// poison-tolerant: a panic in one CWD test must not wedge the others.
    static CWD_LOCK: Mutex<()> = Mutex::new(());

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
    fn scan_refuses_a_present_but_unreadable_config() {
        // Decision 023, issue 065: a broken `.lattice.toml` is a failed
        // commitment, not an absent config — the one-shot scan refuses
        // instead of building an index under fabricated defaults.
        let dir =
            workspace_with_files(&[(".lattice.toml", "[[override\n"), ("README.md", "# Root")]);
        let err = Workspace::scan(dir.path()).expect_err("a broken config refuses the scan");
        assert!(
            matches!(err, WorkspaceError::Config { .. }),
            "the refusal names the config, not a generic failure: {err}"
        );
    }

    #[test]
    fn scan_recording_config_error_holds_the_error_for_the_server() {
        // The server's holding path (decision 023 addendum): the index is
        // built (config-independent features have data to serve) with the
        // load error recorded for the config channel to publish.
        let dir =
            workspace_with_files(&[(".lattice.toml", "[[override\n"), ("README.md", "# Root")]);
        let ws = Workspace::scan_recording_config_error(dir.path())
            .expect("the holding scan builds the index");
        assert!(
            ws.config_error.is_some(),
            "the load error is recorded, not dropped"
        );
        assert!(
            ws.file(Path::new("README.md")).is_some(),
            "documents index despite the broken config"
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
    fn bare_relative_subdir_from_cwd_discovers_root_and_files() {
        // Issue 024 (reopened): a bare single-component relative directory
        // (`docs`, no leading `./`) used to make `find_workspace_root` walk up to
        // the empty path `""` — which `join`s relative to the process CWD and
        // matched `.git`/`.lattice.toml`, returning an empty root that
        // discovers zero files (a silent false-clean). With `start` absolutized
        // the bare-relative form must discover the real root and its files.
        //
        // This must run with the process CWD at the fixture root and lint a path
        // genuinely relative to CWD — the existing `workspace_root_from_subdirectory`
        // joins onto an absolute temp path, so it never exercised this branch.
        let dir = workspace_with_files(&[("README.md", "# Root"), ("docs/guide.md", "# Guide")]);

        let _guard = CWD_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let original = std::env::current_dir().expect("read original cwd");
        std::env::set_current_dir(dir.path()).expect("chdir to fixture root");

        // `Path::new("docs").parent()` is `Some("")`, the empty-path trap.
        let scanned = Workspace::scan(Path::new("docs"));

        std::env::set_current_dir(&original).expect("restore original cwd");

        let ws = scanned.expect("bare-relative scan should succeed");
        assert!(
            ws.file(Path::new("docs/guide.md")).is_some(),
            "bare-relative `docs` must discover the file under it, not zero files"
        );
        assert_eq!(
            ws.files().len(),
            2,
            "bare-relative scan must walk the real tree, not an empty path"
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
    fn fenced_carrier_populates_backlinks() {
        // Decision 015: a `yaml lattice` carrier (here `<details>`-wrapped, at the
        // foot) populates `FileData.frontmatter` backlinks exactly as a leading
        // `---` block would, with no leading frontmatter present.
        let dir = workspace_with_files(&[(
            "target.md",
            "# Target\n\nbody\n\n<details><summary>lattice</summary>\n\n```yaml lattice\nbacklinks:\n  superseded_by:\n    - source.md\n```\n\n</details>\n",
        )]);

        let ws = Workspace::scan(dir.path()).expect("scan should succeed");
        let data = ws
            .file(Path::new("target.md"))
            .expect("should find target.md");
        let fm = data
            .frontmatter
            .as_ref()
            .expect("carrier should populate frontmatter");
        assert_eq!(
            fm.backlinks.get("superseded_by"),
            Some(&vec!["source.md".to_string()]),
            "the fenced carrier populates backlinks: {:?}",
            fm.backlinks
        );
        assert!(
            data.backlink_diagnostics.is_empty(),
            "known predicate produces no diagnostics: {:?}",
            data.backlink_diagnostics
        );
    }

    #[test]
    fn frontmatter_exception_suppresses_structural_diagnostic() {
        // End-to-end (issue 031): an `exceptions.stale_references` entry parsed
        // from a file's frontmatter is threaded into the structural pass and
        // suppresses the matching dangling-reference diagnostic.
        let dir = workspace_with_files(&[(
            "doc.md",
            "---\nexceptions:\n  stale_references:\n    \"gone.md\": \"deliberately dead\"\n---\nSee `gone.md` here.\n",
        )]);

        let ws = Workspace::scan(dir.path()).expect("scan should succeed");
        let data = ws.file(Path::new("doc.md")).expect("should find doc.md");
        let fm = data.frontmatter.as_ref().expect("should have frontmatter");
        assert_eq!(
            fm.exceptions.stale_references.len(),
            1,
            "the exceptions block parses into frontmatter"
        );
        assert!(
            !data
                .structural
                .iter()
                .any(|d| d.message.contains("stale reference")),
            "the exception suppresses the stale-reference diagnostic: {:?}",
            data.structural
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
        // The one-shot `scan` refuses a broken config (decision 023); the
        // server's holding scan still records that the user opted in.
        let dir = workspace_with_files(&[
            (".lattice.toml", "not valid toml {{{}}}"),
            ("README.md", "# Root"),
        ]);

        let ws = Workspace::scan_recording_config_error(dir.path())
            .expect("the holding scan should succeed");
        assert!(ws.has_config(), "broken config still means user opted in");
    }

    // -- Stage-2 structural cache (issue 013) --

    /// Stage-2 differential invariant: every file's cached `structural` vector
    /// must equal a from-scratch `structural::collect` for that file. A drift
    /// here is the "silent stale diagnostic" failure mode the cache risks.
    fn assert_cache_matches_recompute(ws: &Workspace) {
        for (path, file_data) in ws.files() {
            let file_exists = |target: &Path| ws.file(target).is_some();
            let external_exists = |p: &Path| structural::ExternalExistence::stat(p);
            let empty_exceptions = fm::Exceptions::default();
            let exceptions = file_data
                .frontmatter
                .as_ref()
                .map_or(&empty_exceptions, |fm| &fm.exceptions);
            let fresh = structural::collect(
                &file_data.tree,
                path,
                ws.config(),
                &file_exists,
                &external_exists,
                exceptions,
            );
            assert_eq!(
                file_data.structural,
                fresh,
                "cached structural for {} drifted from a fresh collect",
                path.display()
            );
        }
    }

    /// Severity of the make-it-a-link bare-path diagnostic on `path`, if any.
    fn bare_path_severity(ws: &Workspace, path: &Path) -> Option<crate::validation::Severity> {
        ws.file(path)?
            .structural
            .iter()
            .find(|d| d.message.contains("convert to a markdown link"))
            .map(|d| d.severity)
    }

    /// Severity of the stale-reference diagnostic on `path`, if any.
    fn stale_reference_severity(
        ws: &Workspace,
        path: &Path,
    ) -> Option<crate::validation::Severity> {
        ws.file(path)?
            .structural
            .iter()
            .find(|d| d.message.contains("stale reference"))
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
        // a.md references docs/page.md as a bare path. With the target absent it
        // is a dangling reference — the stale-reference warning (issue 028, no
        // make-it-a-link nudge yet); adding the target must flip a.md's cached
        // diagnostic to the make-it-a-link warning even though a.md itself never
        // changed — the membership recompute path.
        let dir = workspace_with_files(&[("a.md", "See docs/page.md for details.\n")]);
        let mut ws = Workspace::scan(dir.path()).expect("scan should succeed");
        assert_eq!(
            stale_reference_severity(&ws, Path::new("a.md")),
            Some(crate::validation::Severity::Warning),
            "absent target should be the stale-reference warning"
        );
        assert_eq!(
            bare_path_severity(&ws, Path::new("a.md")),
            None,
            "absent target draws no make-it-a-link nudge"
        );

        ws.update_content(Path::new("docs/page.md"), "# Page\n");
        assert_eq!(
            bare_path_severity(&ws, Path::new("a.md")),
            Some(crate::validation::Severity::Warning),
            "adding the target should flip the source's bare-path to the make-it-a-link warning"
        );
        assert_eq!(
            stale_reference_severity(&ws, Path::new("a.md")),
            None,
            "a resolving target draws no stale-reference warning"
        );
    }

    #[test]
    fn reload_config_applies_new_artifacts() {
        // Issue 044 / decision 017: a bare reference to the absent `artifact.md`
        // raises a stale-reference diagnostic. Adding `artifact.md` to
        // `[graph] artifacts` in `.lattice.toml` and reloading must clear that
        // diagnostic workspace-wide — no re-scan, no server restart.
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            ("doc.md", "See `artifact.md` here.\n"),
        ]);
        let mut ws = Workspace::scan(dir.path()).expect("scan should succeed");
        assert!(
            ws.config().artifacts.is_empty(),
            "the empty marker resolves to no artifacts"
        );
        assert_eq!(
            stale_reference_severity(&ws, Path::new("doc.md")),
            Some(crate::validation::Severity::Warning),
            "the backticked reference to the absent artifact.md is a stale reference before the edit"
        );

        // Edit the marker on disk to glossary-exempt `artifact.md`.
        fs::write(
            dir.path().join(".lattice.toml"),
            "[graph]\nartifacts = [\"artifact.md\"]\n",
        )
        .expect("rewrite .lattice.toml");
        ws.reload_config();

        assert!(
            ws.config().artifacts.contains("artifact.md"),
            "reload picks up the new artifact from the rewritten marker"
        );
        assert_eq!(
            stale_reference_severity(&ws, Path::new("doc.md")),
            None,
            "the artifact glossary suppresses the stale-reference diagnostic after reload"
        );
        assert_cache_matches_recompute(&ws);
    }

    #[test]
    fn reload_config_picks_up_new_predicate() {
        // A config reload must also refresh config-dependent *parse* output:
        // an unknown backlink predicate raises a `BacklinkDiagnostic`; defining
        // it under `[predicates]` and reloading must clear it.
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            (
                "doc.md",
                "---\nbacklinks:\n  tracks:\n    - other.md\n---\n# Doc\n",
            ),
        ]);
        let mut ws = Workspace::scan(dir.path()).expect("scan should succeed");
        assert!(
            ws.file(Path::new("doc.md"))
                .expect("doc.md indexed")
                .backlink_diagnostics
                .iter()
                .any(|d| d.predicate == "tracks"),
            "the unknown `tracks` predicate is flagged before the edit"
        );

        fs::write(
            dir.path().join(".lattice.toml"),
            "[predicates]\ntracks = \"tracked_by\"\n",
        )
        .expect("rewrite .lattice.toml");
        ws.reload_config();

        assert!(
            ws.file(Path::new("doc.md"))
                .expect("doc.md indexed")
                .backlink_diagnostics
                .is_empty(),
            "defining the predicate clears the unknown-predicate diagnostic after reload"
        );
    }

    #[test]
    fn reload_config_holds_last_good_on_a_broken_rewrite() {
        // Decision 023 addendum, issue 065: a reload hitting an unreadable
        // config is a failed commitment — the previous valid config keeps
        // governing, with the error recorded, never a swap to defaults.
        let dir = workspace_with_files(&[
            (".lattice.toml", "[graph]\nartifacts = [\"artifact.md\"]\n"),
            ("doc.md", "# Doc\n"),
        ]);
        let mut ws = Workspace::scan(dir.path()).expect("scan should succeed");

        fs::write(dir.path().join(".lattice.toml"), "[[override\n")
            .expect("break .lattice.toml on disk");
        ws.reload_config();

        assert!(
            ws.config().artifacts.contains("artifact.md"),
            "the last valid config keeps governing after the failed reload"
        );
        assert!(
            ws.config_error.is_some(),
            "the failed commitment's error is recorded"
        );
    }

    #[test]
    fn reload_config_preserves_unsaved_buffer() {
        // The editor buffer is authoritative (decision 017 §3): a config reload
        // must reparse from the in-memory source, never re-read disk, so an
        // unsaved edit survives the marker change.
        let dir = workspace_with_files(&[(".lattice.toml", ""), ("doc.md", "# On Disk\n")]);
        let mut ws = Workspace::scan(dir.path()).expect("scan should succeed");
        // Simulate an unsaved editor buffer diverging from disk.
        ws.update_content(Path::new("doc.md"), "# In Buffer\n");

        fs::write(
            dir.path().join(".lattice.toml"),
            "[graph]\nartifacts = [\"artifact.md\"]\n",
        )
        .expect("rewrite .lattice.toml");
        ws.reload_config();

        assert!(
            ws.file(Path::new("doc.md"))
                .expect("doc.md indexed")
                .tree
                .source()
                .contains("In Buffer"),
            "reload reparsed from the in-memory buffer, not the on-disk content"
        );
    }

    // -- Parsed-extraction cache (ticket perf 06) --

    /// Differential invariant: every file's cached `headings`/`links` vector
    /// must equal a fresh `Tree::headings()`/`Tree::links()` extraction. A drift
    /// here is the "silent stale extraction" failure mode the cache risks — the
    /// stage-2 spirit applied to fragment and forward-link inputs.
    ///
    /// Links classify against the document's absolute path (decision 019
    /// clause 8), so the fresh recompute joins the workspace-relative key onto
    /// the root — matching how `parse_content` built the cache.
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
                file_data.tree.links(&ws.root().join(path)),
                "cached links for {} drifted from a fresh extraction",
                path.display()
            );
            assert_eq!(
                file_data.anchors,
                file_data.tree.anchors(),
                "cached anchors for {} drifted from a fresh extraction",
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
