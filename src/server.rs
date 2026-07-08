// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! LSP server for Lattice.
//!
//! Publishes diagnostics on file open, save, and change. Provides workspace
//! symbols, rename, references, type hierarchy, and call hierarchy for
//! headings. Supports multiple workspace folders.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};

use crate::block::{
    ElementKind, Heading, HeadingId, LinkKind, NodeId, Syntax, Tree, content_lines, first_line,
    normalize_label,
};
use crate::completion::Context as CompletionContext;
use crate::config::{Config, ConfigError, FragmentAlgorithm};
use crate::line_index::LineIndex;
use crate::lsp;
use crate::span::Span;
use crate::validation::{self, Diagnostic, Severity};
use crate::workspace::{
    FileData, Workspace, WorkspaceView, compute_structural, discover_scope_boundaries,
    find_scope_root, parse_content, target_to_key,
};

/// What the client currently holds for one document: the Lattice diagnostics
/// that produced the published set (kept for cheap change-detection) and their
/// materialized LSP form (what was actually sent).
///
/// Storing both together lets the change-detector compare the cheap Lattice
/// vector and skip the expensive UTF-16 materialization for files an edit did
/// not touch, while still serving the exact bytes the client last received
/// (issue 013 — ticket perf 02). The two are always sized together: each
/// Lattice diagnostic materializes to exactly one LSP diagnostic, so one is
/// empty iff the other is.
struct PublishedDiagnostics {
    /// The Lattice diagnostics whose materialization was last published.
    lattice: Vec<Diagnostic>,
    /// The materialized LSP diagnostics last sent to the client.
    lsp: Vec<lsp::Diagnostic>,
}

/// A single parsed document: the one owner of its [`FileData`] plus the deepest
/// root that covers it (ticket server 10).
///
/// Content has exactly one home regardless of how it arrived — a directory
/// scan, a `didOpen`, or both — so no two containers can disagree about a
/// document, which is what the dual `inner`/`singletons` ownership (issue 051)
/// made possible.
struct Document {
    /// Parsed data, always parsed relative to `primary_root` (or the file name
    /// when rootless), so its link classification matches how the owning root
    /// resolves it.
    data: FileData,
    /// The deepest workspace root whose path covers this document, or `None`
    /// when the document lies outside every folder — a rootless single-file
    /// document (issue 051). `None` documents stay diagnostic-quiet: they are
    /// absent from every root's range scan, so the publish/pull tier never sees
    /// them, while their document-scoped features still resolve by direct path
    /// lookup.
    primary_root: Option<PathBuf>,
}

/// Root-level state for one workspace folder. Carries no file list — membership
/// is a range scan over [`Workspaces::documents`], never a secondary index.
struct RootMeta {
    /// Canonical scan root ([`Workspace::scan`]'s discovered root). May differ
    /// from the map key when the client opened the folder through a symlink
    /// (issue 047): the key is the client-supplied spelling documents resolve
    /// on, this is the canonical form the force-re-materialize comparison and
    /// config reload run against.
    canonical_root: PathBuf,
    /// Configuration loaded from the root.
    config: Config,
    /// Error from loading `.lattice.toml`, if any.
    config_error: Option<ConfigError>,
    /// Whether a `.lattice.toml` was found at the root.
    has_config: bool,
}

/// The server's flat document store: every parsed document keyed by absolute
/// path (its single owner), plus root metadata. Root membership is derived by
/// range scan — `documents.range(root..).take_while(|(p, _)| p.starts_with(root))`
/// — which is component-wise (via `Path::starts_with`), so a `Lattice` folder
/// never captures a `LatticeInternal` sibling the way a string-keyed range
/// would (ticket server 10).
struct Workspaces {
    /// Every parsed document, keyed by absolute path — the single content owner.
    documents: BTreeMap<PathBuf, Document>,
    /// Every active **scope root**, keyed by the (client-spelling) directory a
    /// marker declares — a `.lattice.toml` scope or a `.git` non-root
    /// environment (decision 019). Roots derive from markers, not folders: an
    /// added folder registers the nearest ancestor marker covering it and every
    /// strictly-deeper marker beneath it (walk up, then walk down). A document's
    /// `primary_root` is its deepest covering scope root, and each root's graph
    /// is the range scan filtered to `primary_root == root` — so a nested scope
    /// is a disjoint graph, never swallowed by its host.
    roots: BTreeMap<PathBuf, RootMeta>,
    /// The folders the client actually opened (their client-spelling paths).
    ///
    /// Client folders declare *visibility*; markers declare *structure*
    /// (decision 019 clause 7). A folder is an entry point into whatever scope
    /// covers it, not a root of its own. The active [`Self::roots`] are derived
    /// from this set (each folder's covering marker plus its nested markers), so
    /// removing a folder deregisters exactly the scopes no remaining folder
    /// keeps visible.
    client_folders: BTreeSet<PathBuf>,
    /// Nested `.git` non-root environments (a submodule or vendored repo without
    /// a `.lattice.toml`), by their client-spelling directory (decision 019
    /// resolution 2). These are *not* scopes: excluded from every host scope's
    /// scan and membership, and never eagerly indexed (Lattice does not read a
    /// foreign repo's graph). A document behind one is rootless — opened directly
    /// it serves document-scoped features and structural under defaults (051
    /// semantics) — and a link resolving into one crosses a boundary. Derived
    /// from the open folders alongside [`Self::roots`].
    git_boundaries: BTreeSet<PathBuf>,
    /// Diagnostics last published to the client, keyed by document URI.
    ///
    /// Used to suppress redundant `publishDiagnostics` notifications and to
    /// detect which files an edit moved: a file is only re-published when its
    /// materialized vector changes, and only re-materialized when its Lattice
    /// vector changes (issue 013 — publication diffing, then ticket perf 02's
    /// materialization cache). Only non-empty entries are stored, so an absent
    /// entry means the client currently holds no diagnostics for that URI.
    published: HashMap<String, PublishedDiagnostics>,
    /// URIs of documents currently open in the editor — live between
    /// `textDocument/didOpen` and `textDocument/didClose`.
    ///
    /// The editor buffer is authoritative for these (decision 017 §3): a
    /// `workspace/didChangeWatchedFiles` `changed` event carries *disk* content,
    /// so honoring it for a file open with unsaved edits would clobber the live
    /// buffer with stale bytes — and an open file edited in the editor already
    /// reaches the server through `didChange`, so the watcher event would be a
    /// second source of truth (the issue 009 duplication class). A `changed`
    /// event is therefore dropped while its URI is in this set; create/delete
    /// membership events are honored regardless (ticket server 09). It also
    /// pins buffer authority across a workspace-folder change: a folder added
    /// over an open document keeps the buffer, and a folder removed under one
    /// keeps it serving rootless.
    open_documents: HashSet<String>,
    /// A borrowable default configuration for rootless single-file views
    /// (issue 051): a document outside every root parses and serves its
    /// document-scoped features under defaults, with the graph tier inert.
    default_config: Config,
}

impl Workspaces {
    /// An empty store with no roots and no documents.
    fn new() -> Self {
        Self {
            documents: BTreeMap::new(),
            roots: BTreeMap::new(),
            client_folders: BTreeSet::new(),
            git_boundaries: BTreeSet::new(),
            published: HashMap::new(),
            open_documents: HashSet::new(),
            default_config: Config::default(),
        }
    }

    /// Create from the initial set of workspace folders.
    fn from_params(params: &lsp::InitializeParams) -> Self {
        let mut workspaces = Self::new();

        if let Some(folders) = &params.workspace_folders {
            for folder in folders {
                workspaces.add_folder(&folder.uri);
            }
        }

        // Fall back to deprecated root_uri if no folders resolved.
        if let Some(root_uri) = params
            .root_uri
            .as_ref()
            .filter(|_| workspaces.roots.is_empty())
        {
            workspaces.add_folder(root_uri);
        }

        workspaces
    }

    // --- Membership derivation (range scan, no index) ---

    /// The deepest scope root whose path component-covers `abs`, or `None` when
    /// `abs` lies outside every scope. Deepest = most path components, which for
    /// nested roots (each a prefix of the other) is the longest, unambiguously.
    ///
    /// A document behind a nested `.git` non-root environment
    /// ([`Self::git_boundaries`]) has no graph of its own (decision 019
    /// resolution 2): it is rootless — excluded from every host scope, served
    /// document-scoped under defaults (051 semantics) — so it never resolves to a
    /// covering root.
    fn deepest_root_for(&self, abs: &Path) -> Option<PathBuf> {
        if self.git_boundaries.iter().any(|g| abs.starts_with(g)) {
            return None;
        }
        self.roots
            .keys()
            .filter(|root| abs.starts_with(root))
            .max_by_key(|root| root.components().count())
            .cloned()
    }

    /// The absolute paths of every document under `root`, by range scan.
    fn document_keys_under(&self, root: &Path) -> Vec<PathBuf> {
        self.documents
            .range(root.to_path_buf()..)
            .take_while(|(abs, _)| abs.starts_with(root))
            .map(|(abs, _)| abs.clone())
            .collect()
    }

    /// The configuration a document with the given primary root parses under:
    /// the root's config, or the rootless default (issue 051).
    fn config_for(&self, primary: Option<&Path>) -> &Config {
        primary
            .and_then(|root| self.roots.get(root))
            .map_or(&self.default_config, |meta| &meta.config)
    }

    /// The strictly-deeper scope boundaries nested inside `root`: every
    /// registered scope root `root` is a proper ancestor of, plus every nested
    /// `.git` non-root environment beneath it (decision 019). A link resolving
    /// into one of these has crossed a boundary
    /// ([`WorkspaceLike::crosses_boundary`]).
    fn boundaries_under(&self, root: &Path) -> Vec<PathBuf> {
        self.roots
            .keys()
            .filter(|other| other.as_path() != root && other.starts_with(root))
            .chain(self.git_boundaries.iter().filter(|g| g.starts_with(root)))
            .cloned()
            .collect()
    }

    /// Build the per-root view the graph/diagnostic pipeline and cross-file
    /// document features consume: every document **whose primary root is `root`**,
    /// keyed by path relative to `root`.
    ///
    /// Membership is the range scan tightened to `primary_root == root`
    /// (decision 019, ticket server 10's anticipated filter): a document under a
    /// strictly-deeper boundary belongs to that nested scope's graph, not this
    /// one, so the two scopes are disjoint — the host never sees the nested
    /// scope's files, and vice versa.
    fn root_view(&self, root: &Path) -> WorkspaceView<'_> {
        let mut files = BTreeMap::new();
        for (abs, doc) in self
            .documents
            .range(root.to_path_buf()..)
            .take_while(|(abs, _)| abs.starts_with(root))
        {
            if doc.primary_root.as_deref() != Some(root) {
                continue;
            }
            if let Ok(rel) = abs.strip_prefix(root) {
                files.insert(rel.to_path_buf(), &doc.data);
            }
        }
        let (config, has_config) = self.roots.get(root).map_or_else(
            || (&self.default_config, false),
            |meta| (&meta.config, meta.has_config),
        );
        WorkspaceView::new(
            root.to_path_buf(),
            config,
            has_config,
            files,
            self.boundaries_under(root),
        )
    }

    /// Build a single-file view over one rootless document (issue 051): its
    /// parent directory is the view root and its file name the sole key, exactly
    /// as the old single-file `Workspace` was shaped, so document-scoped
    /// features resolve identically without a workspace.
    fn single_file_view<'a>(&'a self, abs: &Path, doc: &'a Document) -> WorkspaceView<'a> {
        let root = match (abs.parent(), abs.file_name()) {
            (Some(parent), Some(_)) => parent.to_path_buf(),
            _ => PathBuf::new(),
        };
        let mut files = BTreeMap::new();
        files.insert(document_rel(abs, None), &doc.data);
        WorkspaceView::new(root, &self.default_config, false, files, Vec::new())
    }

    // --- Document resolution ---

    /// Resolve a URI to the view and relative path for its **graph/diagnostic**
    /// tier: the deepest covering root, or `None` for a rootless or unindexed
    /// document (which pulls/publishes nothing — issue 051).
    fn resolve(&self, uri: &str) -> Option<(WorkspaceView<'_>, PathBuf)> {
        let abs = uri_to_path(uri);
        let doc = self.documents.get(&abs)?;
        let root = doc.primary_root.as_ref()?;
        let rel = abs.strip_prefix(root).ok()?.to_path_buf();
        Some((self.root_view(root), rel))
    }

    /// Resolve a URI to the view and relative path that serve its
    /// **document-scoped** features (semantic tokens, folding, symbols, hover,
    /// formatting, document links, completion, navigation, …).
    ///
    /// A single direct path lookup: a rooted document resolves against its
    /// deepest root's view; a rootless document (issue 051) against a
    /// single-file view. There is no two-phase workspace-then-singleton
    /// fallback — the flat store made it unnecessary.
    fn resolve_document(&self, uri: &str) -> Option<(WorkspaceView<'_>, PathBuf)> {
        let abs = uri_to_path(uri);
        let doc = self.documents.get(&abs)?;
        match doc.primary_root.as_ref() {
            Some(root) => {
                let rel = abs.strip_prefix(root).ok()?.to_path_buf();
                Some((self.root_view(root), rel))
            }
            None => Some((self.single_file_view(&abs, doc), document_rel(&abs, None))),
        }
    }

    // --- Content upsert / eviction / reconciliation ---

    /// Sync an opened or edited document's in-memory content into the store,
    /// keyed by path. Content has one home: a URI under a folder joins that
    /// root's membership; one outside every folder is rootless (issue 051) and
    /// stays diagnostic-quiet. `primary_root` is computed at insert.
    fn sync_document_content(&mut self, uri: &str, content: &str) {
        let abs = uri_to_path(uri);
        let existed = self.documents.contains_key(&abs);
        let primary = self.deepest_root_for(&abs);
        let rooted = primary.is_some();
        let data = {
            // Links classify against the absolute path (root-free), so the config
            // affects only the frontmatter predicate check — placement is derived
            // separately below.
            let config = self.config_for(primary.as_deref());
            parse_content(content, &abs, config)
        };
        self.documents.insert(
            abs.clone(),
            Document {
                data,
                primary_root: primary,
            },
        );
        if existed || !rooted {
            // An edit to an already-indexed document, or a rootless document
            // (issue 051): no root's membership changed — a rootless document
            // appears in no root's range scan, so no other document's
            // bare-path existence answer can flip. Refresh this document's
            // own cache only.
            self.recompute_structural(&abs);
        } else {
            // A rooted document joined the membership: a bare-path existence
            // answer can flip in any document that shares a root, so
            // recompute the structural caches.
            self.recompute_all_structural();
        }
    }

    /// Reconcile a document to disk: re-read and re-parse it, or drop it if it
    /// is gone (issue 046 didClose semantics; watched create/change/delete).
    /// Its `primary_root` is recomputed at insert.
    fn update_from_disk(&mut self, abs: &Path) {
        let existed = self.documents.contains_key(abs);
        if !abs.is_file() {
            if self.documents.remove(abs).is_some() {
                self.recompute_all_structural();
            }
            return;
        }
        let Ok(content) = std::fs::read_to_string(abs) else {
            // Exists but unreadable: drop it so no stale content lingers —
            // stricter than the test-only incremental `Workspace::update`,
            // which keeps the stale entry and surfaces the read error.
            if self.documents.remove(abs).is_some() {
                self.recompute_all_structural();
            }
            return;
        };
        let primary = self.deepest_root_for(abs);
        let data = {
            let config = self.config_for(primary.as_deref());
            parse_content(&content, abs, config)
        };
        self.documents.insert(
            abs.to_path_buf(),
            Document {
                data,
                primary_root: primary,
            },
        );
        if existed {
            self.recompute_structural(abs);
        } else {
            self.recompute_all_structural();
        }
    }

    /// Drop a rootless single-file document (issue 051) — used on `didClose`,
    /// when the editor discards a buffer that has no disk-backed root to revert
    /// to. A no-op for a rooted or unindexed URI.
    fn remove_single_file(&mut self, uri: &str) {
        let abs = uri_to_path(uri);
        if self
            .documents
            .get(&abs)
            .is_some_and(|doc| doc.primary_root.is_none())
        {
            self.documents.remove(&abs);
        }
    }

    /// Re-key the document store for one `oldUri -> newUri` rename the client
    /// has just performed (`workspace/didRenameFiles` — decision 020 clause 2),
    /// **without a rescan**.
    ///
    /// The move engine's text edits were already applied to buffers by the
    /// client before it renamed on disk (decision 017 §3), so the content at the
    /// new key is authoritative and correct — re-keying just moves the parsed
    /// entries from the old absolute path to the new one and reparses each under
    /// its (possibly changed) primary root, rather than re-reading a whole
    /// scope. A file rename moves the single entry; a directory rename moves
    /// every document under the old prefix. The `open_documents` set and the
    /// per-URI `published` cache are re-keyed alongside so buffer authority and
    /// publication diffing follow the file. Returns whether anything moved.
    ///
    /// Reparsing (not a bare key swap) is required because a document parses
    /// relative to its root, and the link classification / structural existence
    /// checks read the new coordinate; the buffer text is preserved verbatim.
    ///
    /// Returns the old URIs that held a published diagnostic set, so the caller
    /// can send each an explicit empty publish — the re-publish diff iterates
    /// the *current* store and never revisits a vanished key.
    fn rekey_rename(&mut self, old_abs: &Path, new_abs: &Path) -> Vec<String> {
        // Every stored document at, or under, the old path (a file is its own
        // sole member; a directory prefixes several).
        let moved: Vec<PathBuf> = self
            .documents
            .range(old_abs.to_path_buf()..)
            .take_while(|(abs, _)| abs.starts_with(old_abs))
            .map(|(abs, _)| abs.clone())
            .collect();
        let mut cleared = Vec::new();
        for old_key in moved {
            let Some(doc) = self.documents.remove(&old_key) else {
                continue;
            };
            // Translate the key under the rename: the source maps to the
            // destination, a contained member keeps its suffix.
            let new_key = if old_key == old_abs {
                new_abs.to_path_buf()
            } else {
                old_key
                    .strip_prefix(old_abs)
                    .map_or_else(|_| old_key.clone(), |suffix| new_abs.join(suffix))
            };
            // Follow the buffer-authority and publication-diff state to the new
            // key so an open renamed file stays editor-authoritative and its
            // stale publication under the old URI is cleared.
            let old_uri = path_to_uri(&old_key);
            let new_uri = path_to_uri(&new_key);
            if self.open_documents.remove(&old_uri) {
                self.open_documents.insert(new_uri);
            }
            if self.published.remove(&old_uri).is_some() {
                cleared.push(old_uri);
            }
            // Reparse from the preserved buffer under the destination's primary
            // root — placement and coordinate change, content does not.
            let primary = self.deepest_root_for(&new_key);
            let source = doc.data.tree.source().to_string();
            let data = {
                let config = self.config_for(primary.as_deref());
                parse_content(&source, &new_key, config)
            };
            self.documents.insert(
                new_key,
                Document {
                    data,
                    primary_root: primary,
                },
            );
        }
        cleared
    }

    // --- Placement (primary_root) recomputation ---

    /// Recompute a document's deepest covering root and, if it changed, **flip
    /// its `primary_root` in place** — reparsing from its buffer only when the
    /// re-root crosses a config boundary (decision 019 clause 6; ticket
    /// server 11's placement/reparse split, refined by ticket server 12).
    ///
    /// Placement is metadata: the parse tree and its cached links are root-free
    /// (links classify against the absolute path, decision 019 clause 8), so a
    /// re-root within one config cannot change them, and the `FileData` — buffer
    /// included — is preserved untouched, keeping an open editor buffer
    /// authoritative across the change (decision 017 §3) without touching disk or
    /// the parser. One parse-time derivation, however, *is* config-sensitive:
    /// `FileData::backlink_diagnostics`, the frontmatter unknown-predicate check,
    /// reads the predicate vocabulary. A re-root that crosses a scope boundary
    /// (a live split/merge, or a folder add/remove over a marker scope) changes
    /// the effective config, so when the predicate vocabulary actually differs
    /// this reparses from the buffer to refresh that derivation. The
    /// root-dependent structural cache is refreshed by the caller's
    /// `recompute_all_structural` afterward.
    fn refresh_placement(&mut self, abs: &Path) {
        let new_primary = self.deepest_root_for(abs);
        let Some(doc) = self.documents.get(abs) else {
            return;
        };
        let old_primary = doc.primary_root.clone();
        if old_primary == new_primary {
            return;
        }
        // Does the re-root change the predicate vocabulary the parse-time
        // backlink check reads? Only then must the buffer be reparsed.
        let reparse = self.config_for(old_primary.as_deref()).predicates
            != self.config_for(new_primary.as_deref()).predicates;
        if let Some(doc) = self.documents.get_mut(abs) {
            doc.primary_root = new_primary;
        }
        if reparse {
            self.reparse_from_buffer(abs);
        }
    }

    /// Reparse a document from its in-memory buffer under its current primary
    /// root's config (placement unchanged) — used by a config reload
    /// (ticket server 08), which changes the config every owned document parses
    /// under while preserving membership and unsaved buffers.
    ///
    /// The reparse survives ticket server 11's placement/reparse split because
    /// the config still feeds one *parse-time* derivation: the frontmatter
    /// backlink-predicate check (`FileData::backlink_diagnostics`), which flags
    /// an unknown predicate against the config vocabulary and records its line.
    /// Link classification, by contrast, is config- and root-free, so a mere
    /// placement change routes through `refresh_placement` instead (no reparse).
    fn reparse_from_buffer(&mut self, abs: &Path) {
        let Some(doc) = self.documents.get(abs) else {
            return;
        };
        let primary = doc.primary_root.clone();
        let source = doc.data.tree.source().to_string();
        let data = {
            let config = self.config_for(primary.as_deref());
            parse_content(&source, abs, config)
        };
        if let Some(doc) = self.documents.get_mut(abs) {
            doc.data = data;
        }
    }

    // --- Structural cache maintenance ---

    /// Recompute one document's cached structural diagnostics against its
    /// primary root's membership and config. A rootless document is left empty
    /// (issue 051 — single-file documents carry no workspace-tier verdicts).
    fn recompute_structural(&mut self, abs: &Path) {
        let Some(doc) = self.documents.get(abs) else {
            return;
        };
        let Some(root) = doc.primary_root.clone() else {
            if let Some(doc) = self.documents.get_mut(abs) {
                doc.data.structural = Vec::new();
                doc.data.suppressions = crate::structural::FileSuppressions::default();
            }
            return;
        };
        let rel = abs.strip_prefix(&root).unwrap_or(abs).to_path_buf();
        let config = self
            .roots
            .get(&root)
            .map_or(&self.default_config, |meta| &meta.config);
        // Membership under the primary root, by range scan through the flat
        // store: a bare-path target `t` exists iff `root/t` is a document *of
        // this scope* — `primary_root == root` (decision 019). A document under
        // a strictly-deeper boundary lives in a nested scope, so it is not a
        // member here; a mention resolving to it dangles as a stale reference
        // that steers to the `[external]` alias, exactly as a cross-boundary
        // link errors in the graph tier.
        let file_exists = |target: &Path| {
            self.documents
                .get(&root.join(target))
                .is_some_and(|doc| doc.primary_root.as_deref() == Some(root.as_path()))
        };
        let (diagnostics, suppressions) = compute_structural(&doc.data, &rel, config, &file_exists);
        if let Some(doc) = self.documents.get_mut(abs) {
            doc.data.structural = diagnostics;
            doc.data.suppressions = suppressions;
        }
    }

    /// Recompute the structural cache for every document. Required on a
    /// membership change: adding or removing one file can flip a bare-path
    /// existence answer in any document that shares a root.
    fn recompute_all_structural(&mut self) {
        // Count full sweeps so tests can pin which store mutations pay the
        // O(workspace) cost (a rootless open must not). Compiled out of
        // release builds.
        #[cfg(test)]
        STRUCTURAL_SWEEP_COUNT.with(|count| count.set(count.get() + 1));

        let keys: Vec<PathBuf> = self.documents.keys().cloned().collect();
        for abs in &keys {
            self.recompute_structural(abs);
        }
    }

    // --- Workspace-folder changes ---

    /// Add a workspace folder (decision 019 clause 7 — the folder declares
    /// visibility; markers declare structure).
    ///
    /// The folder is rooted at the nearest ancestor marker covering it (or
    /// itself, a fallback scope, when none exists), and that scope plus every
    /// strictly-deeper marker beneath it are registered — so opening a
    /// subdirectory scans the whole scope, and a nested `.lattice.toml` / `.git`
    /// becomes its own graph rather than being swallowed (resolution 1). Every
    /// document under the covering scope then recomputes its deepest primary
    /// root, reparsing across a config boundary and leaving open buffers
    /// authoritative (decision 017 §3), so the editor buffer is never shadowed by
    /// disk and no orphaned entry remains.
    fn add_folder(&mut self, uri: &str) {
        let folder = uri_to_path(uri);
        self.client_folders.insert(folder.clone());
        let covering = find_scope_root(&folder).unwrap_or_else(|| folder.clone());
        self.register_scope(&covering);
        self.rebuild_git_boundaries();
        for key in self.document_keys_under(&covering) {
            self.refresh_placement(&key);
        }
        self.recompute_all_structural();
    }

    /// Register `scope_root` (a client-spelling marker directory, or a folder
    /// fallback) as a scope root, then recurse into every strictly-deeper scope
    /// beneath it (decision 019 clause 1).
    ///
    /// Loads the scope's config and folds its boundary-pruned scan into the flat
    /// store *upsert-if-absent* — an occupied entry (an open buffer, or a
    /// document a sibling scope already holds) keeps its content and the disk
    /// parse is dropped; the provisional primary root is corrected by the
    /// caller's `refresh_placement` loop. Idempotent: an already-registered scope
    /// returns immediately, so an ancestor folder and one of its nested scopes,
    /// both opened, register each scope exactly once.
    fn register_scope(&mut self, scope_root: &Path) {
        if self.roots.contains_key(scope_root) {
            return;
        }
        let Ok(ws) = Workspace::scan(scope_root) else {
            return;
        };
        let parts = ws.into_parts();
        self.roots.insert(
            scope_root.to_path_buf(),
            RootMeta {
                canonical_root: parts.root,
                config: parts.config,
                config_error: parts.config_error,
                has_config: parts.has_config,
            },
        );
        for (rel, data) in parts.files {
            let key = scope_root.join(&rel);
            self.documents.entry(key).or_insert_with(|| Document {
                data,
                primary_root: Some(scope_root.to_path_buf()),
            });
        }
        // Recurse only into nested `.lattice.toml` scopes — those are graphs. A
        // nested `.git`-only environment is not a scope (decision 019 resolution
        // 2): it is left unscanned and tracked as a boundary by
        // `rebuild_git_boundaries`, so a foreign repo is never indexed.
        for nested in discover_scope_boundaries(scope_root) {
            if nested.join(".lattice.toml").is_file() {
                self.register_scope(&nested);
            }
        }
    }

    /// Recompute the nested `.git` non-root environments visible through the open
    /// client folders (decision 019 resolution 2), walking each folder's scope
    /// tree without parsing. Rebuilt whenever the folder set or scope structure
    /// changes, so a `.git` boundary no folder keeps visible drops out.
    fn rebuild_git_boundaries(&mut self) {
        let mut git = BTreeSet::new();
        for folder in &self.client_folders {
            let covering = find_scope_root(folder).unwrap_or_else(|| folder.clone());
            Self::collect_git_boundaries(&covering, &mut git);
        }
        self.git_boundaries = git;
    }

    /// Collect the nested `.git`-only boundaries beneath `scope_root` into `out`,
    /// descending through nested `.lattice.toml` scopes (each of which may hold
    /// its own `.git` sub-environments) but never into a `.git` boundary itself.
    fn collect_git_boundaries(scope_root: &Path, out: &mut BTreeSet<PathBuf>) {
        for boundary in discover_scope_boundaries(scope_root) {
            if boundary.join(".lattice.toml").is_file() {
                Self::collect_git_boundaries(&boundary, out);
            } else {
                out.insert(boundary);
            }
        }
    }

    /// Remove a workspace folder: recompute which scope roots remain visible
    /// through the surviving folders, deregister the scopes none keeps visible,
    /// and re-root or evict the documents that touched.
    ///
    /// A scope root persists while any open folder still covers it — a nested
    /// marker discovered by walk-down survives its own folder's removal, since
    /// the covering folder keeps it visible (decision 019 clause 7). A scan-only
    /// document left uncovered is evicted; an open one keeps serving, rootless or
    /// re-rooted onto the covering scope with no dark window (decision 017 §3),
    /// reparsing across a config boundary via `refresh_placement`.
    fn remove_folder(&mut self, uri: &str) {
        let folder = uri_to_path(uri);
        if !self.client_folders.remove(&folder) {
            return;
        }
        let active = self.active_scope_roots();
        let stale: BTreeSet<PathBuf> = self
            .roots
            .keys()
            .filter(|root| !active.contains(*root))
            .cloned()
            .collect();
        for root in &stale {
            self.roots.remove(root);
        }
        self.rebuild_git_boundaries();
        let affected: Vec<PathBuf> = self
            .documents
            .iter()
            .filter(|(_, doc)| {
                doc.primary_root
                    .as_ref()
                    .is_some_and(|root| stale.contains(root))
            })
            .map(|(abs, _)| abs.clone())
            .collect();
        for key in affected {
            let new_primary = self.deepest_root_for(&key);
            let is_open = self.open_documents.contains(&path_to_uri(&key));
            if new_primary.is_none() && !is_open {
                self.documents.remove(&key);
            } else {
                self.refresh_placement(&key);
            }
        }
        self.recompute_all_structural();
    }

    /// The scope roots visible through the currently-open client folders: each
    /// folder's covering marker plus every strictly-deeper marker beneath it
    /// (decision 019 clause 7). Recomputed on a folder removal to deregister
    /// scopes no surviving folder keeps visible.
    fn active_scope_roots(&self) -> BTreeSet<PathBuf> {
        let mut active = BTreeSet::new();
        for folder in &self.client_folders {
            let covering = find_scope_root(folder).unwrap_or_else(|| folder.clone());
            Self::collect_scope_tree(&covering, &mut active);
        }
        active
    }

    /// Add `scope_root` and every strictly-deeper marker scope beneath it to
    /// `out`, walking client-spelling directories on disk.
    fn collect_scope_tree(scope_root: &Path, out: &mut BTreeSet<PathBuf>) {
        if !out.insert(scope_root.to_path_buf()) {
            return;
        }
        for nested in discover_scope_boundaries(scope_root) {
            Self::collect_scope_tree(&nested, out);
        }
    }

    // --- Live split / merge (decision 019 clause 6) ---

    /// The client-key of the scope root registered at directory `dir`, matched
    /// by its own key or its canonical scan path — so a marker reported under a
    /// symlinked spelling (issue 047) still resolves to the workspace it belongs
    /// to (issue 050). Unlike a prefix match, this is exact: it names the scope
    /// *at* `dir`, not a scope that merely contains it.
    fn registered_root_at(&self, dir: &Path) -> Option<PathBuf> {
        self.roots.iter().find_map(|(key, meta)| {
            (key.as_path() == dir || meta.canonical_root == dir).then(|| key.clone())
        })
    }

    /// Apply a `.lattice.toml` marker create/change/delete event (decision 019
    /// clause 6). Returns whether the event matched a workspace and something was
    /// applied, so the caller knows a re-publish is due.
    ///
    /// Four cases, on `(marker present, scope already registered here)`:
    /// - present + registered → the scope's config changed → hot-reload it
    ///   (ticket server 08).
    /// - present + not registered, inside a visible scope → a **split**: the new
    ///   marker carves its subtree into its own graph.
    /// - absent + registered, still a visible scope (an open folder, or a `.git`
    ///   non-root environment) → hot-reload to defaults (`.lattice.toml` gone).
    /// - absent + registered, a nested `.lattice.toml`-only scope → a **merge**:
    ///   the subtree fuses back into its host.
    fn handle_marker_event(&mut self, marker_uri: &str) -> bool {
        let marker_path = uri_to_path(marker_uri);
        let Some(marker_dir) = marker_path.parent().map(Path::to_path_buf) else {
            return false;
        };
        let toml_present = marker_path.is_file();
        let registered = self.registered_root_at(&marker_dir);

        match (toml_present, registered) {
            (true, Some(root)) => {
                self.reload_root_config(&root);
                true
            }
            (true, None) => {
                if self.deepest_root_for(&marker_dir).is_some() {
                    self.split_scope(&marker_dir);
                    true
                } else {
                    false
                }
            }
            (false, Some(root)) => {
                if self.client_folders.contains(&root) {
                    // The client's own folder stays visible as a fallback / `.git`
                    // scope root; reload to defaults now that `.lattice.toml` is
                    // gone.
                    self.reload_root_config(&root);
                } else {
                    // A nested scope lost its `.lattice.toml`. Deregister it: if a
                    // `.git` remains, `rebuild_git_boundaries` (inside
                    // `merge_scope`) reclassifies it as a non-root environment and
                    // its documents go rootless (051); otherwise they merge back
                    // into the host scope.
                    self.merge_scope(&root);
                }
                true
            }
            (false, None) => false,
        }
    }

    /// Split a newly-created nested marker at `marker_dir` out of its host scope:
    /// register it (and any scopes beneath it), re-root the captured range, and
    /// refresh the boundary neighborhood (decision 019 clause 6).
    ///
    /// Open buffers are preserved (decision 017 §3); only the re-rooted documents
    /// reparse, and then only across a config boundary that changes the predicate
    /// vocabulary — every other document is untouched. The host's now-crossing
    /// plain links resurface as steering errors, and its mentions into the split
    /// subtree as stale references, both computed by the next publish's collect
    /// (no reparse of the host's documents).
    fn split_scope(&mut self, marker_dir: &Path) {
        let host = self.deepest_root_for(marker_dir);
        self.register_scope(marker_dir);
        self.rebuild_git_boundaries();
        let scan_from = host.unwrap_or_else(|| marker_dir.to_path_buf());
        for key in self.document_keys_under(&scan_from) {
            self.refresh_placement(&key);
        }
        self.recompute_all_structural();
    }

    /// Merge a nested scope whose only marker was deleted back into its host
    /// (decision 019 clause 6): deregister it and re-root its documents onto the
    /// covering scope, re-exposing whatever reconciliation debt accrued while the
    /// scopes were separate. A document whose own deeper marker persists keeps
    /// that deeper scope (its `deepest_root_for` is unchanged).
    fn merge_scope(&mut self, scope_root: &Path) {
        if self.roots.remove(scope_root).is_none() {
            return;
        }
        self.rebuild_git_boundaries();
        for key in self.document_keys_under(scope_root) {
            let new_primary = self.deepest_root_for(&key);
            let is_open = self.open_documents.contains(&path_to_uri(&key));
            if new_primary.is_none() && !is_open {
                self.documents.remove(&key);
            } else {
                self.refresh_placement(&key);
            }
        }
        self.recompute_all_structural();
    }

    // --- Config reload (ticket server 08) ---

    /// Reload one root's `.lattice.toml` and re-parse every document it owns
    /// from its in-memory buffer under the fresh config, then recompute the
    /// structural caches. Preserves membership and unsaved buffers.
    ///
    /// The reparse is justified by exactly one config-sensitive *parse-time*
    /// derivation that survives ticket server 11's coordinate move:
    /// `FileData::backlink_diagnostics`, the frontmatter unknown-predicate check,
    /// which reads the predicate vocabulary and records each offending line at
    /// parse time. The config's other consumers — artifacts, overrides, external
    /// aliases (decision 017) — feed the *structural* tier, refreshed below by
    /// `recompute_all_structural` without a reparse. Link classification is
    /// config- and root-free, so it is invariant across a reload; a placement
    /// change, which touches neither the config nor the tree, routes through
    /// `refresh_placement` and never reaches here.
    fn reload_root_config(&mut self, root: &Path) {
        let Some(meta) = self.roots.get_mut(root) else {
            return;
        };
        let canonical = meta.canonical_root.clone();
        let (config, config_error) = match Config::load(&canonical) {
            Ok(c) => (c, None),
            Err(e) => {
                tracing::warn!(root = %canonical.display(), "config reload error, using defaults: {e}");
                (Config::default(), Some(e))
            }
        };
        meta.has_config = canonical.join(".lattice.toml").is_file();
        meta.config = config;
        meta.config_error = config_error;

        let owned: Vec<PathBuf> = self
            .documents
            .iter()
            .filter(|(_, doc)| doc.primary_root.as_deref() == Some(root))
            .map(|(abs, _)| abs.clone())
            .collect();
        for abs in &owned {
            self.reparse_from_buffer(abs);
        }
        self.recompute_all_structural();
    }
}

/// The path a document parses relative to: its path under `primary` for a
/// rooted document, or its file name (matching the old single-file `Workspace`)
/// when rootless.
fn document_rel(abs: &Path, primary: Option<&Path>) -> PathBuf {
    primary.map_or_else(
        || match (abs.parent(), abs.file_name()) {
            (Some(_), Some(name)) => PathBuf::from(name),
            _ => abs.to_path_buf(),
        },
        |root| abs.strip_prefix(root).unwrap_or(abs).to_path_buf(),
    )
}

// ---------------------------------------------------------------------------
// Semantic tokens legend (ticket integration 15)
// ---------------------------------------------------------------------------

/// The single semantic token type Lattice emits. All emphasis runs carry this
/// base type and distinguish themselves through modifiers, so overlapping runs
/// (strong inside emphasis) compose into one token with combined modifiers
/// rather than two illegal overlapping tokens.
const SEMANTIC_TOKEN_TYPE_MARKUP: &str = "markup";
/// Modifier name for strong (`**bold**`) runs.
const SEMANTIC_MODIFIER_BOLD: &str = "bold";
/// Modifier name for emphasis (`*italic*`) runs.
const SEMANTIC_MODIFIER_ITALIC: &str = "italic";
/// Modifier name for strikethrough (`~~struck~~`) runs.
const SEMANTIC_MODIFIER_STRIKETHROUGH: &str = "strikethrough";

/// Token-type index into the legend's `tokenTypes` array. Only `markup`
/// (index 0) exists.
const SEMANTIC_TOKEN_TYPE_MARKUP_INDEX: u32 = 0;
/// Modifier bit for `bold` — index 0 in the legend's `tokenModifiers` array.
const SEMANTIC_MODIFIER_BOLD_BIT: u32 = 1 << 0;
/// Modifier bit for `italic` — index 1 in the legend's `tokenModifiers` array.
const SEMANTIC_MODIFIER_ITALIC_BIT: u32 = 1 << 1;
/// Modifier bit for `strikethrough` — index 2 in the legend's `tokenModifiers`
/// array.
const SEMANTIC_MODIFIER_STRIKETHROUGH_BIT: u32 = 1 << 2;

/// Fixed registration id for the `.lattice.toml` watcher.
///
/// Registration is fire-and-forget — Lattice registers the watcher once after
/// initialization and never unregisters it — so a constant id suffices
/// (decision 017, ticket server 08).
const WATCHED_FILES_REGISTRATION_ID: &str = "lattice-watched-files";

/// Fixed request id for the server-originated `client/registerCapability`
/// request. The client's response is discarded by [`main_loop`], so a constant
/// id is fine.
const REGISTER_CAPABILITY_REQUEST_ID: &str = "lattice-register-capability";

/// Glob the marker watcher subscribes to: the project-level `.lattice.toml`
/// at any depth under a workspace folder (decision 017, ticket server 08).
const LATTICE_TOML_WATCH_GLOB: &str = "**/.lattice.toml";

/// Glob the document watcher subscribes to: every markdown file at any depth
/// under a workspace folder (decision 017, ticket server 09). Catches on-disk
/// `.md` changes for files that are not open in the editor, where
/// `textDocument` sync never fires; the buffer-wins rule reconciles it with the
/// document-sync channel for open files.
const MD_WATCH_GLOB: &str = "**/*.md";

/// Run the LSP server on stdio.
///
/// # Errors
///
/// Returns an error if the connection or initialization fails.
pub fn run() -> Result<()> {
    let (connection, io_threads) = Connection::stdio();
    serve(&connection)?;
    drop(connection); // Close channels so IO threads can exit.
    io_threads.join()?;
    Ok(())
}

/// Drive the LSP lifecycle on an established connection: the capabilities
/// handshake, the watched-files registration, then the message loop.
///
/// Split out from [`run`] so the wire protocol can be exercised over an
/// in-memory connection in tests without spawning real stdio IO threads.
///
/// # Errors
///
/// Returns an error if initialization or the message loop fails.
fn serve(connection: &Connection) -> Result<()> {
    // Two-phase init so the capabilities we advertise can depend on the client's
    // own capabilities: `workspace/willRenameFiles` is advertised only to a
    // client that sends it (decision 020 clause 2). `initialize_start` returns
    // the client params before we must send our own, so we parse them first,
    // build the capabilities conditionally, then finish the handshake.
    let (init_id, init_value) = connection.initialize_start()?;
    let params: lsp::InitializeParams =
        serde_json::from_value(init_value).context("failed to parse InitializeParams")?;

    let capabilities = server_capabilities(&params);
    connection.initialize_finish(init_id, serde_json::json!({ "capabilities": capabilities }))?;

    let workspaces = Workspaces::from_params(&params);

    // File watchers are dynamic-registration only, so register the
    // `.lattice.toml` watcher now — after `initialized` — when the client
    // advertises support. A client without it degrades to startup-only config
    // (decision 017); Lattice never runs its own watcher.
    if params.supports_watched_files_dynamic_registration() {
        register_watched_files(connection)?;
    }

    main_loop(connection, workspaces)?;

    Ok(())
}

/// Build the server capabilities to advertise, gating client-dependent surfaces.
///
/// Every static capability is unconditional; `workspace.fileOperations.willRename`
/// is advertised only when the client sends `workspace/willRenameFiles`
/// (decision 020 clause 2), with registration filters that scope the request to
/// markdown files and folders so the client never sends it for an unrelated
/// asset. A client without the capability gets no `fileOperations` block, so an
/// editor rename behaves exactly as before.
fn server_capabilities(params: &lsp::InitializeParams) -> serde_json::Value {
    let mut capabilities = serde_json::json!({
        "textDocumentSync": {
            "openClose": true,
            "change": 1,
            "save": { "includeText": true }
        },
        "documentSymbolProvider": true,
        "workspaceSymbolProvider": true,
        "renameProvider": { "prepareProvider": true },
        "referencesProvider": true,
        "declarationProvider": true,
        "definitionProvider": true,
        "typeDefinitionProvider": true,
        "implementationProvider": true,
        "typeHierarchyProvider": true,
        "callHierarchyProvider": true,
        "documentLinkProvider": {},
        "foldingRangeProvider": true,
        "hoverProvider": true,
        // Diagnostics are push-only by design (decision 022), not by revert.
        // Push (`publishDiagnostics`) is the only transport that proactively
        // covers the *closed* target file — where a backlink diagnostic lands
        // when its source is edited — so it is the right transport for a graph
        // linter, not a fallback. `didOpen` resets the per-URI publish diff
        // because a client's memory of a reopened document is unknowable
        // (`republish_on_open`), which closes the only gap pull papered over.
        // Advertising pull (`diagnosticProvider`) *and* pushing makes
        // spec-compliant clients (e.g. Neovim 0.11) render every diagnostic
        // twice, so pull is not advertised — and any future pull support must
        // be capability-negotiated per session with disjoint open/closed
        // transports, never merely "don't advertise both".
        "documentFormattingProvider": true,
        "completionProvider": {
            // Destination open, path separator, fragment, title quote, and
            // reference/footnote open (ticket integration 14).
            "triggerCharacters": ["(", "/", "#", "\"", "[", "^"]
        },
        // Inline emphasis highlighting (ticket integration 15). One custom
        // token type, `markup`, carrying `bold` / `italic` / `strikethrough`
        // modifiers, so a character covered by overlapping runs (e.g. the
        // `foo` in `***foo***`) gets a single token with both modifiers.
        // Custom legend entries are spec-legal; clients that don't recognize
        // them skip them. The legend index is positional: `tokenType` and the
        // `tokenModifiers` bitmask in each emitted quintuple index into these
        // arrays. `full/delta` is not advertised — re-encoding only the
        // emphasis runs is already cheap, and a delta seam waits on the perf
        // workstream's "what changed" diff (see `semantic_tokens_full`).
        "semanticTokensProvider": {
            "legend": {
                "tokenTypes": [SEMANTIC_TOKEN_TYPE_MARKUP],
                "tokenModifiers": [
                    SEMANTIC_MODIFIER_BOLD,
                    SEMANTIC_MODIFIER_ITALIC,
                    SEMANTIC_MODIFIER_STRIKETHROUGH,
                ]
            },
            "full": true,
            "range": true
        },
        "workspace": {
            "workspaceFolders": {
                "supported": true,
                "changeNotifications": true
            }
        }
    });

    // The move surface (decision 020 clause 2) is advertised only to a client
    // that sends `workspace/willRenameFiles`. Its registration filters scope the
    // request to markdown files and folders, matching the engine's move domain
    // — an asset rename never trips the client into asking. A client without the
    // capability sees no `fileOperations` block, so it moves files blind, exactly
    // as before this ticket.
    if params.supports_will_rename_files()
        && let Some(workspace) = capabilities
            .get_mut("workspace")
            .and_then(serde_json::Value::as_object_mut)
    {
        workspace.insert(
            "fileOperations".to_string(),
            serde_json::json!({
                "willRename": {
                    "filters": [
                        { "scheme": "file", "pattern": { "glob": MD_WATCH_GLOB, "matches": "file" } },
                        { "scheme": "file", "pattern": { "glob": "**/*", "matches": "folder" } }
                    ]
                }
            }),
        );
    }

    capabilities
}

/// Register the watched-file globs with the client.
///
/// Sends a `client/registerCapability` request for
/// `workspace/didChangeWatchedFiles` with two watcher globs: the marker
/// [`LATTICE_TOML_WATCH_GLOB`] (ticket server 08) and the document
/// [`MD_WATCH_GLOB`] (ticket server 09). This is the only way to subscribe to
/// file changes — there is no static server-capability field for watchers
/// (decision 017). The request is fire-and-forget: the client's `Response` is
/// discarded by [`main_loop`], so a fixed registration id and request id are
/// sufficient.
fn register_watched_files(connection: &Connection) -> Result<()> {
    let params = serde_json::json!({
        "registrations": [
            {
                "id": WATCHED_FILES_REGISTRATION_ID,
                "method": lsp::method::DID_CHANGE_WATCHED_FILES,
                "registerOptions": {
                    "watchers": [
                        { "globPattern": LATTICE_TOML_WATCH_GLOB },
                        { "globPattern": MD_WATCH_GLOB }
                    ]
                }
            }
        ]
    });
    let req = Request::new(
        RequestId::from(REGISTER_CAPABILITY_REQUEST_ID.to_string()),
        lsp::method::REGISTER_CAPABILITY.to_string(),
        params,
    );
    connection.sender.send(Message::Request(req))?;
    Ok(())
}

/// Convert an LSP URI to a filesystem path.
fn uri_to_path(uri: &str) -> PathBuf {
    PathBuf::from(uri.strip_prefix("file://").unwrap_or(uri))
}

/// Convert a filesystem path to an LSP URI string.
fn path_to_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

/// Whether a URI names a markdown file (a case-insensitive `.md` extension),
/// matching how [`crate::workspace`] discovers indexed files. Used to route a
/// `workspace/didChangeWatchedFiles` event onto the document-sync path
/// (ticket server 09).
fn is_markdown_uri(uri: &str) -> bool {
    Path::new(uri)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
}

/// Main message loop.
fn main_loop(connection: &Connection, mut workspaces: Workspaces) -> Result<()> {
    for msg in &connection.receiver {
        match msg {
            Message::Request(req) => {
                if connection.handle_shutdown(&req)? {
                    return Ok(());
                }
                let id = req.id.clone();
                if let Err(err) = handle_request(connection, &workspaces, req) {
                    tracing::error!("request {id} failed: {err:#}");
                    let resp = Response::new_err(
                        id,
                        lsp_server::ErrorCode::InternalError as i32,
                        format!("{err:#}"),
                    );
                    connection.sender.send(Message::Response(resp))?;
                }
            }
            Message::Notification(notif) => {
                if let Err(err) = handle_notification(connection, &mut workspaces, notif) {
                    tracing::error!("notification failed: {err:#}");
                }
            }
            Message::Response(_) => {}
        }
    }
    Ok(())
}

/// Dispatch a request.
#[allow(
    clippy::too_many_lines,
    reason = "flat dispatch table, not complex logic"
)]
fn handle_request(
    connection: &Connection,
    workspaces: &Workspaces,
    req: lsp_server::Request,
) -> Result<()> {
    let resp = match req.method.as_str() {
        lsp::method::DOCUMENT_SYMBOL => {
            let params: lsp::DocumentSymbolParams = serde_json::from_value(req.params)?;
            let symbols = document_symbols(workspaces, &params.text_document.uri);
            Response::new_ok(req.id, symbols)
        }
        lsp::method::WORKSPACE_SYMBOL => {
            let params: lsp::WorkspaceSymbolParams = serde_json::from_value(req.params)?;
            let symbols = workspace_symbols(workspaces, &params.query);
            Response::new_ok(req.id, symbols)
        }
        lsp::method::PREPARE_RENAME => {
            let params: lsp::TextDocumentPositionParams = serde_json::from_value(req.params)?;
            let result = prepare_rename(workspaces, &params);
            Response::new_ok(req.id, result)
        }
        lsp::method::RENAME => {
            let params: lsp::RenameParams = serde_json::from_value(req.params)?;
            let edit = do_rename(workspaces, &params);
            Response::new_ok(req.id, edit)
        }
        lsp::method::WILL_RENAME_FILES => {
            let params: lsp::RenameFilesParams = serde_json::from_value(req.params)?;
            match will_rename_files(workspaces, &params) {
                // The forced edit set: the client applies it, then performs the
                // rename (decision 020 clause 2).
                Ok(edit) => Response::new_ok(req.id, edit),
                // A refused move (decision 020 clause 6). The message names the
                // fix; the JSON-RPC error aborts the rename client-side, so the
                // file does not move.
                Err(message) => {
                    Response::new_err(req.id, lsp_server::ErrorCode::RequestFailed as i32, message)
                }
            }
        }
        lsp::method::REFERENCES => {
            let params: lsp::ReferenceParams = serde_json::from_value(req.params)?;
            let locations = find_references(workspaces, &params);
            Response::new_ok(req.id, locations)
        }
        lsp::method::DECLARATION => {
            let params: lsp::TextDocumentPositionParams = serde_json::from_value(req.params)?;
            let location = go_to_declaration(workspaces, &params);
            Response::new_ok(req.id, location)
        }
        lsp::method::DEFINITION => {
            let params: lsp::TextDocumentPositionParams = serde_json::from_value(req.params)?;
            let location = go_to_definition(workspaces, &params);
            Response::new_ok(req.id, location)
        }
        lsp::method::TYPE_DEFINITION => {
            let params: lsp::TextDocumentPositionParams = serde_json::from_value(req.params)?;
            let location = go_to_type_definition(workspaces, &params);
            Response::new_ok(req.id, location)
        }
        lsp::method::IMPLEMENTATION => {
            let params: lsp::TextDocumentPositionParams = serde_json::from_value(req.params)?;
            let location = go_to_implementation(workspaces, &params);
            Response::new_ok(req.id, location)
        }
        lsp::method::PREPARE_TYPE_HIERARCHY => {
            let params: lsp::TextDocumentPositionParams = serde_json::from_value(req.params)?;
            let items = prepare_type_hierarchy(workspaces, &params);
            Response::new_ok(req.id, items)
        }
        lsp::method::TYPE_HIERARCHY_SUPERTYPES => {
            let params: lsp::TypeHierarchyParams = serde_json::from_value(req.params)?;
            let items = type_hierarchy_supertypes(workspaces, &params.item);
            Response::new_ok(req.id, items)
        }
        lsp::method::TYPE_HIERARCHY_SUBTYPES => {
            let params: lsp::TypeHierarchyParams = serde_json::from_value(req.params)?;
            let items = type_hierarchy_subtypes(workspaces, &params.item);
            Response::new_ok(req.id, items)
        }
        lsp::method::PREPARE_CALL_HIERARCHY => {
            let params: lsp::TextDocumentPositionParams = serde_json::from_value(req.params)?;
            let items = prepare_call_hierarchy(workspaces, &params);
            Response::new_ok(req.id, items)
        }
        lsp::method::CALL_HIERARCHY_INCOMING => {
            let params: lsp::CallHierarchyParams = serde_json::from_value(req.params)?;
            let calls = call_hierarchy_incoming(workspaces, &params.item);
            Response::new_ok(req.id, calls)
        }
        lsp::method::CALL_HIERARCHY_OUTGOING => {
            let params: lsp::CallHierarchyParams = serde_json::from_value(req.params)?;
            let calls = call_hierarchy_outgoing(workspaces, &params.item);
            Response::new_ok(req.id, calls)
        }
        lsp::method::DOCUMENT_LINK => {
            let params: lsp::DocumentSymbolParams = serde_json::from_value(req.params)?;
            let links = document_links(workspaces, &params.text_document.uri);
            Response::new_ok(req.id, links)
        }
        lsp::method::FOLDING_RANGE => {
            let params: lsp::DocumentSymbolParams = serde_json::from_value(req.params)?;
            let ranges = folding_ranges(workspaces, &params.text_document.uri);
            Response::new_ok(req.id, ranges)
        }
        lsp::method::HOVER => {
            let params: lsp::TextDocumentPositionParams = serde_json::from_value(req.params)?;
            let hover = hover_preview(workspaces, &params);
            Response::new_ok(req.id, hover)
        }
        lsp::method::FORMATTING => {
            let params: lsp::DocumentFormattingParams = serde_json::from_value(req.params)?;
            let edits = format_document(workspaces, &params.text_document.uri);
            Response::new_ok(req.id, edits)
        }
        lsp::method::COMPLETION => {
            // `context` (the trigger char) is ignored — the surface and partial
            // are recovered from the line prefix. The extra field deserializes
            // fine into `TextDocumentPositionParams` (unknown fields skipped).
            let params: lsp::TextDocumentPositionParams = serde_json::from_value(req.params)?;
            let list = completion(workspaces, &params);
            Response::new_ok(req.id, list)
        }
        lsp::method::SEMANTIC_TOKENS_FULL => {
            let params: lsp::SemanticTokensParams = serde_json::from_value(req.params)?;
            let tokens = semantic_tokens_full(workspaces, &params.text_document.uri);
            Response::new_ok(req.id, tokens)
        }
        lsp::method::SEMANTIC_TOKENS_RANGE => {
            let params: lsp::SemanticTokensRangeParams = serde_json::from_value(req.params)?;
            let tokens =
                semantic_tokens_range(workspaces, &params.text_document.uri, &params.range);
            Response::new_ok(req.id, tokens)
        }
        _ => Response::new_err(
            req.id,
            lsp_server::ErrorCode::MethodNotFound as i32,
            format!("method not found: {}", req.method),
        ),
    };
    connection.sender.send(Message::Response(resp))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Document symbols
// ---------------------------------------------------------------------------

/// Maximum length for truncated symbol names.
const SYMBOL_NAME_MAX: usize = 60;

/// Truncate a string to `SYMBOL_NAME_MAX` characters, appending `…` if cut.
fn truncate_name(s: &str) -> String {
    if s.len() <= SYMBOL_NAME_MAX {
        return s.to_string();
    }
    let mut end = SYMBOL_NAME_MAX;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Map an `ElementKind` to its LSP `SymbolKind`, or `None` if the node
/// should not be emitted as a symbol.
fn element_symbol_kind(kind: &ElementKind) -> Option<u32> {
    match kind {
        ElementKind::Heading { .. } => Some(lsp::symbol_kind::CLASS),
        ElementKind::Link { .. } | ElementKind::Import { .. } => Some(lsp::symbol_kind::FUNCTION),
        ElementKind::Image { .. } | ElementKind::Video { .. } | ElementKind::Audio { .. } => {
            Some(lsp::symbol_kind::FILE)
        }
        ElementKind::List { .. }
        | ElementKind::Table { .. }
        | ElementKind::DefinitionList
        | ElementKind::Frontmatter
        | ElementKind::FrontmatterMap { .. } => Some(lsp::symbol_kind::STRUCT),
        ElementKind::CodeBlock | ElementKind::Math => Some(lsp::symbol_kind::OBJECT),
        ElementKind::QuoteBlock
        | ElementKind::Admonition { .. }
        | ElementKind::Details
        | ElementKind::Container => Some(lsp::symbol_kind::MODULE),
        ElementKind::FootnoteDef { .. } => Some(lsp::symbol_kind::CONSTANT),
        ElementKind::FormControl => Some(lsp::symbol_kind::EVENT),
        ElementKind::FrontmatterKey { .. } => Some(lsp::symbol_kind::FIELD),
        // Not emitted: leaf content nodes, structural internals, and thematic
        // breaks (`---`/`***`/`___`) — they are visual separators, not outline
        // entries, and only clutter the symbol list.
        ElementKind::Rules
        | ElementKind::Document
        | ElementKind::Paragraph
        | ElementKind::HtmlBlock
        | ElementKind::InlineCode
        | ElementKind::InlineMath
        | ElementKind::InlineHtml
        | ElementKind::Strong
        | ElementKind::Emphasis
        | ElementKind::Strikethrough
        | ElementKind::FootnoteRef { .. }
        | ElementKind::ReferenceDef { .. }
        | ElementKind::DetailsSummary
        | ElementKind::ListItem { .. }
        | ElementKind::TableRow { .. }
        | ElementKind::TableCell
        | ElementKind::DefinitionTerm
        | ElementKind::DefinitionDesc => None,
    }
}

/// Whether an element is a scope boundary (headings inside it do not
/// participate in the document's heading hierarchy).
fn is_scope_boundary(kind: &ElementKind) -> bool {
    matches!(
        kind,
        ElementKind::QuoteBlock
            | ElementKind::Admonition { .. }
            | ElementKind::Details
            | ElementKind::Container
    )
}

/// Generate the symbol name and optional detail for a tree node.
#[allow(
    clippy::too_many_lines,
    reason = "single match over all ElementKind variants"
)]
fn symbol_name(tree: &Tree, node_id: NodeId) -> (String, Option<String>) {
    let node = tree.node(node_id);
    let source = tree.source();
    let raw = &source[node.span.start..node.span.end];

    match &node.kind {
        ElementKind::Heading { level } => {
            let (text, _, _) = tree.heading_content(node_id);
            (format!("H{level}: {text}"), None)
        }
        ElementKind::Link { url, title } => {
            let predicate = if title.is_empty() {
                "references"
            } else {
                title
            };
            let name = format!("Link: {predicate}({url})");
            let display = link_display_text(raw);
            let detail = if display.is_empty() {
                None
            } else {
                Some(display)
            };
            (truncate_name(&name), detail)
        }
        ElementKind::Import { path } => (truncate_name(&format!("Link: import({path})")), None),
        ElementKind::Image { url, .. } => {
            let detail_type = if raw.trim_start().starts_with("<iframe") {
                "iframe"
            } else {
                "image"
            };
            let name = if url.is_empty() {
                format!("File: {detail_type}")
            } else {
                format!("File: {url}")
            };
            (truncate_name(&name), Some(detail_type.to_string()))
        }
        ElementKind::Video { url, .. } => {
            let name = if url.is_empty() {
                "File: video".to_string()
            } else {
                format!("File: {url}")
            };
            (truncate_name(&name), Some("video".to_string()))
        }
        ElementKind::Audio { url, .. } => {
            let name = if url.is_empty() {
                "File: audio".to_string()
            } else {
                format!("File: {url}")
            };
            (truncate_name(&name), Some("audio".to_string()))
        }
        ElementKind::CodeBlock => {
            let lang = code_block_language(raw);
            let title = code_block_title(raw);
            let name = lang.map_or_else(|| "CodeBlock".to_string(), |l| format!("CodeBlock: {l}"));
            (name, title)
        }
        ElementKind::Math => ("Math".to_string(), None),
        ElementKind::Table { .. } => {
            let data_rows = node
                .children
                .iter()
                .filter(|&&c| matches!(tree.node(c).kind, ElementKind::TableRow { header: false }))
                .count();
            ("Table".to_string(), Some(data_rows.to_string()))
        }
        ElementKind::DefinitionList => {
            let term_count = node
                .children
                .iter()
                .filter(|&&c| matches!(tree.node(c).kind, ElementKind::DefinitionTerm))
                .count();
            ("Definitions".to_string(), Some(term_count.to_string()))
        }
        ElementKind::List { ordered, .. } => {
            let item_count = node
                .children
                .iter()
                .filter(|&&c| matches!(tree.node(c).kind, ElementKind::ListItem { .. }))
                .count();
            let name = if *ordered { "Ordered List" } else { "List" };
            (name.to_string(), Some(item_count.to_string()))
        }
        ElementKind::QuoteBlock => ("Blockquote".to_string(), None),
        ElementKind::Admonition { kind } => (format!("Admonition: {kind}"), None),
        ElementKind::Details => {
            let text = details_summary_text(tree, node_id);
            if text.is_empty() {
                ("Details".to_string(), None)
            } else {
                (format!("Details: {}", truncate_name(&text)), None)
            }
        }
        ElementKind::FootnoteDef { label } => (format!("Footnote: [^{label}]"), None),
        ElementKind::Rules => ("Break".to_string(), None),
        ElementKind::Container => {
            let tag = container_tag_name(raw);
            (format!("Container: {tag}"), None)
        }
        ElementKind::FormControl => {
            let tag = container_tag_name(raw);
            (format!("Form: {tag}"), None)
        }
        ElementKind::Frontmatter => {
            let syntax_label = match node.syntax {
                Syntax::Yaml => "YAML",
                Syntax::Toml => "TOML",
                Syntax::Json => "JSON",
                Syntax::Html => "HTML",
                Syntax::Markdown => "Markdown",
            };
            let key_count = node
                .children
                .iter()
                .filter(|&&c| {
                    matches!(
                        tree.node(c).kind,
                        ElementKind::FrontmatterKey { .. } | ElementKind::FrontmatterMap { .. }
                    )
                })
                .count();
            let detail = if key_count > 0 {
                Some(key_count.to_string())
            } else {
                None
            };
            (format!("Frontmatter: {syntax_label}"), detail)
        }
        ElementKind::FrontmatterMap { key } => {
            let child_count = node.children.len();
            let detail = if child_count > 0 {
                Some(child_count.to_string())
            } else {
                None
            };
            (key.clone(), detail)
        }
        ElementKind::FrontmatterKey { key, .. } => {
            let detail = frontmatter_key_detail(tree, node_id);
            (format!("Field: {key}"), detail)
        }
        _ => (String::new(), None),
    }
}

/// Compute detail for a `FrontmatterKey` node.
///
/// If the key has a non-zero leaf count (sequence items), returns the count
/// as detail. This covers both block sequences and flow sequences.
fn frontmatter_key_detail(tree: &Tree, node_id: NodeId) -> Option<String> {
    let node = tree.node(node_id);

    // Only show detail when the parent is a FrontmatterMap (nested key).
    let parent_id = node.parent?;
    let parent = tree.node(parent_id);
    if !matches!(parent.kind, ElementKind::FrontmatterMap { .. }) {
        return None;
    }

    if let ElementKind::FrontmatterKey { leaf_count, .. } = &node.kind
        && *leaf_count > 0
    {
        return Some(leaf_count.to_string());
    }
    None
}

/// Extract the display text from a markdown link like `[text](url)`.
fn link_display_text(raw: &str) -> String {
    if raw.starts_with('[') {
        if let Some(end) = raw.find("](") {
            return raw[1..end].trim().to_string();
        }
        if let Some(end) = raw.find("][") {
            return raw[1..end].trim().to_string();
        }
        if raw.ends_with(']') && !raw.contains("](") {
            return raw[1..raw.len() - 1].trim().to_string();
        }
    }
    // HTML <a> tag: extract inner text
    if let Some(text) = raw
        .find('>')
        .and_then(|start| {
            raw.rfind("</")
                .filter(|&end| end > start)
                .map(|end| (start, end))
        })
        .map(|(s, e)| raw[s + 1..e].trim())
    {
        return text.to_string();
    }
    String::new()
}

/// Extract the language tag from a fenced code block.
fn code_block_language(raw: &str) -> Option<String> {
    let trimmed = first_line(raw).trim();
    // Fenced: ```lang or ~~~lang
    if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
        let fence_char = &trimmed[..1];
        let after_fence = trimmed.trim_start_matches(fence_char.chars().next().unwrap_or('`'));
        let lang = after_fence.trim();
        if lang.is_empty() {
            return None;
        }
        // Strip info string after first space
        let lang = lang.split_whitespace().next().unwrap_or(lang);
        return Some(lang.to_string());
    }
    // Block math
    if trimmed.starts_with("$$") {
        return Some("math".to_string());
    }
    None
}

/// Extract the title (info string after the language) from a fenced code block.
fn code_block_title(raw: &str) -> Option<String> {
    let trimmed = first_line(raw).trim();
    if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
        let fence_char = trimmed.chars().next().unwrap_or('`');
        let after_fence = trimmed.trim_start_matches(fence_char);
        let info = after_fence.trim();
        // Split into language and rest of info string
        let mut parts = info.splitn(2, char::is_whitespace);
        let _lang = parts.next();
        if let Some(rest) = parts.next() {
            let rest = rest.trim();
            if !rest.is_empty() {
                return Some(rest.to_string());
            }
        }
    }
    None
}

/// Extract the `<summary>` text from a `<details>` node.
fn details_summary_text(tree: &Tree, details_id: NodeId) -> String {
    let details = tree.node(details_id);
    let source = tree.source();
    for &child_id in &details.children {
        let child = tree.node(child_id);
        if matches!(child.kind, ElementKind::DetailsSummary) {
            let text = &source[child.span.start..child.span.end];
            // Strip <summary> tags — the span may extend past </summary>.
            let inner = text.trim().strip_prefix("<summary>").unwrap_or(text);
            return inner.find("</summary>").map_or_else(
                || inner.trim().to_string(),
                |end| inner[..end].trim().to_string(),
            );
        }
    }
    String::new()
}

/// Extract the tag name from a generic container's opening tag.
fn container_tag_name(raw: &str) -> String {
    let trimmed = first_line(raw).trim();
    if let Some(after) = trimmed.strip_prefix('<') {
        let end = after
            .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
            .unwrap_or(after.len());
        return after[..end].to_lowercase();
    }
    "container".to_string()
}

/// Extract the first meaningful text from a list item.
fn list_item_text(tree: &Tree, item_id: NodeId) -> String {
    let node = tree.node(item_id);
    let source = tree.source();
    let raw = &source[node.span.start..node.span.end];

    let trimmed = first_line(raw).trim_start();

    // Strip list marker and optional task checkbox
    let text = if trimmed.starts_with("- [")
        || trimmed.starts_with("* [")
        || trimmed.starts_with("+ [")
    {
        let after_marker = &trimmed[2..];
        after_marker
            .strip_prefix("[x] ")
            .or_else(|| after_marker.strip_prefix("[X] "))
            .or_else(|| after_marker.strip_prefix("[ ] "))
            .unwrap_or(after_marker)
    } else if trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("+ ") {
        &trimmed[2..]
    } else {
        // Ordered: strip digits and `. ` or `) `
        let digit_end = trimmed.find(|c: char| !c.is_ascii_digit()).unwrap_or(0);
        if digit_end > 0
            && (trimmed[digit_end..].starts_with(". ") || trimmed[digit_end..].starts_with(") "))
        {
            &trimmed[digit_end + 2..]
        } else {
            trimmed
        }
    };

    text.trim().to_string()
}

/// Build a span-to-line range for a node.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn node_range(tree: &Tree, node_id: NodeId) -> lsp::Range {
    let node = tree.node(node_id);
    let source = tree.source();
    let start_line = (crate::block::byte_offset_to_line(source, node.span.start) - 1) as u32;
    let end_line = (crate::block::byte_offset_to_line(source, node.span.end) - 1) as u32;
    lsp::Range {
        start: lsp::Position {
            line: start_line,
            character: 0,
        },
        end: lsp::Position {
            line: end_line,
            character: 0,
        },
    }
}

/// Build document symbols for a file by walking the tree.
fn document_symbols(workspaces: &Workspaces, uri: &str) -> Option<Vec<lsp::DocumentSymbol>> {
    let (workspace, rel_path) = workspaces.resolve_document(uri)?;
    let file_data = workspace.file(&rel_path)?;
    let tree = &file_data.tree;
    let root = 0; // Document root is always node 0
    let children = tree.node(root).children.clone();
    Some(build_symbol_tree(tree, &children, false))
}

/// A tagged symbol for the nesting pass. Headings carry their level
/// so the nesting algorithm can build the correct hierarchy.
struct TaggedSymbol {
    /// Heading level (1–6), or 0 for non-heading symbols.
    level: u8,
    /// The LSP symbol.
    symbol: lsp::DocumentSymbol,
}

/// Recursively build the symbol tree from a list of child node IDs.
///
/// `inside_scope` is true when we're inside a scope boundary (block quote,
/// details). Headings inside scopes are emitted as flat symbols, not
/// participating in the heading hierarchy.
fn build_symbol_tree(
    tree: &Tree,
    children: &[NodeId],
    inside_scope: bool,
) -> Vec<lsp::DocumentSymbol> {
    let mut tagged: Vec<TaggedSymbol> = Vec::new();

    for &node_id in children {
        let node = tree.node(node_id);

        // Paragraphs: float links up.
        if matches!(node.kind, ElementKind::Paragraph) {
            for sym in collect_floated_links(tree, node_id) {
                tagged.push(TaggedSymbol {
                    level: 0,
                    symbol: sym,
                });
            }
            continue;
        }

        let Some(kind) = element_symbol_kind(&node.kind) else {
            continue;
        };

        let heading_level = match &node.kind {
            ElementKind::Heading { level } => *level,
            _ => 0,
        };

        let (name, detail) = symbol_name(tree, node_id);
        let range = node_range(tree, node_id);

        // Build children based on element type.
        let sym_children = match &node.kind {
            // Tables: emit Field children from header row cells only.
            ElementKind::Table { .. } => {
                let fields = build_table_field_children(tree, node_id);
                if fields.is_empty() {
                    None
                } else {
                    Some(fields)
                }
            }
            // Lists: emit nested sub-list children only.
            ElementKind::List { .. } => {
                let nested = build_nested_list_children(tree, node_id);
                if nested.is_empty() {
                    None
                } else {
                    Some(nested)
                }
            }
            // Definition lists: emit Field children from terms.
            ElementKind::DefinitionList => {
                let fields = build_definition_list_children(tree, node_id);
                if fields.is_empty() {
                    None
                } else {
                    Some(fields)
                }
            }
            // Opaque content blocks and leaf elements: no children.
            ElementKind::CodeBlock
            | ElementKind::Math
            | ElementKind::Link { .. }
            | ElementKind::Image { .. }
            | ElementKind::Video { .. }
            | ElementKind::Audio { .. }
            | ElementKind::Import { .. }
            | ElementKind::FrontmatterKey { .. } => None,
            // Scope containers: recurse normally.
            _ => {
                let node_children = &tree.node(node_id).children;
                if node_children.is_empty() {
                    None
                } else {
                    let in_scope = inside_scope || is_scope_boundary(&node.kind);
                    let child_syms = build_symbol_tree(tree, node_children, in_scope);
                    if child_syms.is_empty() {
                        None
                    } else {
                        Some(child_syms)
                    }
                }
            }
        };

        tagged.push(TaggedSymbol {
            level: heading_level,
            symbol: lsp::DocumentSymbol {
                name,
                detail,
                kind,
                range,
                selection_range: range,
                children: sym_children,
            },
        });
    }

    // If we're inside a scope boundary, headings are flat — no nesting.
    if inside_scope {
        return tagged.into_iter().map(|t| t.symbol).collect();
    }

    // Outside scopes, nest headings by level (H2 under H1, etc.)
    // and attach non-heading symbols to their preceding heading.
    nest_by_heading_level(tagged)
}

/// Nest symbols by heading level: H2 under H1, H3 under H2, etc.
/// Non-heading symbols are attached as children of their preceding heading.
fn nest_by_heading_level(tagged: Vec<TaggedSymbol>) -> Vec<lsp::DocumentSymbol> {
    if !tagged.iter().any(|t| t.level > 0) {
        return tagged.into_iter().map(|t| t.symbol).collect();
    }

    let mut stack: Vec<(u8, lsp::DocumentSymbol)> = Vec::new();
    let mut result: Vec<lsp::DocumentSymbol> = Vec::new();

    for item in tagged {
        if item.level > 0 {
            // Pop symbols at same or deeper level — they're complete.
            while stack.last().is_some_and(|(lvl, _)| *lvl >= item.level) {
                let Some((_, finished)) = stack.pop() else {
                    break;
                };
                if let Some((_, parent)) = stack.last_mut() {
                    parent.children.get_or_insert_with(Vec::new).push(finished);
                } else {
                    result.push(finished);
                }
            }
            stack.push((item.level, item.symbol));
        } else {
            // Non-heading: attach to last heading on stack, else top-level.
            if let Some((_, parent)) = stack.last_mut() {
                parent
                    .children
                    .get_or_insert_with(Vec::new)
                    .push(item.symbol);
            } else {
                result.push(item.symbol);
            }
        }
    }

    // Flush remaining stack.
    while let Some((_, finished)) = stack.pop() {
        if let Some((_, parent)) = stack.last_mut() {
            parent.children.get_or_insert_with(Vec::new).push(finished);
        } else {
            result.push(finished);
        }
    }

    result
}

/// Collect link symbols from a paragraph node (float-up).
fn collect_floated_links(tree: &Tree, para_id: NodeId) -> Vec<lsp::DocumentSymbol> {
    let node = tree.node(para_id);
    let mut links = Vec::new();
    for &child_id in &node.children {
        let child = tree.node(child_id);
        if element_symbol_kind(&child.kind).is_some()
            && matches!(
                child.kind,
                ElementKind::Link { .. }
                    | ElementKind::Image { .. }
                    | ElementKind::Video { .. }
                    | ElementKind::Audio { .. }
                    | ElementKind::Import { .. }
            )
        {
            let kind = element_symbol_kind(&child.kind).unwrap_or(lsp::symbol_kind::FUNCTION);
            let (name, detail) = symbol_name(tree, child_id);
            let range = node_range(tree, child_id);
            links.push(lsp::DocumentSymbol {
                name,
                detail,
                kind,
                range,
                selection_range: range,
                children: None,
            });
        }
    }
    links
}

/// Build `Field` children from a table's header row cells.
fn build_table_field_children(tree: &Tree, table_id: NodeId) -> Vec<lsp::DocumentSymbol> {
    let table = tree.node(table_id);
    let source = tree.source();
    let mut fields = Vec::new();

    for &child_id in &table.children {
        let child = tree.node(child_id);
        if matches!(child.kind, ElementKind::TableRow { header: true }) {
            for &cell_id in &child.children {
                let cell = tree.node(cell_id);
                let text = source[cell.span.start..cell.span.end]
                    .trim()
                    .trim_matches('|')
                    .trim();
                let name = format!("Field: {}", truncate_name(text));
                let range = node_range(tree, cell_id);
                fields.push(lsp::DocumentSymbol {
                    name,
                    detail: None,
                    kind: lsp::symbol_kind::FIELD,
                    range,
                    selection_range: range,
                    children: None,
                });
            }
            break;
        }
    }
    fields
}

/// Build `Field` children from a definition list's term nodes.
fn build_definition_list_children(tree: &Tree, dl_id: NodeId) -> Vec<lsp::DocumentSymbol> {
    let dl = tree.node(dl_id);
    let source = tree.source();
    let mut fields = Vec::new();

    for &child_id in &dl.children {
        let child = tree.node(child_id);
        if matches!(child.kind, ElementKind::DefinitionTerm) {
            let text = source[child.span.start..child.span.end].trim();
            // Strip <dt> and </dt> tags if present (HTML syntax).
            let text = text
                .strip_prefix("<dt>")
                .unwrap_or(text)
                .strip_suffix("</dt>")
                .unwrap_or(text)
                .trim();
            let name = format!("Field: {}", truncate_name(text));
            let range = node_range(tree, child_id);
            fields.push(lsp::DocumentSymbol {
                name,
                detail: None,
                kind: lsp::symbol_kind::FIELD,
                range,
                selection_range: range,
                children: None,
            });
        }
    }
    fields
}

/// Build `Struct` children for nested sub-lists within a list.
///
/// For each `ListItem` that contains a child `List`, emits a `Struct`
/// symbol named by the parent item's text. Items without sub-lists
/// are not emitted.
fn build_nested_list_children(tree: &Tree, list_id: NodeId) -> Vec<lsp::DocumentSymbol> {
    let list = tree.node(list_id);
    let mut children = Vec::new();

    for &item_id in &list.children {
        let item = tree.node(item_id);
        if !matches!(item.kind, ElementKind::ListItem { .. }) {
            continue;
        }

        for &sub_id in &item.children {
            let sub = tree.node(sub_id);
            if let ElementKind::List { ordered, .. } = &sub.kind {
                let item_text = list_item_text(tree, item_id);
                let prefix = if *ordered { "Ordered List" } else { "List" };
                let name = if item_text.is_empty() {
                    prefix.to_string()
                } else {
                    format!("{prefix}: {}", truncate_name(&item_text))
                };

                let sub_item_count = sub
                    .children
                    .iter()
                    .filter(|&&c| matches!(tree.node(c).kind, ElementKind::ListItem { .. }))
                    .count();

                let range = node_range(tree, sub_id);

                // Recurse for deeper nesting.
                let nested = build_nested_list_children(tree, sub_id);
                let nested_children = if nested.is_empty() {
                    None
                } else {
                    Some(nested)
                };

                children.push(lsp::DocumentSymbol {
                    name,
                    detail: Some(sub_item_count.to_string()),
                    kind: lsp::symbol_kind::STRUCT,
                    range,
                    selection_range: range,
                    children: nested_children,
                });
            }
        }
    }
    children
}

// ---------------------------------------------------------------------------
// Workspace symbols
// ---------------------------------------------------------------------------

/// Search symbols across all workspaces, filtered by query.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn workspace_symbols(workspaces: &Workspaces, query: &str) -> Vec<lsp::SymbolInformation> {
    let query_lower = query.to_lowercase();
    let mut symbols = Vec::new();

    // Enumerate rooted documents only — a rootless single-file document (issue
    // 051) is deliberately absent from workspace symbols, as it was when the
    // graph tier enumerated `inner` alone. Each document is visited once under
    // its deepest root, so overlapping folders do not double-list it.
    for (abs, doc) in &workspaces.documents {
        let Some(root) = doc.primary_root.as_deref() else {
            continue;
        };
        let rel_path = abs.strip_prefix(root).unwrap_or(abs);
        collect_workspace_symbols(&doc.data.tree, &query_lower, root, rel_path, &mut symbols);
    }

    symbols
}

/// Collect flat workspace symbols from a tree, filtered by query.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn collect_workspace_symbols(
    tree: &Tree,
    query_lower: &str,
    root: &Path,
    rel_path: &Path,
    out: &mut Vec<lsp::SymbolInformation>,
) {
    let abs_path = root.join(rel_path);
    let uri = path_to_uri(&abs_path);
    let source = tree.source();

    for (node_id, node) in tree.nodes().iter().enumerate() {
        let Some(kind) = element_symbol_kind(&node.kind) else {
            continue;
        };

        // Skip nested lists — only top-level data containers in workspace.
        if matches!(node.kind, ElementKind::List { .. })
            && node
                .parent
                .is_some_and(|p| matches!(tree.node(p).kind, ElementKind::ListItem { .. }))
        {
            continue;
        }

        // Skip frontmatter children — only the top-level container in workspace.
        if matches!(
            node.kind,
            ElementKind::FrontmatterKey { .. } | ElementKind::FrontmatterMap { .. }
        ) {
            continue;
        }

        let (name, _) = symbol_name(tree, node_id);
        if name.is_empty() {
            continue;
        }

        if !query_lower.is_empty() && !name.to_lowercase().contains(query_lower) {
            continue;
        }

        let start_line = (crate::block::byte_offset_to_line(source, node.span.start) - 1) as u32;

        out.push(lsp::SymbolInformation {
            name,
            kind,
            location: lsp::Location {
                uri: uri.clone(),
                range: lsp::Range {
                    start: lsp::Position {
                        line: start_line,
                        character: 0,
                    },
                    end: lsp::Position {
                        line: start_line,
                        character: 0,
                    },
                },
            },
            container_name: Some(rel_path.display().to_string()),
        });
    }
}

// ---------------------------------------------------------------------------
// prepareRename / rename (ticket 04)
// ---------------------------------------------------------------------------

/// Find the heading at a cursor position, returning its text range.
///
/// Uses the tree's `text_span` to compute the exact text range, supporting
/// ATX, setext, and HTML headings without prefix assumptions.
fn prepare_rename(
    workspaces: &Workspaces,
    params: &lsp::TextDocumentPositionParams,
) -> Option<lsp::Range> {
    let (workspace, rel_path) = workspaces.resolve_document(&params.text_document.uri)?;
    let file_data = workspace.file(&rel_path)?;
    let headings = file_data.tree.headings();
    let heading = heading_at_line(&headings, params.position.line)?;

    Some(span_to_lsp_range(
        file_data.tree.source(),
        &file_data.line_index,
        &heading.text_span,
    ))
}

/// Rename a heading's text.
///
/// Uses the tree's `text_span` for the edit range, supporting ATX, setext,
/// and HTML headings.
fn do_rename(workspaces: &Workspaces, params: &lsp::RenameParams) -> Option<lsp::WorkspaceEdit> {
    let (workspace, rel_path) = workspaces.resolve_document(&params.text_document.uri)?;
    let file_data = workspace.file(&rel_path)?;
    let headings = file_data.tree.headings();
    let heading = heading_at_line(&headings, params.position.line)?;

    let range = span_to_lsp_range(
        file_data.tree.source(),
        &file_data.line_index,
        &heading.text_span,
    );

    let mut changes = std::collections::HashMap::new();
    changes.insert(
        params.text_document.uri.clone(),
        vec![lsp::TextEdit {
            range,
            new_text: params.new_name.clone(),
        }],
    );

    Some(lsp::WorkspaceEdit {
        changes: Some(changes),
    })
}

// ---------------------------------------------------------------------------
// Editor move surface — workspace/willRenameFiles (ticket mv/02, decision 020)
// ---------------------------------------------------------------------------

/// Answer a `workspace/willRenameFiles` request with the move engine's forced
/// edit set (decision 020 clause 2).
///
/// Each `(oldUri, newUri)` is translated into a [`crate::mv::compute_move_edits`]
/// call over the source's covering scope; every file's edits are converted to
/// LSP ranges (through that file's cached [`LineIndex`]) and merged into one
/// [`lsp::WorkspaceEdit`]. The client applies it to buffers, then performs the
/// rename — so the buffer-wins rule (decision 017) holds with no new machinery,
/// and the post-rename re-keying rides `workspace/didRenameFiles` plus the
/// watched-file create/delete channel.
///
/// A source outside every scope contributes no edits (there is no edit set to
/// compute — a plain rename already does everything Lattice could; decision 020
/// clause 6), so the rename proceeds unimpeded. Any other refusal
/// (cross-marker, existing destination, markdown-ness flip, …) short-circuits
/// the whole batch: `Err(message)` carries the alias-steering / fix-naming
/// text, which the caller returns as a JSON-RPC error so the client aborts the
/// rename and no file moves.
///
/// # Errors
///
/// Returns the refusal message (a [`crate::mv::MoveError`] `Display`) for the
/// first rename the engine refuses.
fn will_rename_files(
    workspaces: &Workspaces,
    params: &lsp::RenameFilesParams,
) -> Result<lsp::WorkspaceEdit, String> {
    let mut changes: HashMap<String, Vec<lsp::TextEdit>> = HashMap::new();

    for rename in &params.files {
        let old_abs = uri_to_path(&rename.old_uri);
        let new_abs = uri_to_path(&rename.new_uri);

        // Without a covering scope there is no keyspace to compute an edit set
        // over — the source is outside every graph. Contribute nothing and let
        // the client's rename proceed (decision 020 clause 6); refusing here
        // would block a legitimate rename of a file Lattice does not manage.
        let Some(root) = workspaces.deepest_root_for(&old_abs) else {
            continue;
        };

        let view = workspaces.root_view(&root);
        let fs_exists = |p: &Path| p.is_file() || p.is_dir();
        let edits = crate::mv::compute_move_edits(&view, &old_abs, &new_abs, &fs_exists)
            .map_err(|e| e.to_string())?;

        merge_move_edits(workspaces, &edits, &mut changes);
    }

    Ok(lsp::WorkspaceEdit {
        changes: Some(changes),
    })
}

/// Convert one move's per-file byte-span edits into LSP `TextEdit`s and merge
/// them into `changes` (keyed by document URI).
///
/// Each edited file's source and cached [`LineIndex`] come from the flat
/// document store, so the byte→UTF-16 conversion matches every other publish.
/// A file the store does not hold is skipped — the engine only enumerates files
/// in the view, so this is defensive.
fn merge_move_edits(
    workspaces: &Workspaces,
    edits: &crate::mv::MoveEdits,
    changes: &mut HashMap<String, Vec<lsp::TextEdit>>,
) {
    for (abs_path, file_edits) in &edits.edits {
        let Some(doc) = workspaces.documents.get(abs_path) else {
            continue;
        };
        let source = doc.data.tree.source();
        let index = &doc.data.line_index;
        let uri = path_to_uri(abs_path);
        let entry = changes.entry(uri).or_default();
        for edit in file_edits {
            entry.push(lsp::TextEdit {
                range: span_to_lsp_range(source, index, &edit.span),
                new_text: edit.new_text.clone(),
            });
        }
        // A file touched by more than one rename in the batch accumulates edits
        // out of order; sort so the client applies them deterministically.
        entry.sort_by_key(|e| (e.range.start.line, e.range.start.character));
    }
}

/// Find the heading whose line matches the cursor's 0-based line number.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn heading_at_line(headings: &[Heading], lsp_line: u32) -> Option<&Heading> {
    headings
        .iter()
        .find(|h| h.line.saturating_sub(1) as u32 == lsp_line)
}

// ---------------------------------------------------------------------------
// Find references (ticket 05)
// ---------------------------------------------------------------------------

/// Find all documents that link to the file or heading at the cursor,
/// or all call sites of a reference definition.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn find_references(workspaces: &Workspaces, params: &lsp::ReferenceParams) -> Vec<lsp::Location> {
    let Some((workspace, rel_path)) = workspaces.resolve_document(&params.text_document.uri) else {
        return Vec::new();
    };
    let Some(file_data) = workspace.file(&rel_path) else {
        return Vec::new();
    };

    // Check if cursor is on a reference definition — find all call sites.
    let offset = lsp_position_to_byte_offset(file_data.tree.source(), params.position);
    if let Some(label) = ref_def_label_at_offset(&file_data.tree, offset) {
        return find_ref_def_call_sites(workspaces, &params.text_document.uri, &label);
    }

    // Determine if the cursor is on a heading (to filter by fragment).
    let file_headings = file_data.tree.headings();
    let target_heading = heading_at_line(&file_headings, params.position.line);

    let mut locations = Vec::new();

    // Scan every rooted document's links for edges to the cursor's document,
    // matching in absolute space: a source's link target resolved against the
    // source's own root equals the cursor document's absolute path exactly when
    // it physically points there (ticket server 11).
    //
    // The match is restricted to the cursor's **own scope** (decision 019):
    // `find_references` is a graph-edge query — "who links here" — and scopes are
    // disjoint graphs, so a physical `../` reference from a foreign scope is a
    // clause-3 defect, not an edge, and must not surface as a reference. (Plain
    // navigation — go-to-definition, outgoing calls — still follows a link
    // physically; only the reverse graph queries honor the partition.)
    let cursor_abs = uri_to_path(&params.text_document.uri);
    let cursor_root = workspaces
        .documents
        .get(&cursor_abs)
        .and_then(|doc| doc.primary_root.clone());
    for (abs, doc) in &workspaces.documents {
        let Some(root) = doc.primary_root.as_deref() else {
            continue;
        };
        if Some(root) != cursor_root.as_deref() {
            continue;
        }
        let links = doc.data.tree.links(abs);
        for link in &links {
            let LinkKind::IntraProject {
                target, fragment, ..
            } = &link.kind
            else {
                continue;
            };
            if root.join(target) != cursor_abs {
                continue;
            }
            // If cursor is on a heading, only match links with a fragment to that heading.
            if let Some(heading) = target_heading {
                let Some(frag) = fragment else {
                    continue;
                };
                if !heading_matches_fragment(heading, frag) {
                    continue;
                }
            }
            let line = link.line.saturating_sub(1) as u32;
            locations.push(lsp::Location {
                uri: path_to_uri(abs),
                range: lsp::Range {
                    start: lsp::Position { line, character: 0 },
                    end: lsp::Position { line, character: 0 },
                },
            });
        }
    }

    locations
}

/// Find all reference-style link call sites that use a given label.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn find_ref_def_call_sites(workspaces: &Workspaces, uri: &str, label: &str) -> Vec<lsp::Location> {
    let Some((workspace, rel_path)) = workspaces.resolve_document(uri) else {
        return Vec::new();
    };
    let Some(file_data) = workspace.file(&rel_path) else {
        return Vec::new();
    };
    let root = workspace.root();
    let source = file_data.tree.source();
    let mut locations = Vec::new();

    for node in file_data.tree.nodes() {
        if !matches!(node.kind, ElementKind::Link { .. }) {
            continue;
        }
        if let Some(ref_label) = link_ref_label(source, &node.span)
            && ref_label == label
        {
            let line = crate::block::byte_offset_to_line(source, node.span.start);
            let line_lsp = line.saturating_sub(1) as u32;
            locations.push(lsp::Location {
                uri: path_to_uri(&root.join(&rel_path)),
                range: lsp::Range {
                    start: lsp::Position {
                        line: line_lsp,
                        character: 0,
                    },
                    end: lsp::Position {
                        line: line_lsp,
                        character: 0,
                    },
                },
            });
        }
    }

    locations
}

/// Check whether a fragment matches any of a heading's anchor IDs.
fn heading_matches_fragment(heading: &Heading, fragment: &str) -> bool {
    match &heading.id {
        HeadingId::Explicit(id) => id == fragment,
        HeadingId::Computed {
            github,
            gitlab,
            vscode,
        } => fragment == github || fragment == gitlab || fragment == vscode,
    }
}

// ---------------------------------------------------------------------------
// Navigation — go to declaration / definition / type definition / implementation
// ---------------------------------------------------------------------------

/// Go to the declaration of a link.
///
/// For reference-style links (`[text][ref]`), goes to the `[ref]: url`
/// definition line. For inline links, falls through to the target document.
fn go_to_declaration(
    workspaces: &Workspaces,
    params: &lsp::TextDocumentPositionParams,
) -> Option<lsp::Location> {
    let (workspace, rel_path) = workspaces.resolve_document(&params.text_document.uri)?;
    let file_data = workspace.file(&rel_path)?;
    let source = file_data.tree.source();
    let offset = lsp_position_to_byte_offset(source, params.position);

    let (_, node) = file_data.tree.find_link_at_offset(offset)?;

    // If it's a reference-style link, go to the ref def.
    if let Some(label) = link_ref_label(source, &node.span) {
        let (_, def_node) = file_data.tree.find_ref_def(&label)?;
        return Some(lsp::Location {
            uri: params.text_document.uri.clone(),
            range: span_to_lsp_range(source, &file_data.line_index, &def_node.span),
        });
    }

    // Inline link — fall through to definition (target document).
    go_to_definition(workspaces, params)
}

/// Go to the definition of a link.
///
/// A cross-file or non-markdown link resolves to the target document. A
/// same-document anchor (`[…](#heading)`) resolves the fragment against this
/// file's own headings and goes to the heading line — an in-page anchor's
/// "target document" is itself, so the heading is the meaningful destination
/// (issue 021).
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn go_to_definition(
    workspaces: &Workspaces,
    params: &lsp::TextDocumentPositionParams,
) -> Option<lsp::Location> {
    let (workspace, rel_path) = workspaces.resolve_document(&params.text_document.uri)?;
    let file_data = workspace.file(&rel_path)?;
    let source = file_data.tree.source();
    let offset = lsp_position_to_byte_offset(source, params.position);

    let (_, node) = file_data.tree.find_link_at_offset(offset)?;
    if !matches!(node.kind, ElementKind::Link { .. }) {
        return None;
    }

    let root = workspace.root();
    let link = find_classified_link(&file_data.tree, &root.join(&rel_path), node.span)?;

    match &link.kind {
        LinkKind::IntraProject { target, .. } | LinkKind::NonMarkdown { target } => {
            // `root.join` yields the target's absolute path for either target
            // form: it replaces on an absolute (document-relative) target and
            // appends onto a root-relative remainder.
            Some(lsp::Location {
                uri: path_to_uri(&root.join(target)),
                range: lsp::Range::default(),
            })
        }
        LinkKind::IntraDocument { fragment } => {
            let heading = file_data
                .tree
                .headings()
                .into_iter()
                .find(|h| heading_matches_fragment(h, fragment))?;
            let heading_line = heading.line.saturating_sub(1) as u32;
            Some(lsp::Location {
                uri: params.text_document.uri.clone(),
                range: lsp::Range {
                    start: lsp::Position {
                        line: heading_line,
                        character: 0,
                    },
                    end: lsp::Position {
                        line: heading_line,
                        character: 0,
                    },
                },
            })
        }
        LinkKind::External { .. } => None,
    }
}

/// Go to the type definition of a link.
///
/// For links with a fragment, goes to the heading in the target document.
/// Without a fragment, falls through to definition (the document itself).
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn go_to_type_definition(
    workspaces: &Workspaces,
    params: &lsp::TextDocumentPositionParams,
) -> Option<lsp::Location> {
    let (workspace, rel_path) = workspaces.resolve_document(&params.text_document.uri)?;
    let file_data = workspace.file(&rel_path)?;
    let source = file_data.tree.source();
    let offset = lsp_position_to_byte_offset(source, params.position);

    let (_, node) = file_data.tree.find_link_at_offset(offset)?;

    let root = workspace.root();
    let link = find_classified_link(&file_data.tree, &root.join(&rel_path), node.span)?;

    let LinkKind::IntraProject {
        target, fragment, ..
    } = &link.kind
    else {
        return go_to_definition(workspaces, params);
    };

    let Some(fragment) = fragment.as_deref() else {
        // No fragment — fall through to definition (the document itself).
        return go_to_definition(workspaces, params);
    };

    let target_data = workspace.file(target)?;
    let target_headings = target_data.tree.headings();
    let heading = target_headings
        .iter()
        .find(|h| heading_matches_fragment(h, fragment))?;

    let heading_line = heading.line.saturating_sub(1) as u32;
    Some(lsp::Location {
        uri: path_to_uri(&root.join(target)),
        range: lsp::Range {
            start: lsp::Position {
                line: heading_line,
                character: 0,
            },
            end: lsp::Position {
                line: heading_line,
                character: 0,
            },
        },
    })
}

/// A zero-width LSP location at the start of 0-based `line` in `abs_path`.
fn point_location(abs_path: &Path, line: u32) -> lsp::Location {
    lsp::Location {
        uri: path_to_uri(abs_path),
        range: lsp::Range {
            start: lsp::Position { line, character: 0 },
            end: lsp::Position { line, character: 0 },
        },
    }
}

/// Go to the *implementation* of the predicate edge at the cursor.
///
/// An edge is reconcilable from either end (decision 008), so navigation has two
/// entry points:
///
/// - **Body link** `S --[P]--> T`: jump to the edge's counterpart authored on
///   `T` — a reciprocal forward link `T --[opposite_of(P)]--> S`, or, failing
///   that, the frontmatter backlink entry on `T` keyed by `opposite_of(P)`.
/// - **Frontmatter backlink** entry on `T`: jump to the source link in `S` that
///   derives it — `S --[opposite_of(K)]--> T`, where `K` is the backlink key in
///   *either* direction.
///
/// `textDocument/definition` stays distinct: on a body link it resolves to the
/// target *document* (see [`go_to_definition`]), never the counterpart edge.
fn go_to_implementation(
    workspaces: &Workspaces,
    params: &lsp::TextDocumentPositionParams,
) -> Option<lsp::Location> {
    let (workspace, rel_path) = workspaces.resolve_document(&params.text_document.uri)?;
    let file_data = workspace.file(&rel_path)?;

    implementation_from_body_link(&workspace, &rel_path, file_data, params)
        .or_else(|| implementation_from_backlink(&workspace, &rel_path, file_data, params))
}

/// Body-link entry point for [`go_to_implementation`].
///
/// From a body link `S --[P]--> T` under the cursor, resolve the counterpart of
/// the edge as authored on `T`: a reciprocal forward link
/// `T --[opposite_of(P)]--> S` if one exists, else the frontmatter backlink
/// entry on `T` keyed by `opposite_of(P)` listing `S`. Returns `None` when the
/// cursor is not on an intra-project body link, or the target carries no
/// counterpart for the edge.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn implementation_from_body_link(
    workspace: &WorkspaceView,
    rel_path: &Path,
    file_data: &FileData,
    params: &lsp::TextDocumentPositionParams,
) -> Option<lsp::Location> {
    let source = file_data.tree.source();
    let offset = lsp_position_to_byte_offset(source, params.position);
    let (_, node) = file_data.tree.find_link_at_offset(offset)?;
    let root = workspace.root();
    let cursor_link = find_classified_link(&file_data.tree, &root.join(rel_path), node.span)?;

    let LinkKind::IntraProject {
        target, predicate, ..
    } = &cursor_link.kind
    else {
        return None;
    };

    // The counterpart authored on T carries the opposite predicate. `target` is
    // T's absolute path (a document-relative cursor link resolves root-free), so
    // it doubles as the argument that classifies T's own links root-free.
    let paired = workspace.config().opposite_of(predicate)?;
    let target_data = workspace.file(target)?;

    // Prefer a reciprocal body link T --[opposite_of(P)]--> S. T's link target
    // `t` is root-free; map it onto its stored key and compare to S (`rel_path`).
    let target_links = target_data.tree.links(target);
    let reciprocal = target_links.iter().find(|l| {
        let LinkKind::IntraProject {
            target: t,
            predicate: p,
            ..
        } = &l.kind
        else {
            return false;
        };
        p == paired && workspace.resolve_key(t).is_some_and(|k| k == rel_path)
    });
    if let Some(recip) = reciprocal {
        let line = recip.line.saturating_sub(1) as u32;
        return Some(point_location(&root.join(target), line));
    }

    // Otherwise a frontmatter backlink entry on T keyed by opposite_of(P) and
    // listing S. Backlink paths are file-relative to T, so resolve each against
    // T's directory (`target` is T's absolute path) and map the result onto its
    // stored key before comparing to S.
    let lists_source = target_data
        .frontmatter
        .as_ref()
        .and_then(|fm| fm.backlinks.get(paired))
        .is_some_and(|paths| {
            paths.iter().any(|p| {
                workspace
                    .resolve_key(&validation::resolve_backlink_path(target, p))
                    .is_some_and(|k| k == rel_path)
            })
        });
    if lists_source {
        let line = backlink_key_line(target_data, paired)?;
        return Some(point_location(&root.join(target), line));
    }

    None
}

/// Frontmatter entry point for [`go_to_implementation`].
///
/// When the cursor is on a backlink path like `    - decisions/38.md` in the
/// frontmatter of `T`, navigate to the forward link line in the source document
/// `S` that derives it. The justifying link is always `S --[opposite_of(K)]--> T`
/// regardless of the backlink key `K`'s direction (decision 008), so a key that
/// is a forward label (e.g. `supersedes:`) resolves just as an inverse one does.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn implementation_from_backlink(
    workspace: &WorkspaceView,
    rel_path: &Path,
    file_data: &FileData,
    params: &lsp::TextDocumentPositionParams,
) -> Option<lsp::Location> {
    let fm = file_data.frontmatter.as_ref()?;

    // Check cursor is inside frontmatter.
    let cursor_line_1based = params.position.line as usize + 1;
    if cursor_line_1based < fm.start_line || cursor_line_1based > fm.end_line {
        return None;
    }

    // Extract the backlink path from the cursor line.
    let source = file_data.tree.source();
    let line_text = source_line_at(source, params.position.line);
    let path_text = line_text.trim().strip_prefix("- ")?.trim();
    if path_text.is_empty() {
        return None;
    }

    // Find the backlink key listing this path. Decision 008 lets the key name
    // either direction of a vocabulary pair, so accept any known predicate and
    // skip keys unknown in both directions.
    let config = workspace.config();
    let backlink_key = fm.backlinks.iter().find_map(|(key, paths)| {
        (config.is_known_predicate(key) && paths.iter().any(|p| p == path_text))
            .then_some(key.as_str())
    })?;

    // The justifying source link is S --[opposite_of(K)]--> T.
    let paired_predicate = config.opposite_of(backlink_key)?;

    // Find the source document and the forward link. Backlink paths are
    // file-relative to T, so resolve against T's directory (matching validation)
    // before looking S up in the workspace index.
    let source_path = validation::resolve_backlink_path(rel_path, path_text);
    let source_data = workspace.file(&source_path)?;
    let source_abs = workspace.root().join(&source_path);
    let source_links = source_data.tree.links(&source_abs);

    let forward_link = source_links.iter().find(|l| {
        let LinkKind::IntraProject {
            target, predicate, ..
        } = &l.kind
        else {
            return false;
        };
        // S's link target is root-free; map it onto its stored key to compare
        // to T (`rel_path`).
        predicate == paired_predicate
            && workspace.resolve_key(target).is_some_and(|k| k == rel_path)
    })?;

    let line = forward_link.line.saturating_sub(1) as u32;
    Some(point_location(&workspace.root().join(&source_path), line))
}

/// Line (0-based) of the `backlinks` predicate key `predicate` in `file_data`'s
/// frontmatter, or `None` when the file has no such key.
///
/// Resolves to the predicate key line (e.g. `superseded_by:`), the same anchor
/// backlink diagnostics use, rather than an individual list entry.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn backlink_key_line(file_data: &FileData, predicate: &str) -> Option<u32> {
    let tree = &file_data.tree;
    let backlinks_id = tree.nodes().iter().position(
        |n| matches!(&n.kind, ElementKind::FrontmatterMap { key } if key == "backlinks"),
    )?;
    let key_node = tree.children(backlinks_id).iter().find_map(|&cid| {
        let node = tree.node(cid);
        let (ElementKind::FrontmatterKey { key, .. } | ElementKind::FrontmatterMap { key }) =
            &node.kind
        else {
            return None;
        };
        (key == predicate).then_some(node)
    })?;
    let line = crate::block::byte_offset_to_line(tree.source(), key_node.span.start);
    Some(line.saturating_sub(1) as u32)
}

// ---------------------------------------------------------------------------
// Type hierarchy (ticket 08)
// ---------------------------------------------------------------------------

/// Prepare a type hierarchy item for the heading at the cursor.
fn prepare_type_hierarchy(
    workspaces: &Workspaces,
    params: &lsp::TextDocumentPositionParams,
) -> Option<Vec<lsp::HierarchyItem>> {
    let (workspace, rel_path) = workspaces.resolve_document(&params.text_document.uri)?;
    let file_data = workspace.file(&rel_path)?;
    let headings = file_data.tree.headings();
    let heading = heading_at_line(&headings, params.position.line)?;
    let item = heading_to_hierarchy_item(heading, &workspace.root().join(&rel_path));
    Some(vec![item])
}

/// Return the parent heading (supertype) of a heading.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn type_hierarchy_supertypes(
    workspaces: &Workspaces,
    item: &lsp::HierarchyItem,
) -> Option<Vec<lsp::HierarchyItem>> {
    let (workspace, rel_path) = workspaces.resolve_document(&item.uri)?;
    let file_data = workspace.file(&rel_path)?;
    let abs_path = workspace.root().join(&rel_path);
    let headings = file_data.tree.headings();

    let target_level = hierarchy_item_level(item);
    if target_level <= 1 {
        return Some(Vec::new());
    }

    let target_line = item.selection_range.start.line;
    let parent = headings.iter().rev().find(|h| {
        let h_line = h.line.saturating_sub(1) as u32;
        h_line < target_line && h.level < target_level
    });

    let items = parent
        .map(|h| heading_to_hierarchy_item(h, &abs_path))
        .into_iter()
        .collect();
    Some(items)
}

/// Return the immediate child headings (subtypes) of a heading.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn type_hierarchy_subtypes(
    workspaces: &Workspaces,
    item: &lsp::HierarchyItem,
) -> Option<Vec<lsp::HierarchyItem>> {
    let (workspace, rel_path) = workspaces.resolve_document(&item.uri)?;
    let file_data = workspace.file(&rel_path)?;
    let abs_path = workspace.root().join(&rel_path);
    let headings = file_data.tree.headings();

    let target_level = hierarchy_item_level(item);
    let child_level = target_level + 1;
    let target_line = item.selection_range.start.line;

    let mut children = Vec::new();
    let mut started = false;

    for heading in &headings {
        let h_line = heading.line.saturating_sub(1) as u32;

        if h_line == target_line {
            started = true;
            continue;
        }
        if !started {
            continue;
        }
        // Stop at same or higher level — we've left this section.
        if heading.level <= target_level {
            break;
        }
        // Only include direct children.
        if heading.level == child_level {
            children.push(heading_to_hierarchy_item(heading, &abs_path));
        }
    }

    Some(children)
}

// ---------------------------------------------------------------------------
// Call hierarchy (ticket 07)
// ---------------------------------------------------------------------------

/// Prepare a call hierarchy item for the heading at the cursor.
fn prepare_call_hierarchy(
    workspaces: &Workspaces,
    params: &lsp::TextDocumentPositionParams,
) -> Option<Vec<lsp::HierarchyItem>> {
    let (workspace, rel_path) = workspaces.resolve_document(&params.text_document.uri)?;
    let file_data = workspace.file(&rel_path)?;
    let headings = file_data.tree.headings();
    let heading = heading_at_line(&headings, params.position.line)?;
    let item = heading_to_hierarchy_item(heading, &workspace.root().join(&rel_path));
    Some(vec![item])
}

/// Find all incoming calls — links from other files that target this document.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn call_hierarchy_incoming(
    workspaces: &Workspaces,
    item: &lsp::HierarchyItem,
) -> Vec<lsp::CallHierarchyIncomingCall> {
    if workspaces.resolve_document(&item.uri).is_none() {
        return Vec::new();
    }

    let mut calls = Vec::new();

    // Match in absolute space: a source's link target resolved against the
    // source's own root equals the cursor document's absolute path exactly when
    // it points there (ticket server 11). Restricted to the cursor's own scope
    // (decision 019): incoming calls are a graph-edge query, and scopes are
    // disjoint graphs, so a cross-boundary physical reference is a defect, not a
    // caller.
    let cursor_abs = uri_to_path(&item.uri);
    let cursor_root = workspaces
        .documents
        .get(&cursor_abs)
        .and_then(|doc| doc.primary_root.clone());
    for (abs, doc) in &workspaces.documents {
        let Some(root) = doc.primary_root.as_deref() else {
            continue;
        };
        if Some(root) != cursor_root.as_deref() {
            continue;
        }
        let src_path = abs.strip_prefix(root).unwrap_or(abs);
        let links = doc.data.tree.links(abs);
        let headings = doc.data.tree.headings();
        for link in &links {
            let LinkKind::IntraProject { target, .. } = &link.kind else {
                continue;
            };
            if root.join(target) != cursor_abs {
                continue;
            }
            let caller_heading = enclosing_heading(&headings, link.line);

            let caller_item = caller_heading.map_or_else(
                || file_hierarchy_item(abs, src_path),
                |ch| heading_to_hierarchy_item(ch, abs),
            );

            let line = link.line.saturating_sub(1) as u32;
            calls.push(lsp::CallHierarchyIncomingCall {
                from: caller_item,
                from_ranges: vec![lsp::Range {
                    start: lsp::Position { line, character: 0 },
                    end: lsp::Position { line, character: 0 },
                }],
            });
        }
    }

    calls
}

/// Find all outgoing calls — links within the heading's section to other files.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn call_hierarchy_outgoing(
    workspaces: &Workspaces,
    item: &lsp::HierarchyItem,
) -> Vec<lsp::CallHierarchyOutgoingCall> {
    let Some((workspace, rel_path)) = workspaces.resolve_document(&item.uri) else {
        return Vec::new();
    };
    let Some(file_data) = workspace.file(&rel_path) else {
        return Vec::new();
    };
    let headings = file_data.tree.headings();
    let links = file_data.tree.links(&workspace.root().join(&rel_path));

    let item_line = item.selection_range.start.line;
    let item_level = hierarchy_item_level(item);

    // Find the end of this heading's section.
    let section_end: u32 = headings
        .iter()
        .find(|h| {
            let h_line = h.line.saturating_sub(1) as u32;
            h_line > item_line && h.level <= item_level
        })
        .map_or(u32::MAX, |h| h.line.saturating_sub(1) as u32);

    let root = workspace.root();
    let mut calls = Vec::new();

    for link in &links {
        let LinkKind::IntraProject { target, .. } = &link.kind else {
            continue;
        };
        let link_line = link.line.saturating_sub(1) as u32;
        if link_line < item_line || link_line >= section_end {
            continue;
        }

        let target_abs = root.join(target);
        let target_key = target_to_key(root, target);
        let target_headings = workspace.file(target).map(|fd| fd.tree.headings());
        let target_item = target_headings
            .as_ref()
            .and_then(|h| h.first())
            .map_or_else(
                || file_hierarchy_item(&target_abs, &target_key),
                |h| heading_to_hierarchy_item(h, &target_abs),
            );

        calls.push(lsp::CallHierarchyOutgoingCall {
            to: target_item,
            from_ranges: vec![lsp::Range {
                start: lsp::Position {
                    line: link_line,
                    character: 0,
                },
                end: lsp::Position {
                    line: link_line,
                    character: 0,
                },
            }],
        });
    }

    calls
}

// ---------------------------------------------------------------------------
// Document link (ticket 06)
// ---------------------------------------------------------------------------

/// Return clickable document links for all intra-project links.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn document_links(workspaces: &Workspaces, uri: &str) -> Vec<lsp::DocumentLink> {
    let Some((workspace, rel_path)) = workspaces.resolve_document(uri) else {
        return Vec::new();
    };
    let Some(file_data) = workspace.file(&rel_path) else {
        return Vec::new();
    };
    let root = workspace.root();
    let file_links = file_data.tree.links(&root.join(&rel_path));

    let mut links = Vec::new();

    for link in &file_links {
        // DocumentLink is intentionally *file-granularity*. `DocumentLink.target`
        // is a bare URI with no position field, so it can only open a document,
        // never land on a heading. Hence cross-file links deliberately drop their
        // fragment (the `..` below), and same-document anchors are skipped
        // entirely: an in-page anchor's only useful destination is a heading in
        // *this* file, which a URI can't express. Heading-precise navigation is
        // delivered by go-to-definition instead, which returns a `Location` with
        // a range (see `go_to_definition`, issue 021). Do NOT "fix" the skip by
        // emitting a file-top link here — it would send an in-page anchor to the
        // top of the file you're already in, which reads as broken.
        let target_uri = match &link.kind {
            LinkKind::IntraProject { target, .. } | LinkKind::NonMarkdown { target } => {
                path_to_uri(&root.join(target))
            }
            LinkKind::External { .. } | LinkKind::IntraDocument { .. } => continue,
        };
        let line = link.line.saturating_sub(1) as u32;
        links.push(lsp::DocumentLink {
            range: lsp::Range {
                start: lsp::Position { line, character: 0 },
                end: lsp::Position { line, character: 0 },
            },
            target: Some(target_uri),
        });
    }

    links
}

// ---------------------------------------------------------------------------
// Diagnostic collection (shared by the push path and `lattice lint`)
// ---------------------------------------------------------------------------

/// Collect all diagnostics for a workspace: structural (unconditional) +
/// graph (gated by `.lattice.toml`).
fn collect_all_diagnostics(workspace: &WorkspaceView) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();

    // Structural diagnostics: always run, no config required. Read from the
    // per-file cache, which the workspace refreshes only for the reparsed file
    // (or, on a membership change, all files) — so this no longer re-walks
    // every cached tree on each sync (issue 013 — stage 2).
    for (path, file_data) in workspace.files() {
        diagnostics.extend(file_local_diagnostics(file_data, path));
    }

    // Graph diagnostics: only when .lattice.toml is present.
    if workspace.has_config() {
        diagnostics.extend(validation::collect_all(workspace));
    }

    diagnostics.sort_by(|a, b| a.file.cmp(&b.file).then(a.line.cmp(&b.line)));
    diagnostics
}

/// The unconditional (config-independent) diagnostics for a single file: its
/// cached structural diagnostics (issue 013 — stage 2) plus frontmatter parse
/// diagnostics. Returned unsorted — callers sort as they need.
///
/// Shared by the full-workspace collect and the per-file incremental publish so
/// the two cannot drift (the stage-2.5 differential invariant).
fn file_local_diagnostics(file_data: &FileData, rel_path: &Path) -> Vec<Diagnostic> {
    let mut diagnostics = file_data.structural.clone();
    for pd in &file_data.parse_diagnostics {
        let severity = match pd.severity {
            crate::fm::FmSeverity::Error => Severity::Error,
            crate::fm::FmSeverity::Warning => Severity::Warning,
        };
        diagnostics.push(Diagnostic {
            file: rel_path.to_path_buf(),
            line: pd.line,
            severity,
            message: format!("frontmatter: {}", pd.message),
            span: None,
        });
    }
    diagnostics
}

/// The file-local diagnostics for a single file in both forms: the Lattice
/// vector (sorted by line — the change-detection key) and its materialization
/// against the file's source. Both are empty when the file is not indexed.
///
/// This is the structural-tier slice of the full desired set; it excludes graph
/// diagnostics, so [`diff_file_diagnostics`] is sound only in the structural
/// tier (its callers gate on `!has_config()`).
fn file_desired(
    workspace: &WorkspaceView,
    rel_path: &Path,
) -> (Vec<Diagnostic>, Vec<lsp::Diagnostic>) {
    let Some(file_data) = workspace.file(rel_path) else {
        return (Vec::new(), Vec::new());
    };
    let source = file_data.tree.source();
    let index = &file_data.line_index;
    let mut lattice = file_local_diagnostics(file_data, rel_path);
    lattice.sort_by_key(|d| d.line);
    let lsp = lattice
        .iter()
        .map(|d| to_lsp_diagnostic(d, source, index))
        .collect();
    (lattice, lsp)
}

// ---------------------------------------------------------------------------
// Hover preview (ticket 10)
// ---------------------------------------------------------------------------

/// Show a preview of the link target on hover.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn hover_preview(
    workspaces: &Workspaces,
    params: &lsp::TextDocumentPositionParams,
) -> Option<lsp::Hover> {
    let (workspace, rel_path) = workspaces.resolve_document(&params.text_document.uri)?;
    let file_data = workspace.file(&rel_path)?;
    let root = workspace.root();
    let file_links = file_data.tree.links(&root.join(&rel_path));

    // Find the link on the cursor's line.
    let cursor_line = params.position.line;
    let link = file_links
        .iter()
        .find(|l| l.line.saturating_sub(1) as u32 == cursor_line)?;

    let (target, fragment, predicate) = match &link.kind {
        LinkKind::IntraProject {
            target,
            fragment,
            predicate,
            ..
        } => (target.clone(), fragment.clone(), predicate.as_str()),
        LinkKind::NonMarkdown { target } => (target.clone(), None, "references"),
        // No hover for external or intra-document links.
        LinkKind::External { .. } | LinkKind::IntraDocument { .. } => return None,
    };

    let target_data = workspace.file(&target)?;

    // For a graph edge whose predicate was explicitly authored, surface the
    // opposite label the edge derives on its target's backlinks, so an agent
    // sees both ends of the relationship without opening the target (decision
    // 008). Implicit `references` links, non-markdown links, and unknown
    // predicates have no informative paired label, so the clause is omitted.
    let opposite = match &link.kind {
        LinkKind::IntraProject {
            explicit_predicate: true,
            ..
        } => workspace.config().opposite_of(predicate),
        _ => None,
    };

    let preview = build_hover_preview(target_data, fragment.as_deref());
    // Display the root-relative form, not the root-free absolute target.
    let target_key = target_to_key(root, &target);
    let target_display = target_key.display();
    let header = opposite.map_or_else(
        || format!("**{predicate}** → `{target_display}`"),
        |opposite| {
            format!(
                "**{predicate}** → `{target_display}` (derives **{opposite}** on `{target_display}`)"
            )
        },
    );

    Some(lsp::Hover {
        contents: lsp::MarkupContent {
            kind: "markdown".to_string(),
            value: format!("{header}\n\n---\n\n{preview}"),
        },
    })
}

/// Build a ~5 line preview from the target file content.
fn build_hover_preview(target_data: &crate::workspace::FileData, fragment: Option<&str>) -> String {
    let content = target_data.tree.source();
    let lines: Vec<&str> = content_lines(content).collect();
    let headings = target_data.tree.headings();

    // Determine the start line for the preview.
    let start = fragment.map_or_else(
        // No fragment — skip frontmatter.
        || target_data.frontmatter.as_ref().map_or(0, |fm| fm.end_line),
        // Fragment — find the matching heading.
        |frag| {
            headings
                .iter()
                .find(|h| heading_matches_fragment(h, frag))
                .map_or(0, |h| h.line.saturating_sub(1))
        },
    );

    lines
        .iter()
        .skip(start)
        .filter(|l| !l.trim().is_empty())
        .take(5)
        .copied()
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// Folding range (ticket 11)
// ---------------------------------------------------------------------------

/// Return folding ranges for headings and frontmatter.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn folding_ranges(workspaces: &Workspaces, uri: &str) -> Vec<lsp::FoldingRange> {
    let Some((workspace, rel_path)) = workspaces.resolve_document(uri) else {
        return Vec::new();
    };
    let Some(file_data) = workspace.file(&rel_path) else {
        return Vec::new();
    };

    let total_lines = crate::fm::line_count(file_data.tree.source()) as u32;

    let mut ranges = Vec::new();

    // Frontmatter folding range.
    if let Some(fm) = &file_data.frontmatter {
        let start = fm.start_line.saturating_sub(1) as u32;
        let end = fm.end_line.saturating_sub(1) as u32;
        if end > start {
            ranges.push(lsp::FoldingRange {
                start_line: start,
                end_line: end,
                kind: Some("region".to_string()),
            });
        }
    }

    // Heading folding ranges.
    let headings = file_data.tree.headings();
    for (i, heading) in headings.iter().enumerate() {
        let start = heading.line.saturating_sub(1) as u32;
        // End is the line before the next heading at same or higher level, or EOF.
        let end = headings[i + 1..]
            .iter()
            .find(|h| h.level <= heading.level)
            .map_or_else(
                || total_lines.saturating_sub(1),
                |h| (h.line.saturating_sub(1) as u32).saturating_sub(1),
            );
        if end > start {
            ranges.push(lsp::FoldingRange {
                start_line: start,
                end_line: end,
                kind: Some("region".to_string()),
            });
        }
    }

    ranges
}

// ---------------------------------------------------------------------------
// Semantic tokens (ticket integration 15)
// ---------------------------------------------------------------------------

/// A maximal disjoint byte region carrying the union of emphasis modifiers
/// active over it. Reconstructed from the parser's flat, *overlapping* sibling
/// emphasis spans so the emitted token stream can be non-overlapping (an LSP
/// hard requirement), while still styling the `foo` in `***foo***` as both
/// bold and italic.
#[derive(Debug, Clone, Copy)]
struct EmphasisRegion {
    /// Byte start (inclusive) in the source.
    start: usize,
    /// Byte end (exclusive) in the source.
    end: usize,
    /// OR of the `SEMANTIC_MODIFIER_*_BIT` flags active over `[start, end)`.
    modifiers: u32,
}

/// Map an emphasis [`ElementKind`] to its modifier bit, or `None` if the node
/// is not an emphasis run.
fn emphasis_modifier_bit(kind: &ElementKind) -> Option<u32> {
    match kind {
        ElementKind::Strong => Some(SEMANTIC_MODIFIER_BOLD_BIT),
        ElementKind::Emphasis => Some(SEMANTIC_MODIFIER_ITALIC_BIT),
        ElementKind::Strikethrough => Some(SEMANTIC_MODIFIER_STRIKETHROUGH_BIT),
        _ => None,
    }
}

/// Reconstruct the maximal disjoint regions from the parser's overlapping
/// emphasis spans, each tagged with the union of modifiers active over it.
///
/// Parser 26 emits emphasis as flat, *overlapping* sibling spans (e.g.
/// `***foo***` yields a `Strong` over `**foo**` and an `Emphasis` over the
/// whole `***foo***`), but the LSP semantic-tokens protocol requires a flat,
/// non-overlapping token list. We flatten by collecting every emphasis span's
/// endpoints as cut points, then, for each adjacent pair of cut points, OR the
/// modifiers of every span that fully covers that sub-segment. Segments with
/// no active modifier (the gaps between runs) are dropped. The result is sorted
/// by start and pairwise non-overlapping.
///
/// Emphasis runs never appear inside code spans or code blocks — the inline
/// parser excludes those before delimiter matching — so this naturally emits no
/// tokens in code.
fn collect_emphasis_regions(tree: &Tree) -> Vec<EmphasisRegion> {
    // (start, end, modifier_bit) for every emphasis run.
    let mut spans: Vec<(usize, usize, u32)> = Vec::new();
    for node in tree.nodes() {
        if let Some(bit) = emphasis_modifier_bit(&node.kind) {
            spans.push((node.span.start, node.span.end, bit));
        }
    }
    if spans.is_empty() {
        return Vec::new();
    }

    // Sorted, deduped boundary set: every distinct start/end is a cut point.
    let mut cuts: Vec<usize> = Vec::with_capacity(spans.len() * 2);
    for &(start, end, _) in &spans {
        cuts.push(start);
        cuts.push(end);
    }
    cuts.sort_unstable();
    cuts.dedup();

    // For each adjacent cut-point pair, the modifier mask is the OR of every
    // span that fully covers the segment.
    let mut regions: Vec<EmphasisRegion> = Vec::new();
    for window in cuts.windows(2) {
        let (seg_start, seg_end) = (window[0], window[1]);
        let mut modifiers = 0;
        for &(start, end, bit) in &spans {
            if start <= seg_start && seg_end <= end {
                modifiers |= bit;
            }
        }
        if modifiers != 0 {
            regions.push(EmphasisRegion {
                start: seg_start,
                end: seg_end,
                modifiers,
            });
        }
    }
    regions
}

/// Encode emphasis regions as the LSP delta-quintuple stream, restricted to
/// `byte_filter` (the whole document for `/full`, or a range's byte span for
/// `/range`).
///
/// A single LSP token may not span a line break, so each region is split at
/// line boundaries before encoding. Byte→UTF-16 conversion is delegated to the
/// file's cached [`LineIndex`] (`span_to_lsp_range`), the same UTF-16-aware
/// mapping diagnostics use, so multibyte and astral characters map correctly.
/// Tokens are delta-encoded against the previous token's position, as the
/// protocol requires.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line/column values in markdown files won't exceed u32::MAX"
)]
fn encode_semantic_tokens(
    source: &str,
    index: &LineIndex,
    regions: &[EmphasisRegion],
    byte_filter: std::ops::Range<usize>,
) -> lsp::SemanticTokens {
    let mut data: Vec<u32> = Vec::new();
    // Previous token's absolute (line, char) for delta encoding.
    let mut prev_line = 0u32;
    let mut prev_char = 0u32;

    for region in regions {
        let start = region.start.max(byte_filter.start);
        let end = region.end.min(byte_filter.end);
        if start >= end {
            continue;
        }
        let range = span_to_lsp_range(source, index, &Span::new(start, end));
        // Split into one token per line the region touches: an LSP token is
        // single-line, so a region crossing a `\n` becomes several tokens.
        for line in range.start.line..=range.end.line {
            let line_start_char = if line == range.start.line {
                range.start.character
            } else {
                0
            };
            // The line's content end in UTF-16 units, or the region end on the
            // final line.
            let line_end_char = if line == range.end.line {
                range.end.character
            } else {
                let (ls, le) = line_byte_range(source, line);
                source[ls..le].chars().map(char::len_utf16).sum::<usize>() as u32
            };
            let length = line_end_char.saturating_sub(line_start_char);
            if length == 0 {
                continue;
            }
            let delta_line = line - prev_line;
            let delta_start = if delta_line == 0 {
                line_start_char - prev_char
            } else {
                line_start_char
            };
            data.extend_from_slice(&[
                delta_line,
                delta_start,
                length,
                SEMANTIC_TOKEN_TYPE_MARKUP_INDEX,
                region.modifiers,
            ]);
            prev_line = line;
            prev_char = line_start_char;
        }
    }

    lsp::SemanticTokens { data }
}

/// Answer `textDocument/semanticTokens/full`: emphasis tokens over the whole
/// document.
///
/// Returns an empty token set for unknown documents. Styling only — never
/// emits a diagnostic.
///
/// # Perf seam
///
/// `full/delta` is intentionally not served: re-encoding only the emphasis runs
/// is cheap, and a delta handler should consume the perf workstream's reusable
/// "what changed since last parse" diff rather than recompute one — wire it
/// here once that lands (ticket integration 15, perf seam).
fn semantic_tokens_full(workspaces: &Workspaces, uri: &str) -> lsp::SemanticTokens {
    let Some((workspace, rel_path)) = workspaces.resolve_document(uri) else {
        return lsp::SemanticTokens::default();
    };
    let Some(file_data) = workspace.file(&rel_path) else {
        return lsp::SemanticTokens::default();
    };
    let source = file_data.tree.source();
    let regions = collect_emphasis_regions(&file_data.tree);
    encode_semantic_tokens(source, &file_data.line_index, &regions, 0..source.len())
}

/// Answer `textDocument/semanticTokens/range`: emphasis tokens restricted to
/// `range` (the byte span between its endpoints), for large documents.
///
/// Returns an empty token set for unknown documents.
fn semantic_tokens_range(
    workspaces: &Workspaces,
    uri: &str,
    range: &lsp::Range,
) -> lsp::SemanticTokens {
    let Some((workspace, rel_path)) = workspaces.resolve_document(uri) else {
        return lsp::SemanticTokens::default();
    };
    let Some(file_data) = workspace.file(&rel_path) else {
        return lsp::SemanticTokens::default();
    };
    let source = file_data.tree.source();
    let start = file_data.line_index.offset(source, range.start);
    let end = file_data.line_index.offset(source, range.end);
    let regions = collect_emphasis_regions(&file_data.tree);
    encode_semantic_tokens(source, &file_data.line_index, &regions, start..end)
}

// ---------------------------------------------------------------------------
// Formatting (ticket 12)
// ---------------------------------------------------------------------------

/// Format a document's backlink frontmatter.
///
/// Delegates to the shared [`crate::format::format_source`] engine (the single
/// source of formatting semantics, shared with the `lattice format` CLI): it
/// sorts predicate keys alphabetically, sorts paths within each predicate,
/// normalizes whitespace, and — if the config specifies an external formatter —
/// pipes the full document through it after frontmatter sorting. The formatted
/// document is returned as a single whole-document [`lsp::TextEdit`].
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn format_document(workspaces: &Workspaces, uri: &str) -> Option<Vec<lsp::TextEdit>> {
    let (workspace, rel_path) = workspaces.resolve_document(uri)?;
    let file_data = workspace.file(&rel_path)?;

    let source = file_data.tree.source();
    let document = crate::format::format_source(
        source,
        file_data.frontmatter.as_ref(),
        workspace.config().format_command.as_deref(),
    )?;

    // Replace the entire document.
    let total_lines = source.lines().count() as u32;
    let last_line_len = source.lines().last().map_or(0, str::len) as u32;

    let range = lsp::Range {
        start: lsp::Position {
            line: 0,
            character: 0,
        },
        end: lsp::Position {
            line: total_lines.saturating_sub(1),
            character: last_line_len,
        },
    };

    Some(vec![lsp::TextEdit {
        range,
        new_text: document,
    }])
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Convert a heading to a hierarchy item (used for both type and call hierarchy).
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn heading_to_hierarchy_item(heading: &Heading, abs_path: &Path) -> lsp::HierarchyItem {
    let line = heading.line.saturating_sub(1) as u32;
    let range = lsp::Range {
        start: lsp::Position { line, character: 0 },
        end: lsp::Position { line, character: 0 },
    };

    lsp::HierarchyItem {
        name: heading.text.clone(),
        kind: lsp::symbol_kind::CLASS,
        uri: path_to_uri(abs_path),
        range,
        selection_range: range,
        detail: Some(format!("H{}", heading.level)),
        data: None,
    }
}

/// Create a file-level hierarchy item when a link has no enclosing heading.
fn file_hierarchy_item(abs_path: &Path, rel_path: &Path) -> lsp::HierarchyItem {
    let range = lsp::Range::default();
    lsp::HierarchyItem {
        name: rel_path.display().to_string(),
        kind: lsp::symbol_kind::FILE,
        uri: path_to_uri(abs_path),
        range,
        selection_range: range,
        detail: None,
        data: None,
    }
}

/// Find the heading that encloses a given 1-based line number.
fn enclosing_heading(headings: &[Heading], line: usize) -> Option<&Heading> {
    headings.iter().rev().find(|h| h.line < line)
}

/// Extract the heading level from a hierarchy item's detail field.
fn hierarchy_item_level(item: &lsp::HierarchyItem) -> u8 {
    item.detail
        .as_deref()
        .and_then(|d| d.strip_prefix('H'))
        .and_then(|n| n.parse::<u8>().ok())
        .unwrap_or(1)
}

/// Find the classified [`Link`] whose span matches a node span.
///
/// Bridges the gap between `find_link_at_offset` (which finds the tree node)
/// and the classified links from `Tree::links` (which resolve targets).
/// `abs_path` is the document's absolute path, so the classified target is
/// root-free (ticket server 11).
fn find_classified_link(
    tree: &crate::block::Tree,
    abs_path: &Path,
    node_span: Span,
) -> Option<crate::block::Link> {
    tree.links(abs_path)
        .into_iter()
        .find(|l| l.span == node_span)
}

/// Byte range `[start, content_end)` of 0-based `line` in `source`, excluding
/// the line's terminator. Recognizes `\n`, `\r\n`, and bare `\r`. A line past
/// the end of input yields an empty range at `source.len()`.
fn line_byte_range(source: &str, line: u32) -> (usize, usize) {
    let bytes = source.as_bytes();
    let mut idx = 0u32;
    let mut start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let (is_break, next) = match bytes[i] {
            b'\n' => (true, i + 1),
            b'\r' => (
                true,
                if bytes.get(i + 1) == Some(&b'\n') {
                    i + 2
                } else {
                    i + 1
                },
            ),
            _ => (false, i + 1),
        };
        if is_break {
            if idx == line {
                return (start, i);
            }
            idx += 1;
            start = next;
        }
        i = next;
    }
    if idx == line {
        (start, bytes.len())
    } else {
        (bytes.len(), bytes.len())
    }
}

/// Convert an LSP 0-based position to a byte offset in `source`.
///
/// Recognizes `\n`, `\r\n`, and bare `\r`. `character` is a UTF-16 code-unit
/// offset within the line (the LSP default position encoding); it is walked
/// across the line's chars and clamped to the line's content length. A column
/// landing inside a surrogate pair rounds down to the enclosing char's start.
#[must_use]
pub fn lsp_position_to_byte_offset(source: &str, pos: lsp::Position) -> usize {
    let (start, end) = line_byte_range(source, pos.line);
    let mut remaining = pos.character as usize;
    let mut byte = start;
    for ch in source[start..end].chars() {
        let units = ch.len_utf16();
        if remaining < units {
            break;
        }
        remaining -= units;
        byte += ch.len_utf8();
    }
    byte
}

/// Convert a byte `Span` to an LSP `Range` through the file's cached
/// [`LineIndex`], so each endpoint is a binary search rather than an
/// `O(offset)` scan of `source`.
fn span_to_lsp_range(source: &str, index: &LineIndex, span: &Span) -> lsp::Range {
    let start = index.position(source, span.start);
    let end = index.position(source, span.end);
    lsp::Range { start, end }
}

/// Convert a byte offset to an LSP 0-based position.
///
/// Line counting recognizes `\n`, `\r\n`, and bare `\r`. The `character` field
/// is a UTF-16 code-unit offset within the line (the LSP default position
/// encoding), measured from the byte after the previous line break. A byte
/// offset that falls inside a multi-byte char is floored to that char's start
/// so the UTF-16 count cannot split a code point.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line/column values in markdown files won't exceed u32::MAX"
)]
#[must_use]
pub fn byte_offset_to_lsp_position(source: &str, offset: usize) -> lsp::Position {
    let mut offset = offset.min(source.len());
    while offset > 0 && !source.is_char_boundary(offset) {
        offset -= 1;
    }
    let line = (crate::block::byte_offset_to_line(source, offset) - 1) as u32;
    let line_start = source.as_bytes()[..offset]
        .iter()
        .rposition(|&b| b == b'\n' || b == b'\r')
        .map_or(0, |i| i + 1);
    let character = source[line_start..offset]
        .chars()
        .map(char::len_utf16)
        .sum::<usize>() as u32;
    lsp::Position { line, character }
}

/// Extract the normalized reference label from a link's source text,
/// if the link uses reference-style syntax.
///
/// Reference-style links look like `[text][label]`, `[text][]`, or `[text]`
/// (shortcut). Inline links contain `(` after the `]`.
///
/// Uses [`inline::find_matching_bracket`] for correct handling of nested
/// brackets, backslash escapes, and backtick spans.
fn link_ref_label(source: &str, span: &Span) -> Option<String> {
    let raw = &source[span.start..span.end];

    // Skip image prefix.
    let text = raw.strip_prefix('!').unwrap_or(raw);
    if !text.starts_with('[') {
        return None;
    }

    // Find the closing `]` for the link text.
    let text_close = crate::inline::find_matching_bracket(text.as_bytes(), 0)?;
    let after = &text[text_close + 1..];

    // Inline link: [text](url)
    if after.starts_with('(') {
        return None;
    }

    // Full reference: [text][label]
    if after.starts_with('[') {
        let label_start = 1;
        let label_end = after.find(']').unwrap_or(after.len());
        let label_text = &after[label_start..label_end];
        if label_text.is_empty() {
            // Collapsed reference [text][] — label is the link text
            let link_text = &text[1..text_close];
            return Some(normalize_label(link_text));
        }
        return Some(normalize_label(label_text));
    }

    // Shortcut reference: [text] — label is the link text
    let link_text = &text[1..text_close];
    Some(normalize_label(link_text))
}

/// Check if the byte offset falls on a `ReferenceDef` node, returning
/// its normalized label.
fn ref_def_label_at_offset(tree: &crate::block::Tree, offset: usize) -> Option<String> {
    for node in tree.nodes() {
        if let ElementKind::ReferenceDef { label, .. } = &node.kind
            && node.span.start <= offset
            && offset < node.span.end
        {
            return Some(label.clone());
        }
    }
    None
}

/// Get the text of a 0-based line in the source (recognizing `\n`, `\r\n`,
/// and bare `\r`), excluding the line terminator.
fn source_line_at(source: &str, lsp_line: u32) -> &str {
    let (start, end) = line_byte_range(source, lsp_line);
    &source[start..end]
}

// ---------------------------------------------------------------------------
// Completion (decision 007, ticket integration 14)
// ---------------------------------------------------------------------------

/// Build completion candidates for the construct under the cursor.
///
/// Returns `None` when the cursor is not in a completion site (prose) or sits
/// inside a code span, code block, or math node. Otherwise returns the
/// candidate list for the detected surface — possibly empty (e.g. a fragment
/// against a target that is not yet a resolvable file).
fn completion(
    workspaces: &Workspaces,
    params: &lsp::TextDocumentPositionParams,
) -> Option<lsp::CompletionList> {
    let (workspace, rel_path) = workspaces.resolve_document(&params.text_document.uri)?;
    let file_data = workspace.file(&rel_path)?;
    let tree = &file_data.tree;
    let source = tree.source();
    let offset = lsp_position_to_byte_offset(source, params.position);

    // No completion inside code or math — the tree is authoritative here, so a
    // link-shaped string in a code span (e.g. `` `[x](y` ``) is suppressed even
    // though its line prefix would otherwise look like a destination.
    if offset_in_code(tree, offset) {
        return None;
    }

    let (line_start, _) = line_byte_range(source, params.position.line);
    let prefix = &source[line_start..offset];
    let context = crate::completion::detect(prefix)?;

    let pos = params.position;
    let items = match context {
        CompletionContext::Path { partial } => {
            complete_path(&workspace, &rel_path, partial, source, offset, pos)
        }
        CompletionContext::Fragment { target, partial } => {
            complete_fragment(&workspace, &rel_path, target, partial, source, offset, pos)
        }
        CompletionContext::Predicate { target, partial } => {
            complete_predicate(workspace.config(), target, partial, source, offset, pos)
        }
        CompletionContext::ReferenceLabel { partial } => {
            complete_reference_label(tree, partial, source, offset, pos)
        }
        CompletionContext::Footnote { partial } => {
            complete_footnote(tree, partial, source, offset, pos)
        }
    };

    Some(lsp::CompletionList {
        is_incomplete: false,
        items,
    })
}

/// Whether `offset` falls inside a code span, code block, or math node.
fn offset_in_code(tree: &Tree, offset: usize) -> bool {
    tree.nodes().iter().any(|node| {
        matches!(
            node.kind,
            ElementKind::CodeBlock
                | ElementKind::Math
                | ElementKind::InlineCode
                | ElementKind::InlineMath
        ) && node.span.start <= offset
            && offset < node.span.end
    })
}

/// The range a completion replaces: the `partial`-length slice ending at the
/// cursor.
fn replace_range(
    source: &str,
    cursor_offset: usize,
    cursor_pos: lsp::Position,
    partial: &str,
) -> lsp::Range {
    let start = byte_offset_to_lsp_position(source, cursor_offset.saturating_sub(partial.len()));
    lsp::Range {
        start,
        end: cursor_pos,
    }
}

/// Build a completion item that replaces `range` with `label`.
fn completion_item(
    label: String,
    kind: u32,
    detail: Option<String>,
    sort_text: Option<String>,
    range: lsp::Range,
) -> lsp::CompletionItem {
    lsp::CompletionItem {
        filter_text: Some(label.clone()),
        text_edit: Some(lsp::TextEdit {
            range,
            new_text: label.clone(),
        }),
        label,
        kind: Some(kind),
        detail,
        sort_text,
    }
}

/// Case-insensitive prefix test for completion filtering.
fn matches_prefix(candidate: &str, partial: &str) -> bool {
    candidate
        .to_lowercase()
        .starts_with(&partial.to_lowercase())
}

/// Complete link-target paths in a destination: workspace files and
/// directories under the typed (relative) directory, with only the trailing
/// filename segment replaced.
fn complete_path(
    workspace: &WorkspaceView,
    rel_path: &Path,
    partial: &str,
    source: &str,
    offset: usize,
    pos: lsp::Position,
) -> Vec<lsp::CompletionItem> {
    // Split into the committed directory prefix and the filename being typed.
    let (dir_part, name_part) = partial
        .rfind('/')
        .map_or(("", partial), |i| (&partial[..=i], &partial[i + 1..]));

    let cur_dir = rel_path.parent().unwrap_or_else(|| Path::new(""));
    let rel_dir = crate::block::normalize_path(&cur_dir.join(dir_part));
    // Don't list outside the workspace — those files aren't graph nodes.
    if rel_dir.starts_with("..") {
        return Vec::new();
    }
    let base = workspace.root().join(&rel_dir);

    // Only the filename segment is replaced; the directory prefix stays put.
    let range = replace_range(source, offset, pos, name_part);

    // Walk just the immediate directory, honoring `.gitignore` and skipping
    // hidden entries (`.git`, dotfiles) exactly as workspace discovery does, so
    // path completion never offers files the index itself would exclude.
    let mut items = Vec::new();
    for entry in ignore::WalkBuilder::new(&base)
        .max_depth(Some(1))
        .build()
        .flatten()
    {
        if entry.depth() == 0 {
            continue; // the base directory itself
        }
        let Some(name) = entry.file_name().to_str() else {
            continue;
        };
        if !matches_prefix(name, name_part) {
            continue;
        }
        if entry.file_type().is_some_and(|t| t.is_dir()) {
            // Directories sort first (`0` prefix) and re-trigger on the `/`.
            items.push(completion_item(
                format!("{name}/"),
                lsp::completion_item_kind::FOLDER,
                None,
                Some(format!("0{name}")),
                range,
            ));
        } else {
            items.push(completion_item(
                name.to_string(),
                lsp::completion_item_kind::FILE,
                None,
                Some(format!("1{name}")),
                range,
            ));
        }
    }
    items
}

/// Complete heading fragments: the target document's anchors (explicit `{#id}`
/// and computed slugs), or the current document's for an in-doc `#`.
fn complete_fragment(
    workspace: &WorkspaceView,
    rel_path: &Path,
    target: &str,
    partial: &str,
    source: &str,
    offset: usize,
    pos: lsp::Position,
) -> Vec<lsp::CompletionItem> {
    let target_rel = if target.is_empty() {
        rel_path.to_path_buf()
    } else {
        resolve_fragment_target(rel_path, target)
    };
    let Some(target_data) = workspace.file(&target_rel) else {
        return Vec::new();
    };

    let config = workspace.config();
    let range = replace_range(source, offset, pos, partial);

    let mut items = Vec::new();
    let mut seen = HashSet::new();
    for heading in target_data.tree.headings() {
        for anchor in heading_anchors(&heading, config) {
            if matches_prefix(&anchor, partial) && seen.insert(anchor.clone()) {
                items.push(completion_item(
                    anchor,
                    lsp::completion_item_kind::VALUE,
                    Some(heading.text.clone()),
                    None,
                    range,
                ));
            }
        }
    }
    items
}

/// Resolve a half-typed destination path against the current file's directory.
fn resolve_fragment_target(rel_path: &Path, target: &str) -> PathBuf {
    let parent = rel_path.parent().unwrap_or_else(|| Path::new(""));
    crate::block::normalize_path(&parent.join(target))
}

/// The anchor IDs a heading offers for fragment completion.
///
/// An explicit `{#id}` is the sole anchor. Otherwise the computed slug(s): the
/// configured algorithm's slug when `fragments` is set, else all three
/// conventions (deduplicated) since the default validates against any.
fn heading_anchors(heading: &Heading, config: &Config) -> Vec<String> {
    match &heading.id {
        HeadingId::Explicit(id) => vec![id.clone()],
        HeadingId::Computed {
            github,
            gitlab,
            vscode,
        } => match config.policy.fragments {
            Some(FragmentAlgorithm::Github) => vec![github.clone()],
            Some(FragmentAlgorithm::Gitlab) => vec![gitlab.clone()],
            Some(FragmentAlgorithm::Vscode) => vec![vscode.clone()],
            None => {
                let mut anchors = vec![github.clone()];
                for slug in [gitlab, vscode] {
                    if !anchors.contains(slug) {
                        anchors.push(slug.clone());
                    }
                }
                anchors
            }
        },
    }
}

/// Complete the predicate vocabulary inside a title string.
///
/// Offers both members of each vocabulary pair (decision 008 — a link may name
/// either direction): the label is the predicate, the detail its opposite.
/// Yields nothing when the destination does not take a predicate (external or
/// non-markdown links carry a plain title, not a predicate).
fn complete_predicate(
    config: &Config,
    target: &str,
    partial: &str,
    source: &str,
    offset: usize,
    pos: lsp::Position,
) -> Vec<lsp::CompletionItem> {
    if !target_takes_predicate(target) {
        return Vec::new();
    }

    let range = replace_range(source, offset, pos, partial);
    let mut items = Vec::new();
    let mut seen = HashSet::new();
    for (forward, inverse) in &config.predicates {
        if matches_prefix(forward, partial) && seen.insert(forward.clone()) {
            items.push(completion_item(
                forward.clone(),
                lsp::completion_item_kind::KEYWORD,
                Some(inverse.clone()),
                None,
                range,
            ));
        }
        if matches_prefix(inverse, partial) && seen.insert(inverse.clone()) {
            items.push(completion_item(
                inverse.clone(),
                lsp::completion_item_kind::KEYWORD,
                Some(forward.clone()),
                None,
                range,
            ));
        }
    }
    items
}

/// Whether a destination URL takes a predicate — an intra-project markdown
/// link. External links and non-markdown targets carry a plain title; a
/// fragment-only link (`#section`) is not a graph edge.
fn target_takes_predicate(target: &str) -> bool {
    let target = target.trim();
    if target.is_empty()
        || target.starts_with("http://")
        || target.starts_with("https://")
        || target.starts_with("mailto:")
    {
        return false;
    }
    let path = target.split_once('#').map_or(target, |(p, _)| p);
    !path.is_empty()
        && Path::new(path)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
}

/// Complete the document's defined link reference labels.
fn complete_reference_label(
    tree: &Tree,
    partial: &str,
    source: &str,
    offset: usize,
    pos: lsp::Position,
) -> Vec<lsp::CompletionItem> {
    // Definition labels are stored normalized; match the partial the same way.
    let normalized = normalize_label(partial);
    let range = replace_range(source, offset, pos, partial);

    let mut items = Vec::new();
    let mut seen = HashSet::new();
    for node in tree.nodes() {
        if let ElementKind::ReferenceDef { label, url, .. } = &node.kind
            && label.starts_with(&normalized)
            && seen.insert(label.clone())
        {
            let detail = (!url.is_empty()).then(|| url.clone());
            items.push(completion_item(
                label.clone(),
                lsp::completion_item_kind::REFERENCE,
                detail,
                None,
                range,
            ));
        }
    }
    items
}

/// Complete the document's defined footnote labels.
fn complete_footnote(
    tree: &Tree,
    partial: &str,
    source: &str,
    offset: usize,
    pos: lsp::Position,
) -> Vec<lsp::CompletionItem> {
    let range = replace_range(source, offset, pos, partial);

    let mut items = Vec::new();
    let mut seen = HashSet::new();
    for node in tree.nodes() {
        if let ElementKind::FootnoteDef { label } = &node.kind
            && matches_prefix(label, partial)
            && seen.insert(label.clone())
        {
            items.push(completion_item(
                label.clone(),
                lsp::completion_item_kind::CONSTANT,
                Some("footnote".to_string()),
                None,
                range,
            ));
        }
    }
    items
}

// ---------------------------------------------------------------------------
// Notifications
// ---------------------------------------------------------------------------

/// Dispatch a notification.
fn handle_notification(
    connection: &Connection,
    workspaces: &mut Workspaces,
    notif: Notification,
) -> Result<()> {
    match notif.method.as_str() {
        lsp::method::DID_OPEN => {
            let params: lsp::DidOpenTextDocumentParams = serde_json::from_value(notif.params)?;
            // The editor now owns this document's truth: while it is open, a
            // watched-file `changed` event for the same URI is dropped so the
            // synced buffer is never clobbered by stale disk bytes (decision 017
            // §3, ticket server 09).
            workspaces
                .open_documents
                .insert(params.text_document.uri.clone());
            // Index the buffer wherever it belongs: a document inside a scanned
            // workspace folder updates that workspace; one opened outside every
            // folder becomes a single-file document (issue 051) so its
            // document-scoped features are served without a workspace. The
            // republish below enumerates scanned workspaces only, so a single-file
            // document emits no diagnostics — the graph tier has nothing to say.
            workspaces.sync_document_content(&params.text_document.uri, &params.text_document.text);
            republish_on_open(connection, workspaces, &params.text_document.uri)?;
        }
        lsp::method::DID_CLOSE => {
            let params: lsp::DidCloseTextDocumentParams = serde_json::from_value(notif.params)?;
            // The buffer is gone: the document is no longer editor-authoritative,
            // so watched-file `changed` events for it are honored again (decision
            // 017 §3), and per the LSP spec content authority reverts to the
            // filesystem — reconcile the index to disk (issue 046).
            workspaces.open_documents.remove(&params.text_document.uri);
            reconcile_closed_document(connection, workspaces, &params.text_document.uri)?;
        }
        lsp::method::DID_SAVE => {
            let params: lsp::DidSaveTextDocumentParams = serde_json::from_value(notif.params)?;
            if let Some(text) = &params.text {
                // The save payload carries the text: upsert it by path.
                workspaces.sync_document_content(&params.text_document.uri, text);
            } else {
                // No text in the save payload: reconcile the document to disk.
                let abs = uri_to_path(&params.text_document.uri);
                workspaces.update_from_disk(&abs);
            }
            publish_all_diagnostics(connection, workspaces, &one_uri(&params.text_document.uri))?;
        }
        lsp::method::DID_CHANGE => {
            let params: lsp::DidChangeTextDocumentParams = serde_json::from_value(notif.params)?;
            if let Some(change) = params.content_changes.into_iter().last() {
                // A didChange targets an already-open (already-indexed) document,
                // so it never changes membership. Upsert the buffer, then choose
                // the publish path from the document's placement:
                //
                // - rooted with `.lattice.toml`: the full graph path — a
                //   link/backlink edit reaches other files — re-materializing
                //   only the files the whole-graph recompute shows changed
                //   (ticket perf 02), with the edited URI forced.
                // - rooted without config: the cheap structural-tier delta —
                //   only the edited file's diagnostics can change (issue 013 —
                //   stage 2.5).
                // - rootless (issue 051): publish nothing; the graph tier has
                //   nothing to say for a single file.
                workspaces.sync_document_content(&params.text_document.uri, &change.text);
                let abs = uri_to_path(&params.text_document.uri);
                let publish = match workspaces
                    .documents
                    .get(&abs)
                    .and_then(|doc| doc.primary_root.clone())
                {
                    Some(root) => Some(
                        workspaces
                            .roots
                            .get(&root)
                            .is_some_and(|meta| meta.has_config),
                    ),
                    None => None,
                };
                match publish {
                    Some(true) => publish_all_diagnostics(
                        connection,
                        workspaces,
                        &one_uri(&params.text_document.uri),
                    )?,
                    Some(false) => {
                        publish_file_diagnostics(
                            connection,
                            workspaces,
                            &params.text_document.uri,
                        )?;
                    }
                    None => {}
                }
            }
        }
        lsp::method::DID_CHANGE_WORKSPACE_FOLDERS => {
            let params: lsp::DidChangeWorkspaceFoldersParams =
                serde_json::from_value(notif.params)?;
            for removed in &params.event.removed {
                workspaces.remove_folder(&removed.uri);
            }
            for added in &params.event.added {
                workspaces.add_folder(&added.uri);
            }
            // No single file's text changed — added folders bring cache-miss
            // files that re-materialize regardless, and removed ones are cleared
            // by the diff's absent-file pass. Open documents kept their buffers
            // across the change (buffer-wins), so no dark window.
            publish_all_diagnostics(connection, workspaces, &HashSet::new())?;
        }
        lsp::method::DID_CHANGE_WATCHED_FILES => {
            let params: lsp::DidChangeWatchedFilesParams = serde_json::from_value(notif.params)?;
            handle_watched_files_change(connection, workspaces, &params)?;
        }
        lsp::method::DID_RENAME_FILES => {
            let params: lsp::RenameFilesParams = serde_json::from_value(notif.params)?;
            handle_did_rename_files(connection, workspaces, &params)?;
        }
        _ => {}
    }
    Ok(())
}

/// Apply one `workspace/didChangeWatchedFiles` batch and re-publish.
///
/// Two registered globs (decision 017): the `.lattice.toml` marker (ticket
/// server 08) and `**/*.md` documents (ticket server 09). Each URI takes its
/// own reconciliation path, in two passes over the batch.
///
/// The marker pass runs FIRST, over the whole batch, before any `.md` change
/// is applied: a client watcher that debounces coalesces a config edit and
/// the document edits around it into one notification, in arbitrary order,
/// and every document (re)parse below must happen under the config that was
/// on disk with it. `reload_config` re-parsing the whole index would paper
/// over the wrong order today, but ordering the passes makes the correctness
/// structural instead of incidental (issue 050).
fn handle_watched_files_change(
    connection: &Connection,
    workspaces: &mut Workspaces,
    params: &lsp::DidChangeWatchedFilesParams,
) -> Result<()> {
    let mut reloaded = false;
    // Marker directories already handled in this batch: a create+modify pair
    // coalesced into one notification is applied once, not twice — each apply
    // is a full reparse of the affected scope (and, for split/merge, a
    // re-rooting).
    let mut handled_markers: HashSet<PathBuf> = HashSet::new();
    for change in &params.changes {
        // The marker watch is config: any event type reloads it. Guard on the
        // suffix so an unrelated future glob can never trigger a config
        // reload.
        if !change.uri.ends_with(".lattice.toml") {
            continue;
        }
        let Some(marker_dir) = uri_to_path(&change.uri).parent().map(Path::to_path_buf) else {
            continue;
        };
        if !handled_markers.insert(marker_dir) {
            continue;
        }
        // A marker create/change/delete either reloads a scope's config, splits
        // a new nested scope out of its host, or merges a vanished one back in
        // (decision 019 clause 6). A miss leaves the workspace silently on stale
        // (or default) config until the next marker event — the config-dead
        // failure shape of issue 050 — so it is worth a trace.
        if workspaces.handle_marker_event(&change.uri) {
            reloaded = true;
        } else {
            tracing::warn!(
                uri = %change.uri,
                "config marker event matches no workspace; reload skipped"
            );
        }
    }
    // `.md` URIs whose on-disk state was applied. The whole batch is
    // re-published in a single graph-aware pass below — a content or
    // membership change can move *other* files' backlink/forward edges, so
    // this mirrors how `didSave` re-publishes through
    // `publish_all_diagnostics`, but folds N changed files into one
    // O(workspace) recompute instead of N (ticket perf 07).
    let mut changed_docs: HashSet<String> = HashSet::new();
    for change in &params.changes {
        if !is_markdown_uri(&change.uri) {
            continue;
        }
        match change.change_type {
            // `changed` carries disk content; for a file open in the editor
            // the synced buffer wins, so the event is dropped (decision 017
            // §3). The open file already reaches the server through
            // `didChange`, so honoring the watcher too would also
            // double-deliver (the issue 009 class).
            lsp::file_change_type::CHANGED if workspaces.open_documents.contains(&change.uri) => {}
            // created / deleted are membership changes honored regardless of
            // open state; a non-open `changed` re-reads disk. All three route
            // through `update_from_disk`, which re-reads disk — reparsing a
            // created/changed file and dropping a deleted one — and refreshes
            // the structural caches. Only files under a folder are tracked (the
            // watcher glob is folder-scoped), so an event outside every root is
            // ignored.
            lsp::file_change_type::CREATED
            | lsp::file_change_type::CHANGED
            | lsp::file_change_type::DELETED => {
                let abs = uri_to_path(&change.uri);
                if workspaces.deepest_root_for(&abs).is_some() {
                    workspaces.update_from_disk(&abs);
                    changed_docs.insert(change.uri.clone());
                }
            }
            _ => {}
        }
    }
    // A marker change invalidates the whole workspace (predicates, artifacts,
    // overrides, and external aliases all feed parse and structural
    // analysis), so take the full re-publish path, not the
    // `has_config()`-gated single-file delta (decision 017). When `.md` files
    // also changed, the batched publish below already runs the full graph
    // diff against the freshly reloaded config, so a marker-only notification
    // is the only case that needs this empty-set publish.
    if reloaded && changed_docs.is_empty() {
        publish_all_diagnostics(connection, workspaces, &HashSet::new())?;
    }
    // Re-publish the whole applied `.md` batch in ONE graph-aware pass,
    // naming every changed document so each one's materialization is
    // refreshed unconditionally (its disk content changed). The single
    // whole-graph diff also catches any other file whose edges moved — one
    // recompute for the batch, not one per file (ticket perf 07).
    if !changed_docs.is_empty() {
        publish_all_diagnostics(connection, workspaces, &changed_docs)?;
    }
    Ok(())
}

/// Apply a `workspace/didRenameFiles` confirmation (decision 020 clause 2):
/// re-key the store for every rename the client just performed, then re-publish.
///
/// The `willRenameFiles` handler already returned the forced edits, and the
/// client applied them to buffers and renamed on disk — so the content now
/// living at each new path is correct. This re-keys the parsed entries onto the
/// new coordinates **without a rescan** ([`Workspaces::rekey_rename`]),
/// preserving open buffers (decision 017 §3). Each moved document's old URI gets
/// an explicit empty publish to clear the client's stale diagnostics under the
/// old name; then one graph-aware re-publish names the new URIs so their
/// diagnostics (and any referrer whose edge the coordinate change moved) land at
/// the renamed positions — the engine's isomorphism, observed end-to-end.
///
/// The watched-file create/delete channel (decision 017) delivers the same
/// membership change independently; this confirmation is idempotent with it —
/// a re-key of an already-moved key finds nothing and no-ops.
fn handle_did_rename_files(
    connection: &Connection,
    workspaces: &mut Workspaces,
    params: &lsp::RenameFilesParams,
) -> Result<()> {
    let mut cleared_uris: Vec<String> = Vec::new();
    let mut renamed_uris: HashSet<String> = HashSet::new();
    for rename in &params.files {
        let old_abs = uri_to_path(&rename.old_uri);
        let new_abs = uri_to_path(&rename.new_uri);
        let cleared = workspaces.rekey_rename(&old_abs, &new_abs);
        if !cleared.is_empty() || workspaces.deepest_root_for(&new_abs).is_some() {
            renamed_uris.insert(path_to_uri(&new_abs));
        }
        cleared_uris.extend(cleared);
    }
    // A membership change: any file's bare-path or backlink edge may have moved.
    workspaces.recompute_all_structural();

    // Clear the client's stale diagnostics under each vanished old URI.
    for uri in cleared_uris {
        let params = lsp::PublishDiagnosticsParams {
            uri,
            diagnostics: Vec::new(),
        };
        let notif = Notification::new(lsp::method::PUBLISH_DIAGNOSTICS.to_string(), params);
        connection.sender.send(Message::Notification(notif))?;
    }

    // Re-publish the whole graph in one pass, forcing the renamed documents to
    // re-materialize at their new coordinates.
    publish_all_diagnostics(connection, workspaces, &renamed_uris)?;
    Ok(())
}

/// Reconcile a just-closed document's index entry to disk and re-publish.
///
/// On `textDocument/didClose` the editor discards the buffer and content
/// authority reverts to the filesystem (LSP spec, issue 046). Re-read `uri`
/// from disk via [`Workspace::update`] — which re-parses the file, or drops it
/// if it is gone — so a buffer closed with unsaved edits leaves no discarded
/// content indexed; disk never changed, so no watcher event would correct it.
/// Re-publishes through the graph-aware [`publish_all_diagnostics`] path,
/// naming the URI: reverting from buffer to disk content can move cross-file
/// backlink/forward edges, so this mirrors the watched-file `changed` branch
/// for closed files. A rootless document (issue 051) is simply dropped — it has
/// no disk-backed root to revert to and published no diagnostics to clear.
fn reconcile_closed_document(
    connection: &Connection,
    workspaces: &mut Workspaces,
    uri: &str,
) -> Result<()> {
    let abs = uri_to_path(uri);
    match workspaces
        .documents
        .get(&abs)
        .map(|doc| doc.primary_root.is_some())
    {
        Some(true) => {
            // Rooted: content authority reverts to the filesystem — reconcile
            // to disk (reparse, or drop if gone) and re-publish.
            workspaces.update_from_disk(&abs);
            publish_all_diagnostics(connection, workspaces, &one_uri(uri))
        }
        // Rootless single-file document: drop it; it published nothing.
        Some(false) => {
            workspaces.remove_single_file(uri);
            Ok(())
        }
        None => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

/// Build a one-element force-rematerialize set for the single-document callers
/// of [`publish_all_diagnostics`] / [`diff_diagnostics`] (a `didOpen` /
/// `didSave` / `didChange` / `didClose` names exactly the document it touched).
fn one_uri(uri: &str) -> HashSet<String> {
    let mut set = HashSet::with_capacity(1);
    set.insert(uri.to_string());
    set
}

/// Republish a just-opened document's current diagnostics unconditionally.
///
/// `didOpen` is a client-state boundary (decision 022): the server cannot know
/// what a client remembers about a document it just opened, so the per-URI
/// last-published record is invalidated before the sync's publish pass. That
/// forces the diff to re-send a document carrying diagnostics even when its
/// content is unchanged — closing the false-clean gap a client that drops its
/// per-URI record on reopen would otherwise hit.
///
/// A clean indexed file needs one extra step: its desired set is empty and it
/// holds no cache entry, so the diff suppresses it (an unchanged empty is not a
/// change). Push-only owes it an *explicit* empty publish there, not a skip — so
/// when the pass leaves this document with no cache entry (i.e. it is clean), an
/// empty `publishDiagnostics` is sent for it. A rootless or unindexed open
/// (issue 051) resolves to no workspace and publishes nothing, as before.
///
/// No diagnostics are recomputed beyond the ordinary publish pass.
fn republish_on_open(
    connection: &Connection,
    workspaces: &mut Workspaces,
    uri: &str,
) -> Result<()> {
    // The publish/cache key for this document when it is indexed under a root
    // (the same base `diff_diagnostics` keys the cache by). `None` for a
    // rootless or unindexed open, which publishes nothing.
    let canonical = workspaces
        .resolve(uri)
        .map(|(workspace, rel_path)| path_to_uri(&workspace.root().join(rel_path)));

    // Invalidate the last-published record so the diff re-sends the current set
    // even when the content is unchanged.
    if let Some(canonical) = &canonical {
        workspaces.published.remove(canonical);
    }

    publish_all_diagnostics(connection, workspaces, &one_uri(uri))?;

    // After the pass, a document that carries diagnostics has its cache entry
    // repopulated; a clean one has none (only non-empty entries are cached). The
    // clean file was suppressed by the diff, so send it the explicit empty
    // publish push-only owes it.
    if let Some(canonical) = canonical
        && !workspaces.published.contains_key(&canonical)
    {
        let params = lsp::PublishDiagnosticsParams {
            uri: canonical,
            diagnostics: Vec::new(),
        };
        let notif = Notification::new(lsp::method::PUBLISH_DIAGNOSTICS.to_string(), params);
        connection.sender.send(Message::Notification(notif))?;
    }

    Ok(())
}

/// Publish diagnostics for the files whose diagnostics changed.
///
/// The cheap whole-graph recompute still happens internally (see
/// [`diff_diagnostics`]), but the expensive per-diagnostic materialization and
/// the `publishDiagnostics` notifications are both restricted to the documents
/// an edit actually moved — collapsing the per-keystroke cost from `O(files)`
/// down to the handful that changed (issue 013 — publication diffing, then
/// ticket perf 02's materialization cache).
///
/// `changed_uris` names the documents whose source text just changed, if any,
/// so each one's materialization is refreshed unconditionally; see
/// [`diff_diagnostics`] for why an edited file cannot trust its cached LSP form,
/// and why a whole batch of changed files is forced together in one pass.
fn publish_all_diagnostics(
    connection: &Connection,
    workspaces: &mut Workspaces,
    changed_uris: &HashSet<String>,
) -> Result<()> {
    for (uri, diagnostics) in diff_diagnostics(workspaces, changed_uris) {
        let params = lsp::PublishDiagnosticsParams { uri, diagnostics };
        let notif = Notification::new(lsp::method::PUBLISH_DIAGNOSTICS.to_string(), params);
        connection.sender.send(Message::Notification(notif))?;
    }

    Ok(())
}

/// Publish the diagnostic delta for a single file (issue 013 — stage 2.5).
///
/// Recomputes the desired diagnostics for just `uri` and sends a
/// `publishDiagnostics` only if its vector changed. This avoids the
/// `O(workspace)` materialize/diff that [`publish_all_diagnostics`] pays every
/// sync. It is correct only when the triggering edit cannot affect any other
/// file's diagnostics — i.e. a content edit (no membership change) in the
/// structural tier — so the caller must gate on that.
fn publish_file_diagnostics(
    connection: &Connection,
    workspaces: &mut Workspaces,
    uri: &str,
) -> Result<()> {
    if let Some((uri, diagnostics)) = diff_file_diagnostics(workspaces, uri) {
        let params = lsp::PublishDiagnosticsParams { uri, diagnostics };
        let notif = Notification::new(lsp::method::PUBLISH_DIAGNOSTICS.to_string(), params);
        connection.sender.send(Message::Notification(notif))?;
    }

    Ok(())
}

/// Diff one file's freshly computed diagnostics against the last published set,
/// updating the cache and returning the `(uri, diagnostics)` to send when it
/// changed (including the transition to empty, which clears the file). Returns
/// `None` when nothing changed or the URI resolves to no workspace.
///
/// The single-file counterpart to [`diff_diagnostics`]; it touches only this
/// file's cache entry, leaving every other file's last-published set intact —
/// which is correct precisely under the structural-tier, no-membership-change
/// precondition its caller enforces.
fn diff_file_diagnostics(
    workspaces: &mut Workspaces,
    uri: &str,
) -> Option<(String, Vec<lsp::Diagnostic>)> {
    let (canonical, lattice, lsp) = {
        let (workspace, rel_path) = workspaces.resolve(uri)?;
        let canonical = path_to_uri(&workspace.root().join(&rel_path));
        let (lattice, lsp) = file_desired(&workspace, &rel_path);
        (canonical, lattice, lsp)
    };

    let unchanged = workspaces
        .published
        .get(&canonical)
        .map_or(lsp.is_empty(), |prev| prev.lsp == lsp);
    if unchanged {
        return None;
    }

    // Keep the cache invariant: only non-empty entries are stored, so an absent
    // entry means "the client currently holds none". Caching the Lattice vector
    // alongside the LSP form keeps this entry coherent with the full path's
    // change-detector (ticket perf 02).
    if lsp.is_empty() {
        workspaces.published.remove(&canonical);
    } else {
        workspaces.published.insert(
            canonical.clone(),
            PublishedDiagnostics {
                lattice,
                lsp: lsp.clone(),
            },
        );
    }

    Some((canonical, lsp))
}

/// Compute the full desired diagnostic set across all workspaces, keyed by
/// document URI, materializing every file from scratch.
///
/// Every indexed file gets an entry — an empty vector when it has no
/// diagnostics — so a caller can tell a file that just became clean apart from
/// one that left the workspace. This is the unconditional from-scratch
/// recompute that the differential tests use as their oracle; production goes
/// through [`diff_diagnostics`], which materializes only the files an edit
/// moved.
#[cfg(test)]
fn desired_diagnostics(workspaces: &Workspaces) -> BTreeMap<String, Vec<lsp::Diagnostic>> {
    let mut desired: BTreeMap<String, Vec<lsp::Diagnostic>> = BTreeMap::new();
    let empty = LineIndex::default();

    // Ascending root order: a document under overlapping folders is inserted by
    // the shallow root first and overwritten by the deepest, so the deepest
    // workspace's set wins the shared URI (matching `diff_diagnostics`).
    for root in workspaces.roots.keys() {
        let workspace = workspaces.root_view(root);
        let all_diagnostics = collect_all_diagnostics(&workspace);

        let mut by_file: BTreeMap<PathBuf, Vec<lsp::Diagnostic>> = BTreeMap::new();
        for diag in &all_diagnostics {
            let fd = workspace.file(&diag.file);
            let source = fd.map_or("", |fd| fd.tree.source());
            let index = fd.map_or(&empty, |fd| &fd.line_index);
            by_file
                .entry(diag.file.clone())
                .or_default()
                .push(to_lsp_diagnostic(diag, source, index));
        }

        for rel_path in workspace.files().keys() {
            let uri = path_to_uri(&root.join(rel_path));
            let diagnostics = by_file.remove(rel_path).unwrap_or_default();
            desired.insert(uri, diagnostics);
        }
    }

    desired
}

/// Diff the freshly computed diagnostics against the last-published set,
/// returning only the `(uri, diagnostics)` pairs that must be sent and updating
/// the cache to match.
///
/// Runs the cheap whole-graph recompute ([`collect_all_diagnostics`]) and then,
/// per file, compares the new Lattice diagnostic vector against the cached one.
/// A file whose Lattice vector is unchanged keeps its cached materialization
/// untouched — the expensive UTF-16 [`to_lsp_diagnostic`] pass runs only for the
/// files the recompute shows actually moved — so a graph-tier (`.lattice.toml`)
/// sync no longer re-materializes every file on every keystroke. Detection,
/// not prediction: the whole-graph recompute already reflects cross-file edges
/// (a missing backlink reported on the *source*), so a dependent file an edit
/// touched only indirectly is caught the same way (issue 013 — ticket perf 02).
///
/// `changed_uris` names the documents whose source text just changed, if any.
/// Each is force-re-materialized unconditionally: a length-preserving edit can
/// leave the Lattice vector byte-identical yet shift a span's UTF-16 column (an
/// astral-plane swap upstream of the span on its line), so the cached LSP form
/// cannot be trusted for an edited file even when its Lattice vector matches.
/// Every *other* file's source is unchanged, so Lattice-vector equality there
/// does guarantee an identical materialization. Passing a set (rather than a
/// single URI) lets one pass force-re-materialize a whole batch of changed
/// files — a bulk on-disk change reconciles all of them in one O(workspace)
/// recompute instead of one per file (ticket perf 07). Pass an empty set when
/// no single file's text changed (e.g. a workspace-folder add/remove — newly
/// scanned files are cache misses and re-materialize regardless).
///
/// A pair is sent when its materialized vector differs from what the client last
/// received, including the transition to empty — a file that became clean, or
/// one that left the workspace (deleted, or its folder removed) — so stale
/// diagnostics are cleared. Only non-empty entries are cached, so an absent
/// entry means "the client currently holds none". The result is sorted by URI
/// for deterministic output.
fn diff_diagnostics(
    workspaces: &mut Workspaces,
    changed_uris: &HashSet<String>,
) -> Vec<(String, Vec<lsp::Diagnostic>)> {
    // A file the detector decided to (re-)materialize: its fresh Lattice and LSP
    // vectors, plus whether the LSP form differs from what the client holds.
    struct Materialized {
        uri: String,
        lattice: Vec<Diagnostic>,
        lsp: Vec<lsp::Diagnostic>,
        send: bool,
    }

    // Count this whole-workspace recompute pass so tests can assert that a
    // batched watched-file notification collapses to one pass, not one per
    // changed file (ticket perf 07). Compiled out of release builds.
    #[cfg(test)]
    RECOMPUTE_COUNT.with(|count| count.set(count.get() + 1));

    // Canonicalize each changed URI to the form the cache is keyed by, so the
    // force-re-materialize check below lines up with the per-file URIs. A
    // document's canonical form joins its primary root's canonical scan path
    // (which differs from the client-supplied folder key only under a symlink —
    // issue 047).
    let changed_canonical: HashSet<String> = changed_uris
        .iter()
        .filter_map(|uri| {
            let abs = uri_to_path(uri);
            let doc = workspaces.documents.get(&abs)?;
            let root = doc.primary_root.as_ref()?;
            let meta = workspaces.roots.get(root)?;
            let rel = abs.strip_prefix(root).ok()?;
            Some(path_to_uri(&meta.canonical_root.join(rel)))
        })
        .collect();

    let mut materialized: Vec<Materialized> = Vec::new();
    let mut present: HashSet<String> = HashSet::new();

    // Phase 1 — detection. With an immutable view of the store and the published
    // cache, recompute each file's Lattice vector, decide whether it changed,
    // and materialize only the changed files. Collect owned results so the cache
    // can be mutated afterward.
    {
        let published = &workspaces.published;
        // Fallback index for the defensive unindexed-file path; real files use
        // their own cached `line_index`.
        let empty = LineIndex::default();

        // Deepest root first (reverse key order), and each absolute URI is
        // claimed by the first (deepest) root that indexes it: nested roots
        // range-scan the same absolute file, and letting both compute the same
        // publish-cache key makes successive passes alternate between the two
        // roots' diagnostic sets — the deeper one's vector one pass, the
        // shallower's the next (issue 050's flip-flop shape). The deepest root
        // owning the URI matches how `resolve` routes document events and how
        // the test oracle `desired_diagnostics` settles the same URI.
        for (root, meta) in workspaces.roots.iter().rev() {
            let workspace = workspaces.root_view(root);
            let mut by_file: BTreeMap<PathBuf, Vec<Diagnostic>> = BTreeMap::new();
            for diag in collect_all_diagnostics(&workspace) {
                by_file.entry(diag.file.clone()).or_default().push(diag);
            }

            // The publish/cache URI is keyed by the client-supplied folder path
            // (`root`); the force-re-materialize check lines up with
            // `changed_canonical`, derived from the root's canonical scan path.
            // The two bases coincide unless the client opened the folder through
            // a symlink; when they differ, the comparison is run on the
            // canonical root so a moved diagnostic is not skipped (issue 047).
            let root_is_canonical = root.as_path() == meta.canonical_root.as_path();

            for rel_path in workspace.files().keys() {
                let uri = path_to_uri(&root.join(rel_path));
                if !present.insert(uri.clone()) {
                    // Already claimed by a deeper root in this pass.
                    continue;
                }

                let lattice = by_file.remove(rel_path).unwrap_or_default();
                let cached = published.get(&uri);
                let force = if root_is_canonical {
                    changed_canonical.contains(&uri)
                } else {
                    changed_canonical.contains(&path_to_uri(&meta.canonical_root.join(rel_path)))
                };

                // Reuse the cached materialization when this file's source is
                // unchanged (it is not the edited file) and its Lattice vector
                // still matches what produced the cached LSP form.
                if !force {
                    match cached {
                        Some(prev) if prev.lattice == lattice => continue,
                        None if lattice.is_empty() => continue,
                        _ => {}
                    }
                }

                let fd = workspace.file(rel_path);
                let source = fd.map_or("", |fd| fd.tree.source());
                let index = fd.map_or(&empty, |fd| &fd.line_index);
                let lsp: Vec<lsp::Diagnostic> = lattice
                    .iter()
                    .map(|d| to_lsp_diagnostic(d, source, index))
                    .collect();
                let send = cached.map_or(!lsp.is_empty(), |prev| prev.lsp != lsp);
                materialized.push(Materialized {
                    uri,
                    lattice,
                    lsp,
                    send,
                });
            }
        }
    }

    // Keyed by URI so the result is deterministically ordered.
    let mut to_send: BTreeMap<String, Vec<lsp::Diagnostic>> = BTreeMap::new();

    // Phase 2 — apply. Update only the changed entries in place; untouched files
    // keep their cache entries, so this stays O(changed), not O(workspace).
    for entry in materialized {
        if entry.send {
            to_send.insert(entry.uri.clone(), entry.lsp.clone());
        }
        if entry.lsp.is_empty() {
            workspaces.published.remove(&entry.uri);
        } else {
            workspaces.published.insert(
                entry.uri,
                PublishedDiagnostics {
                    lattice: entry.lattice,
                    lsp: entry.lsp,
                },
            );
        }
    }

    // Phase 3 — clear files that left the workspace (cached but no longer
    // present): send an empty vector and drop the entry.
    let absent: Vec<String> = workspaces
        .published
        .keys()
        .filter(|uri| !present.contains(uri.as_str()))
        .cloned()
        .collect();
    for uri in absent {
        workspaces.published.remove(&uri);
        to_send.insert(uri, Vec::new());
    }

    to_send.into_iter().collect()
}

// Counts `to_lsp_diagnostic` calls so tests can assert that an incremental
// publish re-materializes only the files whose diagnostics changed, rather than
// the whole workspace (ticket perf 02 acceptance). Compiled out of release
// builds, so the hot path pays nothing.
#[cfg(test)]
thread_local! {
    static MATERIALIZE_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

// Counts `diff_diagnostics` invocations — one per whole-workspace recompute /
// publish pass — so tests can assert that a batched watched-file notification
// collapses N changed files into a single pass, not N (ticket perf 07).
// Compiled out of release builds.
#[cfg(test)]
thread_local! {
    static RECOMPUTE_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

// Counts `recompute_all_structural` sweeps — the O(workspace) structural-cache
// pass a membership change forces — so tests can assert which store mutations
// pay it (a rootless open must not). Compiled out of release builds.
#[cfg(test)]
thread_local! {
    static STRUCTURAL_SWEEP_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Convert a Lattice diagnostic to an LSP diagnostic.
///
/// Builds the range from the diagnostic's byte span when present (precise
/// underline); otherwise falls back to a whole-line range anchored on
/// `diag.line`. `source` is the text of the file the diagnostic belongs to and
/// `index` is that file's cached [`LineIndex`], through which the byte→position
/// conversion is routed (ticket perf 01).
fn to_lsp_diagnostic(diag: &Diagnostic, source: &str, index: &LineIndex) -> lsp::Diagnostic {
    #[cfg(test)]
    MATERIALIZE_COUNT.with(|count| count.set(count.get() + 1));

    let severity = match diag.severity {
        Severity::Error => lsp::diagnostic_severity::ERROR,
        Severity::Warning => lsp::diagnostic_severity::WARNING,
        Severity::Info => lsp::diagnostic_severity::INFORMATION,
        Severity::Hint => lsp::diagnostic_severity::HINT,
    };

    let range = diag.span.map_or_else(
        || whole_line_range(source, index, diag.line),
        |span| span_to_lsp_range(source, index, &span),
    );

    lsp::Diagnostic {
        range,
        severity: Some(severity),
        source: Some("lattice".to_string()),
        message: diag.message.clone(),
    }
}

/// An LSP range covering an entire line's content (column 0 to the line's end,
/// excluding the terminator). Used for diagnostics that carry only a line
/// anchor, so the underline at least covers the line instead of a zero-width
/// point at column 0. The two endpoint conversions route through the file's
/// cached [`LineIndex`].
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
fn whole_line_range(source: &str, index: &LineIndex, line: usize) -> lsp::Range {
    let (start, end) = line_byte_range(source, line.saturating_sub(1) as u32);
    lsp::Range {
        start: index.position(source, start),
        end: index.position(source, end),
    }
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

    use super::*;
    use crate::block::{HeadingId, Syntax};
    use crate::span::Span;

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    /// Build a test heading with default `text_span` and `syntax` fields.
    fn test_heading(line: usize, level: u8, text: &str, id: HeadingId) -> Heading {
        Heading {
            line,
            level,
            text: text.to_string(),
            id,
            text_span: Span::new(0, 0),
            syntax: Syntax::Markdown,
        }
    }

    /// Create a temp directory with `.git` marker and the given files.
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

    /// Build a `Workspaces` with one scanned folder from a temp directory.
    fn scan_workspaces(dir: &tempfile::TempDir) -> Workspaces {
        let mut workspaces = Workspaces::new();
        workspaces.add_folder(&path_to_uri(dir.path()));
        workspaces
    }

    /// Build a file URI from a temp directory and a relative path.
    fn file_uri(dir: &tempfile::TempDir, rel: &str) -> String {
        path_to_uri(&dir.path().join(rel))
    }

    // -----------------------------------------------------------------------
    // Encoding edge cases: symbol-name extraction (ticket 21)
    // -----------------------------------------------------------------------

    #[test]
    fn truncate_name_short_unchanged() {
        let name = "短い名前"; // well under the limit
        assert_eq!(
            truncate_name(name),
            name,
            "names within SYMBOL_NAME_MAX are returned verbatim"
        );
    }

    #[test]
    fn truncate_name_multibyte_boundary_is_char_safe() {
        // One ASCII byte shifts every `é` boundary to an odd byte offset, so
        // the cut at SYMBOL_NAME_MAX (60, even) lands *inside* a two-byte `é`
        // and truncation must back off to byte 59.
        let name = format!("a{}", "é".repeat(40)); // 1 + 80 = 81 bytes
        assert!(
            !name.is_char_boundary(SYMBOL_NAME_MAX),
            "test setup: byte 60 must fall mid-character"
        );
        let truncated = truncate_name(&name);
        assert!(
            std::str::from_utf8(truncated.as_bytes()).is_ok(),
            "truncated name must remain valid UTF-8"
        );
        assert_eq!(
            truncated,
            format!("a{}…", "é".repeat(29)),
            "cut retreats to a char boundary: 'a' + 29 whole 'é' + ellipsis"
        );
    }

    #[test]
    fn truncate_name_emoji_boundary_is_char_safe() {
        // 59 ASCII bytes place the first 4-byte emoji across bytes 59..63, so
        // the cut at byte 60 is mid-emoji and must retreat to byte 59.
        let name = format!("{}{}", "a".repeat(59), "😀".repeat(5)); // 59 + 20 = 79 bytes
        assert!(
            !name.is_char_boundary(SYMBOL_NAME_MAX),
            "test setup: byte 60 must fall mid-emoji"
        );
        let truncated = truncate_name(&name);
        assert!(
            std::str::from_utf8(truncated.as_bytes()).is_ok(),
            "truncated emoji name must remain valid UTF-8"
        );
        assert_eq!(
            truncated.matches('😀').count(),
            0,
            "the split emoji is dropped entirely, never emitted as partial bytes"
        );
        assert_eq!(
            truncated,
            format!("{}…", "a".repeat(59)),
            "cut retreats to byte 59: 59 ASCII chars + ellipsis"
        );
    }

    #[test]
    fn code_block_language_multibyte() {
        assert_eq!(
            code_block_language("```日本語\ncode\n```").as_deref(),
            Some("日本語"),
            "a multi-byte fence info string yields the full language tag"
        );
    }

    #[test]
    fn code_block_language_multibyte_crlf() {
        assert_eq!(
            code_block_language("```日本語\r\ncode\r\n```").as_deref(),
            Some("日本語"),
            "CRLF after a multi-byte info string does not corrupt the language tag"
        );
    }

    #[test]
    fn code_block_language_bare_cr() {
        // A pure bare-CR document: the language tag must not fold in the
        // following lines (`str::lines` would not break on the bare CR).
        assert_eq!(
            code_block_language("```rust\rcode\r```").as_deref(),
            Some("rust"),
            "bare CR after the info string does not fold later lines into the tag"
        );
    }

    #[test]
    fn container_tag_name_bare_cr() {
        assert_eq!(
            container_tag_name("<details>\rmore\r</details>"),
            "details",
            "bare CR after the opening tag does not corrupt the tag name"
        );
    }

    #[test]
    fn list_item_text_multibyte() {
        let tree = crate::block::parse_tree("- 日本語 café 😀\n", None);
        let item = tree
            .nodes()
            .iter()
            .enumerate()
            .find(|(_, n)| matches!(n.kind, ElementKind::ListItem { .. }))
            .map(|(id, _)| id)
            .expect("a list item node should exist");
        assert_eq!(
            list_item_text(&tree, item),
            "日本語 café 😀",
            "multi-byte list item text is extracted intact"
        );
    }

    // -----------------------------------------------------------------------
    // Encoding edge cases: LSP position conversion across line endings
    // -----------------------------------------------------------------------

    #[test]
    fn lsp_position_crlf_round_trips() {
        let src = "ab\r\ncd\r\nef"; // c@4, e@8
        let p = byte_offset_to_lsp_position(src, 4);
        assert_eq!(
            (p.line, p.character),
            (1, 0),
            "byte 4 is line 1 col 0 under CRLF (the pair is one break)"
        );
        assert_eq!(
            lsp_position_to_byte_offset(src, p),
            4,
            "position → offset round-trips under CRLF"
        );
        assert_eq!(
            source_line_at(src, 1),
            "cd",
            "line 1 text excludes the CRLF"
        );
        assert_eq!(source_line_at(src, 2), "ef", "last line under CRLF");
    }

    #[test]
    fn lsp_position_bare_cr_round_trips() {
        // a@0 b@1 \r@2 c@3 d@4 \r@5 e@6 f@7
        let src = "ab\rcd\ref";
        let p = byte_offset_to_lsp_position(src, 3);
        assert_eq!((p.line, p.character), (1, 0), "bare CR starts a new line");
        assert_eq!(
            lsp_position_to_byte_offset(src, p),
            3,
            "position → offset round-trips under bare CR"
        );
        let p2 = byte_offset_to_lsp_position(src, 7);
        assert_eq!((p2.line, p2.character), (2, 1), "f is line 2 col 1");
        assert_eq!(source_line_at(src, 2), "ef", "bare-CR line text");
    }

    #[test]
    fn lsp_character_is_utf16_offset_within_line() {
        // é is two UTF-8 bytes but one UTF-16 unit, so 'b' sits at byte 4 of
        // line 0 yet UTF-16 column 3 (a, é, space).
        let src = "aé b\nx";
        let p = byte_offset_to_lsp_position(src, 4);
        assert_eq!(
            (p.line, p.character),
            (0, 3),
            "character is a UTF-16 code-unit offset within the line"
        );
        assert_eq!(
            lsp_position_to_byte_offset(src, p),
            4,
            "the UTF-16 column maps back to the byte offset"
        );
        // 'x' sits at byte 6 (after the LF); byte 5 is the LF itself.
        assert_eq!(
            byte_offset_to_lsp_position(src, 6).line,
            1,
            "the LF still advances to line 1"
        );
    }

    #[test]
    fn line_byte_range_past_eof_is_empty() {
        let src = "only\none\r\n";
        let (start, end) = line_byte_range(src, 99);
        assert_eq!(
            (start, end),
            (src.len(), src.len()),
            "a line past EOF yields an empty range at the end"
        );
    }

    /// Property: byte offset → LSP position → byte offset round-trips for any
    /// source and any char-boundary offset, across `\n` / `\r\n` / bare `\r`
    /// and multi-byte content. Pins the new line/column machinery generatively.
    #[allow(
        clippy::wildcard_imports,
        reason = "proptest's prelude is its conventional import"
    )]
    mod position_props {
        use super::super::{byte_offset_to_lsp_position, lsp_position_to_byte_offset};
        use proptest::prelude::*;

        /// Strings mixing ASCII, 2/3/4-byte characters, and `\n`/`\r` in any
        /// arrangement (so `\r\n`, bare `\r`, and bare `\n` all occur).
        fn position_source() -> impl Strategy<Value = String> {
            proptest::collection::vec(
                prop_oneof![
                    (b'a'..=b'z').prop_map(char::from),
                    Just('é'),
                    Just('日'),
                    Just('🎉'),
                    Just('\n'),
                    Just('\r'),
                ],
                0..40,
            )
            .prop_map(|cs| cs.into_iter().collect())
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(512))]

            #[test]
            fn lsp_position_byte_round_trips(src in position_source(), seed in any::<usize>()) {
                let mut off = seed % (src.len() + 1);
                while !src.is_char_boundary(off) {
                    off -= 1;
                }
                // Skip the one degenerate case: an offset strictly inside a
                // `\r\n` pair, which is not a stable round-trip point.
                let b = src.as_bytes();
                prop_assume!(!(off > 0 && b[off - 1] == b'\r' && b.get(off) == Some(&b'\n')));

                let pos = byte_offset_to_lsp_position(&src, off);
                prop_assert_eq!(lsp_position_to_byte_offset(&src, pos), off);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Existing tests: diagnostics and document symbols
    // -----------------------------------------------------------------------

    #[test]
    fn error_maps_to_lsp_error() {
        // A spanned diagnostic underlines exactly its byte span. "zzzz" is on
        // line 3 (LSP line 2) at bytes 4..8.
        let source = "x\ny\nzzzz\n";
        let diag = Diagnostic {
            file: PathBuf::from("a.md"),
            line: 3,
            severity: Severity::Error,
            message: "target does not exist".to_string(),
            span: Some(Span::new(4, 8)),
        };
        let d = to_lsp_diagnostic(&diag, source, &LineIndex::new(source));
        assert_eq!(
            d.severity,
            Some(lsp::diagnostic_severity::ERROR),
            "error should map to LSP ERROR"
        );
        assert_eq!(d.range.start.line, 2, "line 3 should map to LSP line 2");
        assert_eq!(d.range.start.character, 0, "span starts at column 0");
        assert_eq!(d.range.end.character, 4, "span covers the four z's");
        assert_eq!(
            d.source.as_deref(),
            Some("lattice"),
            "source should be lattice"
        );
    }

    #[test]
    fn warning_maps_to_lsp_warning() {
        // A line-only diagnostic (span: None) underlines the whole line.
        let source = "first line\nsecond line\n";
        let diag = Diagnostic {
            file: PathBuf::from("b.md"),
            line: 1,
            severity: Severity::Warning,
            message: "missing backlink".to_string(),
            span: None,
        };
        let d = to_lsp_diagnostic(&diag, source, &LineIndex::new(source));
        assert_eq!(
            d.severity,
            Some(lsp::diagnostic_severity::WARNING),
            "warning should map to LSP WARNING"
        );
        assert_eq!(d.range.start.line, 0, "line 1 should map to LSP line 0");
        assert_eq!(
            d.range.start.character, 0,
            "whole-line range starts at column 0"
        );
        assert_eq!(
            d.range.end.character, 10,
            "whole-line range ends at the line's length"
        );
    }

    #[test]
    fn info_maps_to_lsp_information() {
        let source = "note\n";
        let diag = Diagnostic {
            file: PathBuf::from("c.md"),
            line: 1,
            severity: Severity::Info,
            message: "no explicit predicate".to_string(),
            span: None,
        };
        let d = to_lsp_diagnostic(&diag, source, &LineIndex::new(source));
        assert_eq!(
            d.severity,
            Some(lsp::diagnostic_severity::INFORMATION),
            "info should map to LSP INFORMATION"
        );
    }

    #[test]
    fn heading_symbols_nest_by_level() {
        let tagged = vec![
            TaggedSymbol {
                level: 1,
                symbol: lsp::DocumentSymbol {
                    name: "Title".to_string(),
                    detail: None,
                    kind: lsp::symbol_kind::CLASS,
                    range: lsp::Range::default(),
                    selection_range: lsp::Range::default(),
                    children: None,
                },
            },
            TaggedSymbol {
                level: 2,
                symbol: lsp::DocumentSymbol {
                    name: "Section".to_string(),
                    detail: None,
                    kind: lsp::symbol_kind::CLASS,
                    range: lsp::Range::default(),
                    selection_range: lsp::Range::default(),
                    children: None,
                },
            },
            TaggedSymbol {
                level: 2,
                symbol: lsp::DocumentSymbol {
                    name: "Another".to_string(),
                    detail: None,
                    kind: lsp::symbol_kind::CLASS,
                    range: lsp::Range::default(),
                    selection_range: lsp::Range::default(),
                    children: None,
                },
            },
        ];

        let symbols = nest_by_heading_level(tagged);
        assert_eq!(symbols.len(), 1, "should have one top-level symbol");
        assert_eq!(symbols[0].name, "Title", "top-level should be the H1");
        let children = symbols[0]
            .children
            .as_ref()
            .expect("H1 should have children");
        assert_eq!(children.len(), 2, "H1 should have two H2 children");
        assert_eq!(children[0].name, "Section", "first child should be Section");
        assert_eq!(
            children[1].name, "Another",
            "second child should be Another"
        );
    }

    #[test]
    fn uri_to_path_extracts_path() {
        let path = uri_to_path("file:///home/user/project/doc.md");
        assert_eq!(
            path,
            PathBuf::from("/home/user/project/doc.md"),
            "should extract filesystem path from URI"
        );
    }

    #[test]
    fn path_to_uri_creates_file_uri() {
        let uri = path_to_uri(Path::new("/home/user/project/doc.md"));
        assert_eq!(
            uri, "file:///home/user/project/doc.md",
            "should create file:// URI"
        );
    }

    // -----------------------------------------------------------------------
    // Helper unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn heading_at_line_finds_match() {
        let headings = vec![
            test_heading(
                1,
                1,
                "Title",
                HeadingId::Computed {
                    github: "title".to_string(),
                    gitlab: "title".to_string(),
                    vscode: "title".to_string(),
                },
            ),
            test_heading(
                5,
                2,
                "Section",
                HeadingId::Computed {
                    github: "section".to_string(),
                    gitlab: "section".to_string(),
                    vscode: "section".to_string(),
                },
            ),
        ];

        let h = heading_at_line(&headings, 0);
        assert_eq!(
            h.map(|h| h.text.as_str()),
            Some("Title"),
            "LSP line 0 should match line 1 heading"
        );

        let h = heading_at_line(&headings, 4);
        assert_eq!(
            h.map(|h| h.text.as_str()),
            Some("Section"),
            "LSP line 4 should match line 5 heading"
        );

        assert!(
            heading_at_line(&headings, 2).is_none(),
            "no heading on line 3"
        );
    }

    #[test]
    fn heading_matches_explicit_fragment() {
        let heading = test_heading(1, 1, "Custom ID", HeadingId::Explicit("my-id".to_string()));
        assert!(
            heading_matches_fragment(&heading, "my-id"),
            "explicit ID should match"
        );
        assert!(
            !heading_matches_fragment(&heading, "custom-id"),
            "slug should not match explicit ID"
        );
    }

    #[test]
    fn heading_matches_computed_fragments() {
        let heading = test_heading(
            1,
            2,
            "Hello World!",
            HeadingId::Computed {
                github: "hello-world".to_string(),
                gitlab: "hello-world-1".to_string(),
                vscode: "hello-world-2".to_string(),
            },
        );
        assert!(
            heading_matches_fragment(&heading, "hello-world"),
            "github slug should match"
        );
        assert!(
            heading_matches_fragment(&heading, "hello-world-1"),
            "gitlab slug should match"
        );
        assert!(
            heading_matches_fragment(&heading, "hello-world-2"),
            "vscode slug should match"
        );
        assert!(
            !heading_matches_fragment(&heading, "other"),
            "unrelated fragment should not match"
        );
    }

    #[test]
    fn enclosing_heading_finds_nearest_above() {
        let headings = vec![
            test_heading(1, 1, "Title", HeadingId::Explicit("title".to_string())),
            test_heading(5, 2, "Section", HeadingId::Explicit("section".to_string())),
        ];

        assert!(
            enclosing_heading(&headings, 1).is_none(),
            "line 1 has no enclosing heading (it IS line 1)"
        );
        assert_eq!(
            enclosing_heading(&headings, 3).map(|h| h.text.as_str()),
            Some("Title"),
            "line 3 is enclosed by Title"
        );
        assert_eq!(
            enclosing_heading(&headings, 8).map(|h| h.text.as_str()),
            Some("Section"),
            "line 8 is enclosed by Section"
        );
    }

    #[test]
    fn hierarchy_item_level_parses_detail() {
        let item = lsp::HierarchyItem {
            name: String::new(),
            kind: lsp::symbol_kind::CLASS,
            uri: String::new(),
            range: lsp::Range::default(),
            selection_range: lsp::Range::default(),
            detail: Some("H3".to_string()),
            data: None,
        };
        assert_eq!(hierarchy_item_level(&item), 3, "should parse H3 as level 3");

        let no_detail = lsp::HierarchyItem {
            detail: None,
            ..item
        };
        assert_eq!(
            hierarchy_item_level(&no_detail),
            1,
            "missing detail should default to 1"
        );
    }

    // -----------------------------------------------------------------------
    // Workspace symbols (ticket 13)
    // -----------------------------------------------------------------------

    #[test]
    fn workspace_symbols_returns_all_on_empty_query() {
        let dir = workspace_with_files(&[("a.md", "# Alpha\n## Beta\n"), ("b.md", "# Gamma\n")]);
        let workspaces = scan_workspaces(&dir);

        let symbols = workspace_symbols(&workspaces, "");
        let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"H1: Alpha"), "should contain H1: Alpha");
        assert!(names.contains(&"H2: Beta"), "should contain H2: Beta");
        assert!(names.contains(&"H1: Gamma"), "should contain H1: Gamma");
        assert_eq!(symbols.len(), 3, "should return all 3 headings");
    }

    #[test]
    fn workspace_symbols_filters_by_query() {
        let dir = workspace_with_files(&[("a.md", "# Alpha\n## Beta\n"), ("b.md", "# Gamma\n")]);
        let workspaces = scan_workspaces(&dir);

        let symbols = workspace_symbols(&workspaces, "alph");
        assert_eq!(symbols.len(), 1, "should match only Alpha");
        assert_eq!(
            symbols[0].name, "H1: Alpha",
            "should be case-insensitive match"
        );
    }

    #[test]
    fn workspace_symbols_includes_container_name() {
        let dir = workspace_with_files(&[("docs/guide.md", "# Guide\n")]);
        let workspaces = scan_workspaces(&dir);

        let symbols = workspace_symbols(&workspaces, "");
        assert_eq!(symbols.len(), 1, "should find one heading");
        assert_eq!(
            symbols[0].container_name.as_deref(),
            Some("docs/guide.md"),
            "container should be the relative file path"
        );
    }

    // -----------------------------------------------------------------------
    // prepareRename / rename (ticket 04)
    // -----------------------------------------------------------------------

    #[test]
    fn prepare_rename_returns_heading_range() {
        let dir = workspace_with_files(&[("a.md", "# Title\n\nSome text\n")]);
        let workspaces = scan_workspaces(&dir);

        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "a.md"),
            },
            position: lsp::Position {
                line: 0,
                character: 0,
            },
        };
        let range = prepare_rename(&workspaces, &params).expect("should find heading");
        // "# Title" → text starts at character 2 (after "# "), length 5
        assert_eq!(range.start.character, 2, "text starts after '# '");
        assert_eq!(range.end.character, 7, "text ends at 2 + len('Title')");
    }

    #[test]
    fn prepare_rename_returns_none_on_prose() {
        let dir = workspace_with_files(&[("a.md", "# Title\n\nSome text\n")]);
        let workspaces = scan_workspaces(&dir);

        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "a.md"),
            },
            position: lsp::Position {
                line: 2,
                character: 0,
            },
        };
        assert!(
            prepare_rename(&workspaces, &params).is_none(),
            "prose line should not be renamable"
        );
    }

    #[test]
    fn rename_produces_correct_edit() {
        let dir = workspace_with_files(&[("a.md", "## Old Name\n")]);
        let workspaces = scan_workspaces(&dir);
        let uri = file_uri(&dir, "a.md");

        let params = lsp::RenameParams {
            text_document: lsp::TextDocumentIdentifier { uri: uri.clone() },
            position: lsp::Position {
                line: 0,
                character: 0,
            },
            new_name: "New Name".to_string(),
        };
        let edit = do_rename(&workspaces, &params).expect("should produce edit");
        let changes = edit.changes.expect("should have changes");
        let edits = changes.get(&uri).expect("should have edits for the file");
        assert_eq!(edits.len(), 1, "should have one edit");
        assert_eq!(edits[0].new_text, "New Name", "new text should match");
        // "## Old Name" → text starts at 3 (after "## "), length 8
        assert_eq!(edits[0].range.start.character, 3, "edit starts after '## '");
        assert_eq!(
            edits[0].range.end.character, 11,
            "edit ends at 3 + len('Old Name')"
        );
    }

    // -----------------------------------------------------------------------
    // Find references (ticket 05)
    // -----------------------------------------------------------------------

    #[test]
    fn find_references_returns_linking_files() {
        let dir = workspace_with_files(&[
            ("a.md", "# A\n\n[see B](b.md \"references\")\n"),
            ("b.md", "# B\n\nSome content\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        // Cursor on a non-heading line returns all links targeting the file.
        let params = lsp::ReferenceParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "b.md"),
            },
            position: lsp::Position {
                line: 2,
                character: 0,
            },
        };
        let locations = find_references(&workspaces, &params);
        assert_eq!(locations.len(), 1, "b.md should have one reference");
        assert!(
            locations[0].uri.ends_with("a.md"),
            "reference should come from a.md"
        );
    }

    #[test]
    fn find_references_on_heading_filters_by_fragment() {
        let dir = workspace_with_files(&[
            (
                "a.md",
                "# A\n\n[whole file](b.md \"references\")\n[section](b.md#details \"references\")\n",
            ),
            ("b.md", "# B\n\n## Details\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        // Cursor on "## Details" heading (line 3 in file, LSP line 2).
        let params = lsp::ReferenceParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "b.md"),
            },
            position: lsp::Position {
                line: 2,
                character: 0,
            },
        };
        let locations = find_references(&workspaces, &params);
        assert_eq!(
            locations.len(),
            1,
            "only the fragment link should match, not the whole-file link"
        );
    }

    #[test]
    fn find_references_no_links_returns_empty() {
        let dir = workspace_with_files(&[("a.md", "# A\n"), ("b.md", "# B\n")]);
        let workspaces = scan_workspaces(&dir);

        let params = lsp::ReferenceParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "a.md"),
            },
            position: lsp::Position {
                line: 0,
                character: 0,
            },
        };
        assert!(
            find_references(&workspaces, &params).is_empty(),
            "no links to a.md should mean empty results"
        );
    }

    // -----------------------------------------------------------------------
    // Type hierarchy (ticket 08)
    // -----------------------------------------------------------------------

    #[test]
    fn prepare_type_hierarchy_on_heading() {
        let dir = workspace_with_files(&[("a.md", "# Title\n\n## Section\n")]);
        let workspaces = scan_workspaces(&dir);

        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "a.md"),
            },
            position: lsp::Position {
                line: 2,
                character: 0,
            },
        };
        let items = prepare_type_hierarchy(&workspaces, &params).expect("should find heading");
        assert_eq!(items.len(), 1, "should return one item");
        assert_eq!(items[0].name, "Section", "should be the H2");
        assert_eq!(
            items[0].detail.as_deref(),
            Some("H2"),
            "detail should encode level"
        );
    }

    #[test]
    fn type_hierarchy_supertypes_returns_parent() {
        let dir = workspace_with_files(&[("a.md", "# Title\n\n## Section\n\n### Sub\n")]);
        let workspaces = scan_workspaces(&dir);

        // Start from the H3.
        let h3 = test_heading(
            5,
            3,
            "Sub",
            HeadingId::Computed {
                github: "sub".to_string(),
                gitlab: "sub".to_string(),
                vscode: "sub".to_string(),
            },
        );
        let h3_item = heading_to_hierarchy_item(&h3, &dir.path().join("a.md"));
        let parents =
            type_hierarchy_supertypes(&workspaces, &h3_item).expect("should return parents");
        assert_eq!(parents.len(), 1, "H3 should have one parent");
        assert_eq!(parents[0].name, "Section", "parent should be the H2");
    }

    #[test]
    fn type_hierarchy_subtypes_returns_children() {
        let dir = workspace_with_files(&[("a.md", "# Title\n\n## One\n\n## Two\n\n### Nested\n")]);
        let workspaces = scan_workspaces(&dir);

        // Start from the H1.
        let h1 = test_heading(
            1,
            1,
            "Title",
            HeadingId::Computed {
                github: "title".to_string(),
                gitlab: "title".to_string(),
                vscode: "title".to_string(),
            },
        );
        let h1_item = heading_to_hierarchy_item(&h1, &dir.path().join("a.md"));
        let children =
            type_hierarchy_subtypes(&workspaces, &h1_item).expect("should return children");
        let names: Vec<&str> = children.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["One", "Two"], "H1 children should be the H2s");
    }

    #[test]
    fn type_hierarchy_h1_has_no_supertypes() {
        let dir = workspace_with_files(&[("a.md", "# Title\n")]);
        let workspaces = scan_workspaces(&dir);

        let h1 = test_heading(
            1,
            1,
            "Title",
            HeadingId::Computed {
                github: "title".to_string(),
                gitlab: "title".to_string(),
                vscode: "title".to_string(),
            },
        );
        let h1_item = heading_to_hierarchy_item(&h1, &dir.path().join("a.md"));
        let parents =
            type_hierarchy_supertypes(&workspaces, &h1_item).expect("should return empty");
        assert!(parents.is_empty(), "H1 should have no supertypes");
    }

    // -----------------------------------------------------------------------
    // Call hierarchy (ticket 07)
    // -----------------------------------------------------------------------

    #[test]
    fn call_hierarchy_outgoing_finds_links_in_section() {
        let dir = workspace_with_files(&[
            ("a.md", "# A\n\n[see B](b.md \"references\")\n"),
            ("b.md", "# B\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        let h1 = test_heading(
            1,
            1,
            "A",
            HeadingId::Computed {
                github: "a".to_string(),
                gitlab: "a".to_string(),
                vscode: "a".to_string(),
            },
        );
        let h1_item = heading_to_hierarchy_item(&h1, &dir.path().join("a.md"));
        let calls = call_hierarchy_outgoing(&workspaces, &h1_item);
        assert_eq!(calls.len(), 1, "should have one outgoing call");
        assert_eq!(calls[0].to.name, "B", "outgoing call should target B");
    }

    #[test]
    fn call_hierarchy_incoming_finds_callers() {
        let dir = workspace_with_files(&[
            ("a.md", "# A\n\n[see B](b.md \"references\")\n"),
            ("b.md", "# B\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        let b = test_heading(
            1,
            1,
            "B",
            HeadingId::Computed {
                github: "b".to_string(),
                gitlab: "b".to_string(),
                vscode: "b".to_string(),
            },
        );
        let b_item = heading_to_hierarchy_item(&b, &dir.path().join("b.md"));
        let calls = call_hierarchy_incoming(&workspaces, &b_item);
        assert_eq!(calls.len(), 1, "should have one incoming call");
        assert_eq!(
            calls[0].from.name, "A",
            "incoming call should come from A's heading"
        );
    }

    #[test]
    fn call_hierarchy_outgoing_scoped_to_section() {
        let dir = workspace_with_files(&[
            (
                "a.md",
                "# A\n\n## S1\n\n[link1](b.md \"references\")\n\n## S2\n\n[link2](c.md \"references\")\n",
            ),
            ("b.md", "# B\n"),
            ("c.md", "# C\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        // Ask for outgoing calls from S1 only.
        let s1 = test_heading(
            3,
            2,
            "S1",
            HeadingId::Computed {
                github: "s1".to_string(),
                gitlab: "s1".to_string(),
                vscode: "s1".to_string(),
            },
        );
        let s1_item = heading_to_hierarchy_item(&s1, &dir.path().join("a.md"));
        let calls = call_hierarchy_outgoing(&workspaces, &s1_item);
        assert_eq!(calls.len(), 1, "S1 should have one outgoing call");
        assert_eq!(calls[0].to.name, "B", "S1's link goes to B, not C");
    }

    #[test]
    fn call_hierarchy_incoming_uses_file_item_when_no_heading() {
        // Link appears before any heading in the file.
        let dir = workspace_with_files(&[
            ("a.md", "[see B](b.md \"references\")\n\n# A\n"),
            ("b.md", "# B\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        let b = test_heading(
            1,
            1,
            "B",
            HeadingId::Computed {
                github: "b".to_string(),
                gitlab: "b".to_string(),
                vscode: "b".to_string(),
            },
        );
        let b_item = heading_to_hierarchy_item(&b, &dir.path().join("b.md"));
        let calls = call_hierarchy_incoming(&workspaces, &b_item);
        assert_eq!(calls.len(), 1, "should have one incoming call");
        assert_eq!(
            calls[0].from.kind,
            lsp::symbol_kind::FILE,
            "caller with no enclosing heading should be a FILE item"
        );
    }

    // -----------------------------------------------------------------------
    // Document link (ticket 06)
    // -----------------------------------------------------------------------

    #[test]
    fn document_links_returns_intra_project_links() {
        let dir = workspace_with_files(&[
            ("a.md", "# A\n\n[see B](b.md \"references\")\n"),
            ("b.md", "# B\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        let links = document_links(&workspaces, &file_uri(&dir, "a.md"));
        assert_eq!(links.len(), 1, "should return one document link");
        let target = links[0].target.as_ref().expect("should have target URI");
        assert!(target.ends_with("b.md"), "target should point to b.md");
    }

    #[test]
    fn document_links_skips_external() {
        let dir =
            workspace_with_files(&[("a.md", "# A\n\n[ext](https://example.com)\n[b](b.md)\n")]);
        let workspaces = scan_workspaces(&dir);

        let links = document_links(&workspaces, &file_uri(&dir, "a.md"));
        // Only the intra-project link to b.md, not the https link.
        assert_eq!(links.len(), 1, "should skip external links");
    }

    // -----------------------------------------------------------------------
    // Hover preview (ticket 10)
    // -----------------------------------------------------------------------

    #[test]
    fn hover_on_link_shows_preview() {
        let dir = workspace_with_files(&[
            ("a.md", "# A\n\n[see B](b.md \"supersedes\")\n"),
            ("b.md", "# B\n\nFirst line.\n\nSecond line.\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "a.md"),
            },
            position: lsp::Position {
                line: 2,
                character: 0,
            },
        };
        let hover = hover_preview(&workspaces, &params).expect("should produce hover");
        assert!(
            hover.contents.value.contains("supersedes"),
            "hover should include predicate"
        );
        assert!(
            hover.contents.value.contains("# B"),
            "hover should include target content"
        );
    }

    #[test]
    fn hover_surfaces_derived_opposite_label() {
        // The hover on a forward link shows the opposite label the edge derives
        // on the target, so both ends are visible without opening it.
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            ("a.md", "# A\n\n[see B](b.md \"supersedes\")\n"),
            ("b.md", "# B\n\nFirst line.\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "a.md"),
            },
            position: lsp::Position {
                line: 2,
                character: 0,
            },
        };
        let hover = hover_preview(&workspaces, &params).expect("should produce hover");
        assert!(
            hover.contents.value.contains("derives **superseded_by**"),
            "hover should surface the derived opposite label: {}",
            hover.contents.value
        );
    }

    #[test]
    fn hover_omits_derived_label_for_unknown_predicate() {
        // An unknown predicate has no paired label, so the derives clause is
        // omitted and the authored predicate is still echoed verbatim.
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            ("a.md", "# A\n\n[see B](b.md \"bogus\")\n"),
            ("b.md", "# B\n\nFirst line.\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "a.md"),
            },
            position: lsp::Position {
                line: 2,
                character: 0,
            },
        };
        let hover = hover_preview(&workspaces, &params).expect("should produce hover");
        assert!(
            !hover.contents.value.contains("derives"),
            "unknown predicate should have no derived label: {}",
            hover.contents.value
        );
        assert!(
            hover.contents.value.contains("**bogus**"),
            "unknown predicate should still be echoed verbatim: {}",
            hover.contents.value
        );
    }

    #[test]
    fn hover_omits_derived_label_for_implicit_predicate() {
        // A plain link with no authored predicate defaults to `references`; the
        // derived clause is gated off so the common case stays terse, but the
        // hover still renders the header and preview.
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            ("a.md", "# A\n\n[see B](b.md)\n"),
            ("b.md", "# B\n\nFirst line.\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "a.md"),
            },
            position: lsp::Position {
                line: 2,
                character: 0,
            },
        };
        let hover = hover_preview(&workspaces, &params).expect("should produce hover");
        assert!(
            !hover.contents.value.contains("derives"),
            "implicit predicate should have no derived label: {}",
            hover.contents.value
        );
        assert!(
            hover.contents.value.contains("**references**"),
            "hover should still render the header for a plain link: {}",
            hover.contents.value
        );
    }

    #[test]
    fn hover_on_fragment_link_shows_heading_content() {
        let dir = workspace_with_files(&[
            ("a.md", "# A\n\n[details](b.md#details \"references\")\n"),
            ("b.md", "# B\n\nPreamble.\n\n## Details\n\nThe details.\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "a.md"),
            },
            position: lsp::Position {
                line: 2,
                character: 0,
            },
        };
        let hover = hover_preview(&workspaces, &params).expect("should produce hover");
        assert!(
            hover.contents.value.contains("## Details"),
            "hover should start from the fragment heading"
        );
    }

    #[test]
    fn hover_on_prose_returns_none() {
        let dir = workspace_with_files(&[("a.md", "# A\n\nJust text.\n")]);
        let workspaces = scan_workspaces(&dir);

        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "a.md"),
            },
            position: lsp::Position {
                line: 2,
                character: 0,
            },
        };
        assert!(
            hover_preview(&workspaces, &params).is_none(),
            "prose should not produce hover"
        );
    }

    // -----------------------------------------------------------------------
    // Folding range (ticket 11)
    // -----------------------------------------------------------------------

    #[test]
    fn folding_ranges_for_headings() {
        let dir = workspace_with_files(&[("a.md", "# Title\n\nContent\n\n## Section\n\nMore\n")]);
        let workspaces = scan_workspaces(&dir);

        let ranges = folding_ranges(&workspaces, &file_uri(&dir, "a.md"));
        assert!(
            ranges.len() >= 2,
            "should have folding ranges for H1 and H2"
        );
        // H1 should fold from line 0.
        assert_eq!(ranges[0].start_line, 0, "H1 folding should start at line 0");
    }

    #[test]
    fn folding_ranges_include_frontmatter() {
        let dir = workspace_with_files(&[(
            "a.md",
            "---\nbacklinks:\n  referenced_by:\n    - b.md\n---\n\n# Title\n",
        )]);
        let workspaces = scan_workspaces(&dir);

        let ranges = folding_ranges(&workspaces, &file_uri(&dir, "a.md"));
        let fm_range = ranges
            .iter()
            .find(|r| r.start_line == 0)
            .expect("should have frontmatter folding range");
        assert!(
            fm_range.end_line >= 4,
            "frontmatter fold should cover the --- delimiters"
        );
    }

    // -----------------------------------------------------------------------
    // Formatting (ticket 12)
    // -----------------------------------------------------------------------

    #[test]
    fn format_sorts_backlink_predicates() {
        let dir = workspace_with_files(&[(
            "a.md",
            "---\nbacklinks:\n  referenced_by:\n    - c.md\n  amended_by:\n    - b.md\n---\n\n# A\n",
        )]);
        let workspaces = scan_workspaces(&dir);

        let edits =
            format_document(&workspaces, &file_uri(&dir, "a.md")).expect("should produce edits");
        assert_eq!(edits.len(), 1, "should have one edit replacing frontmatter");
        let new_text = &edits[0].new_text;
        let amended_pos = new_text
            .find("amended_by")
            .expect("should contain amended_by");
        let referenced_pos = new_text
            .find("referenced_by")
            .expect("should contain referenced_by");
        assert!(
            amended_pos < referenced_pos,
            "amended_by should come before referenced_by (alphabetical)"
        );
    }

    #[test]
    fn format_sorts_paths_within_predicate() {
        let dir = workspace_with_files(&[(
            "a.md",
            "---\nbacklinks:\n  referenced_by:\n    - z.md\n    - a.md\n---\n\n# A\n",
        )]);
        let workspaces = scan_workspaces(&dir);

        let edits =
            format_document(&workspaces, &file_uri(&dir, "a.md")).expect("should produce edits");
        let new_text = &edits[0].new_text;
        let a_pos = new_text.find("a.md").expect("should contain a.md");
        let z_pos = new_text.find("z.md").expect("should contain z.md");
        assert!(a_pos < z_pos, "a.md should come before z.md (alphabetical)");
    }

    #[test]
    fn format_returns_none_without_backlinks() {
        let dir = workspace_with_files(&[("a.md", "# A\n\nNo frontmatter.\n")]);
        let workspaces = scan_workspaces(&dir);

        assert!(
            format_document(&workspaces, &file_uri(&dir, "a.md")).is_none(),
            "no frontmatter should mean no formatting edits"
        );
    }

    // -----------------------------------------------------------------------
    // Navigation — declaration (ticket 08)
    // -----------------------------------------------------------------------

    #[test]
    fn declaration_reference_link_goes_to_def() {
        let dir = workspace_with_files(&[(
            "a.md",
            "# A\n\n[see B][ref]\n\n[ref]: b.md \"references\"\n",
        )]);
        let workspaces = scan_workspaces(&dir);

        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "a.md"),
            },
            position: lsp::Position {
                line: 2,
                character: 5,
            },
        };
        let loc = go_to_declaration(&workspaces, &params).expect("should find declaration");
        assert!(
            loc.uri.ends_with("a.md"),
            "declaration should be in the same file"
        );
        assert_eq!(
            loc.range.start.line, 4,
            "declaration should point to the ref def line"
        );
    }

    // -----------------------------------------------------------------------
    // Symbol emission (ticket 09)
    // -----------------------------------------------------------------------

    /// Parse content and return document symbols.
    fn symbols_for(content: &str) -> Vec<lsp::DocumentSymbol> {
        let dir = workspace_with_files(&[("test.md", content)]);
        let workspaces = scan_workspaces(&dir);
        document_symbols(&workspaces, &file_uri(&dir, "test.md")).expect("should produce symbols")
    }

    /// Recursively find all symbols matching a predicate.
    fn find_symbols(
        syms: &[lsp::DocumentSymbol],
        pred: &dyn Fn(&lsp::DocumentSymbol) -> bool,
    ) -> Vec<lsp::DocumentSymbol> {
        let mut found = Vec::new();
        for sym in syms {
            if pred(sym) {
                found.push(sym.clone());
            }
            if let Some(children) = &sym.children {
                found.extend(find_symbols(children, pred));
            }
        }
        found
    }

    #[test]
    fn heading_emits_class_kind() {
        let syms = symbols_for("# Title\n\n## Section\n");
        assert_eq!(syms.len(), 1, "should have one top-level heading");
        assert_eq!(
            syms[0].kind,
            lsp::symbol_kind::CLASS,
            "heading should be CLASS"
        );
        assert_eq!(syms[0].name, "H1: Title", "heading name should embed level");
    }

    #[test]
    fn heading_nested_levels_in_name() {
        let syms = symbols_for("# Top\n\n## Mid\n\n### Deep\n");
        let all = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::CLASS);
        assert_eq!(all.len(), 3, "should have three headings");
        assert_eq!(all[0].name, "H1: Top", "H1 should have level in name");
        assert_eq!(all[1].name, "H2: Mid", "H2 should have level in name");
        assert_eq!(all[2].name, "H3: Deep", "H3 should have level in name");
    }

    #[test]
    fn link_emits_function_kind() {
        let syms = symbols_for("# H\n\n[text](other.md \"references\")\n");
        let links = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::FUNCTION);
        assert_eq!(links.len(), 1, "should have one link symbol");
        assert_eq!(
            links[0].name, "Link: references(other.md)",
            "link name should have Link: prefix"
        );
        assert_eq!(
            links[0].detail.as_deref(),
            Some("text"),
            "link detail should be display text"
        );
    }

    #[test]
    fn link_without_predicate_defaults_to_references() {
        let syms = symbols_for("[go](other.md)\n");
        let links = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::FUNCTION);
        assert_eq!(links.len(), 1, "should have one link");
        assert!(
            links[0].name.starts_with("Link: references("),
            "should default to references predicate"
        );
    }

    #[test]
    fn image_emits_file_kind() {
        let syms = symbols_for("![alt text](image.png)\n");
        let images = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::FILE);
        assert_eq!(images.len(), 1, "should have one image symbol");
        assert_eq!(
            images[0].name, "File: image.png",
            "image name should be File: url"
        );
        assert_eq!(
            images[0].detail.as_deref(),
            Some("image"),
            "image detail should be 'image'"
        );
    }

    #[test]
    fn ordered_list_emits_struct() {
        let syms = symbols_for("1. first\n2. second\n");
        let lists = find_symbols(&syms, &|s| {
            s.kind == lsp::symbol_kind::STRUCT && s.name.starts_with("Ordered List")
        });
        assert_eq!(lists.len(), 1, "should have one ordered list");
        assert_eq!(
            lists[0].name, "Ordered List",
            "ordered list name should be 'Ordered List'"
        );
        assert_eq!(
            lists[0].detail.as_deref(),
            Some("2"),
            "ordered list detail should be item count"
        );
    }

    #[test]
    fn unordered_list_emits_struct() {
        let syms = symbols_for("- alpha\n- beta\n");
        let lists = find_symbols(&syms, &|s| {
            s.kind == lsp::symbol_kind::STRUCT && s.name == "List"
        });
        assert_eq!(lists.len(), 1, "should have one unordered list");
        assert_eq!(
            lists[0].detail.as_deref(),
            Some("2"),
            "list detail should be item count"
        );
    }

    #[test]
    fn flat_list_has_no_children() {
        let syms = symbols_for("- alpha\n- beta\n- gamma\n");
        let lists = find_symbols(&syms, &|s| {
            s.kind == lsp::symbol_kind::STRUCT && s.name == "List"
        });
        assert_eq!(lists.len(), 1, "should have one list");
        assert!(
            lists[0].children.is_none(),
            "flat list should have no children"
        );
        assert_eq!(
            lists[0].detail.as_deref(),
            Some("3"),
            "detail should be item count"
        );
    }

    #[test]
    fn nested_list_emits_struct_children() {
        let syms = symbols_for("- parent\n  - child\n");
        let top = find_symbols(&syms, &|s| {
            s.kind == lsp::symbol_kind::STRUCT && s.name == "List"
        });
        assert_eq!(top.len(), 1, "should have one top-level list");
        assert_eq!(top[0].detail.as_deref(), Some("1"), "top list has 1 item");
        let children = top[0]
            .children
            .as_ref()
            .expect("nested list should have children");
        let sub_list = children
            .iter()
            .find(|c| c.name == "List: parent")
            .expect("should have sub-list named by parent item");
        assert_eq!(
            sub_list.kind,
            lsp::symbol_kind::STRUCT,
            "sub-list should be Struct"
        );
        assert_eq!(
            sub_list.detail.as_deref(),
            Some("1"),
            "sub-list should have item count"
        );
    }

    #[test]
    fn deeply_nested_lists_preserve_hierarchy() {
        let syms = symbols_for("- A\n  - B\n    - C\n");
        let top = find_symbols(&syms, &|s| {
            s.kind == lsp::symbol_kind::STRUCT && s.name == "List"
        });
        assert_eq!(top.len(), 1, "should have one top-level list");
        let level1 = top[0]
            .children
            .as_ref()
            .expect("should have nested children");
        assert_eq!(level1.len(), 1, "one sub-list under A");
        assert_eq!(level1[0].name, "List: A", "sub-list named by parent A");
        let level2 = level1[0]
            .children
            .as_ref()
            .expect("should have deeper nesting");
        assert_eq!(level2.len(), 1, "one sub-sub-list under B");
        assert_eq!(level2[0].name, "List: B", "sub-sub-list named by parent B");
        assert_eq!(
            level2[0].detail.as_deref(),
            Some("1"),
            "deepest list has 1 item"
        );
    }

    #[test]
    fn table_emits_struct_with_field_children() {
        let syms = symbols_for(
            "| status | issue |\n|--------|-------|\n| open | bug |\n| closed | fix |\n",
        );
        let tables = find_symbols(&syms, &|s| {
            s.kind == lsp::symbol_kind::STRUCT && s.name == "Table"
        });
        assert_eq!(tables.len(), 1, "should have one table");
        assert_eq!(
            tables[0].detail.as_deref(),
            Some("2"),
            "table detail should be data row count"
        );
        let children = tables[0]
            .children
            .as_ref()
            .expect("table should have Field children");
        assert_eq!(children.len(), 2, "should have two Field children");
        assert_eq!(
            children[0].name, "Field: status",
            "first field should be status"
        );
        assert_eq!(
            children[1].name, "Field: issue",
            "second field should be issue"
        );
        assert!(
            children.iter().all(|c| c.kind == lsp::symbol_kind::FIELD),
            "all children should be Field kind"
        );
    }

    #[test]
    fn code_block_emits_object_no_property_child() {
        let syms = symbols_for("```rust\nfn main() {}\n```\n");
        let blocks = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::OBJECT);
        assert_eq!(blocks.len(), 1, "should have one code block");
        assert_eq!(
            blocks[0].name, "CodeBlock: rust",
            "code block name should include language"
        );
        assert!(
            blocks[0].children.is_none(),
            "code block should have no children"
        );
    }

    #[test]
    fn code_block_with_title_in_detail() {
        let syms = symbols_for("```rust title=config.toml\nlet x = 1;\n```\n");
        let blocks = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::OBJECT);
        assert_eq!(blocks.len(), 1, "should have one code block");
        assert_eq!(
            blocks[0].name, "CodeBlock: rust",
            "code block name should include language"
        );
        assert_eq!(
            blocks[0].detail.as_deref(),
            Some("title=config.toml"),
            "code block detail should be the title"
        );
    }

    #[test]
    fn code_block_without_language() {
        let syms = symbols_for("```\nsome code\n```\n");
        let blocks = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::OBJECT);
        assert_eq!(blocks.len(), 1, "should have one code block");
        assert_eq!(
            blocks[0].name, "CodeBlock",
            "unnamed code block should be 'CodeBlock'"
        );
    }

    #[test]
    fn blockquote_emits_module_named_blockquote() {
        let syms = symbols_for("> quoted text\n");
        let quotes = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::MODULE);
        assert_eq!(quotes.len(), 1, "should have one blockquote");
        assert_eq!(
            quotes[0].name, "Blockquote",
            "blockquote name should be 'Blockquote'"
        );
    }

    #[test]
    fn admonition_emits_module_with_prefix() {
        let syms = symbols_for("> [!WARNING]\n> Be careful!\n");
        let modules = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::MODULE);
        assert_eq!(modules.len(), 1, "should have one admonition");
        assert_eq!(
            modules[0].name, "Admonition: WARNING",
            "admonition name should have prefix"
        );
        assert!(
            modules[0].detail.is_none(),
            "admonition should have no detail"
        );
    }

    #[test]
    fn html_div_warning_emits_admonition_module() {
        let syms = symbols_for("<div class=\"warning\">\n\nBe careful!\n\n</div>\n");
        let modules = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::MODULE);
        assert_eq!(modules.len(), 1, "should have one admonition");
        assert_eq!(
            modules[0].name, "Admonition: WARNING",
            "HTML admonition name should be the type"
        );
    }

    #[test]
    fn html_video_emits_file() {
        let syms = symbols_for("<video src=\"vid.mp4\"></video>\n");
        let files = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::FILE);
        assert_eq!(files.len(), 1, "video should emit one File symbol");
        assert_eq!(
            files[0].name, "File: vid.mp4",
            "video name should be File: url"
        );
        assert_eq!(
            files[0].detail.as_deref(),
            Some("video"),
            "detail should be 'video'"
        );
    }

    #[test]
    fn markdown_video_emits_file() {
        let syms = symbols_for("![demo](demo.mp4)\n");
        let files = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::FILE);
        assert_eq!(files.len(), 1, "video image should emit one File symbol");
        assert_eq!(
            files[0].name, "File: demo.mp4",
            "video name should be File: url"
        );
        assert_eq!(
            files[0].detail.as_deref(),
            Some("video"),
            "detail should be 'video'"
        );
    }

    #[test]
    fn html_form_control_emits_event() {
        let syms = symbols_for("<input type=\"text\">\n");
        let events = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::EVENT);
        assert_eq!(events.len(), 1, "input should emit one Event symbol");
        assert_eq!(
            events[0].name, "Form: input",
            "form control name should have Form: prefix"
        );
    }

    #[test]
    fn thematic_break_not_emitted() {
        let syms = symbols_for("# Heading\n\n---\n");
        let all = find_symbols(&syms, &|_| true);
        assert!(
            all.iter().all(|s| s.name != "Break"),
            "thematic breaks are visual separators and should not appear in the symbol list"
        );
    }

    #[test]
    fn paragraph_not_emitted_as_symbol() {
        let syms = symbols_for("Just a paragraph.\n");
        let all = find_symbols(&syms, &|_| true);
        assert!(
            all.iter().all(|s| s.name != "Just a paragraph."),
            "paragraphs should not be emitted as symbols"
        );
    }

    #[test]
    fn paragraph_links_float_up() {
        let syms = symbols_for("# Section\n\nSee [link](other.md) for details.\n");
        let links = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::FUNCTION);
        assert_eq!(links.len(), 1, "floated link should be emitted");
    }

    #[test]
    fn scope_boundary_headings_are_flat() {
        let content = "# Top\n\n> ## Quoted heading\n>\n> text\n\n## After\n";
        let syms = symbols_for(content);
        assert_eq!(syms.len(), 1, "should have one top-level heading");
        let top_children = syms[0].children.as_ref().expect("Top should have children");

        let after = top_children
            .iter()
            .find(|s| s.name == "H2: After")
            .expect("'H2: After' should be a child of 'Top'");
        assert_eq!(
            after.kind,
            lsp::symbol_kind::CLASS,
            "After should be a heading"
        );

        let quote = top_children
            .iter()
            .find(|s| s.kind == lsp::symbol_kind::MODULE)
            .expect("blockquote should be a child of Top");
        let inner_headings = find_symbols(quote.children.as_deref().unwrap_or(&[]), &|s| {
            s.kind == lsp::symbol_kind::CLASS
        });
        assert_eq!(
            inner_headings.len(),
            1,
            "quoted heading should be inside the blockquote"
        );
    }

    #[test]
    fn non_heading_symbols_nest_under_heading() {
        let tagged = vec![
            TaggedSymbol {
                level: 1,
                symbol: lsp::DocumentSymbol {
                    name: "Title".to_string(),
                    detail: None,
                    kind: lsp::symbol_kind::CLASS,
                    range: lsp::Range::default(),
                    selection_range: lsp::Range::default(),
                    children: None,
                },
            },
            TaggedSymbol {
                level: 0,
                symbol: lsp::DocumentSymbol {
                    name: "link".to_string(),
                    detail: None,
                    kind: lsp::symbol_kind::FUNCTION,
                    range: lsp::Range::default(),
                    selection_range: lsp::Range::default(),
                    children: None,
                },
            },
        ];

        let symbols = nest_by_heading_level(tagged);
        assert_eq!(
            symbols.len(),
            1,
            "non-heading symbol should nest under heading"
        );
        let children = symbols[0]
            .children
            .as_ref()
            .expect("heading should have children");
        assert_eq!(
            children[0].kind,
            lsp::symbol_kind::FUNCTION,
            "child should be the non-heading symbol"
        );
    }

    #[test]
    fn workspace_symbols_includes_links() {
        let dir = workspace_with_files(&[("a.md", "# Title\n\n[go](b.md)\n"), ("b.md", "# B\n")]);
        let workspaces = scan_workspaces(&dir);
        let syms = workspace_symbols(&workspaces, "");
        assert!(
            syms.iter().any(|s| s.kind == lsp::symbol_kind::FUNCTION),
            "workspace symbols should include links"
        );
    }

    #[test]
    fn workspace_symbols_query_filters_new_kinds() {
        let dir = workspace_with_files(&[("a.md", "# Title\n\n## Section\n")]);
        let workspaces = scan_workspaces(&dir);
        let syms = workspace_symbols(&workspaces, "Section");
        assert_eq!(syms.len(), 1, "query should filter to matching symbols");
        assert_eq!(
            syms[0].name, "H2: Section",
            "should match the section heading"
        );
    }

    #[test]
    fn footnote_def_emits_constant() {
        let syms = symbols_for("text[^1]\n\n[^1]: footnote content\n");
        let footnotes = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::CONSTANT);
        assert_eq!(footnotes.len(), 1, "should have one footnote definition");
        assert_eq!(
            footnotes[0].name, "Footnote: [^1]",
            "footnote name should have prefix and label"
        );
    }

    #[test]
    fn heading_nesting_deep() {
        let tagged = vec![
            TaggedSymbol {
                level: 1,
                symbol: lsp::DocumentSymbol {
                    name: "H1".to_string(),
                    detail: None,
                    kind: lsp::symbol_kind::CLASS,
                    range: lsp::Range::default(),
                    selection_range: lsp::Range::default(),
                    children: None,
                },
            },
            TaggedSymbol {
                level: 2,
                symbol: lsp::DocumentSymbol {
                    name: "H2".to_string(),
                    detail: None,
                    kind: lsp::symbol_kind::CLASS,
                    range: lsp::Range::default(),
                    selection_range: lsp::Range::default(),
                    children: None,
                },
            },
            TaggedSymbol {
                level: 3,
                symbol: lsp::DocumentSymbol {
                    name: "H3".to_string(),
                    detail: None,
                    kind: lsp::symbol_kind::CLASS,
                    range: lsp::Range::default(),
                    selection_range: lsp::Range::default(),
                    children: None,
                },
            },
        ];

        let symbols = nest_by_heading_level(tagged);
        assert_eq!(symbols.len(), 1, "should have one top-level H1");
        let h2 = &symbols[0].children.as_ref().expect("H1 children")[0];
        assert_eq!(h2.name, "H2", "child should be H2");
        let h3 = &h2.children.as_ref().expect("H2 children")[0];
        assert_eq!(h3.name, "H3", "grandchild should be H3");
    }

    #[test]
    fn no_freed_symbol_kinds_emitted() {
        // Verify freed kinds (String=15, Key=20, Array=18, Enum=10,
        // EnumMember=22, Boolean=17, Property=7, Namespace=3) are never used.
        let content = "# Title\n\n[link](a.md)\n\n- item\n  - sub\n\n| A |\n|---|\n| 1 |\n\n```rust\ncode\n```\n\n---\n\n> quote\n";
        let syms = symbols_for(content);
        let all = find_symbols(&syms, &|_| true);
        let freed = [3, 7, 10, 15, 17, 18, 20, 22];
        for sym in &all {
            assert!(
                !freed.contains(&sym.kind),
                "freed SymbolKind {} should not be emitted (symbol: {})",
                sym.kind,
                sym.name,
            );
        }
    }

    #[test]
    fn blockquote_children_show_internal_structure() {
        let syms = symbols_for("> # Inner heading\n>\n> [link](a.md)\n");
        let quotes = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::MODULE);
        assert_eq!(quotes.len(), 1, "should have one blockquote");
        let children = quotes[0]
            .children
            .as_ref()
            .expect("blockquote should have children");
        assert!(
            children.iter().any(|c| c.kind == lsp::symbol_kind::CLASS),
            "blockquote should contain heading"
        );
    }

    #[test]
    fn generic_container_emits_module() {
        let syms = symbols_for("<div>\n\nContent inside.\n\n</div>\n");
        let modules = find_symbols(&syms, &|s| {
            s.kind == lsp::symbol_kind::MODULE && s.name.starts_with("Container:")
        });
        assert_eq!(modules.len(), 1, "should have one container");
        assert_eq!(
            modules[0].name, "Container: div",
            "container name should include tag"
        );
    }

    #[test]
    fn details_emits_module_with_summary() {
        // Blank line after <details> triggers parsed container mode.
        let syms = symbols_for(
            "<details>\n\n<summary>Click here</summary>\n\nHidden content.\n\n</details>\n",
        );
        let modules = find_symbols(&syms, &|s| {
            s.kind == lsp::symbol_kind::MODULE && s.name.starts_with("Details")
        });
        assert_eq!(modules.len(), 1, "should have one details");
        assert_eq!(
            modules[0].name, "Details: Click here",
            "details name should include summary"
        );
    }

    #[test]
    fn ordered_list_name_distinguishes() {
        let syms = symbols_for("1. a\n2. b\n3. c\n");
        let lists = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::STRUCT);
        assert_eq!(lists.len(), 1, "should have one list");
        assert_eq!(
            lists[0].name, "Ordered List",
            "ordered list uses 'Ordered List'"
        );
    }

    #[test]
    fn workspace_symbols_skip_nested_lists() {
        let dir = workspace_with_files(&[("a.md", "- top\n  - nested\n")]);
        let workspaces = scan_workspaces(&dir);
        let syms = workspace_symbols(&workspaces, "");
        let list_count = syms
            .iter()
            .filter(|s| s.kind == lsp::symbol_kind::STRUCT)
            .count();
        assert_eq!(
            list_count, 1,
            "workspace should only include top-level list"
        );
    }

    #[test]
    fn workspace_symbols_include_tables() {
        let dir = workspace_with_files(&[("a.md", "| A |\n|---|\n| 1 |\n")]);
        let workspaces = scan_workspaces(&dir);
        let syms = workspace_symbols(&workspaces, "");
        assert!(
            syms.iter()
                .any(|s| s.kind == lsp::symbol_kind::STRUCT && s.name == "Table"),
            "workspace should include tables"
        );
    }

    #[test]
    fn math_emits_object() {
        let syms = symbols_for("$$\nx^2 + y^2 = z^2\n$$\n");
        let blocks = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::OBJECT);
        assert_eq!(blocks.len(), 1, "should have one math block");
        assert_eq!(blocks[0].name, "Math", "math block name should be 'Math'");
    }

    #[test]
    fn import_emits_function() {
        let syms = symbols_for("@./other.md\n");
        let links = find_symbols(&syms, &|s| s.kind == lsp::symbol_kind::FUNCTION);
        assert_eq!(links.len(), 1, "should have one import");
        assert_eq!(
            links[0].name, "Link: import(./other.md)",
            "import name should have Link: import prefix"
        );
    }

    // -----------------------------------------------------------------------
    // Navigation — declaration (ticket 08, continued)
    // -----------------------------------------------------------------------

    #[test]
    fn declaration_inline_link_falls_through_to_target() {
        let dir = workspace_with_files(&[
            ("a.md", "# A\n\n[see B](b.md \"references\")\n"),
            ("b.md", "# B\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "a.md"),
            },
            position: lsp::Position {
                line: 2,
                character: 5,
            },
        };
        let loc = go_to_declaration(&workspaces, &params).expect("should fall through to target");
        assert!(
            loc.uri.ends_with("b.md"),
            "inline link declaration should go to target document"
        );
    }

    // -----------------------------------------------------------------------
    // Navigation — definition (ticket 08)
    // -----------------------------------------------------------------------

    #[test]
    fn definition_link_goes_to_target() {
        let dir = workspace_with_files(&[
            ("a.md", "# A\n\n[see B](b.md \"references\")\n"),
            ("b.md", "# B\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "a.md"),
            },
            position: lsp::Position {
                line: 2,
                character: 5,
            },
        };
        let loc = go_to_definition(&workspaces, &params).expect("should find definition");
        assert!(
            loc.uri.ends_with("b.md"),
            "definition should go to target document"
        );
    }

    #[test]
    fn definition_same_document_anchor_goes_to_heading() {
        // Issue 021: a same-document anchor navigates to its own heading.
        let dir = workspace_with_files(&[("a.md", "# A\n\n[jump](#details)\n\n## Details\n")]);
        let workspaces = scan_workspaces(&dir);

        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "a.md"),
            },
            position: lsp::Position {
                line: 2,
                character: 2,
            },
        };
        let loc = go_to_definition(&workspaces, &params)
            .expect("same-document anchor should resolve to its heading");
        assert!(
            loc.uri.ends_with("a.md"),
            "same-document anchor stays in the current file"
        );
        assert_eq!(
            loc.range.start.line, 4,
            "definition should go to the `## Details` heading line"
        );
    }

    // -----------------------------------------------------------------------
    // Navigation — type definition (ticket 08)
    // -----------------------------------------------------------------------

    #[test]
    fn type_definition_with_fragment_goes_to_heading() {
        let dir = workspace_with_files(&[
            ("a.md", "# A\n\n[details](b.md#details \"references\")\n"),
            ("b.md", "# B\n\n## Details\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "a.md"),
            },
            position: lsp::Position {
                line: 2,
                character: 5,
            },
        };
        let loc = go_to_type_definition(&workspaces, &params).expect("should find heading");
        assert!(
            loc.uri.ends_with("b.md"),
            "type definition should go to target"
        );
        assert_eq!(
            loc.range.start.line, 2,
            "type definition should go to the heading line"
        );
    }

    #[test]
    fn type_definition_without_fragment_goes_to_document() {
        let dir = workspace_with_files(&[
            ("a.md", "# A\n\n[see B](b.md \"references\")\n"),
            ("b.md", "# B\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "a.md"),
            },
            position: lsp::Position {
                line: 2,
                character: 5,
            },
        };
        // Without fragment, type definition falls through to definition.
        let loc = go_to_type_definition(&workspaces, &params)
            .expect("should fall through to target document");
        assert!(
            loc.uri.ends_with("b.md"),
            "type definition without fragment should go to target document"
        );
    }

    // -----------------------------------------------------------------------
    // Navigation — implementation (ticket 08)
    // -----------------------------------------------------------------------

    #[test]
    fn implementation_backlink_goes_to_forward_link() {
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            ("a.md", "# A\n\n[see B](b.md \"supersedes\")\n"),
            (
                "b.md",
                "---\nbacklinks:\n  superseded_by:\n    - a.md\n---\n\n# B\n",
            ),
        ]);
        let workspaces = scan_workspaces(&dir);

        // Cursor on the backlink path "    - a.md" (line 3, 0-based).
        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "b.md"),
            },
            position: lsp::Position {
                line: 3,
                character: 6,
            },
        };
        let loc = go_to_implementation(&workspaces, &params).expect("should find forward link");
        assert!(
            loc.uri.ends_with("a.md"),
            "implementation should go to the source document"
        );
        assert_eq!(
            loc.range.start.line, 2,
            "implementation should point to the forward link line"
        );
    }

    #[test]
    fn implementation_forward_label_backlink_goes_to_forward_link() {
        // Gap 1: a backlink keyed by a *forward* label (`supersedes:`) — legal
        // under decision 008 — must still resolve to the source link that
        // derives it (`a.md "superseded_by" b.md`).
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            ("a.md", "# A\n\n[see B](b.md \"superseded_by\")\n"),
            (
                "b.md",
                "---\nbacklinks:\n  supersedes:\n    - a.md\n---\n\n# B\n",
            ),
        ]);
        let workspaces = scan_workspaces(&dir);

        // Cursor on the backlink path "    - a.md" (line 3, 0-based).
        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "b.md"),
            },
            position: lsp::Position {
                line: 3,
                character: 6,
            },
        };
        let loc = go_to_implementation(&workspaces, &params)
            .expect("forward-label backlink should resolve its source link");
        assert!(
            loc.uri.ends_with("a.md"),
            "implementation should go to the source document"
        );
        assert_eq!(
            loc.range.start.line, 2,
            "implementation should point to the forward link line"
        );
    }

    #[test]
    fn implementation_body_link_goes_to_reciprocal_link() {
        // Gap 2: a body link that is one half of a reciprocal pair jumps to the
        // reciprocal link authored on the target.
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            ("a.md", "# A\n\n[to B](b.md \"superseded_by\")\n"),
            ("b.md", "# B\n\n[to A](a.md \"supersedes\")\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        // Cursor on a.md's body link (line 2, 0-based).
        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "a.md"),
            },
            position: lsp::Position {
                line: 2,
                character: 2,
            },
        };
        let loc = go_to_implementation(&workspaces, &params)
            .expect("body link should resolve its reciprocal link");
        assert!(
            loc.uri.ends_with("b.md"),
            "implementation should go to the reciprocal link's document"
        );
        assert_eq!(
            loc.range.start.line, 2,
            "implementation should point to the reciprocal link line"
        );
    }

    #[test]
    fn implementation_body_link_goes_to_frontmatter_backlink() {
        // Gap 2: a one-sided edge — the counterpart is a frontmatter backlink on
        // the target, with no reciprocal body link.
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            ("a.md", "# A\n\n[to B](b.md \"supersedes\")\n"),
            (
                "b.md",
                "---\nbacklinks:\n  superseded_by:\n    - a.md\n---\n\n# B\n",
            ),
        ]);
        let workspaces = scan_workspaces(&dir);

        // Cursor on a.md's body link (line 2, 0-based).
        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "a.md"),
            },
            position: lsp::Position {
                line: 2,
                character: 2,
            },
        };
        let loc = go_to_implementation(&workspaces, &params)
            .expect("body link should resolve its frontmatter backlink");
        assert!(
            loc.uri.ends_with("b.md"),
            "implementation should go to the target's frontmatter"
        );
        assert_eq!(
            loc.range.start.line, 2,
            "implementation should point to the `superseded_by:` key line"
        );
    }

    #[test]
    fn definition_on_body_link_stays_on_target_document() {
        // The counterpart navigation is `implementation`-only: `definition` on
        // the same body link must resolve to the target *document* (line 0), not
        // the reciprocal link.
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            ("a.md", "# A\n\n[to B](b.md \"superseded_by\")\n"),
            ("b.md", "# B\n\n[to A](a.md \"supersedes\")\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "a.md"),
            },
            position: lsp::Position {
                line: 2,
                character: 2,
            },
        };
        let loc =
            go_to_definition(&workspaces, &params).expect("definition should resolve the target");
        assert!(
            loc.uri.ends_with("b.md"),
            "definition should go to the target document"
        );
        assert_eq!(
            loc.range.start.line, 0,
            "definition should point to the document, not the reciprocal link"
        );
    }

    #[test]
    fn implementation_body_link_without_counterpart_is_none() {
        // A body link whose target carries no counterpart yields no jump rather
        // than a wrong one.
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            ("a.md", "# A\n\n[to B](b.md \"supersedes\")\n"),
            ("b.md", "# B\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "a.md"),
            },
            position: lsp::Position {
                line: 2,
                character: 2,
            },
        };
        assert!(
            go_to_implementation(&workspaces, &params).is_none(),
            "a body link without a counterpart should yield no implementation jump"
        );
    }

    #[test]
    fn implementation_backlink_resolves_file_relative_path_in_nested_dirs() {
        // Backlink paths are file-relative to the document that holds them. With
        // the source and target in different directories, the entry reads
        // `../docs/a.md` and must resolve against the target's directory — not
        // be treated as a workspace-relative path.
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            ("docs/a.md", "# A\n\n[B](../tickets/b.md \"supersedes\")\n"),
            (
                "tickets/b.md",
                "---\nbacklinks:\n  superseded_by:\n    - ../docs/a.md\n---\n\n# B\n",
            ),
        ]);
        let workspaces = scan_workspaces(&dir);

        // Cursor on the backlink path "    - ../docs/a.md" (line 3, 0-based).
        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "tickets/b.md"),
            },
            position: lsp::Position {
                line: 3,
                character: 8,
            },
        };
        let loc = go_to_implementation(&workspaces, &params)
            .expect("file-relative backlink should resolve its source link");
        assert!(
            loc.uri.ends_with("docs/a.md"),
            "implementation should resolve the source document across directories: {}",
            loc.uri
        );
        assert_eq!(
            loc.range.start.line, 2,
            "implementation should point to the forward link line"
        );
    }

    #[test]
    fn implementation_body_link_resolves_file_relative_backlink_in_nested_dirs() {
        // Gap-2 fallback across directories: the target's backlink entry is
        // file-relative (`../docs/a.md`) and must resolve against the target's
        // directory before being matched to the source.
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            ("docs/a.md", "# A\n\n[B](../tickets/b.md \"supersedes\")\n"),
            (
                "tickets/b.md",
                "---\nbacklinks:\n  superseded_by:\n    - ../docs/a.md\n---\n\n# B\n",
            ),
        ]);
        let workspaces = scan_workspaces(&dir);

        // Cursor on docs/a.md's body link (line 2, 0-based).
        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "docs/a.md"),
            },
            position: lsp::Position {
                line: 2,
                character: 1,
            },
        };
        let loc = go_to_implementation(&workspaces, &params)
            .expect("body link should resolve a file-relative frontmatter backlink");
        assert!(
            loc.uri.ends_with("tickets/b.md"),
            "implementation should resolve the target's frontmatter across directories: {}",
            loc.uri
        );
        assert_eq!(
            loc.range.start.line, 2,
            "implementation should point to the `superseded_by:` key line"
        );
    }

    // -----------------------------------------------------------------------
    // Find references from ref def (ticket 08)
    // -----------------------------------------------------------------------

    #[test]
    fn find_references_from_ref_def() {
        let dir = workspace_with_files(&[(
            "a.md",
            "# A\n\n[first][ref]\n\n[second][ref]\n\n[ref]: b.md \"references\"\n",
        )]);
        let workspaces = scan_workspaces(&dir);

        // Cursor on the ref def line (line 6, 0-based).
        let params = lsp::ReferenceParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(&dir, "a.md"),
            },
            position: lsp::Position {
                line: 6,
                character: 0,
            },
        };
        let locations = find_references(&workspaces, &params);
        assert_eq!(locations.len(), 2, "ref def should have two call sites");
    }

    // -----------------------------------------------------------------------
    // Rename with tree spans (ticket 08)
    // -----------------------------------------------------------------------

    #[test]
    fn rename_setext_heading() {
        let dir = workspace_with_files(&[("a.md", "Title\n=====\n")]);
        let workspaces = scan_workspaces(&dir);
        let uri = file_uri(&dir, "a.md");

        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier { uri: uri.clone() },
            position: lsp::Position {
                line: 0,
                character: 0,
            },
        };
        let range = prepare_rename(&workspaces, &params).expect("should find setext heading");
        assert_eq!(range.start.line, 0, "setext heading is on line 0");
        assert_eq!(range.start.character, 0, "setext heading text starts at 0");
        assert_eq!(
            range.end.character, 5,
            "setext heading text ends at len('Title')"
        );

        let rename_params = lsp::RenameParams {
            text_document: lsp::TextDocumentIdentifier { uri },
            position: lsp::Position {
                line: 0,
                character: 0,
            },
            new_name: "New Title".to_string(),
        };
        let edit = do_rename(&workspaces, &rename_params).expect("should produce edit");
        let changes = edit.changes.expect("should have changes");
        assert!(!changes.is_empty(), "should have changes for the file");
    }

    #[test]
    fn rename_html_heading() {
        let dir = workspace_with_files(&[("a.md", "<h1>Title</h1>\n")]);
        let workspaces = scan_workspaces(&dir);
        let uri = file_uri(&dir, "a.md");

        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier { uri },
            position: lsp::Position {
                line: 0,
                character: 0,
            },
        };
        let range = prepare_rename(&workspaces, &params).expect("should find HTML heading");
        assert_eq!(
            range.start.character, 4,
            "HTML heading text starts after <h1>"
        );
        assert_eq!(
            range.end.character, 9,
            "HTML heading text ends before </h1>"
        );
    }

    // -----------------------------------------------------------------------
    // Helper unit tests (ticket 08)
    // -----------------------------------------------------------------------

    #[test]
    fn lsp_position_to_byte_offset_basic() {
        let source = "line1\nline2\nline3\n";
        assert_eq!(
            lsp_position_to_byte_offset(
                source,
                lsp::Position {
                    line: 0,
                    character: 0
                }
            ),
            0,
            "start of first line"
        );
        assert_eq!(
            lsp_position_to_byte_offset(
                source,
                lsp::Position {
                    line: 1,
                    character: 0
                }
            ),
            6,
            "start of second line"
        );
        assert_eq!(
            lsp_position_to_byte_offset(
                source,
                lsp::Position {
                    line: 1,
                    character: 3
                }
            ),
            9,
            "middle of second line"
        );
    }

    #[test]
    fn span_to_lsp_range_basic() {
        let source = "# Title\n\nContent\n";
        let span = Span::new(2, 7); // "Title"
        let range = span_to_lsp_range(source, &LineIndex::new(source), &span);
        assert_eq!(range.start.line, 0, "span starts on line 0");
        assert_eq!(range.start.character, 2, "span starts at character 2");
        assert_eq!(range.end.line, 0, "span ends on line 0");
        assert_eq!(range.end.character, 7, "span ends at character 7");
    }

    // Regression: issue 003 — `character` must be a UTF-16 code-unit offset,
    // not a byte offset, on lines with multi-byte content.
    #[test]
    fn position_character_is_utf16_not_bytes() {
        // `é` is 2 UTF-8 bytes but 1 UTF-16 unit, so "header" begins at byte 8
        // yet UTF-16 column 7.
        let source = "# café header\n";
        let pos = byte_offset_to_lsp_position(source, 8);
        assert_eq!(pos.line, 0, "header is on line 0");
        assert_eq!(
            pos.character, 7,
            "UTF-16 column counts é as one code unit (byte col would be 8)"
        );
        assert_eq!(
            lsp_position_to_byte_offset(source, pos),
            8,
            "UTF-16 column 7 maps back to byte 8"
        );
    }

    #[test]
    fn position_character_counts_astral_as_two_utf16_units() {
        // 😀 (U+1F600) is 4 UTF-8 bytes and 2 UTF-16 code units.
        // 'x'=byte 0, 😀=bytes 1..5, 'y'=byte 5.
        let source = "x😀y\n";
        let pos = byte_offset_to_lsp_position(source, 5);
        assert_eq!(
            pos.character, 3,
            "x(1) + emoji(2 UTF-16 units) = column 3 at 'y'"
        );
        assert_eq!(
            lsp_position_to_byte_offset(source, pos),
            5,
            "column 3 round-trips back to byte 5"
        );
        // A column inside the surrogate pair rounds down to the emoji's start.
        let mid = lsp_position_to_byte_offset(
            source,
            lsp::Position {
                line: 0,
                character: 2,
            },
        );
        assert_eq!(
            mid, 1,
            "mid-surrogate column floors to the emoji's byte start"
        );
    }

    #[test]
    fn position_round_trip_multibyte_and_crlf() {
        // Reuses the shared invariant the property/fuzz suites assert, over
        // multi-byte content and mixed line endings.
        crate::invariants::assert_position_round_trip("# café 😀 header\r\nsecond λ line\n");
    }

    #[test]
    fn link_ref_label_inline() {
        let source = "[text](url \"title\")";
        let span = Span::new(0, source.len());
        assert!(
            link_ref_label(source, &span).is_none(),
            "inline link should not have a ref label"
        );
    }

    #[test]
    fn link_ref_label_full_reference() {
        let source = "[text][my-ref]";
        let span = Span::new(0, source.len());
        assert_eq!(
            link_ref_label(source, &span).as_deref(),
            Some("my-ref"),
            "full reference label"
        );
    }

    #[test]
    fn link_ref_label_collapsed() {
        let source = "[My Ref][]";
        let span = Span::new(0, source.len());
        assert_eq!(
            link_ref_label(source, &span).as_deref(),
            Some("my ref"),
            "collapsed reference uses link text as label"
        );
    }

    #[test]
    fn link_ref_label_shortcut() {
        let source = "[shortcut]";
        let span = Span::new(0, source.len());
        assert_eq!(
            link_ref_label(source, &span).as_deref(),
            Some("shortcut"),
            "shortcut reference uses link text as label"
        );
    }

    // -----------------------------------------------------------------------
    // Frontmatter symbols (ticket 13)
    // -----------------------------------------------------------------------

    #[test]
    fn frontmatter_emits_struct_with_field_children() {
        let content = "---\ntitle: Doc\ndate: 2026-05-24\n---\n\n# Heading\n";
        let syms = symbols_for(content);
        let fm = find_symbols(&syms, &|s| {
            s.kind == lsp::symbol_kind::STRUCT && s.name.starts_with("Frontmatter:")
        });
        assert_eq!(fm.len(), 1, "should have one frontmatter symbol");
        assert_eq!(
            fm[0].name, "Frontmatter: YAML",
            "frontmatter name includes syntax"
        );
        assert_eq!(
            fm[0].detail.as_deref(),
            Some("2"),
            "detail is top-level key count"
        );
        let children = fm[0]
            .children
            .as_ref()
            .expect("frontmatter should have children");
        assert_eq!(children.len(), 2, "should have two Field children");
        assert_eq!(
            children[0].name, "Field: title",
            "first field should be title"
        );
        assert_eq!(
            children[1].name, "Field: date",
            "second field should be date"
        );
        assert!(
            children.iter().all(|c| c.kind == lsp::symbol_kind::FIELD),
            "all children should be Field kind"
        );
    }

    #[test]
    fn frontmatter_nested_map_emits_struct_children() {
        let content = "---\nbacklinks:\n  superseded_by:\n    - decisions/38.md\n  amended_by:\n    - decisions/38.md\n    - tickets/14h.md\n---\n";
        let syms = symbols_for(content);
        let fm = find_symbols(&syms, &|s| {
            s.kind == lsp::symbol_kind::STRUCT && s.name == "Frontmatter: YAML"
        });
        assert_eq!(fm.len(), 1, "should have one frontmatter");
        let children = fm[0]
            .children
            .as_ref()
            .expect("frontmatter should have children");
        assert_eq!(children.len(), 1, "should have one child (backlinks map)");
        assert_eq!(
            children[0].name, "backlinks",
            "map child should be named by key"
        );
        assert_eq!(
            children[0].kind,
            lsp::symbol_kind::STRUCT,
            "map child should be Struct"
        );
        assert_eq!(
            children[0].detail.as_deref(),
            Some("2"),
            "map detail is child count"
        );
    }

    #[test]
    fn frontmatter_backlinks_predicates_show_source_count() {
        let content = "---\nbacklinks:\n  superseded_by:\n    - decisions/38.md\n  amended_by:\n    - decisions/38.md\n    - tickets/14h.md\n---\n";
        let syms = symbols_for(content);
        let predicates = find_symbols(&syms, &|s| {
            s.kind == lsp::symbol_kind::FIELD && s.name.starts_with("Field: ")
        });
        let superseded = predicates
            .iter()
            .find(|s| s.name == "Field: superseded_by")
            .expect("should find superseded_by");
        assert_eq!(
            superseded.detail.as_deref(),
            Some("1"),
            "superseded_by has 1 source"
        );
        let amended = predicates
            .iter()
            .find(|s| s.name == "Field: amended_by")
            .expect("should find amended_by");
        assert_eq!(
            amended.detail.as_deref(),
            Some("2"),
            "amended_by has 2 sources"
        );
    }

    #[test]
    fn frontmatter_leaf_values_not_in_outline() {
        let content = "---\ntitle: \"Hooks Primary Capture\"\ndate: 2026-05-24\nbacklinks:\n  superseded_by:\n    - decisions/38.md\n---\n";
        let syms = symbols_for(content);
        let all = find_symbols(&syms, &|_| true);
        // Values like "Hooks Primary Capture", "2026-05-24", "decisions/38.md"
        // should never appear as symbol names.
        for sym in &all {
            assert!(
                !sym.name.contains("Hooks Primary Capture"),
                "leaf values should not appear in outline"
            );
            assert!(
                !sym.name.contains("2026-05-24"),
                "date values should not appear in outline"
            );
            assert!(
                !sym.name.contains("decisions/38.md"),
                "path values should not appear in outline"
            );
        }
    }

    #[test]
    fn frontmatter_selection_range_is_precise() {
        let content = "---\ntitle: Doc\ndate: 2026-05-24\n---\n\n# Heading\n";
        let syms = symbols_for(content);
        let fields = find_symbols(&syms, &|s| {
            s.kind == lsp::symbol_kind::FIELD && s.name.starts_with("Field:")
        });
        let fm = find_symbols(&syms, &|s| s.name == "Frontmatter: YAML");
        let fm_range = &fm[0].range;
        // Each field's selection_range should NOT equal the full frontmatter range.
        for field in &fields {
            assert_ne!(
                field.selection_range, *fm_range,
                "field selection_range should not be the full frontmatter block"
            );
        }
    }

    #[test]
    fn frontmatter_workspace_symbols_top_level_only() {
        let dir = workspace_with_files(&[(
            "a.md",
            "---\ntitle: Doc\nbacklinks:\n  superseded_by:\n    - b.md\n---\n\n# H\n",
        )]);
        let workspaces = scan_workspaces(&dir);
        let ws_syms = workspace_symbols(&workspaces, "");
        let fm_count = ws_syms
            .iter()
            .filter(|s| s.name.starts_with("Frontmatter:"))
            .count();
        assert_eq!(fm_count, 1, "workspace should have one frontmatter symbol");
        // Backlink predicates should NOT appear in workspace symbols.
        assert!(
            !ws_syms.iter().any(|s| s.name.contains("superseded_by")),
            "predicate keys should not appear in workspace symbols"
        );
    }

    #[test]
    fn frontmatter_top_level_scalar_has_no_detail() {
        let content = "---\ntitle: Doc\n---\n\n# Heading\n";
        let syms = symbols_for(content);
        let fields = find_symbols(&syms, &|s| s.name == "Field: title");
        assert_eq!(fields.len(), 1, "should have title field");
        assert_eq!(
            fields[0].detail, None,
            "top-level scalar key should have no detail"
        );
    }

    #[test]
    fn frontmatter_flow_sequence_counts_items() {
        let content = "---\nbacklinks:\n  referenced_by: [a.md, b.md, c.md]\n---\n";
        let syms = symbols_for(content);
        let fields = find_symbols(&syms, &|s| s.name == "Field: referenced_by");
        assert_eq!(fields.len(), 1, "should find referenced_by field");
        assert_eq!(
            fields[0].detail.as_deref(),
            Some("3"),
            "flow sequence should count all items"
        );
    }

    // -----------------------------------------------------------------------
    // Definition list symbols
    // -----------------------------------------------------------------------

    #[test]
    fn definition_list_emits_struct_with_field_children() {
        // Blank line after <dl> triggers markdown mode so nested tags are parsed.
        let syms = symbols_for(concat!(
            "<dl>\n\n",
            "<dt>API</dt>\n\n",
            "<dd>The public interface</dd>\n\n",
            "<dt>SDK</dt>\n\n",
            "<dd>Client libraries</dd>\n\n",
            "<dt>CLI</dt>\n\n",
            "<dd>Command-line tool</dd>\n\n",
            "</dl>\n",
        ));
        let defs = find_symbols(&syms, &|s| {
            s.kind == lsp::symbol_kind::STRUCT && s.name == "Definitions"
        });
        assert_eq!(defs.len(), 1, "should have one definition list");
        assert_eq!(
            defs[0].detail.as_deref(),
            Some("3"),
            "detail should be term count"
        );
        let children = defs[0]
            .children
            .as_ref()
            .expect("definition list should have Field children");
        assert_eq!(children.len(), 3, "should have three Field children");
        assert_eq!(children[0].name, "Field: API", "first term");
        assert_eq!(children[1].name, "Field: SDK", "second term");
        assert_eq!(children[2].name, "Field: CLI", "third term");
        assert!(
            children.iter().all(|c| c.kind == lsp::symbol_kind::FIELD),
            "all children should be Field kind"
        );
    }

    #[test]
    fn definition_list_descriptions_not_in_symbols() {
        let syms = symbols_for(concat!(
            "<dl>\n\n",
            "<dt>Term</dt>\n\n",
            "<dd>Description</dd>\n\n",
            "</dl>\n",
        ));
        let descs = find_symbols(&syms, &|s| {
            s.name.contains("Description") && s.kind != lsp::symbol_kind::FIELD
        });
        assert!(
            descs.is_empty(),
            "descriptions should not appear as standalone symbols"
        );
    }

    #[test]
    fn definition_list_workspace_symbols_include_container() {
        let dir = workspace_with_files(&[("a.md", "<dl>\n\n<dt>X</dt>\n\n<dd>Y</dd>\n\n</dl>\n")]);
        let workspaces = scan_workspaces(&dir);
        let syms = workspace_symbols(&workspaces, "");
        assert!(
            syms.iter()
                .any(|s| s.kind == lsp::symbol_kind::STRUCT && s.name == "Definitions"),
            "workspace symbols should include definition list container"
        );
    }

    #[test]
    fn definition_list_terms_not_in_workspace_symbols() {
        let dir = workspace_with_files(&[(
            "a.md",
            "<dl>\n\n<dt>API</dt>\n\n<dd>Interface</dd>\n\n</dl>\n",
        )]);
        let workspaces = scan_workspaces(&dir);
        let syms = workspace_symbols(&workspaces, "");
        assert!(
            !syms.iter().any(|s| s.name.contains("API")),
            "terms should not appear in workspace symbols"
        );
    }

    // -----------------------------------------------------------------------
    // Publication diffing (issue 013)
    // -----------------------------------------------------------------------

    /// Replace a file's content in the single-workspace test set, keyed under
    /// the sole root's path.
    fn edit(workspaces: &mut Workspaces, rel: &str, content: &str) {
        let root = workspaces
            .roots
            .keys()
            .next()
            .expect("test workspace exists")
            .clone();
        workspaces.sync_document_content(&path_to_uri(&root.join(rel)), content);
    }

    #[test]
    fn diffing_first_publish_skips_clean_files() {
        // a.md has a duplicate heading slug (structural, config-independent);
        // b.md and c.md are clean.
        let dir = workspace_with_files(&[
            ("a.md", "# A\n\n# A\n"),
            ("b.md", "# B\n"),
            ("c.md", "# C\n"),
        ]);
        let mut workspaces = scan_workspaces(&dir);

        let sent = diff_diagnostics(&mut workspaces, &HashSet::new());
        assert_eq!(
            sent.len(),
            1,
            "only the file with diagnostics is published on first pass: {sent:?}"
        );
        assert_eq!(
            sent[0].0,
            file_uri(&dir, "a.md"),
            "the published file is a.md"
        );
        assert!(
            !sent[0].1.is_empty(),
            "a.md is published with its diagnostics"
        );
    }

    #[test]
    fn diffing_skips_unchanged_on_resync() {
        let dir = workspace_with_files(&[("a.md", "# A\n\n# A\n"), ("b.md", "# B\n")]);
        let mut workspaces = scan_workspaces(&dir);

        let first = diff_diagnostics(&mut workspaces, &HashSet::new());
        assert_eq!(first.len(), 1, "first pass publishes a.md");

        let second = diff_diagnostics(&mut workspaces, &HashSet::new());
        assert!(
            second.is_empty(),
            "a re-sync with no edits publishes nothing: {second:?}"
        );
    }

    #[test]
    fn diffing_resends_changed_file() {
        let dir = workspace_with_files(&[("a.md", "# A\n")]);
        let mut workspaces = scan_workspaces(&dir);

        let first = diff_diagnostics(&mut workspaces, &HashSet::new());
        assert!(first.is_empty(), "clean file publishes nothing: {first:?}");

        edit(&mut workspaces, "a.md", "# A\n\n# A\n");
        let second = diff_diagnostics(&mut workspaces, &HashSet::new());
        assert_eq!(second.len(), 1, "introducing a diagnostic republishes a.md");
        assert_eq!(
            second[0].0,
            file_uri(&dir, "a.md"),
            "the republished file is a.md"
        );
        assert!(!second[0].1.is_empty(), "vector carries the new diagnostic");
    }

    #[test]
    fn diffing_clears_file_that_became_clean() {
        let dir = workspace_with_files(&[("a.md", "# A\n\n# A\n")]);
        let mut workspaces = scan_workspaces(&dir);

        let first = diff_diagnostics(&mut workspaces, &HashSet::new());
        assert_eq!(first.len(), 1, "first pass publishes a.md's diagnostic");

        edit(&mut workspaces, "a.md", "# A\n");
        let second = diff_diagnostics(&mut workspaces, &HashSet::new());
        assert_eq!(
            second.len(),
            1,
            "fixing the file sends one clearing publish: {second:?}"
        );
        assert_eq!(
            second[0].0,
            file_uri(&dir, "a.md"),
            "the cleared file is a.md"
        );
        assert!(
            second[0].1.is_empty(),
            "the clearing publish carries an empty vector"
        );

        let third = diff_diagnostics(&mut workspaces, &HashSet::new());
        assert!(
            third.is_empty(),
            "a clean file is not re-cleared on the next sync: {third:?}"
        );
    }

    #[test]
    fn diffing_clears_removed_file() {
        let dir = workspace_with_files(&[("a.md", "# A\n\n# A\n"), ("b.md", "# B\n")]);
        let mut workspaces = scan_workspaces(&dir);

        let first = diff_diagnostics(&mut workspaces, &HashSet::new());
        assert_eq!(first.len(), 1, "first pass publishes a.md");

        // Delete a.md from disk and reconcile it out of the store.
        fs::remove_file(dir.path().join("a.md")).expect("remove a.md");
        workspaces.update_from_disk(&dir.path().join("a.md"));

        let second = diff_diagnostics(&mut workspaces, &HashSet::new());
        assert_eq!(
            second.len(),
            1,
            "removing a file sends one clearing publish: {second:?}"
        );
        assert_eq!(
            second[0].0,
            file_uri(&dir, "a.md"),
            "the removed file is cleared"
        );
        assert!(
            second[0].1.is_empty(),
            "the clearing publish carries an empty vector"
        );
    }

    #[cfg(unix)]
    #[test]
    fn diffing_force_rematerializes_under_symlinked_root() {
        // Issue 047: the force-re-materialize check must compare on a single,
        // consistent path basis. When a client opens the workspace folder through
        // a symlink, the `Workspaces` map is keyed by the symlinked (non-canonical)
        // folder path the client sent, while `Workspace::scan` canonicalizes the
        // root. An astral-plane swap upstream of a diagnostic's span leaves the
        // Lattice vector byte-identical yet shifts its UTF-16 column, so only the
        // forced re-materialization of the edited file re-publishes the corrected
        // span. With the pre-fix bug the comparison mixed the canonical (changed)
        // and client-supplied (per-file) bases, so under the symlink the force
        // missed and the moved diagnostic was never re-published.

        // Real workspace root: four ASCII chars + one trailing space yields a
        // trailing-whitespace warning whose span is bytes 4..5 (UTF-16 column 4).
        let real = workspace_with_files(&[("a.md", "aaaa \n")]);

        // A symlink to the real root: the non-canonical folder path the client
        // sends. Its string form differs from the canonical root, so the map key
        // and `workspace.root()` diverge — exactly the issue 047 condition.
        let link_parent = tempfile::tempdir().expect("create symlink parent dir");
        let link = link_parent.path().join("ws");
        std::os::unix::fs::symlink(real.path(), &link).expect("create workspace symlink");

        // Add the folder through the symlink: `add_folder` scans (canonicalizing
        // to the real root) but keys `roots`/`documents` by the symlinked path
        // the client sent, so the map key and `canonical_root` diverge — exactly
        // the issue 047 condition.
        let mut workspaces = Workspaces::new();
        workspaces.add_folder(&path_to_uri(&link));
        assert_ne!(
            workspaces.roots[&link].canonical_root.as_path(),
            link.as_path(),
            "test setup: the symlinked folder path must differ from the canonical root"
        );

        // Seed the client cache: a.md publishes its trailing-whitespace warning at
        // UTF-16 column 4 (after four ASCII chars), keyed by the symlinked URI.
        let changed_uri = path_to_uri(&link.join("a.md"));
        let first = diff_diagnostics(&mut workspaces, &HashSet::new());
        assert_eq!(
            first.len(),
            1,
            "first pass publishes a.md's trailing-whitespace warning: {first:?}"
        );
        assert_eq!(
            first[0].0, changed_uri,
            "the published URI is keyed by the symlinked folder path"
        );
        assert_eq!(
            first[0].1[0].range.start.character, 4,
            "the span starts after four ASCII chars (UTF-16 column 4)"
        );

        // Length-preserving astral swap: replace the four ASCII chars with one
        // astral char (also four bytes). The trailing-whitespace span stays bytes
        // 4..5, so the Lattice vector is byte-identical, but the UTF-16 column of
        // its start shifts 4 -> 2. Only the forced re-materialization can catch it.
        edit(&mut workspaces, "a.md", "😀 \n");
        let sent = diff_diagnostics(&mut workspaces, &one_uri(&changed_uri));
        assert_eq!(
            sent.len(),
            1,
            "the edited file re-publishes despite an unchanged Lattice vector: {sent:?}"
        );
        assert_eq!(
            sent[0].0, changed_uri,
            "the re-published URI is the symlinked a.md"
        );
        assert_eq!(
            sent[0].1[0].range.start.character, 2,
            "the astral swap moves the span start to UTF-16 column 2"
        );
    }

    /// Apply one `publishDiagnostics` to the simulated client (replace
    /// semantics; an empty vector removes the entry, matching the cache rule).
    fn apply_publish(
        client: &mut HashMap<String, Vec<lsp::Diagnostic>>,
        uri: String,
        diagnostics: Vec<lsp::Diagnostic>,
    ) {
        if diagnostics.is_empty() {
            client.remove(&uri);
        } else {
            client.insert(uri, diagnostics);
        }
    }

    /// Reset the materialization counter, returning the count accumulated by a
    /// closure that drives a publish — so a test can assert how many diagnostics
    /// a single sync re-materialized (ticket perf 02).
    fn count_materializations<T>(f: impl FnOnce() -> T) -> (usize, T) {
        MATERIALIZE_COUNT.with(|count| count.set(0));
        let value = f();
        let count = MATERIALIZE_COUNT.with(std::cell::Cell::get);
        (count, value)
    }

    /// Reset the recompute counter, returning the number of whole-workspace
    /// recompute / publish passes a closure drove — so a test can assert that a
    /// batched watched-file notification collapses to one pass (ticket perf 07).
    fn count_recomputes<T>(f: impl FnOnce() -> T) -> (usize, T) {
        RECOMPUTE_COUNT.with(|count| count.set(0));
        let value = f();
        let count = RECOMPUTE_COUNT.with(std::cell::Cell::get);
        (count, value)
    }

    /// Run `f` and return how many `recompute_all_structural` sweeps it drove —
    /// the O(workspace) structural pass a membership change forces — so a test
    /// can assert which store mutations pay it (a rootless open must not).
    fn count_structural_sweeps<T>(f: impl FnOnce() -> T) -> (usize, T) {
        STRUCTURAL_SWEEP_COUNT.with(|count| count.set(0));
        let value = f();
        let count = STRUCTURAL_SWEEP_COUNT.with(std::cell::Cell::get);
        (count, value)
    }

    /// Assert the client's accumulated diagnostics equal a from-scratch full
    /// publish (the non-empty entries of `desired_diagnostics`).
    fn assert_client_matches(
        workspaces: &Workspaces,
        client: &HashMap<String, Vec<lsp::Diagnostic>>,
        context: &str,
    ) {
        let expected: HashMap<String, Vec<lsp::Diagnostic>> = desired_diagnostics(workspaces)
            .into_iter()
            .filter(|(_, diagnostics)| !diagnostics.is_empty())
            .collect();
        assert_eq!(
            client, &expected,
            "diffed publish stream must equal a from-scratch publish {context}"
        );
    }

    #[test]
    fn diffing_published_stream_matches_full_recompute() {
        // The safety net from issue 013: replaying the diffed publish stream
        // into a client must reproduce, at every step, exactly what a
        // from-scratch full publish would show. Config present so the graph
        // tier (forward links, backlink reconciliation) participates too.
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            ("a.md", "# A\n"),
            ("b.md", "# B\n"),
            ("index.md", "# Index\n"),
        ]);
        let mut workspaces = scan_workspaces(&dir);

        // The simulated client: URI -> diagnostics, mutated by each publish.
        let mut client: HashMap<String, Vec<lsp::Diagnostic>> = HashMap::new();

        // (file, new content) — edits that add, remove, and move diagnostics,
        // including a cross-file backlink dependency: index -> a "supersedes"
        // expects `superseded_by: index.md` in a's frontmatter, and the
        // missing-backlink warning is reported on the *source* (index.md).
        let steps: &[(&str, &str)] = &[
            // add a duplicate-heading-slug diagnostic on a.md
            ("a.md", "# A\n\n# A\n"),
            // forward link expects a backlink on a.md -> warns on index.md
            ("index.md", "[a](a.md \"supersedes\")\n"),
            // satisfy the backlink and drop the heading diagnostic
            (
                "a.md",
                "---\nbacklinks:\n  superseded_by:\n    - index.md\n---\n# A\n",
            ),
            // a broken forward link error on b.md
            ("b.md", "[gone](missing.md \"references\")\n"),
            // remove the forward link -> a.md's backlink is now stale
            ("index.md", "# Index\n"),
            // fix b.md
            ("b.md", "# B\n"),
        ];

        // Initial sync, then one per edit. After every sync the client must
        // equal the from-scratch desired (non-empty) set.
        for (uri, diagnostics) in diff_diagnostics(&mut workspaces, &HashSet::new()) {
            apply_publish(&mut client, uri, diagnostics);
        }
        assert_client_matches(&workspaces, &client, "after initial sync");

        for (i, (rel, content)) in steps.iter().enumerate() {
            edit(&mut workspaces, rel, content);
            // Drive the full path with the edited URI, as a graph-tier didChange
            // does, so the force-re-materialize branch is exercised too.
            let changed = file_uri(&dir, rel);
            for (uri, diagnostics) in diff_diagnostics(&mut workspaces, &one_uri(&changed)) {
                apply_publish(&mut client, uri, diagnostics);
            }
            assert_client_matches(&workspaces, &client, &format!("after edit {i} ({rel})"));
        }
    }

    #[test]
    fn incremental_file_publish_matches_full_recompute() {
        // Stage-2.5 safety net: in the structural tier a content edit changes
        // only the edited file, so the per-file incremental publish
        // (`diff_file_diagnostics`) must reproduce, at every step, exactly what
        // a from-scratch full publish would show. No `.lattice.toml` -> the
        // structural tier the incremental path is gated to.
        let dir = workspace_with_files(&[("a.md", "# A\n"), ("b.md", "# B\n"), ("c.md", "# C\n")]);
        let mut workspaces = scan_workspaces(&dir);
        let mut client: HashMap<String, Vec<lsp::Diagnostic>> = HashMap::new();

        // Seed the client with the initial full publish, as didOpen would.
        for (uri, diagnostics) in diff_diagnostics(&mut workspaces, &HashSet::new()) {
            apply_publish(&mut client, uri, diagnostics);
        }
        assert_client_matches(&workspaces, &client, "after initial sync");

        // didChange-style edits that add, move, and clear diagnostics across
        // different files — each published via the per-file incremental path.
        let steps: &[(&str, &str)] = &[
            ("a.md", "# A\n\n# A\n"),               // add a duplicate-slug diagnostic
            ("b.md", "Visit docs/page.md here.\n"), // add a bare-path hint
            ("a.md", "# A\n"),                      // clear a.md's diagnostic
            ("c.md", "trailing \n"),                // add trailing whitespace on c.md
        ];
        for (i, (rel, content)) in steps.iter().enumerate() {
            edit(&mut workspaces, rel, content);
            let uri = file_uri(&dir, rel);
            if let Some((uri, diagnostics)) = diff_file_diagnostics(&mut workspaces, &uri) {
                apply_publish(&mut client, uri, diagnostics);
            }
            assert_client_matches(&workspaces, &client, &format!("after edit {i} ({rel})"));
        }
    }

    #[test]
    fn graph_tier_incremental_publish_matches_full_recompute() {
        // Ticket perf 02: in the graph tier (`.lattice.toml` present) the full
        // publish path runs the whole-graph recompute, then re-materializes and
        // re-publishes only the files whose Lattice vector changed — yet must
        // still reproduce, at every step, exactly what a from-scratch full
        // publish would show (the stage-2.5 differential invariant, extended to
        // config-present workspaces). Each step drives `diff_diagnostics` with
        // the edited URI, as a graph-tier `didChange` does, and several clean
        // files ride along to prove they are not disturbed.
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            ("index.md", "# Index\n"),
            ("a.md", "# A\n"),
            ("b.md", "# B\n"),
            ("clean1.md", "# Clean\n"),
            ("clean2.md", "# Clean\n"),
        ]);
        let mut workspaces = scan_workspaces(&dir);
        let mut client: HashMap<String, Vec<lsp::Diagnostic>> = HashMap::new();

        for (uri, diagnostics) in diff_diagnostics(&mut workspaces, &HashSet::new()) {
            apply_publish(&mut client, uri, diagnostics);
        }
        assert_client_matches(&workspaces, &client, "after initial sync");

        // Each edit changes exactly one file's text but its graph consequence
        // can land on a *different* file — the cross-file dependency the full
        // path must catch through the recompute, not the edited URI alone.
        let steps: &[(&str, &str)] = &[
            // index supersedes a -> missing-backlink warning on index.md
            ("index.md", "[a](a.md \"supersedes\")\n"),
            // a adds the reciprocal backlink -> clears index.md's warning, even
            // though a.md is the file that was edited
            (
                "a.md",
                "---\nbacklinks:\n  superseded_by:\n    - index.md\n---\n# A\n",
            ),
            // a broken forward-link error appears on b.md
            ("b.md", "[gone](missing.md \"references\")\n"),
            // fix b.md
            ("b.md", "# B\n"),
        ];
        for (i, (rel, content)) in steps.iter().enumerate() {
            edit(&mut workspaces, rel, content);
            let changed = file_uri(&dir, rel);
            for (uri, diagnostics) in diff_diagnostics(&mut workspaces, &one_uri(&changed)) {
                apply_publish(&mut client, uri, diagnostics);
            }
            assert_client_matches(&workspaces, &client, &format!("after edit {i} ({rel})"));
        }
    }

    #[test]
    fn graph_tier_didchange_rematerializes_only_changed_files() {
        // Ticket perf 02 acceptance: a graph-tier `didChange` re-materializes and
        // re-publishes only the files whose diagnostics changed — asserted by
        // counting materializations and publishes, not latency. index.md carries
        // two missing-backlink warnings; editing a.md's frontmatter clears one of
        // them, so index.md changes although a.md was the file edited. The four
        // other files must be neither re-materialized nor re-published.
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            (
                "index.md",
                "[a](a.md \"supersedes\")\n[b](b.md \"supersedes\")\n",
            ),
            ("a.md", "# A\n"),
            ("b.md", "# B\n"),
            ("clean1.md", "# Clean\n"),
            ("clean2.md", "# Clean\n"),
            ("clean3.md", "# Clean\n"),
        ]);
        let mut workspaces = scan_workspaces(&dir);
        let mut client: HashMap<String, Vec<lsp::Diagnostic>> = HashMap::new();

        for (uri, diagnostics) in diff_diagnostics(&mut workspaces, &HashSet::new()) {
            apply_publish(&mut client, uri, diagnostics);
        }
        assert_client_matches(&workspaces, &client, "after initial sync");

        // Add the reciprocal backlink for index -> a, clearing one of index.md's
        // two warnings. a.md is the edited file; index.md is the one that moves.
        edit(
            &mut workspaces,
            "a.md",
            "---\nbacklinks:\n  superseded_by:\n    - index.md\n---\n# A\n",
        );
        let changed = file_uri(&dir, "a.md");
        let (materializations, sent) =
            count_materializations(|| diff_diagnostics(&mut workspaces, &one_uri(&changed)));

        // a.md (the edited file) materializes to nothing; index.md re-materializes
        // its one remaining warning. The four clean files are untouched, so the
        // whole sync materializes exactly one diagnostic — not the workspace.
        assert_eq!(
            materializations, 1,
            "only the changed, non-empty file is re-materialized: {sent:?}"
        );
        let sent_uris: Vec<&str> = sent.iter().map(|(uri, _)| uri.as_str()).collect();
        assert_eq!(
            sent_uris,
            vec![file_uri(&dir, "index.md").as_str()],
            "only index.md is re-published"
        );

        for (uri, diagnostics) in sent {
            apply_publish(&mut client, uri, diagnostics);
        }
        assert_client_matches(&workspaces, &client, "after backlink edit");
    }

    // -----------------------------------------------------------------------
    // Completion (decision 007, ticket integration 14)
    // -----------------------------------------------------------------------

    /// Request completion at a 0-based (line, character) position in `rel`.
    fn complete_at(
        workspaces: &Workspaces,
        dir: &tempfile::TempDir,
        rel: &str,
        line: u32,
        character: u32,
    ) -> Option<lsp::CompletionList> {
        let params = lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: file_uri(dir, rel),
            },
            position: lsp::Position { line, character },
        };
        completion(workspaces, &params)
    }

    /// The labels of a completion list.
    fn labels(list: &lsp::CompletionList) -> Vec<String> {
        list.items.iter().map(|i| i.label.clone()).collect()
    }

    #[test]
    fn completion_path_offers_workspace_files_and_dirs() {
        let dir = workspace_with_files(&[
            ("doc.md", "[x]("),
            ("other.md", "# Other\n"),
            ("guide.md", "# Guide\n"),
            ("sub/page.md", "# Page\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        let list = complete_at(&workspaces, &dir, "doc.md", 0, 4).expect("path completion");
        let got = labels(&list);
        assert!(
            got.contains(&"other.md".to_string()),
            "offers sibling md: {got:?}"
        );
        assert!(
            got.contains(&"guide.md".to_string()),
            "offers sibling md: {got:?}"
        );
        assert!(
            got.contains(&"sub/".to_string()),
            "offers subdirectory: {got:?}"
        );
        assert!(
            !got.iter().any(|l| l.starts_with('.')),
            "hidden entries (.git) are skipped: {got:?}"
        );
    }

    #[test]
    fn completion_path_respects_relative_directory() {
        let dir = workspace_with_files(&[("a.md", "[x](docs/"), ("docs/inner.md", "# Inner\n")]);
        let workspaces = scan_workspaces(&dir);

        // Cursor after `[x](docs/` — column 9.
        let list = complete_at(&workspaces, &dir, "a.md", 0, 9).expect("path completion");
        let got = labels(&list);
        assert_eq!(
            got,
            vec!["inner.md".to_string()],
            "only the typed directory's contents are offered: {got:?}"
        );
        // The replacement covers just the filename segment, leaving `docs/`.
        let edit = list.items[0].text_edit.as_ref().expect("text edit");
        assert_eq!(
            edit.range.start.character, 9,
            "edit starts after the directory separator, not replacing it"
        );
    }

    #[test]
    fn completion_path_skips_gitignored() {
        let dir = workspace_with_files(&[
            (".gitignore", "secret.md\nbuild/\n"),
            ("doc.md", "[x]("),
            ("visible.md", "# Visible\n"),
            ("secret.md", "# Secret\n"),
            ("build/out.md", "# Out\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        let got = labels(&complete_at(&workspaces, &dir, "doc.md", 0, 4).expect("path completion"));
        assert!(
            got.contains(&"visible.md".to_string()),
            "offers tracked files: {got:?}"
        );
        assert!(
            !got.contains(&"secret.md".to_string()),
            "a gitignored file is not offered: {got:?}"
        );
        assert!(
            !got.contains(&"build/".to_string()),
            "a gitignored directory is not offered: {got:?}"
        );
    }

    #[test]
    fn completion_fragment_offers_target_headings() {
        let dir = workspace_with_files(&[
            ("doc.md", "[x](target.md#"),
            ("target.md", "# Hello World\n\n## Setup {#install}\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        let list = complete_at(&workspaces, &dir, "doc.md", 0, 14).expect("fragment completion");
        let got = labels(&list);
        assert!(
            got.contains(&"hello-world".to_string()),
            "offers the computed slug: {got:?}"
        );
        assert!(
            got.contains(&"install".to_string()),
            "offers the explicit anchor id: {got:?}"
        );
        let hello = list
            .items
            .iter()
            .find(|i| i.label == "hello-world")
            .expect("hello-world item");
        assert_eq!(
            hello.detail.as_deref(),
            Some("Hello World"),
            "detail is the heading text"
        );
    }

    #[test]
    fn completion_fragment_in_doc_offers_current_headings() {
        let dir = workspace_with_files(&[("doc.md", "# Top\n\n[x](#")]);
        let workspaces = scan_workspaces(&dir);

        let list = complete_at(&workspaces, &dir, "doc.md", 2, 5).expect("in-doc fragment");
        assert_eq!(
            labels(&list),
            vec!["top".to_string()],
            "an in-doc `#` completes the current file's headings"
        );
    }

    #[test]
    fn completion_predicate_offers_vocabulary_with_inverse_detail() {
        let dir = workspace_with_files(&[
            (".lattice.toml", "[predicates]\ntracks = \"tracked_by\"\n"),
            ("doc.md", "[x](target.md \""),
            ("target.md", "# Target\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        let list = complete_at(&workspaces, &dir, "doc.md", 0, 15).expect("predicate completion");
        let supersedes = list
            .items
            .iter()
            .find(|i| i.label == "supersedes")
            .expect("offers a forward predicate");
        assert_eq!(
            supersedes.detail.as_deref(),
            Some("superseded_by"),
            "a forward predicate's detail is its inverse"
        );
        // Both directions are offered (decision 008): the inverse member of a
        // pair, detailed with its forward.
        let superseded_by = list
            .items
            .iter()
            .find(|i| i.label == "superseded_by")
            .expect("offers the inverse direction too");
        assert_eq!(
            superseded_by.detail.as_deref(),
            Some("supersedes"),
            "an inverse predicate's detail is its forward"
        );
        let got = labels(&list);
        assert!(
            got.contains(&"tracks".to_string()) && got.contains(&"tracked_by".to_string()),
            "offers both directions of a config-defined predicate: {got:?}"
        );
        // Selecting any item inserts a known predicate (either direction), which
        // clears a missing/unknown-predicate diagnostic on the link.
        let (workspace, _) = workspaces
            .resolve(&file_uri(&dir, "doc.md"))
            .expect("resolve workspace");
        for item in &list.items {
            assert!(
                workspace.config().is_known_predicate(&item.label),
                "every offered predicate is known to the vocabulary: {}",
                item.label
            );
        }
    }

    #[test]
    fn completion_predicate_filters_by_partial() {
        let dir = workspace_with_files(&[
            ("doc.md", "[x](target.md \"sup"),
            ("target.md", "# Target\n"),
        ]);
        let workspaces = scan_workspaces(&dir);

        let list = complete_at(&workspaces, &dir, "doc.md", 0, 18).expect("predicate completion");
        assert_eq!(
            labels(&list),
            vec!["supersedes".to_string(), "superseded_by".to_string()],
            "both directions matching the typed partial are offered"
        );
    }

    #[test]
    fn completion_predicate_skipped_on_exempt_links() {
        // Predicates apply only to intra-project markdown links; external and
        // non-markdown destinations carry a plain title, not a predicate.
        let dir = workspace_with_files(&[
            ("ext.md", "[x](https://example.com \""),
            ("asset.md", "[x](diagram.png \""),
        ]);
        let workspaces = scan_workspaces(&dir);

        let external =
            complete_at(&workspaces, &dir, "ext.md", 0, 25).expect("known title context");
        assert!(
            external.items.is_empty(),
            "no predicate completion on an external link: {:?}",
            labels(&external)
        );
        let asset = complete_at(&workspaces, &dir, "asset.md", 0, 17).expect("known title context");
        assert!(
            asset.items.is_empty(),
            "no predicate completion on a non-markdown link: {:?}",
            labels(&asset)
        );
    }

    #[test]
    fn completion_reference_label_offers_definitions() {
        let dir =
            workspace_with_files(&[("doc.md", "[def]: https://example.com/page\n\nSee [link][")]);
        let workspaces = scan_workspaces(&dir);

        let list = complete_at(&workspaces, &dir, "doc.md", 2, 11).expect("reference completion");
        let def = list
            .items
            .iter()
            .find(|i| i.label == "def")
            .expect("offers the defined reference label");
        assert_eq!(
            def.detail.as_deref(),
            Some("https://example.com/page"),
            "detail is the definition's URL"
        );
    }

    #[test]
    fn completion_footnote_offers_definitions() {
        let dir =
            workspace_with_files(&[("doc.md", "Body.[^note]\n\n[^note]: A footnote.\n\nMore [^")]);
        let workspaces = scan_workspaces(&dir);

        let list = complete_at(&workspaces, &dir, "doc.md", 4, 7).expect("footnote completion");
        assert_eq!(
            labels(&list),
            vec!["note".to_string()],
            "offers the defined footnote label"
        );
    }

    #[test]
    fn completion_none_in_prose() {
        let dir = workspace_with_files(&[("doc.md", "just some prose here\n")]);
        let workspaces = scan_workspaces(&dir);

        assert!(
            complete_at(&workspaces, &dir, "doc.md", 0, 10).is_none(),
            "prose is not a completion site"
        );
    }

    #[test]
    fn completion_none_in_code_block() {
        // A fenced code block whose body is a link-shaped string: the line
        // prefix looks like a destination, but the tree marks it as code.
        let dir = workspace_with_files(&[("doc.md", "```\n[x](\n```\n")]);
        let workspaces = scan_workspaces(&dir);

        assert!(
            complete_at(&workspaces, &dir, "doc.md", 1, 4).is_none(),
            "no completion inside a code block"
        );
    }

    // -----------------------------------------------------------------------
    // Semantic tokens (ticket integration 15)
    // -----------------------------------------------------------------------

    /// One decoded semantic token in absolute coordinates (the delta encoding
    /// undone), for assertion convenience.
    #[derive(Debug, PartialEq, Eq)]
    struct DecodedToken {
        line: u32,
        start: u32,
        length: u32,
        token_type: u32,
        modifiers: u32,
    }

    /// Decode the LSP delta-quintuple stream into absolute tokens, asserting the
    /// stream is well-formed (a multiple of five) along the way.
    fn decode_tokens(tokens: &lsp::SemanticTokens) -> Vec<DecodedToken> {
        assert_eq!(
            tokens.data.len() % 5,
            0,
            "semantic token data must be a flat sequence of 5-tuples"
        );
        let mut out = Vec::new();
        let mut line = 0u32;
        let mut character = 0u32;
        for chunk in tokens.data.chunks_exact(5) {
            let (delta_line, delta_start, length, token_type, modifiers) =
                (chunk[0], chunk[1], chunk[2], chunk[3], chunk[4]);
            if delta_line == 0 {
                character += delta_start;
            } else {
                line += delta_line;
                character = delta_start;
            }
            out.push(DecodedToken {
                line,
                start: character,
                length,
                token_type,
                modifiers,
            });
        }
        out
    }

    /// Request full semantic tokens for `rel`.
    fn tokens_for(
        workspaces: &Workspaces,
        dir: &tempfile::TempDir,
        rel: &str,
    ) -> Vec<DecodedToken> {
        decode_tokens(&semantic_tokens_full(workspaces, &file_uri(dir, rel)))
    }

    /// Assert tokens are sorted by (line, start) and pairwise non-overlapping —
    /// the LSP protocol's hard requirement.
    fn assert_sorted_non_overlapping(tokens: &[DecodedToken]) {
        for pair in tokens.windows(2) {
            let (a, b) = (&pair[0], &pair[1]);
            let ordered = a.line < b.line || (a.line == b.line && a.start <= b.start);
            assert!(
                ordered,
                "tokens must be sorted by (line, start): {a:?} then {b:?}"
            );
            if a.line == b.line {
                assert!(
                    a.start + a.length <= b.start,
                    "tokens on a line must not overlap: {a:?} then {b:?}"
                );
            }
        }
    }

    #[test]
    fn semantic_tokens_legend_indices_match_capability_order() {
        // The emitted `tokenType`/`tokenModifiers` indices are positional into
        // the legend arrays declared in the capabilities blob. Guard that the
        // bit/index constants match the declared modifier order
        // (bold, italic, strikethrough) so the two can't silently drift.
        assert_eq!(
            SEMANTIC_TOKEN_TYPE_MARKUP_INDEX, 0,
            "markup is the only (index-0) token type"
        );
        assert_eq!(SEMANTIC_MODIFIER_BOLD_BIT, 1, "bold is legend modifier 0");
        assert_eq!(
            SEMANTIC_MODIFIER_ITALIC_BIT, 2,
            "italic is legend modifier 1"
        );
        assert_eq!(
            SEMANTIC_MODIFIER_STRIKETHROUGH_BIT, 4,
            "strikethrough is legend modifier 2"
        );
    }

    #[test]
    fn semantic_tokens_basic_strong_emphasis_strikethrough() {
        let dir = workspace_with_files(&[("doc.md", "**a** *b* ~~c~~\n")]);
        let workspaces = scan_workspaces(&dir);
        let tokens = tokens_for(&workspaces, &dir, "doc.md");
        assert_sorted_non_overlapping(&tokens);
        // `**a**` cols 0..5 bold; `*b*` cols 6..9 italic; `~~c~~` cols 10..15
        // strikethrough.
        assert_eq!(
            tokens,
            vec![
                DecodedToken {
                    line: 0,
                    start: 0,
                    length: 5,
                    token_type: 0,
                    modifiers: SEMANTIC_MODIFIER_BOLD_BIT,
                },
                DecodedToken {
                    line: 0,
                    start: 6,
                    length: 3,
                    token_type: 0,
                    modifiers: SEMANTIC_MODIFIER_ITALIC_BIT,
                },
                DecodedToken {
                    line: 0,
                    start: 10,
                    length: 5,
                    token_type: 0,
                    modifiers: SEMANTIC_MODIFIER_STRIKETHROUGH_BIT,
                },
            ],
            "one token per run, each with its own modifier"
        );
    }

    #[test]
    fn semantic_tokens_triple_emphasis_combines_bold_italic() {
        // `***foo***` is the central overlap case: parser 26 emits Strong over
        // `**foo**` and Emphasis over the whole `***foo***`. The flattening must
        // yield non-overlapping tokens whose union covers `***foo***`, with
        // `foo` carrying BOTH bold and italic.
        let dir = workspace_with_files(&[("doc.md", "***foo***\n")]);
        let workspaces = scan_workspaces(&dir);
        let tokens = tokens_for(&workspaces, &dir, "doc.md");
        assert_sorted_non_overlapping(&tokens);

        // Tokens must tile `***foo***` (cols 0..9) with no gap or overlap.
        assert_eq!(tokens.first().map(|t| t.start), Some(0), "starts at col 0");
        let last = tokens.last().expect("at least one token");
        assert_eq!(
            last.start + last.length,
            9,
            "tokens tile through the closing delimiters (col 9)"
        );
        let bold = SEMANTIC_MODIFIER_BOLD_BIT;
        let italic = SEMANTIC_MODIFIER_ITALIC_BIT;
        // Find the region covering `foo` (cols 3..6) — it must carry both.
        let foo = tokens
            .iter()
            .find(|t| t.start <= 3 && 6 <= t.start + t.length)
            .expect("a token covers the inner `foo`");
        assert_eq!(
            foo.modifiers,
            bold | italic,
            "the inner `foo` carries both bold and italic"
        );
        // Every token has the markup type and a non-empty modifier set; no
        // token is plain.
        for t in &tokens {
            assert_eq!(t.token_type, 0, "all tokens are the markup type");
            assert_ne!(t.modifiers, 0, "no token without an emphasis modifier");
        }
    }

    #[test]
    fn semantic_tokens_no_strikethrough_for_flanking_tildes() {
        // The ticket-26 flanking fix end to end: `~89 of ~162` has no
        // strikethrough run, so no token is emitted. (Acceptance criterion.)
        let dir = workspace_with_files(&[("doc.md", "~89 of ~162\n")]);
        let workspaces = scan_workspaces(&dir);
        let tokens = tokens_for(&workspaces, &dir, "doc.md");
        assert!(
            tokens.is_empty(),
            "left-flanking-only single tildes produce no strikethrough token: {tokens:?}"
        );
    }

    #[test]
    fn semantic_tokens_none_in_prose_or_code() {
        // Plain prose, an inline code span containing emphasis-looking text, and
        // a fenced code block: parser 26 keeps emphasis runs out of code, so the
        // tree has none there and no token is emitted.
        let dir = workspace_with_files(&[(
            "doc.md",
            "just prose here\n\n`**not bold**`\n\n```\n*not em*\n```\n",
        )]);
        let workspaces = scan_workspaces(&dir);
        let tokens = tokens_for(&workspaces, &dir, "doc.md");
        assert!(
            tokens.is_empty(),
            "no tokens in prose, inline code, or code blocks: {tokens:?}"
        );
    }

    #[test]
    fn semantic_tokens_emit_no_diagnostics() {
        // Requesting semantic tokens is styling-only and must never publish a
        // diagnostic. The parse tree for an emphasis document carries none.
        let dir = workspace_with_files(&[("doc.md", "**bold** and *em* and ~~strike~~\n")]);
        let workspaces = scan_workspaces(&dir);
        let (ws, rel) = workspaces
            .resolve(&file_uri(&dir, "doc.md"))
            .expect("doc.md resolves");
        let file_data = ws.file(&rel).expect("doc.md parsed");
        assert!(
            file_data.tree.diagnostics().is_empty(),
            "emphasis recognition emits no diagnostics: {:?}",
            file_data.tree.diagnostics()
        );
        // And the surface itself still produces tokens (sanity).
        let tokens = tokens_for(&workspaces, &dir, "doc.md");
        assert_eq!(tokens.len(), 3, "three emphasis runs, three tokens");
    }

    #[test]
    fn semantic_tokens_round_trip_multibyte_and_crlf() {
        // UTF-16 offsets must round-trip on multibyte + CRLF input, mirroring
        // the `position_round_trip_*` tests. `café` is 4 chars / 5 bytes; 😀 is
        // 2 UTF-16 units; λ is 1 unit but 2 bytes. Each line is one run.
        let content = "**café 😀** done\r\n*λ done*\n";
        let dir = workspace_with_files(&[("doc.md", content)]);
        let workspaces = scan_workspaces(&dir);
        let tokens = tokens_for(&workspaces, &dir, "doc.md");
        assert_sorted_non_overlapping(&tokens);
        assert_eq!(tokens.len(), 2, "one strong run, one emphasis run");

        // Line 0: `**café 😀**` — cols 0..11 (** + café=4 + space + 😀=2 UTF-16
        // units + **).
        assert_eq!(
            tokens[0],
            DecodedToken {
                line: 0,
                start: 0,
                length: 11,
                token_type: 0,
                modifiers: SEMANTIC_MODIFIER_BOLD_BIT,
            },
            "bold run measured in UTF-16 units (astral 😀 = 2)"
        );
        // Line 1 (CRLF-terminated line 0): `*λ done*` — cols 0..8.
        assert_eq!(
            tokens[1],
            DecodedToken {
                line: 1,
                start: 0,
                length: 8,
                token_type: 0,
                modifiers: SEMANTIC_MODIFIER_ITALIC_BIT,
            },
            "emphasis run on the line after a CRLF break"
        );

        // The decoded UTF-16 columns must map back to the run's byte span via
        // the same inverse conversion diagnostics use.
        let (ws, rel) = workspaces
            .resolve(&file_uri(&dir, "doc.md"))
            .expect("doc.md resolves");
        let file_data = ws.file(&rel).expect("doc.md parsed");
        let source = file_data.tree.source();
        let bold_start = file_data.line_index.offset(
            source,
            lsp::Position {
                line: 0,
                character: 0,
            },
        );
        assert_eq!(
            bold_start,
            source.find("**").expect("** present"),
            "UTF-16 column 0 maps back to the run's byte start"
        );
    }

    #[test]
    fn semantic_tokens_range_restricts_to_byte_span() {
        // `/range` emits only the runs intersecting the requested range. Two
        // runs on two lines; ask for line 0 only.
        let dir = workspace_with_files(&[("doc.md", "**a**\n*b*\n")]);
        let workspaces = scan_workspaces(&dir);
        let range = lsp::Range {
            start: lsp::Position {
                line: 0,
                character: 0,
            },
            end: lsp::Position {
                line: 1,
                character: 0,
            },
        };
        let tokens = decode_tokens(&semantic_tokens_range(
            &workspaces,
            &file_uri(&dir, "doc.md"),
            &range,
        ));
        assert_eq!(
            tokens,
            vec![DecodedToken {
                line: 0,
                start: 0,
                length: 5,
                token_type: 0,
                modifiers: SEMANTIC_MODIFIER_BOLD_BIT,
            }],
            "only the line-0 bold run falls in the requested range"
        );
    }

    #[test]
    fn semantic_tokens_unknown_document_is_empty() {
        let dir = workspace_with_files(&[("doc.md", "**a**\n")]);
        let workspaces = scan_workspaces(&dir);
        let tokens = decode_tokens(&semantic_tokens_full(
            &workspaces,
            &file_uri(&dir, "missing.md"),
        ));
        assert!(
            tokens.is_empty(),
            "an unknown document yields an empty token set"
        );
    }

    // -----------------------------------------------------------------------
    // Single-file (rootless) documents (issue 051)
    //
    // Lattice is always a markdown language server: a document opened outside
    // every workspace folder — a fully rootless session, or an open-from-
    // outside in a rooted one — must still serve its document-scoped features,
    // while the graph tier stays quiet (it has nothing to say for one file).
    // -----------------------------------------------------------------------

    /// A `Workspaces` with no scanned workspace folders — a rootless session.
    fn rootless_workspaces() -> Workspaces {
        Workspaces::new()
    }

    /// Position params for `uri` at `(line, character)`.
    fn position_params(uri: &str, line: u32, character: u32) -> lsp::TextDocumentPositionParams {
        lsp::TextDocumentPositionParams {
            text_document: lsp::TextDocumentIdentifier {
                uri: uri.to_string(),
            },
            position: lsp::Position { line, character },
        }
    }

    #[test]
    fn single_file_rootless_serves_semantic_tokens() {
        // The headline of issue 051: a rootless `didOpen` must serve semantic
        // tokens, not the silent `{ "data": [] }` degradation. The buffer is
        // in-memory, so no file needs to exist on disk.
        let mut workspaces = rootless_workspaces();
        let uri = "file:///tmp/lattice-051-scratch.md";
        workspaces.sync_document_content(uri, "**bold**, *italic*, and ~~strikethrough~~\n");

        let tokens = decode_tokens(&semantic_tokens_full(&workspaces, uri));
        assert_sorted_non_overlapping(&tokens);
        let modifiers: Vec<u32> = tokens.iter().map(|t| t.modifiers).collect();
        assert!(
            modifiers.contains(&SEMANTIC_MODIFIER_BOLD_BIT),
            "rootless tokens must include the bold run, got {tokens:?}"
        );
        assert!(
            modifiers.contains(&SEMANTIC_MODIFIER_ITALIC_BIT),
            "rootless tokens must include the italic run, got {tokens:?}"
        );
        assert!(
            modifiers.contains(&SEMANTIC_MODIFIER_STRIKETHROUGH_BIT),
            "rootless tokens must include the strikethrough run, got {tokens:?}"
        );
    }

    #[test]
    fn single_file_rootless_serves_document_scoped_features() {
        // Beyond semantic tokens: symbols, folding, document links, formatting,
        // and hover must all serve a rootless document. The document is named
        // `doc.md` and carries a self-referential link so hover — a cross-file
        // preview feature — has an in-index target to resolve.
        let mut workspaces = rootless_workspaces();
        let uri = "file:///tmp/lattice-051-features/doc.md";
        workspaces.sync_document_content(
            uri,
            "---\nbacklinks:\n  references:\n    - z.md\n    - a.md\n---\n# Title\n\n## Section\n\n[self](doc.md \"references\")\n",
        );

        let symbols = document_symbols(&workspaces, uri).expect("rootless document symbols");
        assert!(
            !symbols.is_empty(),
            "rootless document symbols must include the headings, got {symbols:?}"
        );

        let folds = folding_ranges(&workspaces, uri);
        assert!(
            !folds.is_empty(),
            "rootless folding must fold the frontmatter/heading, got {folds:?}"
        );

        let links = document_links(&workspaces, uri);
        assert!(
            !links.is_empty(),
            "rootless document links must emit the intra-project link, got {links:?}"
        );

        let edits = format_document(&workspaces, uri);
        assert!(
            edits.is_some(),
            "rootless formatting must sort the frontmatter backlinks, got {edits:?}"
        );

        // The self-referential link resolves to the document itself, so the
        // hover preview has content to show — proving hover runs against the
        // single-file document rather than short-circuiting at resolution.
        let hover = hover_preview(&workspaces, &position_params(uri, 10, 0))
            .expect("rootless hover on the self-link should produce a preview");
        assert!(
            hover.contents.value.contains("references"),
            "rootless hover surfaces the link predicate, got {}",
            hover.contents.value
        );
    }

    #[test]
    fn single_file_in_rooted_session_serves_document_outside_every_root() {
        // Case (b): a rooted session where a document is opened from *outside*
        // every workspace folder degrades the same way rootless does — so it
        // must be served identically, while an in-root document is unaffected.
        let dir = workspace_with_files(&[("inside.md", "**inside**\n")]);
        let mut workspaces = scan_workspaces(&dir);

        let outside_uri = "file:///tmp/lattice-051-outside.md";
        workspaces.sync_document_content(outside_uri, "**outside**\n");

        let outside = decode_tokens(&semantic_tokens_full(&workspaces, outside_uri));
        assert!(
            !outside.is_empty(),
            "a document opened outside every root must still serve tokens, got {outside:?}"
        );

        // The in-root document keeps serving through the ordinary workspace path.
        let inside = tokens_for(&workspaces, &dir, "inside.md");
        assert!(
            !inside.is_empty(),
            "an in-root document is unaffected by the single-file path, got {inside:?}"
        );
    }

    #[test]
    fn single_file_document_stays_diagnostic_quiet() {
        // Graph/structural diagnostics have nothing to say for a single file:
        // content that would raise a stale-reference diagnostic inside a
        // workspace must produce no pushed diagnostics rootless rather than
        // erroring.
        let mut workspaces = rootless_workspaces();
        let uri = "file:///tmp/lattice-051-quiet.md";
        // A bare path to an absent file — a stale reference in a real workspace.
        workspaces.sync_document_content(uri, "See docs/page.md for details.\n");

        let pushed = diff_diagnostics(&mut workspaces, &one_uri(uri));
        assert!(
            pushed.is_empty(),
            "a single-file document pushes no diagnostics, got {pushed:?}"
        );
    }

    #[test]
    fn single_file_closed_document_is_dropped() {
        // A `didClose` on a single-file document drops it — there is no disk-
        // backed workspace to revert to — so a later request no longer resolves.
        let mut workspaces = rootless_workspaces();
        let uri = "file:///tmp/lattice-051-close.md";
        workspaces.sync_document_content(uri, "**live**\n");
        assert!(
            !decode_tokens(&semantic_tokens_full(&workspaces, uri)).is_empty(),
            "the open single-file document serves tokens"
        );

        workspaces.remove_single_file(uri);
        assert!(
            decode_tokens(&semantic_tokens_full(&workspaces, uri)).is_empty(),
            "the closed single-file document no longer resolves"
        );
    }

    // -----------------------------------------------------------------------
    // Workspace-folder changes over the flat document store (ticket server 10)
    //
    // These drive the exact store mutations the `DID_CHANGE_WORKSPACE_FOLDERS`
    // handler dispatches to (`add_folder` / `remove_folder`) with an open
    // buffer simulated as `didOpen` does (`open_documents` + a diverging
    // `sync_document_content`), then assert the buffer-authority, eviction, and
    // deepest-owns contracts on the observable feature/diagnostic surface.
    // -----------------------------------------------------------------------

    #[test]
    fn folder_add_over_open_rootless_document_keeps_buffer_and_leaves_no_orphan() {
        // Disk holds one content; the open editor buffer diverges. Adding a
        // folder that covers the open document must keep the BUFFER (decision
        // 017 §3), gain the root, and leave exactly one document — no orphaned
        // rootless entry shadowed by disk.
        let dir = workspace_with_files(&[("doc.md", "# On Disk\n")]);
        let root = fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        let abs = root.join("doc.md");
        let uri = path_to_uri(&abs);

        let mut workspaces = Workspaces::new();
        // didOpen with an unsaved buffer diverging from disk, before any folder.
        workspaces.open_documents.insert(uri.clone());
        workspaces.sync_document_content(&uri, "# In Buffer\n\n# In Buffer\n");
        assert!(
            workspaces
                .documents
                .get(&abs)
                .is_some_and(|d| d.primary_root.is_none()),
            "the open document is rootless before the folder is added"
        );

        // DID_CHANGE_WORKSPACE_FOLDERS add.
        workspaces.add_folder(&path_to_uri(&root));

        assert_eq!(
            workspaces.documents.len(),
            1,
            "the open buffer gained a root — no orphaned rootless entry remains"
        );
        let doc = workspaces.documents.get(&abs).expect("document present");
        assert_eq!(
            doc.primary_root.as_deref(),
            Some(root.as_path()),
            "the document now belongs to the added folder"
        );
        assert!(
            doc.data.tree.source().contains("In Buffer")
                && !doc.data.tree.source().contains("On Disk"),
            "the editor buffer is served across the transition, not stale disk"
        );

        // Now rooted, the buffer's duplicate heading publishes as a structural
        // diagnostic — proving the graph/diagnostic tier now sees it.
        let sent = diff_diagnostics(&mut workspaces, &HashSet::new());
        assert!(
            sent.iter().any(|(u, d)| u == &uri && !d.is_empty()),
            "the newly-rooted buffer publishes its diagnostics: {sent:?}"
        );
    }

    #[test]
    fn folder_remove_under_open_document_keeps_serving_rootless() {
        // A folder removed under an open document must not create a dark window:
        // the document recomputes to `primary_root: None` and keeps serving its
        // document-scoped features, now diagnostic-quiet (issue 051).
        let dir = workspace_with_files(&[("doc.md", "# Title\n\n## Section\n")]);
        let root = fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        let abs = root.join("doc.md");
        let uri = path_to_uri(&abs);

        let mut workspaces = Workspaces::new();
        workspaces.add_folder(&path_to_uri(&root));
        workspaces.open_documents.insert(uri.clone());
        workspaces.sync_document_content(&uri, "# Title\n\n## Section\n");
        assert!(
            document_symbols(&workspaces, &uri).is_some_and(|s| !s.is_empty()),
            "the rooted open document serves symbols before the removal"
        );

        // DID_CHANGE_WORKSPACE_FOLDERS remove while the document is open.
        workspaces.remove_folder(&path_to_uri(&root));

        let doc = workspaces.documents.get(&abs).expect("open document kept");
        assert!(
            doc.primary_root.is_none(),
            "the open document recomputed to rootless"
        );
        assert!(
            document_symbols(&workspaces, &uri).is_some_and(|s| !s.is_empty()),
            "document-scoped features keep serving with no dark window"
        );
        assert!(
            diff_diagnostics(&mut workspaces, &one_uri(&uri)).is_empty(),
            "a rootless document publishes no diagnostics"
        );
    }

    #[test]
    fn folder_remove_evicts_scan_only_documents() {
        // A removed folder's scan-only documents (not open in the editor) are
        // evicted; only the open document survives, now rootless.
        let dir = workspace_with_files(&[("a.md", "# A\n"), ("b.md", "# B\n")]);
        let root = fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        let a_abs = root.join("a.md");
        let b_abs = root.join("b.md");

        let mut workspaces = Workspaces::new();
        workspaces.add_folder(&path_to_uri(&root));
        // Open a.md; b.md stays scan-only.
        let a_uri = path_to_uri(&a_abs);
        workspaces.open_documents.insert(a_uri.clone());
        workspaces.sync_document_content(&a_uri, "# A\n");
        assert_eq!(workspaces.documents.len(), 2, "two documents were scanned");

        workspaces.remove_folder(&path_to_uri(&root));

        assert!(
            workspaces.documents.contains_key(&a_abs),
            "the open document survives the removal (rootless)"
        );
        assert!(
            !workspaces.documents.contains_key(&b_abs),
            "the scan-only document is evicted"
        );
        assert_eq!(
            workspaces.documents.len(),
            1,
            "only the open document remains"
        );
    }

    #[test]
    fn nested_folder_without_marker_is_entry_point_into_covering_scope() {
        // Decision 019 clause 7 (ticket server 12): a client folder WITHOUT its
        // own marker declares visibility, not structure — it is an entry point
        // into whatever scope covers it, not a root. This retires ticket 10's
        // config-less deepest-wins quirk: with only the outer `.lattice.toml`, a
        // nested `sub` folder is part of the outer scope, so `sub/inner.md`
        // belongs to the outer root, its link resolves in the outer graph, and it
        // resolves against the outer root — NOT a config-less `sub` root.
        //
        // (Previously this test pinned the opposite: `sub` became its own
        // deepest-wins root and owned resolution. 019 makes markers, not folders,
        // declare where a graph begins.)
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            ("top.md", "[to inner](sub/inner.md \"references\")\n"),
            ("sub/inner.md", "# Inner\n"),
        ]);
        let root = fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        let sub = root.join("sub");
        let inner_abs = sub.join("inner.md");

        let mut workspaces = Workspaces::new();
        workspaces.add_folder(&path_to_uri(&root));
        workspaces.add_folder(&path_to_uri(&sub));

        // `sub` carries no marker, so it is not a registered scope root.
        assert!(
            !workspaces.roots.contains_key(&sub),
            "a folder without its own marker is not a scope root"
        );
        // `sub/inner.md` belongs to the covering (outer) scope.
        assert_eq!(
            workspaces
                .documents
                .get(&inner_abs)
                .and_then(|d| d.primary_root.as_deref()),
            Some(root.as_path()),
            "the nested-folder document belongs to the covering scope, not a config-less sub root"
        );

        // The outer graph owns `sub/inner.md`, so `top.md`'s link into it
        // resolves rather than dangling.
        let ancestor = workspaces.root_view(&root);
        let diags = collect_all_diagnostics(&ancestor);
        assert!(
            !diags
                .iter()
                .any(|d| d.file.as_path() == Path::new("top.md")
                    && d.message.contains("does not exist")),
            "the covering scope sees the nested-folder file, so the link resolves: {diags:?}"
        );

        // The shared descendant file resolves against the covering root.
        let inner_uri = path_to_uri(&inner_abs);
        let (inner_view, _) = workspaces
            .resolve(&inner_uri)
            .expect("the descendant file resolves");
        assert_eq!(
            inner_view.root(),
            root.as_path(),
            "the covering scope owns resolution — the deepest-wins quirk is retired"
        );
    }

    #[test]
    fn placement_change_flips_primary_root_without_reparsing() {
        // Acceptance 1 (ticket server 11), preserved under decision 019: a
        // `primary_root` change that does not cross a config boundary is a
        // metadata flip, never a reparse. Removing the sole folder covering an
        // open document re-roots it to rootless; because link classification is
        // root-free and the effective config (defaults on both sides) is
        // unchanged, the parser (`Tree::links`) must not run.
        //
        // (Ticket 11 pinned this by re-rooting a nested MARKER scope's document
        // onto the ancestor on folder removal. Decision 019 makes that marker a
        // permanent scope that survives its own folder's removal — see
        // `removing_a_marker_scopes_folder_keeps_the_scope` — so the
        // config-preserving flip is now the rootless transition.)
        let dir = workspace_with_files(&[("doc.md", "[peer](peer.md \"references\")\n")]);
        let root = fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        let doc_abs = root.join("doc.md");
        let doc_uri = path_to_uri(&doc_abs);

        let mut workspaces = Workspaces::new();
        workspaces.add_folder(&path_to_uri(&root));
        // Open the buffer so it survives the removal (decision 017 §3).
        workspaces.open_documents.insert(doc_uri.clone());
        workspaces.sync_document_content(&doc_uri, "[peer](peer.md \"references\")\n");
        assert_eq!(
            workspaces
                .documents
                .get(&doc_abs)
                .and_then(|d| d.primary_root.as_deref()),
            Some(root.as_path()),
            "the open document belongs to the folder before removal"
        );

        // Re-root to rootless by removing the folder, counting parser runs.
        crate::block::reset_extract_counts();
        workspaces.remove_folder(&path_to_uri(&root));
        assert_eq!(
            crate::block::links_extract_count(),
            0,
            "a config-preserving re-root must not re-run the parser (links classify root-free)"
        );
        assert!(
            workspaces
                .documents
                .get(&doc_abs)
                .is_some_and(|d| d.primary_root.is_none()),
            "the open document re-rooted to rootless without a reparse"
        );
    }

    #[test]
    fn removing_a_marker_scopes_folder_keeps_the_scope() {
        // Decision 019 clause 7 (ticket server 12): markers declare structure,
        // folders declare visibility. A nested marker scope discovered by
        // walk-down survives the removal of its own client folder, because the
        // covering folder keeps it visible — so its documents are not re-rooted,
        // and nothing reparses.
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            ("sub/.lattice.toml", ""),
            ("sub/inner.md", "[peer](peer.md \"references\")\n"),
            ("sub/peer.md", "# Peer\n"),
        ]);
        let root = fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        let sub = root.join("sub");
        let inner_abs = sub.join("inner.md");

        let mut workspaces = Workspaces::new();
        workspaces.add_folder(&path_to_uri(&root));
        workspaces.add_folder(&path_to_uri(&sub));
        assert_eq!(
            workspaces
                .documents
                .get(&inner_abs)
                .and_then(|d| d.primary_root.as_deref()),
            Some(sub.as_path()),
            "the nested marker scope owns its document"
        );

        crate::block::reset_extract_counts();
        workspaces.remove_folder(&path_to_uri(&sub));
        assert_eq!(
            crate::block::links_extract_count(),
            0,
            "removing a still-covered marker scope's folder disturbs nothing — no reparse"
        );
        assert!(
            workspaces.roots.contains_key(&sub),
            "the nested marker scope stays registered — the outer folder keeps it visible"
        );
        assert_eq!(
            workspaces
                .documents
                .get(&inner_abs)
                .and_then(|d| d.primary_root.as_deref()),
            Some(sub.as_path()),
            "the nested document keeps its scope — no re-root"
        );
    }

    /// The cached first-link target of `inner_abs` (a document-relative link,
    /// resolved to an absolute path — decision 019 clause 8).
    fn first_link_target(workspaces: &Workspaces, inner_abs: &Path) -> PathBuf {
        match &workspaces
            .documents
            .get(inner_abs)
            .expect("inner indexed")
            .data
            .links[0]
            .kind
        {
            LinkKind::IntraProject { target, .. } => target.clone(),
            other => panic!("expected an intra-project link, got {other:?}"),
        }
    }

    #[test]
    fn nested_scope_is_a_disjoint_graph_regardless_of_entry_point() {
        // Decision 019 / acceptance 1 (ticket server 12): a nested `.lattice.toml`
        // partitions the tree into two disjoint graphs, and the partition is the
        // same whichever entry point observes it. Entering from the outer root
        // (walk-down) or opening the nested scope directly both give the nested
        // scope ownership of its documents; the outer scope never sees them.
        //
        // (Under ticket 11 the ancestor's range-scan view still saw the
        // descendant, so the two views "agreed" on the link target. 019 makes the
        // scopes disjoint — the ancestor no longer sees the descendant at all.)
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            ("sub/.lattice.toml", ""),
            ("sub/inner.md", "[peer](peer.md \"references\")\n"),
            ("sub/peer.md", "# Peer\n"),
        ]);
        let root = fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        let sub = root.join("sub");
        let inner_abs = sub.join("inner.md");
        let peer_abs = sub.join("peer.md");

        // Entry point A: open only the outer root. Walk-down registers the
        // nested scope; `sub/inner.md` belongs to it, not to the outer scope.
        let mut a = Workspaces::new();
        a.add_folder(&path_to_uri(&root));
        assert!(
            a.roots.contains_key(&sub),
            "walk-down from the outer root registers the nested scope"
        );
        assert_eq!(
            a.documents
                .get(&inner_abs)
                .and_then(|d| d.primary_root.as_deref()),
            Some(sub.as_path()),
            "the nested scope owns its document, entered from the outer root"
        );
        // The nested scope resolves the intra-scope link to sub/peer.md.
        let target = first_link_target(&a, &inner_abs);
        let sub_view = a.root_view(&sub);
        let key = sub_view
            .resolve_key(&target)
            .expect("nested scope resolves its intra-scope link");
        assert_eq!(
            sub_view.root().join(key),
            peer_abs,
            "the nested scope resolves inner.md's link to sub/peer.md"
        );
        // The outer scope is disjoint: its graph contains neither nested file.
        let root_view = a.root_view(&root);
        assert!(
            root_view.file(Path::new("sub/inner.md")).is_none()
                && root_view.file(Path::new("sub/peer.md")).is_none(),
            "the outer scope's graph does not see the nested scope's documents"
        );

        // Entry point B: open the nested scope directly. The same partition — the
        // nested scope owns its documents and resolves the same link.
        let mut b = Workspaces::new();
        b.add_folder(&path_to_uri(&sub));
        assert_eq!(
            b.documents
                .get(&inner_abs)
                .and_then(|d| d.primary_root.as_deref()),
            Some(sub.as_path()),
            "the nested scope owns its document, entered directly"
        );
        let target_b = first_link_target(&b, &inner_abs);
        let sub_view_b = b.root_view(&sub);
        let key_b = sub_view_b
            .resolve_key(&target_b)
            .expect("direct entry resolves the same intra-scope link");
        assert_eq!(
            sub_view_b.root().join(key_b),
            peer_abs,
            "the same link resolves to sub/peer.md regardless of entry point"
        );
    }

    #[test]
    fn marker_split_reroots_subtree_without_reparsing_host() {
        // Decision 019 clause 6 (ticket server 12): creating a nested marker
        // mid-session splits its subtree into its own graph. The host's
        // now-crossing plain link resurfaces as the steering error, the split
        // subtree re-roots — and only the split subtree is (re)scanned; the host's
        // documents are never reparsed.
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            ("outer.md", "[down](sub/inner.md \"references\")\n"),
            ("sub/inner.md", "# Inner\n"),
        ]);
        let root = fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        let sub = root.join("sub");
        let outer_abs = root.join("outer.md");
        let inner_abs = sub.join("inner.md");

        let mut workspaces = Workspaces::new();
        workspaces.add_folder(&path_to_uri(&root));

        // Before the split: one scope; the link is in-scope and resolves.
        let before = collect_all_diagnostics(&workspaces.root_view(&root));
        assert!(
            !before
                .iter()
                .any(|d| d.message.contains("outside this scope")),
            "before the split the link is in-scope: {before:?}"
        );
        assert_eq!(
            workspaces
                .documents
                .get(&inner_abs)
                .and_then(|d| d.primary_root.as_deref()),
            Some(root.as_path()),
            "before the split sub/inner.md belongs to the single scope"
        );

        // Create the nested marker and deliver its watcher event.
        fs::write(sub.join(".lattice.toml"), "").expect("create nested marker");
        let marker_uri = path_to_uri(&sub.join(".lattice.toml"));
        crate::block::reset_extract_counts();
        assert!(
            workspaces.handle_marker_event(&marker_uri),
            "the split is applied"
        );
        // Only the split subtree (sub/inner.md) is scanned; the host is untouched.
        assert_eq!(
            crate::block::links_extract_count(),
            1,
            "only the split subtree is scanned — the host document is not reparsed"
        );

        assert!(
            workspaces.roots.contains_key(&sub),
            "the nested scope is registered"
        );
        assert_eq!(
            workspaces
                .documents
                .get(&inner_abs)
                .and_then(|d| d.primary_root.as_deref()),
            Some(sub.as_path()),
            "sub/inner.md re-rooted into the new scope"
        );
        assert_eq!(
            workspaces
                .documents
                .get(&outer_abs)
                .and_then(|d| d.primary_root.as_deref()),
            Some(root.as_path()),
            "outer.md stays in the host scope"
        );
        // The host's link now crosses the boundary → steering error.
        let after = collect_all_diagnostics(&workspaces.root_view(&root));
        assert!(
            after
                .iter()
                .any(|d| d.file.as_path() == Path::new("outer.md")
                    && d.message.contains("outside this scope")),
            "the now-crossing link steers to the alias: {after:?}"
        );
    }

    #[test]
    fn marker_merge_fuses_scopes_and_resurfaces_debt() {
        // Decision 019 clause 6: deleting a nested marker merges its subtree back
        // into the host — the scopes fuse (a config-preserving metadata flip, no
        // reparse), the boundary-crossing steering error clears, and the
        // reconciliation debt suppressed while the scopes were separate (the
        // missing backlink for the now-in-scope edge) re-surfaces.
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            ("sub/.lattice.toml", ""),
            ("outer.md", "[down](sub/inner.md \"references\")\n"),
            ("sub/inner.md", "# Inner\n"),
        ]);
        let root = fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        let sub = root.join("sub");
        let inner_abs = sub.join("inner.md");

        let mut workspaces = Workspaces::new();
        workspaces.add_folder(&path_to_uri(&root));
        let before = collect_all_diagnostics(&workspaces.root_view(&root));
        assert!(
            before
                .iter()
                .any(|d| d.message.contains("outside this scope")),
            "the cross-boundary link steers before the merge: {before:?}"
        );

        // Delete the nested marker and deliver its watcher event.
        fs::remove_file(sub.join(".lattice.toml")).expect("delete nested marker");
        let marker_uri = path_to_uri(&sub.join(".lattice.toml"));
        crate::block::reset_extract_counts();
        assert!(
            workspaces.handle_marker_event(&marker_uri),
            "the merge is applied"
        );
        assert_eq!(
            crate::block::links_extract_count(),
            0,
            "the merge re-roots without a reparse (config unchanged)"
        );

        assert!(
            !workspaces.roots.contains_key(&sub),
            "the nested scope deregisters"
        );
        assert_eq!(
            workspaces
                .documents
                .get(&inner_abs)
                .and_then(|d| d.primary_root.as_deref()),
            Some(root.as_path()),
            "sub/inner.md re-roots into the host scope"
        );

        let after = collect_all_diagnostics(&workspaces.root_view(&root));
        assert!(
            !after
                .iter()
                .any(|d| d.message.contains("outside this scope")),
            "the steering error clears on fusion: {after:?}"
        );
        assert!(
            after
                .iter()
                .any(|d| d.message.contains("expected backlink")),
            "the suppressed reconciliation debt (a missing backlink) re-surfaces: {after:?}"
        );
    }

    #[test]
    fn nested_git_excluded_from_host_and_served_rootless_under_defaults() {
        // Decision 019 resolution 2 (ticket server 12): a nested `.git` (no
        // marker) is a non-root environment — excluded from the host graph and
        // never eagerly indexed. Opened directly, its document is rootless and
        // serves document-scoped features under defaults (051 semantics).
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            (
                "outer.md",
                "[into vendor](vendor/inner.md \"references\")\n",
            ),
        ]);
        let root = fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        fs::create_dir_all(root.join("vendor")).expect("create vendor dir");
        fs::create_dir(root.join("vendor/.git")).expect("create nested .git");
        fs::write(root.join("vendor/inner.md"), "# Vendored\n\n## Section\n")
            .expect("write vendored file");
        let inner_abs = root.join("vendor/inner.md");
        let inner_uri = path_to_uri(&inner_abs);

        let mut workspaces = Workspaces::new();
        workspaces.add_folder(&path_to_uri(&root));

        // The nested `.git` is never eagerly indexed: its file is not in the store.
        assert!(
            !workspaces.documents.contains_key(&inner_abs),
            "a nested `.git` repo is not eagerly scanned"
        );
        // The host's link into it crosses the boundary → steering error.
        let host = collect_all_diagnostics(&workspaces.root_view(&root));
        assert!(
            host.iter()
                .any(|d| d.file.as_path() == Path::new("outer.md")
                    && d.message.contains("outside this scope")),
            "the link into the nested `.git` steers to an alias: {host:?}"
        );

        // Opened directly, the document is rootless (excluded from the host graph)
        // and serves document-scoped features (symbols) under defaults.
        workspaces.open_documents.insert(inner_uri.clone());
        workspaces.sync_document_content(&inner_uri, "# Vendored\n\n## Section\n");
        assert!(
            workspaces
                .documents
                .get(&inner_abs)
                .is_some_and(|d| d.primary_root.is_none()),
            "a document behind a nested `.git` is rootless, not a host member"
        );
        assert!(
            document_symbols(&workspaces, &inner_uri).is_some_and(|s| !s.is_empty()),
            "the nested `.git` document serves document-scoped features under defaults"
        );
        // It is still excluded from the host graph after opening.
        assert!(
            workspaces
                .root_view(&root)
                .file(Path::new("vendor/inner.md"))
                .is_none(),
            "the nested `.git` document never joins the host graph"
        );
    }

    #[test]
    fn cross_boundary_physical_reference_is_not_a_reference() {
        // Navigation decision (ticket server 12): `find_references` is a
        // graph-edge query, and scopes are disjoint graphs (decision 019). A
        // physical `../` reference from a foreign scope is a clause-3 defect, not
        // an edge — so it must NOT surface as a reference to the target.
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            ("sub/.lattice.toml", ""),
            // The nested scope physically points up-and-over at the host's target.
            ("sub/inner.md", "[cross](../target.md \"references\")\n"),
            ("target.md", "# Target\n\nbody\n"),
            // A same-scope reference to the target, which MUST surface.
            ("peer.md", "[peer](target.md \"references\")\n"),
        ]);
        let root = fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        let sub = root.join("sub");
        let target_uri = path_to_uri(&root.join("target.md"));

        let mut workspaces = Workspaces::new();
        workspaces.add_folder(&path_to_uri(&root));
        assert!(
            workspaces.roots.contains_key(&sub),
            "the nested scope is registered"
        );

        let refs = find_references(
            &workspaces,
            &lsp::ReferenceParams {
                text_document: lsp::TextDocumentIdentifier { uri: target_uri },
                // A non-heading line, so all links to the file match (not just
                // fragment-links to a heading).
                position: lsp::Position {
                    line: 2,
                    character: 0,
                },
            },
        );
        let ref_uris: Vec<&str> = refs.iter().map(|loc| loc.uri.as_str()).collect();
        assert!(
            ref_uris.iter().any(|u| u.ends_with("/peer.md")),
            "a same-scope reference to the target surfaces: {ref_uris:?}"
        );
        assert!(
            !ref_uris.iter().any(|u| u.ends_with("/inner.md")),
            "a cross-boundary physical reference from the nested scope is not a reference: {ref_uris:?}"
        );
    }

    #[test]
    fn rootless_open_skips_workspace_structural_sweep() {
        // A rootless document appears in no root's range scan, so opening one
        // cannot flip any rooted document's bare-path existence answer — it
        // must not pay the O(workspace) structural sweep. A rooted open that
        // grows membership still must (ticket server 10 review).
        let dir = workspace_with_files(&[("a.md", "# A\n"), ("b.md", "# B\n")]);
        let root = fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        let mut workspaces = Workspaces::new();
        workspaces.add_folder(&path_to_uri(&root));

        let (sweeps, ()) = count_structural_sweeps(|| {
            workspaces.sync_document_content("file:///tmp/lattice-10r-scratch.md", "# S\n");
        });
        assert_eq!(sweeps, 0, "a rootless open sweeps no structural caches");

        let (sweeps, ()) = count_structural_sweeps(|| {
            workspaces.sync_document_content(&path_to_uri(&root.join("new.md")), "# N\n");
        });
        assert_eq!(
            sweeps, 1,
            "a rooted open that grows membership sweeps exactly once"
        );
    }

    /// Drain server messages until a `Response` for `id`, returning its result.
    fn recv_response_for(client: &Connection, id: i32) -> serde_json::Value {
        for _ in 0..32 {
            if let Message::Response(resp) = recv_message(client)
                && resp.id == RequestId::from(id)
            {
                return resp.result.unwrap_or(serde_json::Value::Null);
            }
        }
        panic!("no response for request {id} after 32 messages");
    }

    #[test]
    fn rootless_session_serves_semantic_tokens_end_to_end() {
        // The issue 051 repro end-to-end: a rootless `initialize` (no
        // workspaceFolders, rootUri null) followed by a `didOpen` and a
        // `textDocument/semanticTokens/full` request must return non-empty
        // token data — before the fix it returned `{ "data": [] }`.
        let (server, client) = Connection::memory();
        let server_thread = std::thread::spawn(move || serve(&server));

        // Rootless handshake: no folders, no rootUri, no dynamic registration
        // (so the server originates no registerCapability and the stream stays
        // clean).
        let init = Request::new(
            RequestId::from(1),
            "initialize".to_string(),
            serde_json::json!({
                "capabilities": {},
                "rootUri": serde_json::Value::Null,
                "workspaceFolders": serde_json::Value::Null
            }),
        );
        client
            .sender
            .send(Message::Request(init))
            .expect("send initialize");
        let resp = recv_message(&client);
        assert!(
            matches!(resp, Message::Response(_)),
            "a rootless initialize must be answered with a response, got {resp:?}"
        );
        send_notification(&client, "initialized", serde_json::json!({}));

        let doc_uri = "file:///tmp/lattice-051-e2e.md";
        open_doc(
            &client,
            doc_uri,
            "**bold**, *italic*, and ~~strikethrough~~\n",
        );

        let tokens_req_id = 42;
        client
            .sender
            .send(Message::Request(Request::new(
                RequestId::from(tokens_req_id),
                lsp::method::SEMANTIC_TOKENS_FULL.to_string(),
                serde_json::json!({ "textDocument": { "uri": doc_uri } }),
            )))
            .expect("send semanticTokens/full");

        let result = recv_response_for(&client, tokens_req_id);
        let data = result
            .get("data")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        assert!(
            !data.is_empty(),
            "a rootless session must serve non-empty semantic tokens, got {result:?}"
        );
        assert_eq!(
            data.len() % 5,
            0,
            "token data must be a flat 5-tuple stream, got {data:?}"
        );

        shutdown(&client, server_thread);
    }

    // -----------------------------------------------------------------------
    // Watched-files hot-reload (ticket server 08, decision 017)
    //
    // Wire-level: an in-memory `Connection` pair drives the real `serve`
    // lifecycle so the server→client `client/registerCapability` request and
    // the `workspace/didChangeWatchedFiles` reload path are exercised exactly
    // as a client would.
    // -----------------------------------------------------------------------

    /// Block for the next message from the server, failing on timeout.
    fn recv_message(client: &Connection) -> Message {
        client
            .receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("timed out waiting for a server message")
    }

    /// Drain server messages until a `publishDiagnostics` for `uri`, returning
    /// its diagnostics array (the wire JSON — the lsp types are send-only).
    fn recv_publish_for(client: &Connection, uri: &str) -> Vec<serde_json::Value> {
        for _ in 0..32 {
            if let Message::Notification(notif) = recv_message(client)
                && notif.method == lsp::method::PUBLISH_DIAGNOSTICS
                && notif.params.get("uri").and_then(serde_json::Value::as_str) == Some(uri)
            {
                return notif
                    .params
                    .get("diagnostics")
                    .and_then(serde_json::Value::as_array)
                    .cloned()
                    .unwrap_or_default();
            }
        }
        panic!("no publishDiagnostics for {uri} after 32 messages");
    }

    /// Send a JSON-RPC notification from the client to the server.
    fn send_notification(client: &Connection, method: &str, params: serde_json::Value) {
        client
            .sender
            .send(Message::Notification(Notification::new(
                method.to_string(),
                params,
            )))
            .expect("send notification");
    }

    /// Drive `initialize` + `initialized`, returning once the server is ready.
    /// `dynamic_registration` advertises the watched-files client capability.
    fn handshake(client: &Connection, root_uri: &str, dynamic_registration: bool) {
        let init = Request::new(
            RequestId::from(1),
            "initialize".to_string(),
            serde_json::json!({
                "capabilities": {
                    "workspace": {
                        "didChangeWatchedFiles": {
                            "dynamicRegistration": dynamic_registration
                        }
                    }
                },
                "workspaceFolders": [ { "uri": root_uri } ]
            }),
        );
        client
            .sender
            .send(Message::Request(init))
            .expect("send initialize");
        let resp = recv_message(client);
        assert!(
            matches!(resp, Message::Response(_)),
            "initialize must be answered with a response, got {resp:?}"
        );
        send_notification(client, "initialized", serde_json::json!({}));
    }

    /// Open `doc.md` with `text` so the server publishes its diagnostics.
    fn open_doc(client: &Connection, uri: &str, text: &str) {
        send_notification(
            client,
            lsp::method::DID_OPEN,
            serde_json::json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": "markdown",
                    "version": 1,
                    "text": text
                }
            }),
        );
    }

    /// Shut the server down cleanly and join its thread.
    fn shutdown(client: &Connection, server_thread: std::thread::JoinHandle<Result<()>>) {
        client
            .sender
            .send(Message::Request(Request::new(
                RequestId::from(9999),
                "shutdown".to_string(),
                serde_json::Value::Null,
            )))
            .expect("send shutdown");
        send_notification(client, "exit", serde_json::json!(null));
        server_thread
            .join()
            .expect("server thread panicked")
            .expect("server returned an error");
    }

    /// Drain server messages until a `Response` for `id`, returning its error
    /// code — the response must be an error, not a result.
    fn recv_error_code_for(client: &Connection, id: i32) -> i32 {
        for _ in 0..32 {
            if let Message::Response(resp) = recv_message(client)
                && resp.id == RequestId::from(id)
            {
                return resp
                    .error
                    .expect("an unadvertised method is answered with an error response")
                    .code;
            }
        }
        panic!("no response for request {id} after 32 messages");
    }

    // -----------------------------------------------------------------------
    // Push-only diagnostics (decision 022, ticket integration 16)
    //
    // Wire-level: `didOpen` is a client-state boundary, so reopening an
    // unchanged document re-publishes its current set — the client's memory is
    // unknowable — and an unadvertised pull request is answered MethodNotFound.
    // -----------------------------------------------------------------------

    #[test]
    fn reopen_republishes_unchanged_document() {
        // Close then reopen with identical text: the server cannot know the
        // client still holds the document's diagnostics, so it re-sends the
        // current set — the non-empty set for a file carrying diagnostics, and an
        // explicit empty set for a clean file (a publish, never a skip).
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            ("a.md", "# A\n\n[broken](nonexistent.md \"references\")\n"),
            ("clean.md", "# Clean\n"),
        ]);
        let root = fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        let root_uri = path_to_uri(&root);
        let dirty_uri = path_to_uri(&root.join("a.md"));
        let clean_uri = path_to_uri(&root.join("clean.md"));

        let (server, client) = Connection::memory();
        let server_thread = std::thread::spawn(move || serve(&server));

        // No dynamic registration, so the server originates no registerCapability
        // and the stream carries only publishDiagnostics.
        handshake(&client, &root_uri, false);

        // First open establishes each document's published record: a.md carries
        // its broken-reference diagnostic, clean.md gets an explicit empty set.
        open_doc(
            &client,
            &dirty_uri,
            "# A\n\n[broken](nonexistent.md \"references\")\n",
        );
        assert!(
            !recv_publish_for(&client, &dirty_uri).is_empty(),
            "opening a.md publishes its broken-reference diagnostic"
        );
        open_doc(&client, &clean_uri, "# Clean\n");
        assert!(
            recv_publish_for(&client, &clean_uri).is_empty(),
            "opening the clean clean.md publishes an explicit empty set"
        );

        // Close both. Disk matches the buffer, so neither close moves a publish.
        send_close(&client, &dirty_uri);
        send_close(&client, &clean_uri);

        // Reopen with identical text. Even though nothing changed, each document
        // is re-published: the diff record was invalidated on open.
        open_doc(
            &client,
            &dirty_uri,
            "# A\n\n[broken](nonexistent.md \"references\")\n",
        );
        assert!(
            !recv_publish_for(&client, &dirty_uri).is_empty(),
            "reopening the unchanged a.md re-publishes its current (non-empty) set"
        );

        open_doc(&client, &clean_uri, "# Clean\n");
        assert!(
            recv_publish_for(&client, &clean_uri).is_empty(),
            "reopening the unchanged clean.md re-publishes an explicit empty set"
        );

        shutdown(&client, server_thread);
    }

    #[test]
    fn pull_diagnostic_request_is_method_not_found() {
        // Pull is unadvertised and, by design, unimplemented (decision 022): a
        // `textDocument/diagnostic` request over the wire falls to the default
        // dispatch arm and is answered with a MethodNotFound error, not a report.
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            ("a.md", "# A\n\n[broken](nonexistent.md \"references\")\n"),
        ]);
        let root = fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        let root_uri = path_to_uri(&root);
        let doc_uri = path_to_uri(&root.join("a.md"));

        let (server, client) = Connection::memory();
        let server_thread = std::thread::spawn(move || serve(&server));

        handshake(&client, &root_uri, false);

        let req_id = 7;
        client
            .sender
            .send(Message::Request(Request::new(
                RequestId::from(req_id),
                "textDocument/diagnostic".to_string(),
                serde_json::json!({ "textDocument": { "uri": doc_uri } }),
            )))
            .expect("send textDocument/diagnostic");

        let code = recv_error_code_for(&client, req_id);
        assert_eq!(
            code,
            lsp_server::ErrorCode::MethodNotFound as i32,
            "an unadvertised pull request is answered MethodNotFound, got code {code}"
        );

        shutdown(&client, server_thread);
    }

    #[test]
    fn watched_files_registration_and_reload() {
        // Full acceptance loop (ticket server 08): a client advertising
        // dynamic registration receives a `client/registerCapability` request
        // for `**/.lattice.toml`; editing the marker and delivering the
        // watched-file change reloads config and re-publishes — the stale
        // reference is cleared by the new `[graph] artifacts` entry.
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            ("doc.md", "See `artifact.md` here.\n"),
        ]);
        let root = fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        let root_uri = path_to_uri(&root);
        let doc_uri = path_to_uri(&root.join("doc.md"));
        let marker_uri = path_to_uri(&root.join(".lattice.toml"));

        let (server, client) = Connection::memory();
        let server_thread = std::thread::spawn(move || serve(&server));

        handshake(&client, &root_uri, true);

        // The server registers the marker watcher.
        let reg = recv_message(&client);
        let Message::Request(reg) = reg else {
            panic!("expected a client/registerCapability request, got {reg:?}");
        };
        assert_eq!(
            reg.method,
            lsp::method::REGISTER_CAPABILITY,
            "the server originates a registerCapability request"
        );
        let reg_json = serde_json::to_string(&reg.params).expect("serialize registration params");
        assert!(
            reg_json.contains(lsp::method::DID_CHANGE_WATCHED_FILES),
            "the registration is for the watched-files method: {reg_json}"
        );
        assert!(
            reg_json.contains(LATTICE_TOML_WATCH_GLOB),
            "the registration watches the marker glob: {reg_json}"
        );
        assert!(
            reg_json.contains(MD_WATCH_GLOB),
            "the registration also watches the markdown document glob (ticket server 09): {reg_json}"
        );

        // Opening doc.md surfaces the stale-reference diagnostic.
        open_doc(&client, &doc_uri, "See `artifact.md` here.\n");
        let before = recv_publish_for(&client, &doc_uri);
        assert!(
            !before.is_empty(),
            "doc.md starts with the stale-reference diagnostic, got {before:?}"
        );

        // Edit the marker on disk, then deliver the watched-file change.
        fs::write(
            root.join(".lattice.toml"),
            "[graph]\nartifacts = [\"artifact.md\"]\n",
        )
        .expect("rewrite the marker");
        send_notification(
            &client,
            lsp::method::DID_CHANGE_WATCHED_FILES,
            serde_json::json!({ "changes": [ { "uri": marker_uri, "type": 2 } ] }),
        );

        // The reload re-publishes doc.md, now cleared by the artifact glossary.
        let after = recv_publish_for(&client, &doc_uri);
        assert!(
            after.is_empty(),
            "the reloaded [graph] artifacts entry clears doc.md, got {after:?}"
        );

        shutdown(&client, server_thread);
    }

    #[test]
    fn wire_marker_create_splits_scope_preserving_buffers() {
        // Wire-level split (decision 019 clause 6, ticket server 12 acceptance):
        // creating a nested marker mid-session re-roots its subtree into a new
        // graph. The host's now-crossing plain link surfaces the steering error,
        // and a re-rooted OPEN document keeps its unsaved buffer (decision 017 §3)
        // — served, not clobbered by disk.
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            ("outer.md", "[down](sub/inner.md \"references\")\n"),
            ("sub/inner.md", "# Inner\n"),
        ]);
        let root = fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        let root_uri = path_to_uri(&root);
        let outer_uri = path_to_uri(&root.join("outer.md"));
        let inner_uri = path_to_uri(&root.join("sub/inner.md"));

        let (server, client) = Connection::memory();
        let server_thread = std::thread::spawn(move || serve(&server));
        handshake(&client, &root_uri, true);
        let _reg = recv_message(&client); // client/registerCapability

        // Open sub/inner.md with an unsaved buffer that differs from disk: a link
        // that resolves in the single scope but escapes once the scope splits.
        open_doc(
            &client,
            &inner_uri,
            "# Inner\n\n[up](../outer.md \"references\")\n",
        );
        let inner_before = recv_publish_for(&client, &inner_uri);
        assert!(
            !any_message_contains(&inner_before, "outside this scope"),
            "before the split the buffer's `../outer.md` link is in-scope: {inner_before:?}"
        );

        // Create the nested marker on disk and deliver the watcher event.
        fs::write(root.join("sub/.lattice.toml"), "").expect("create nested marker");
        let marker_uri = path_to_uri(&root.join("sub/.lattice.toml"));
        send_watched_change(&client, &marker_uri, lsp::file_change_type::CREATED);

        // The host's link now crosses the boundary → steering error.
        let outer_after = recv_publish_for(&client, &outer_uri);
        assert!(
            any_message_contains(&outer_after, "outside this scope"),
            "the split surfaces the steering error on the host's link: {outer_after:?}"
        );
        // The re-rooted OPEN document served its preserved buffer: the buffer-only
        // `../outer.md` link now escapes the nested scope and steers.
        let inner_after = recv_publish_for(&client, &inner_uri);
        assert!(
            any_message_contains(&inner_after, "outside this scope"),
            "the re-rooted document kept its buffer (its escaping link now steers): {inner_after:?}"
        );

        shutdown(&client, server_thread);
    }

    #[test]
    fn wire_marker_delete_merges_scopes_and_resurfaces_debt() {
        // Wire-level merge (decision 019 clause 6, ticket server 12 acceptance):
        // deleting a nested marker fuses the scopes. The boundary-crossing
        // steering error clears, and the reconciliation debt suppressed while the
        // scopes were separate — the missing backlink for the now-live edge —
        // re-surfaces.
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            ("sub/.lattice.toml", ""),
            ("outer.md", "[down](sub/inner.md \"references\")\n"),
            ("sub/inner.md", "# Inner\n"),
        ]);
        let root = fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        let root_uri = path_to_uri(&root);
        let outer_uri = path_to_uri(&root.join("outer.md"));

        let (server, client) = Connection::memory();
        let server_thread = std::thread::spawn(move || serve(&server));
        handshake(&client, &root_uri, true);
        let _reg = recv_message(&client); // client/registerCapability

        // The link into the nested scope steers (two disjoint graphs).
        open_doc(&client, &outer_uri, "[down](sub/inner.md \"references\")\n");
        let before = recv_publish_for(&client, &outer_uri);
        assert!(
            any_message_contains(&before, "outside this scope"),
            "the cross-boundary link steers before the merge: {before:?}"
        );

        // Delete the nested marker and deliver the watcher event.
        fs::remove_file(root.join("sub/.lattice.toml")).expect("delete nested marker");
        let marker_uri = path_to_uri(&root.join("sub/.lattice.toml"));
        send_watched_change(&client, &marker_uri, lsp::file_change_type::DELETED);

        // The scopes fuse: the steering error clears (the link is now in-scope),
        // and the missing-backlink debt for the now-live edge re-surfaces.
        let after = recv_publish_for(&client, &outer_uri);
        assert!(
            !any_message_contains(&after, "outside this scope"),
            "the merge clears the steering error: {after:?}"
        );
        assert!(
            any_message_contains(&after, "expected backlink"),
            "the fused scope re-surfaces the missing-backlink debt: {after:?}"
        );

        shutdown(&client, server_thread);
    }

    #[test]
    fn no_registration_without_dynamic_capability() {
        // A client that does not advertise dynamic registration must receive no
        // registration, and the server must keep working (decision 017's
        // graceful degradation — no own-watcher, no panic).
        let dir = workspace_with_files(&[("doc.md", "See `artifact.md` here.\n")]);
        let root = fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        let root_uri = path_to_uri(&root);
        let doc_uri = path_to_uri(&root.join("doc.md"));

        let (server, client) = Connection::memory();
        let server_thread = std::thread::spawn(move || serve(&server));

        handshake(&client, &root_uri, false);

        // No registerCapability request is sent: the server is silent until a
        // notification drives a publish, so a short wait must time out.
        assert!(
            client
                .receiver
                .recv_timeout(std::time::Duration::from_millis(300))
                .is_err(),
            "a client without dynamic registration receives no registration request"
        );

        // The server still serves: a didOpen yields diagnostics.
        open_doc(&client, &doc_uri, "See `artifact.md` here.\n");
        let publish = recv_publish_for(&client, &doc_uri);
        assert!(
            !publish.is_empty(),
            "the server still publishes diagnostics without a watcher, got {publish:?}"
        );

        shutdown(&client, server_thread);
    }

    #[test]
    fn survives_malformed_watched_files_payload() {
        // Ticket 06 resilience holds: a malformed `didChangeWatchedFiles`
        // payload is logged-and-dropped, never fatal — a follow-up didOpen
        // still publishes.
        let dir = workspace_with_files(&[("doc.md", "See `artifact.md` here.\n")]);
        let root = fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        let root_uri = path_to_uri(&root);
        let doc_uri = path_to_uri(&root.join("doc.md"));

        let (server, client) = Connection::memory();
        let server_thread = std::thread::spawn(move || serve(&server));

        handshake(&client, &root_uri, true);
        let reg = recv_message(&client);
        assert!(
            matches!(reg, Message::Request(_)),
            "the registration request precedes the malformed payload, got {reg:?}"
        );

        // A payload with no `changes` array fails to deserialize.
        send_notification(
            &client,
            lsp::method::DID_CHANGE_WATCHED_FILES,
            serde_json::json!({ "unexpected": true }),
        );

        // The server survived: a follow-up didOpen still publishes.
        open_doc(&client, &doc_uri, "See `artifact.md` here.\n");
        let publish = recv_publish_for(&client, &doc_uri);
        assert!(
            !publish.is_empty(),
            "the server survived the malformed payload and still publishes, got {publish:?}"
        );

        shutdown(&client, server_thread);
    }

    // -----------------------------------------------------------------------
    // Watched-file document sync (ticket server 09, decision 017 §3)
    //
    // Same in-memory `Connection` harness as the ticket-08 tests: a `**/*.md`
    // watcher reconciled with the document-sync channel by the buffer-wins
    // rule and an open-document set.
    // -----------------------------------------------------------------------

    /// Deliver a `workspace/didChangeWatchedFiles` event for one URI with the
    /// given `FileChangeType` (1 = created, 2 = changed, 3 = deleted).
    fn send_watched_change(client: &Connection, uri: &str, change_type: u8) {
        send_notification(
            client,
            lsp::method::DID_CHANGE_WATCHED_FILES,
            serde_json::json!({ "changes": [ { "uri": uri, "type": change_type } ] }),
        );
    }

    /// Deliver a `textDocument/didClose` for `uri`.
    fn send_close(client: &Connection, uri: &str) {
        send_notification(
            client,
            lsp::method::DID_CLOSE,
            serde_json::json!({ "textDocument": { "uri": uri } }),
        );
    }

    /// Deliver a `textDocument/didChange` for `uri` that replaces the whole
    /// document with `text` (a single full-content change).
    fn send_change(client: &Connection, uri: &str, text: &str) {
        send_notification(
            client,
            lsp::method::DID_CHANGE,
            serde_json::json!({
                "textDocument": { "uri": uri, "version": 2 },
                "contentChanges": [ { "text": text } ]
            }),
        );
    }

    /// Whether any diagnostic in `diags` (wire JSON) has a message containing
    /// `needle`.
    fn any_message_contains(diags: &[serde_json::Value], needle: &str) -> bool {
        diags.iter().any(|d| {
            d.get("message")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|m| m.contains(needle))
        })
    }

    /// Send a request and drain server messages until its matching response,
    /// returning the response `result` (or `Null` on an error response).
    fn request_response(
        client: &Connection,
        id: i32,
        method: &str,
        params: serde_json::Value,
    ) -> serde_json::Value {
        client
            .sender
            .send(Message::Request(Request::new(
                RequestId::from(id),
                method.to_string(),
                params,
            )))
            .expect("send request");
        for _ in 0..32 {
            if let Message::Response(resp) = recv_message(client)
                && resp.id == RequestId::from(id)
            {
                return resp.result.unwrap_or(serde_json::Value::Null);
            }
        }
        panic!("no response for request {id} after 32 messages");
    }

    #[test]
    fn closed_md_disk_change_updates_own_diagnostics() {
        // A `.md` file that is never opened in the editor still tracks disk: a
        // clean file edited on disk to introduce a stale reference reaches the
        // workspace graph through the watcher, without a restart.
        let dir = workspace_with_files(&[("doc.md", "# Clean\n")]);
        let root = fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        let root_uri = path_to_uri(&root);
        let doc_uri = path_to_uri(&root.join("doc.md"));

        let (server, client) = Connection::memory();
        let server_thread = std::thread::spawn(move || serve(&server));

        handshake(&client, &root_uri, true);
        let reg = recv_message(&client);
        assert!(
            matches!(reg, Message::Request(_)),
            "the registration request precedes the watched change, got {reg:?}"
        );

        // doc.md is never opened. Mutate it on disk, then deliver the watcher
        // `changed` event.
        fs::write(root.join("doc.md"), "See `gone.md` here.\n").expect("rewrite doc.md on disk");
        send_watched_change(&client, &doc_uri, lsp::file_change_type::CHANGED);

        let after = recv_publish_for(&client, &doc_uri);
        assert!(
            any_message_contains(&after, "stale reference"),
            "a closed file's on-disk change reaches the graph without a restart, got {after:?}"
        );

        shutdown(&client, server_thread);
    }

    #[test]
    fn watched_create_and_delete_update_cross_file_edges() {
        // a.md makes a bare-path reference to docs/page.md and is never opened.
        // Creating the target on disk and delivering the watcher `created`
        // event flips a.md to the make-it-a-link nudge — a cross-file edge moved
        // by a membership change; deleting it flips a.md back to a stale
        // reference. Both reach the closed source through the graph-aware
        // re-publish.
        let dir = workspace_with_files(&[("a.md", "See docs/page.md for details.\n")]);
        let root = fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        let root_uri = path_to_uri(&root);
        let a_uri = path_to_uri(&root.join("a.md"));
        let page_uri = path_to_uri(&root.join("docs/page.md"));

        let (server, client) = Connection::memory();
        let server_thread = std::thread::spawn(move || serve(&server));

        handshake(&client, &root_uri, true);
        let reg = recv_message(&client);
        assert!(
            matches!(reg, Message::Request(_)),
            "the registration request precedes the membership changes, got {reg:?}"
        );

        // Create the target on disk; deliver the `created` event.
        fs::create_dir_all(root.join("docs")).expect("create docs dir");
        fs::write(root.join("docs/page.md"), "# Page\n").expect("write page.md");
        send_watched_change(&client, &page_uri, lsp::file_change_type::CREATED);
        let created = recv_publish_for(&client, &a_uri);
        assert!(
            any_message_contains(&created, "convert to a markdown link"),
            "creating the target flips the closed source's bare path to make-it-a-link, got {created:?}"
        );

        // Delete the target; deliver the `deleted` event.
        fs::remove_file(root.join("docs/page.md")).expect("delete page.md");
        send_watched_change(&client, &page_uri, lsp::file_change_type::DELETED);
        let deleted = recv_publish_for(&client, &a_uri);
        assert!(
            any_message_contains(&deleted, "stale reference"),
            "deleting the target flips the closed source back to a stale reference, got {deleted:?}"
        );

        shutdown(&client, server_thread);
    }

    #[test]
    fn open_md_buffer_wins_then_close_reenables_watcher() {
        // Decision 017 §3: while a `.md` file is open the synced buffer is
        // authoritative. A watched `changed` event carrying divergent disk bytes
        // is dropped — no diagnostic regresses to the on-disk version, and the
        // open file's single edit is never delivered twice (issue 009 class).
        // After didClose the watcher is re-enabled and the next `changed` event
        // re-reads disk.
        let dir = workspace_with_files(&[("doc.md", "See `disk-only.md` here.\n")]);
        let root = fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        let root_uri = path_to_uri(&root);
        let doc_uri = path_to_uri(&root.join("doc.md"));

        let (server, client) = Connection::memory();
        let server_thread = std::thread::spawn(move || serve(&server));

        handshake(&client, &root_uri, true);
        let reg = recv_message(&client);
        assert!(
            matches!(reg, Message::Request(_)),
            "the registration request precedes the document sync, got {reg:?}"
        );

        // Open with a buffer that references a different (also absent) file than
        // disk. The publish reflects the buffer.
        open_doc(&client, &doc_uri, "See `buffer-only.md` here.\n");
        let opened = recv_publish_for(&client, &doc_uri);
        assert!(
            any_message_contains(&opened, "buffer-only.md"),
            "the open buffer's diagnostic reflects the buffer, not the disk, got {opened:?}"
        );

        // A watched `changed` event for the still-open file is dropped: no
        // publish arrives, so the buffer is not clobbered by stale disk bytes.
        send_watched_change(&client, &doc_uri, lsp::file_change_type::CHANGED);
        assert!(
            client
                .receiver
                .recv_timeout(std::time::Duration::from_millis(300))
                .is_err(),
            "an open file's watched `changed` event is dropped — buffer wins, no double delivery"
        );

        // Close the file, then deliver the same event: now honored, disk wins.
        send_close(&client, &doc_uri);
        send_watched_change(&client, &doc_uri, lsp::file_change_type::CHANGED);
        let after = recv_publish_for(&client, &doc_uri);
        assert!(
            any_message_contains(&after, "disk-only.md"),
            "after didClose the watcher re-reads disk: the diagnostic now reflects disk, got {after:?}"
        );

        shutdown(&client, server_thread);
    }

    #[test]
    fn did_close_reverts_to_disk_content() {
        // didClose reconciles the index to disk: content authority reverts to
        // the filesystem (issue 046). The file is not dropped (it still exists
        // on disk), but its indexed content reverts from the discarded buffer
        // to the on-disk text. Observe via documentSymbol after the close: the
        // disk heading is resolved, not the buffer heading.
        let dir = workspace_with_files(&[("doc.md", "# On Disk\n")]);
        let root = fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        let root_uri = path_to_uri(&root);
        let doc_uri = path_to_uri(&root.join("doc.md"));

        let (server, client) = Connection::memory();
        let server_thread = std::thread::spawn(move || serve(&server));

        // No watcher needed for this test; skip the registration entirely.
        handshake(&client, &root_uri, false);

        // Open with a buffer heading that differs from disk, then close without
        // saving. The clean open re-publishes an explicit empty set (decision
        // 022: didOpen is a client-state boundary) and the equally clean disk
        // content publishes nothing on close; `request_response` below skips the
        // empty publish notification while scanning for its response.
        open_doc(&client, &doc_uri, "# In Buffer\n");
        send_close(&client, &doc_uri);

        let symbols = request_response(
            &client,
            2,
            lsp::method::DOCUMENT_SYMBOL,
            serde_json::json!({ "textDocument": { "uri": doc_uri } }),
        );
        let json = serde_json::to_string(&symbols).expect("serialize document symbols");
        assert!(
            json.contains("On Disk"),
            "after didClose the index reverts to the on-disk content: {json}"
        );
        assert!(
            !json.contains("In Buffer"),
            "the discarded buffer content is no longer indexed after didClose: {json}"
        );

        shutdown(&client, server_thread);
    }

    #[test]
    fn did_close_unsaved_edit_clears_buffer_diagnostic() {
        // Acceptance (issue 046): open a clean file, introduce a broken link in
        // the buffer without saving, then close without saving. The published
        // diagnostics must revert to the on-disk (clean) state — the phantom
        // diagnostic clears on close, since disk never changed.
        let dir = workspace_with_files(&[("doc.md", "# Clean\n")]);
        let root = fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        let root_uri = path_to_uri(&root);
        let doc_uri = path_to_uri(&root.join("doc.md"));

        let (server, client) = Connection::memory();
        let server_thread = std::thread::spawn(move || serve(&server));

        handshake(&client, &root_uri, false);

        // Open matching the clean disk content. `didOpen` is a client-state
        // boundary (decision 022), so it re-publishes an explicit empty set even
        // for a clean file; drain it before the edit. Then edit the buffer to add
        // a stale reference — the didChange publishes it.
        open_doc(&client, &doc_uri, "# Clean\n");
        assert!(
            recv_publish_for(&client, &doc_uri).is_empty(),
            "opening the clean buffer re-publishes an explicit empty set"
        );
        send_change(&client, &doc_uri, "See `gone.md` here.\n");
        let dirty = recv_publish_for(&client, &doc_uri);
        assert!(
            any_message_contains(&dirty, "stale reference"),
            "the unsaved buffer edit surfaces the stale-reference diagnostic, got {dirty:?}"
        );

        // Close without saving: the index reconciles to the clean disk file, so
        // the diagnostic clears (an empty publish).
        send_close(&client, &doc_uri);
        let after = recv_publish_for(&client, &doc_uri);
        assert!(
            after.is_empty(),
            "didClose reverts the index to clean disk content, clearing the phantom diagnostic, got {after:?}"
        );

        shutdown(&client, server_thread);
    }

    #[test]
    fn did_close_keeps_saved_content_indexed() {
        // Acceptance (issue 046): open, edit, and save, then close. The saved
        // content is on disk, so the close re-reads it — the saved content stays
        // indexed with no regression to a pre-save version. Observe via
        // documentSymbol after the close: the saved heading is resolved.
        let dir = workspace_with_files(&[("doc.md", "# Original\n")]);
        let root = fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        let root_uri = path_to_uri(&root);
        let doc_uri = path_to_uri(&root.join("doc.md"));

        let (server, client) = Connection::memory();
        let server_thread = std::thread::spawn(move || serve(&server));

        handshake(&client, &root_uri, false);

        // Open matching disk, edit the heading in the buffer, then save that
        // edit to disk and deliver didSave with the saved text. All three steps
        // keep the file clean, so none publishes a diagnostic (the clean open
        // re-publishes only an explicit empty set, which `request_response`
        // skips below).
        open_doc(&client, &doc_uri, "# Original\n");
        send_change(&client, &doc_uri, "# Edited\n");
        fs::write(root.join("doc.md"), "# Edited\n").expect("save doc.md to disk");
        send_notification(
            &client,
            lsp::method::DID_SAVE,
            serde_json::json!({
                "textDocument": { "uri": doc_uri },
                "text": "# Edited\n"
            }),
        );

        // Close: the re-read of disk holds the saved content, so the index keeps
        // the saved heading — closing did not regress to the pre-save version.
        send_close(&client, &doc_uri);
        let symbols = request_response(
            &client,
            3,
            lsp::method::DOCUMENT_SYMBOL,
            serde_json::json!({ "textDocument": { "uri": doc_uri } }),
        );
        let json = serde_json::to_string(&symbols).expect("serialize document symbols");
        assert!(
            json.contains("Edited"),
            "after save+close the index holds the saved content: {json}"
        );
        assert!(
            !json.contains("Original"),
            "the pre-save heading is gone after save+close: {json}"
        );

        shutdown(&client, server_thread);
    }

    #[test]
    fn did_close_drops_file_deleted_while_open() {
        // Acceptance (issue 046): a file open in the editor whose disk copy is
        // deleted while open is dropped from the index on close — the re-read of
        // disk finds nothing, so a closed source that referenced it flips from
        // the existing-file nudge to a stale reference. The reference resolves
        // against the index, so it stays valid until target.md is dropped.
        let dir = workspace_with_files(&[
            ("a.md", "See `target.md` for details.\n"),
            ("target.md", "# Target\n"),
        ]);
        let root = fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        let root_uri = path_to_uri(&root);
        let a_uri = path_to_uri(&root.join("a.md"));
        let target_uri = path_to_uri(&root.join("target.md"));

        let (server, client) = Connection::memory();
        let server_thread = std::thread::spawn(move || serve(&server));

        handshake(&client, &root_uri, false);

        // Open target.md (it becomes editor-authoritative). The full re-publish
        // surfaces a.md's existing-file nudge, since target.md is in the index.
        open_doc(&client, &target_uri, "# Target\n");
        let before = recv_publish_for(&client, &a_uri);
        assert!(
            any_message_contains(&before, "refers to an existing file"),
            "while target.md is indexed, a.md's reference is the existing-file nudge, got {before:?}"
        );

        // Delete target.md on disk while it is open, then close it. The close
        // must reconcile to disk — the file is gone, so it is dropped from the
        // index and a.md's reference flips to a stale reference.
        fs::remove_file(root.join("target.md")).expect("delete target.md while open");
        send_close(&client, &target_uri);
        let after = recv_publish_for(&client, &a_uri);
        assert!(
            any_message_contains(&after, "stale reference"),
            "closing a file deleted on disk drops it from the index, so a.md's reference goes stale, got {after:?}"
        );

        shutdown(&client, server_thread);
    }

    /// Drain every pending `publishDiagnostics` non-blockingly, returning a map
    /// of URI -> its last-published diagnostics wire JSON (the lsp types are
    /// send-only). Used to inspect a single batched publish pass without a
    /// `serve` thread, so the recompute counter (a thread-local) stays on the
    /// test thread.
    fn drain_publishes(client: &Connection) -> HashMap<String, Vec<serde_json::Value>> {
        let mut published: HashMap<String, Vec<serde_json::Value>> = HashMap::new();
        while let Ok(msg) = client.receiver.try_recv() {
            if let Message::Notification(notif) = msg
                && notif.method == lsp::method::PUBLISH_DIAGNOSTICS
                && let Some(uri) = notif
                    .params
                    .get("uri")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            {
                let diagnostics = notif
                    .params
                    .get("diagnostics")
                    .and_then(serde_json::Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                published.insert(uri, diagnostics);
            }
        }
        published
    }

    #[test]
    fn watched_batch_publishes_in_one_pass() {
        // Ticket perf 07: a single `didChangeWatchedFiles` notification carrying
        // N changed `.md` files must collapse into exactly ONE whole-workspace
        // recompute / publish pass — not N — while still re-materializing every
        // changed file. Three closed files each gain a stale-reference
        // diagnostic on disk; the one batched pass publishes all three.
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            ("a.md", "# Clean A\n"),
            ("b.md", "# Clean B\n"),
            ("c.md", "# Clean C\n"),
        ]);
        let mut workspaces = scan_workspaces(&dir);

        // Seed the published cache with the initial (clean) state: no
        // diagnostics, so the batch below is a genuine gain for each file.
        let initial = diff_diagnostics(&mut workspaces, &HashSet::new());
        assert!(
            initial.is_empty(),
            "clean files publish nothing initially: {initial:?}"
        );

        // Mutate all three files on disk to introduce a stale-reference
        // diagnostic, then deliver one notification carrying all three changes.
        for rel in ["a.md", "b.md", "c.md"] {
            fs::write(dir.path().join(rel), "See `gone.md` here.\n")
                .unwrap_or_else(|e| panic!("rewrite {rel} on disk: {e}"));
        }
        let a_uri = file_uri(&dir, "a.md");
        let b_uri = file_uri(&dir, "b.md");
        let c_uri = file_uri(&dir, "c.md");
        let notif = Notification::new(
            lsp::method::DID_CHANGE_WATCHED_FILES.to_string(),
            serde_json::json!({ "changes": [
                { "uri": a_uri, "type": lsp::file_change_type::CHANGED },
                { "uri": b_uri, "type": lsp::file_change_type::CHANGED },
                { "uri": c_uri, "type": lsp::file_change_type::CHANGED },
            ] }),
        );

        // Drive the handler on the test thread so the recompute counter (a
        // thread-local) observes the pass; a memory connection captures the
        // publishes for inspection.
        let (server, client) = Connection::memory();
        let (passes, result) =
            count_recomputes(|| handle_notification(&server, &mut workspaces, notif));
        result.expect("handle the batched watched-file notification");

        // The whole batch ran in exactly one recompute pass, not one per file.
        assert_eq!(
            passes, 1,
            "a batch of 3 changed files publishes in one pass, not 3"
        );

        // Every changed file re-materialized in that single pass.
        let published = drain_publishes(&client);
        for uri in [&a_uri, &b_uri, &c_uri] {
            let diags = published
                .get(uri)
                .unwrap_or_else(|| panic!("{uri} was published in the batch pass"));
            assert!(
                any_message_contains(diags, "stale reference"),
                "{uri}'s on-disk change re-materialized in the one pass, got {diags:?}"
            );
        }
    }

    #[test]
    fn watched_single_change_is_one_pass() {
        // Ticket perf 07: the N = 1 case is unchanged — a one-file watched
        // change is the same single pass it always was, with the changed file
        // re-materialized.
        let dir = workspace_with_files(&[(".lattice.toml", ""), ("doc.md", "# Clean\n")]);
        let mut workspaces = scan_workspaces(&dir);
        let initial = diff_diagnostics(&mut workspaces, &HashSet::new());
        assert!(
            initial.is_empty(),
            "the clean file publishes nothing initially: {initial:?}"
        );

        fs::write(dir.path().join("doc.md"), "See `gone.md` here.\n")
            .expect("rewrite doc.md on disk");
        let doc_uri = file_uri(&dir, "doc.md");
        let notif = Notification::new(
            lsp::method::DID_CHANGE_WATCHED_FILES.to_string(),
            serde_json::json!({ "changes": [
                { "uri": doc_uri, "type": lsp::file_change_type::CHANGED },
            ] }),
        );

        let (server, client) = Connection::memory();
        let (passes, result) =
            count_recomputes(|| handle_notification(&server, &mut workspaces, notif));
        result.expect("handle the single watched-file notification");
        assert_eq!(passes, 1, "a single changed file is one pass");

        let published = drain_publishes(&client);
        let diags = published
            .get(&doc_uri)
            .expect("doc.md was published in the pass");
        assert!(
            any_message_contains(diags, "stale reference"),
            "the single closed file's on-disk change re-materialized, got {diags:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Config hot-reload hardening (issue 050)
    // -----------------------------------------------------------------------

    #[test]
    fn watched_batch_applies_config_change_before_md_changes() {
        // Issue 050: a debouncing client watcher coalesces a config edit and
        // the document edits around it into ONE notification, in arbitrary
        // order. The marker pass runs first regardless, so every `.md`
        // re-read in the batch parses under the config that was on disk with
        // it. This drives the adversarial order — the md change listed
        // before the marker CREATED event (the mid-session creation case).
        let dir = workspace_with_files(&[("doc.md", "# Clean\n")]);
        let mut workspaces = scan_workspaces(&dir);
        let initial = diff_diagnostics(&mut workspaces, &HashSet::new());
        assert!(
            initial.is_empty(),
            "the clean file publishes nothing initially: {initial:?}"
        );

        fs::write(
            dir.path().join(".lattice.toml"),
            "[graph]\nartifacts = [\"artifact.md\"]\n",
        )
        .expect("create the marker mid-session");
        fs::write(
            dir.path().join("doc.md"),
            "See `artifact.md` and `dead.md` here.\n",
        )
        .expect("rewrite doc.md on disk");
        let doc_uri = file_uri(&dir, "doc.md");
        let marker_uri = file_uri(&dir, ".lattice.toml");
        let notif = Notification::new(
            lsp::method::DID_CHANGE_WATCHED_FILES.to_string(),
            serde_json::json!({ "changes": [
                { "uri": doc_uri, "type": lsp::file_change_type::CHANGED },
                { "uri": marker_uri, "type": lsp::file_change_type::CREATED },
            ] }),
        );

        let (server, client) = Connection::memory();
        handle_notification(&server, &mut workspaces, notif).expect("handle the mixed batch");

        let published = drain_publishes(&client);
        let diags = published
            .get(&doc_uri)
            .expect("doc.md was published in the batch pass");
        assert!(
            any_message_contains(diags, "dead.md"),
            "the md change itself was applied — `dead.md` is a stale reference: {diags:?}"
        );
        assert!(
            !any_message_contains(diags, "artifact.md"),
            "the md change parsed under the batch's own freshly created config — \
             `artifact.md` is glossary-suppressed: {diags:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn marker_event_under_canonical_spelling_still_reloads() {
        // Issue 050: a config event dropped on a URI-spelling mismatch leaves
        // the workspace silently on stale config. The folder is opened
        // through a symlink (the map key), while the watcher reports the
        // mid-session marker creation under the canonical root — the reload
        // must still find the workspace via its canonical scan root.
        let real = workspace_with_files(&[("doc.md", "See `artifact.md` here.\n")]);
        let canonical_root = fs::canonicalize(real.path()).expect("canonicalize real root");
        let link_parent = tempfile::tempdir().expect("create symlink parent dir");
        let link = link_parent.path().join("ws");
        std::os::unix::fs::symlink(real.path(), &link).expect("create workspace symlink");
        let link_uri = path_to_uri(&link);
        let doc_uri = path_to_uri(&link.join("doc.md"));

        let (server, client) = Connection::memory();
        let server_thread = std::thread::spawn(move || serve(&server));

        handshake(&client, &link_uri, true);
        let reg = recv_message(&client);
        assert!(
            matches!(reg, Message::Request(_)),
            "the registration request precedes the marker event, got {reg:?}"
        );

        open_doc(&client, &doc_uri, "See `artifact.md` here.\n");
        let before = recv_publish_for(&client, &doc_uri);
        assert!(
            any_message_contains(&before, "stale reference"),
            "doc.md starts with the stale-reference diagnostic, got {before:?}"
        );

        // Create the marker mid-session and deliver its watcher event under
        // the canonical spelling, not the symlinked folder's.
        fs::write(
            canonical_root.join(".lattice.toml"),
            "[graph]\nartifacts = [\"artifact.md\"]\n",
        )
        .expect("create the marker mid-session");
        let marker_uri = path_to_uri(&canonical_root.join(".lattice.toml"));
        send_watched_change(&client, &marker_uri, lsp::file_change_type::CREATED);

        let after = recv_publish_for(&client, &doc_uri);
        assert!(
            after.is_empty(),
            "the canonically-spelled marker event reloads the symlink-keyed workspace: {after:?}"
        );

        shutdown(&client, server_thread);
    }

    #[test]
    fn overlapping_folders_do_not_alternate_publishes() {
        // Issue 050's flip-flop shape: two workspace folders, one nested in
        // the other, index the same absolute file. The shallow folder cannot
        // see the deep folder's `.lattice.toml` (config discovery walks up,
        // not down), so the two workspaces disagree about the file's
        // diagnostics — and with both feeding the URI-keyed publish cache,
        // successive passes alternated between the config-aware set and the
        // config-less one. The deepest workspace must own the URI, matching
        // how `resolve` routes the file's document events.
        let dir = workspace_with_files(&[
            (
                "sub/.lattice.toml",
                "[graph]\nartifacts = [\"artifact.md\"]\n",
            ),
            ("sub/doc.md", "See `artifact.md` and `dead.md` here.\n"),
        ]);
        let root = fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        let sub = root.join("sub");
        // Add both folders: the shallow root first, then the nested one. The
        // shared `sub/doc.md` is scanned once by each, but the flat store keeps
        // one document whose deepest (primary) root is `sub`.
        let mut workspaces = Workspaces::new();
        workspaces.add_folder(&path_to_uri(&root));
        workspaces.add_folder(&path_to_uri(&sub));
        assert!(
            !workspaces.roots[&root].has_config && workspaces.roots[&sub].has_config,
            "test setup: only the deep folder sees the marker"
        );
        let doc_uri = path_to_uri(&root.join("sub/doc.md"));

        // First pass: exactly one publish for the shared URI, computed by the
        // deep (config-aware) workspace — `artifact.md` glossary-suppressed,
        // `dead.md` stale.
        let first = diff_diagnostics(&mut workspaces, &HashSet::new());
        assert_eq!(
            first.len(),
            1,
            "the shared file publishes exactly once: {first:?}"
        );
        assert_eq!(first[0].0, doc_uri, "the publish is for the shared URI");
        let messages: Vec<&str> = first[0].1.iter().map(|d| d.message.as_str()).collect();
        assert!(
            messages.iter().any(|m| m.contains("dead.md"))
                && !messages.iter().any(|m| m.contains("artifact.md")),
            "the deep workspace's config-aware set wins: {messages:?}"
        );

        // Follow-up passes with nothing changed must publish nothing — under
        // the shared-cache bug, each pass flipped the URI to whichever
        // workspace's set the cache did not currently hold.
        let forced = diff_diagnostics(&mut workspaces, &one_uri(&doc_uri));
        assert!(
            forced.is_empty(),
            "a forced re-materialization of the unchanged file publishes nothing: {forced:?}"
        );
        let idle = diff_diagnostics(&mut workspaces, &HashSet::new());
        assert!(
            idle.is_empty(),
            "an idle pass publishes nothing — no alternation: {idle:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Editor move surface — workspace/willRenameFiles (ticket mv/02,
    // decision 020 clause 2)
    // -----------------------------------------------------------------------

    /// Build the `InitializeParams` a client sends, optionally advertising the
    /// `workspace.fileOperations.willRename` capability.
    fn init_params_with_will_rename(will_rename: bool) -> lsp::InitializeParams {
        serde_json::from_value(serde_json::json!({
            "capabilities": {
                "workspace": {
                    "fileOperations": { "willRename": will_rename }
                }
            },
            "workspaceFolders": []
        }))
        .expect("deserialize InitializeParams")
    }

    /// Send a `workspace/willRenameFiles` request for one `old -> new` rename and
    /// return the raw response (result or error) for `id`.
    fn will_rename_request(
        client: &Connection,
        id: i32,
        old_uri: &str,
        new_uri: &str,
    ) -> lsp_server::Response {
        client
            .sender
            .send(Message::Request(Request::new(
                RequestId::from(id),
                lsp::method::WILL_RENAME_FILES.to_string(),
                serde_json::json!({
                    "files": [ { "oldUri": old_uri, "newUri": new_uri } ]
                }),
            )))
            .expect("send willRenameFiles");
        for _ in 0..32 {
            if let Message::Response(resp) = recv_message(client)
                && resp.id == RequestId::from(id)
            {
                return resp;
            }
        }
        panic!("no response for willRenameFiles request {id}");
    }

    /// The `WorkspaceEdit.changes` map keyed by URI, as `Vec<lsp::TextEdit>`,
    /// extracted from a successful willRename response.
    fn changes_of(resp: &lsp_server::Response) -> HashMap<String, Vec<lsp::TextEdit>> {
        let result = resp
            .result
            .as_ref()
            .expect("a successful willRename carries a WorkspaceEdit result");
        let changes = result
            .get("changes")
            .expect("the WorkspaceEdit has a changes map");
        serde_json::from_value(changes.clone()).expect("deserialize the changes map")
    }

    #[test]
    fn will_rename_capability_gated_on_client() {
        // Advertised only to a client that sends the request; absent otherwise —
        // a client without it moves files blind, exactly as before (decision 020
        // clause 2, the graceful-degradation half of the acceptance).
        let with = server_capabilities(&init_params_with_will_rename(true));
        let file_ops = with.get("workspace").and_then(|w| w.get("fileOperations"));
        assert!(
            file_ops.is_some(),
            "a willRename-capable client is offered the fileOperations surface: {with}"
        );
        let filters = file_ops
            .and_then(|f| f.get("willRename"))
            .and_then(|w| w.get("filters"))
            .and_then(serde_json::Value::as_array)
            .expect("willRename carries registration filters");
        let filters_json = serde_json::to_string(filters).expect("serialize filters");
        assert!(
            filters_json.contains("file") && filters_json.contains("folder"),
            "the filters scope the request to markdown files and folders: {filters_json}"
        );

        let without = server_capabilities(&init_params_with_will_rename(false));
        assert!(
            without
                .get("workspace")
                .and_then(|w| w.get("fileOperations"))
                .is_none(),
            "a client without the capability sees no fileOperations block: {without}"
        );
    }

    #[test]
    fn will_rename_updates_referrer_and_moved_file() {
        // A clean two-file graph — a forward link and its reciprocal backlink.
        // Moving the target to a deeper directory must retarget the referrer's
        // forward link AND re-relativize the moved file's own backlink entry (its
        // depth to hub.md changed), in one merged WorkspaceEdit (decision 020
        // clause 4).
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            (
                "hub.md",
                "# Hub\n\n[to target](docs/target.md \"references\")\n",
            ),
            (
                "docs/target.md",
                "---\nbacklinks:\n  referenced_by:\n    - ../hub.md\n---\n# Target\n",
            ),
        ]);
        let workspaces = scan_workspaces(&dir);
        let old_uri = file_uri(&dir, "docs/target.md");
        let new_uri = file_uri(&dir, "archive/deep/target.md");

        let edit = will_rename_files(
            &workspaces,
            &lsp::RenameFilesParams {
                files: vec![lsp::FileRename { old_uri, new_uri }],
            },
        );
        let edit = edit.expect("a valid in-scope move computes an edit set");
        let changes = edit.changes.expect("the workspace edit carries changes");

        let hub_uri = file_uri(&dir, "hub.md");
        let target_uri = file_uri(&dir, "docs/target.md");
        let hub_edits = changes
            .get(&hub_uri)
            .expect("the referrer's forward link is edited");
        assert_eq!(
            hub_edits.len(),
            1,
            "exactly the one forward link is retargeted: {hub_edits:?}"
        );
        assert_eq!(
            hub_edits[0].new_text, "archive/deep/target.md",
            "the referrer's link points at the new location"
        );
        let target_edits = changes
            .get(&target_uri)
            .expect("the moved file's own backlink entry is re-relativized");
        assert_eq!(
            target_edits.len(),
            1,
            "exactly the one backlink entry is re-rendered: {target_edits:?}"
        );
        assert_eq!(
            target_edits[0].new_text, "../../hub.md",
            "the moved file's backlink re-relativizes to hub.md from its deeper location"
        );
    }

    #[test]
    fn will_rename_cross_marker_is_refused() {
        // A move whose destination lands inside a nested scope crosses a marker
        // boundary: it is an extraction, not a rename (decision 020 clause 6).
        // The engine refuses with the alias-steering message, and the handler
        // computes no edits — so the client aborts and the file does not move.
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            ("a.md", "# A\n"),
            ("nested/.lattice.toml", ""),
            ("nested/keep.md", "# Keep\n"),
        ]);
        let workspaces = scan_workspaces(&dir);
        let old_uri = file_uri(&dir, "a.md");
        let new_uri = file_uri(&dir, "nested/a.md");

        let result = will_rename_files(
            &workspaces,
            &lsp::RenameFilesParams {
                files: vec![lsp::FileRename { old_uri, new_uri }],
            },
        );
        let message = result.expect_err("a cross-marker move is refused");
        assert!(
            message.contains("external"),
            "the refusal steers to the `[external]` alias: {message}"
        );
    }

    #[test]
    fn wire_will_rename_cross_marker_answers_error_and_file_stays() {
        // Over the wire (decision 020 clause 2 + clause 6): a cross-marker move
        // is answered with a JSON-RPC error carrying the alias-steering message,
        // not a WorkspaceEdit — so the client aborts and the file does not move.
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            ("a.md", "# A\n"),
            ("nested/.lattice.toml", ""),
            ("nested/keep.md", "# Keep\n"),
        ]);
        let root = fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        let root_uri = path_to_uri(&root);
        let old_uri = path_to_uri(&root.join("a.md"));
        let new_uri = path_to_uri(&root.join("nested/a.md"));

        let (server, client) = Connection::memory();
        let server_thread = std::thread::spawn(move || serve(&server));
        handshake(&client, &root_uri, false);

        let resp = will_rename_request(&client, 77, &old_uri, &new_uri);
        assert!(
            resp.result.is_none(),
            "a refused move carries no WorkspaceEdit result: {resp:?}"
        );
        let error = resp.error.expect("a refused move answers with an error");
        assert_eq!(
            error.code,
            lsp_server::ErrorCode::RequestFailed as i32,
            "the refusal is a RequestFailed error, got code {}",
            error.code
        );
        assert!(
            error.message.contains("external"),
            "the error names the fix (the `[external]` alias): {}",
            error.message
        );
        // The server computed no edit and left the source in place; the client,
        // seeing the error, aborts — the file is still at its old path.
        assert!(
            root.join("a.md").is_file() && !root.join("nested/a.md").exists(),
            "the refused move left the file at its old path"
        );

        shutdown(&client, server_thread);
    }

    /// Splice an `lsp::TextEdit` list into `source` back-to-front (so earlier
    /// byte offsets stay valid), converting each LSP range to a byte range.
    fn apply_text_edits(source: &str, edits: &[lsp::TextEdit]) -> String {
        let mut byte_edits: Vec<(usize, usize, &str)> = edits
            .iter()
            .map(|e| {
                let start = lsp_position_to_byte_offset(source, e.range.start);
                let end = lsp_position_to_byte_offset(source, e.range.end);
                (start, end, e.new_text.as_str())
            })
            .collect();
        byte_edits.sort_by_key(|(start, _, _)| *start);
        let mut text = source.to_string();
        for (start, end, new_text) in byte_edits.iter().rev() {
            text.replace_range(*start..*end, new_text);
        }
        text
    }

    #[test]
    fn will_rename_isomorphism_end_to_end() {
        // The governing property observed over the LSP surface: apply the edit
        // set the willRename handler returns, perform the rename, and the graph
        // is unchanged up to the coordinate move. Clean stays clean; the moved
        // file's own drift (a broken forward link) transports verbatim.
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            (
                "hub.md",
                "# Hub\n\n[to target](docs/target.md \"references\")\n",
            ),
            (
                "docs/target.md",
                // A reciprocal backlink (clean edge) plus a deliberately broken
                // forward link that must survive the move at new coordinates.
                "---\nbacklinks:\n  referenced_by:\n    - ../hub.md\n---\n# Target\n\n[gone](ghost.md \"references\")\n",
            ),
        ]);
        let root = dir.path().to_path_buf();

        // Pre-move: exactly the one broken-link error on target.md.
        let pre = crate::validation::collect_all(&Workspace::scan(&root).expect("scan pre"));
        assert_eq!(
            pre.len(),
            1,
            "one pre-move drift (the broken link): {pre:#?}"
        );

        let workspaces = scan_workspaces(&dir);
        let old_uri = file_uri(&dir, "docs/target.md");
        let new_uri = file_uri(&dir, "archive/target.md");
        let edit = will_rename_files(
            &workspaces,
            &lsp::RenameFilesParams {
                files: vec![lsp::FileRename { old_uri, new_uri }],
            },
        )
        .expect("the move computes an edit set");
        let changes = edit.changes.expect("changes present");

        // Apply every returned text edit to disk (the client applies to buffers;
        // on save/rename the bytes land on disk identically).
        for (uri, text_edits) in &changes {
            let path = uri_to_path(uri);
            let source = fs::read_to_string(&path).expect("read edited file");
            let edited = apply_text_edits(&source, text_edits);
            fs::write(&path, edited).expect("write edited file");
        }
        // Perform the rename on disk (the client's job after applying edits).
        fs::create_dir_all(root.join("archive")).expect("create archive dir");
        fs::rename(root.join("docs/target.md"), root.join("archive/target.md"))
            .expect("perform the rename");

        // Post-move: the same single drift, now anchored at the renamed file.
        let post = crate::validation::collect_all(&Workspace::scan(&root).expect("scan post"));
        assert_eq!(
            post.len(),
            1,
            "the diagnostic count is preserved by the coordinate move: {post:#?}"
        );
        assert_eq!(
            post[0].file,
            PathBuf::from("archive/target.md"),
            "the surviving drift transported to the renamed coordinate: {post:#?}"
        );
        // The clean forward-link/backlink edge stayed clean: the only diagnostic
        // is the pre-existing broken link, not a freshly-broken referrer.
        assert!(
            post[0].message.contains("ghost.md"),
            "the transported drift is the same broken link, not a new break: {post:#?}"
        );
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one linear wire-level lifecycle: init, open, willRename, apply, rename, didRename, assert"
    )]
    fn did_rename_files_rekeys_without_rescan_and_republishes() {
        // Wire-level end-to-end (decision 020 clause 2): after the client applies
        // the edits and renames, `workspace/didRenameFiles` re-keys the store
        // without a rescan. The moved file's own drift (a broken forward link)
        // transports to its NEW URI, its OLD URI is cleared, and the referrer's
        // retargeted link resolves again.
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            (
                "hub.md",
                "# Hub\n\n[to target](docs/target.md \"references\")\n",
            ),
            (
                "docs/target.md",
                "---\nbacklinks:\n  referenced_by:\n    - ../hub.md\n---\n# Target\n\n[gone](ghost.md \"references\")\n",
            ),
        ]);
        let root = fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        let root_uri = path_to_uri(&root);
        let hub_uri = path_to_uri(&root.join("hub.md"));
        let old_uri = path_to_uri(&root.join("docs/target.md"));
        let new_uri = path_to_uri(&root.join("archive/target.md"));

        let (server, client) = Connection::memory();
        let server_thread = std::thread::spawn(move || serve(&server));

        // Advertise both dynamic registration and willRename so the full lifecycle
        // is exercised.
        let init = Request::new(
            RequestId::from(1),
            "initialize".to_string(),
            serde_json::json!({
                "capabilities": {
                    "workspace": {
                        "didChangeWatchedFiles": { "dynamicRegistration": true },
                        "fileOperations": { "willRename": true }
                    }
                },
                "workspaceFolders": [ { "uri": root_uri } ]
            }),
        );
        client
            .sender
            .send(Message::Request(init))
            .expect("send initialize");
        // The initialize response advertises the willRename surface.
        let init_resp = loop {
            if let Message::Response(resp) = recv_message(&client) {
                break resp;
            }
        };
        let caps = init_resp
            .result
            .expect("initialize result")
            .get("capabilities")
            .expect("capabilities present")
            .clone();
        assert!(
            caps.get("workspace")
                .and_then(|w| w.get("fileOperations"))
                .is_some(),
            "the initialize result advertises the willRename surface: {caps}"
        );
        send_notification(&client, "initialized", serde_json::json!({}));
        let _reg = recv_message(&client); // client/registerCapability

        // Open both documents; the clean graph publishes empty sets.
        open_doc(
            &client,
            &hub_uri,
            "# Hub\n\n[to target](docs/target.md \"references\")\n",
        );
        assert!(
            recv_publish_for(&client, &hub_uri).is_empty(),
            "hub.md is clean before the move"
        );
        open_doc(
            &client,
            &old_uri,
            "---\nbacklinks:\n  referenced_by:\n    - ../hub.md\n---\n# Target\n\n[gone](ghost.md \"references\")\n",
        );
        assert!(
            any_message_contains(&recv_publish_for(&client, &old_uri), "ghost.md"),
            "target.md carries its broken-link drift before the move"
        );

        // Ask for the move; apply the returned edits to the open buffers via
        // didChange (buffer-wins), and to disk, then rename on disk.
        let resp = will_rename_request(&client, 42, &old_uri, &new_uri);
        let changes = changes_of(&resp);
        for (uri, text_edits) in &changes {
            let path = uri_to_path(uri);
            let source = fs::read_to_string(&path).expect("read edited file");
            let edited = apply_text_edits(&source, text_edits);
            // The client applies the edit to its live buffer first.
            send_change(&client, uri, &edited);
            fs::write(&path, edited).expect("write edited file");
        }
        // Drain any republishes the buffer edits triggered.
        for uri in changes.keys() {
            let _ = recv_publish_for(&client, uri);
        }
        fs::create_dir_all(root.join("archive")).expect("create archive dir");
        fs::rename(root.join("docs/target.md"), root.join("archive/target.md"))
            .expect("perform the rename");

        // Confirm the rename: didRenameFiles re-keys the store without a rescan.
        send_notification(
            &client,
            lsp::method::DID_RENAME_FILES,
            serde_json::json!({
                "files": [ { "oldUri": old_uri, "newUri": new_uri } ]
            }),
        );

        // The old URI is cleared, and the moved file's drift transports to its
        // new URI — the isomorphism preserved end-to-end over the LSP surface.
        let cleared = recv_publish_for(&client, &old_uri);
        assert!(
            cleared.is_empty(),
            "the vanished old URI is cleared: {cleared:?}"
        );
        let moved = recv_publish_for(&client, &new_uri);
        assert!(
            any_message_contains(&moved, "ghost.md"),
            "the renamed file's broken-link drift transported to its new coordinate: {moved:?}"
        );

        shutdown(&client, server_thread);
    }

    #[test]
    fn no_will_rename_capability_degrades_to_nothing() {
        // A client that does not advertise willRename receives no fileOperations
        // block and the server behaves exactly as before — an editor rename is
        // performed blind. The server still serves everything else.
        let dir = workspace_with_files(&[
            (".lattice.toml", ""),
            ("doc.md", "# Doc\n\n[x](gone.md \"references\")\n"),
        ]);
        let root = fs::canonicalize(dir.path()).expect("canonicalize temp dir");
        let root_uri = path_to_uri(&root);
        let doc_uri = path_to_uri(&root.join("doc.md"));

        let (server, client) = Connection::memory();
        let server_thread = std::thread::spawn(move || serve(&server));

        // Handshake WITHOUT the willRename capability, capturing the result.
        let init = Request::new(
            RequestId::from(1),
            "initialize".to_string(),
            serde_json::json!({
                "capabilities": { "workspace": {} },
                "workspaceFolders": [ { "uri": root_uri } ]
            }),
        );
        client
            .sender
            .send(Message::Request(init))
            .expect("send initialize");
        let init_resp = loop {
            if let Message::Response(resp) = recv_message(&client) {
                break resp;
            }
        };
        let caps = init_resp
            .result
            .expect("initialize result")
            .get("capabilities")
            .expect("capabilities present")
            .clone();
        assert!(
            caps.get("workspace")
                .and_then(|w| w.get("fileOperations"))
                .is_none(),
            "no fileOperations surface is advertised without the client capability: {caps}"
        );
        send_notification(&client, "initialized", serde_json::json!({}));

        // The server still publishes diagnostics as before.
        open_doc(&client, &doc_uri, "# Doc\n\n[x](gone.md \"references\")\n");
        let publish = recv_publish_for(&client, &doc_uri);
        assert!(
            !publish.is_empty(),
            "the server still serves diagnostics without the move surface: {publish:?}"
        );

        shutdown(&client, server_thread);
    }
}
