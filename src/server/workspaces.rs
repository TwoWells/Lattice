// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! The server's document state: the two-tier document store, the scope/root
//! registry derived from markers and client folders, and the workspace views
//! every LSP surface reads through.
//!
//! This is decision 019's scope model (roots derive from markers, not folders)
//! and decision 024's two-tier store (saved copy plus per-buffer overlay) in one
//! place: [`Workspaces`] owns both and is the only thing that mutates them. The
//! surfaces in the sibling modules take `&Workspaces` and resolve a view; the
//! notification handlers take `&mut Workspaces` and drive its writers.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::config::{Config, ConfigError};
use crate::lsp;
use crate::overrides::OverrideVerdicts;
use crate::store::{DiskUpdate, Document, DocumentStore, TIERS, Tier};
use crate::uri::{path_to_uri, uri_to_path};
use crate::validation::Diagnostic;
use crate::workspace::{
    Boundary, BoundaryKind, FileData, Workspace, WorkspaceView, compute_structural,
    discover_scope_boundaries, find_scope_root, parse_content,
};

// Counts `recompute_all_structural` sweeps — the O(workspace) structural-cache
// pass a membership change forces — so tests can assert which store mutations
// pay it (a rootless open must not). Compiled out of release builds.
#[cfg(test)]
thread_local! {
    pub static STRUCTURAL_SWEEP_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

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
pub struct PublishedDiagnostics {
    /// The Lattice diagnostics whose materialization was last published.
    pub lattice: Vec<Diagnostic>,
    /// The materialized LSP diagnostics last sent to the client.
    pub lsp: Vec<lsp::Diagnostic>,
}

/// Root-level state for one workspace folder. Carries no file list — membership
/// is a range scan over [`Workspaces::documents`], never a secondary index.
pub struct RootMeta {
    /// Canonical scan root ([`Workspace::scan`]'s discovered root). May differ
    /// from the map key when the client opened the folder through a symlink
    /// (issue 047): the key is the client-supplied spelling documents resolve
    /// on, this is the canonical form the force-re-materialize comparison and
    /// config reload run against.
    pub canonical_root: PathBuf,
    /// Configuration loaded from the root.
    pub config: Config,
    /// Error from loading `.lattice.toml`, if any — published on the config's
    /// URI at commitment points (decision 023 clause 4).
    pub config_error: Option<ConfigError>,
    /// Whether `config` is a genuine commitment: a successfully loaded
    /// `.lattice.toml`, or genuine absence (defaults are the semantics of a
    /// repo that declared nothing). `false` only when the config was broken
    /// at scope registration with no last-good to hold (decision 023
    /// addendum, issue 065): the `Config::default()` in `config` is then a
    /// fabrication nobody wrote, so the root publishes nothing computed under
    /// it — only the load error, on the config URI — until the next valid
    /// commitment. A *reload* failure never clears this: the previous valid
    /// config is held and keeps governing.
    pub config_committed: bool,
    /// Whether a `.lattice.toml` was found at the root.
    pub has_config: bool,
    /// The `[[override]]` expect-aggregate verdicts held from the last
    /// commitment point — `didOpen`, `didSave`, or a watched-files batch
    /// (decision 023, issue 064). Every published set is filtered through this
    /// held verdict at the materialization seam; `didChange` recomputes live
    /// diagnostics but never re-adjudicates, so counts move mid-edit while the
    /// suppression decision does not. Starts empty (nothing suppressed) and is
    /// populated by the first commitment publish.
    pub verdicts: OverrideVerdicts,
}

/// The server's document state: the two-tier [`DocumentStore`] (decision 024)
/// plus root metadata. Root membership is derived by range scan over the store
/// — which is component-wise (via `Path::starts_with`), so a `Lattice` folder
/// never captures a `LatticeInternal` sibling the way a string-keyed range
/// would (ticket server 10).
pub struct Workspaces {
    /// The saved store (disk truth) and the overlay store (open buffers), each
    /// with one writer channel enforced by the [`crate::store`] module
    /// boundary (decision 024 clause 1, issue 067).
    pub store: DocumentStore,
    /// Every active **scope root**, keyed by the (client-spelling) directory a
    /// marker declares — a `.lattice.toml` scope or a `.git` non-root
    /// environment (decision 019). Roots derive from markers, not folders: an
    /// added folder registers the nearest ancestor marker covering it and every
    /// strictly-deeper marker beneath it (walk up, then walk down). A document's
    /// `primary_root` is its deepest covering scope root, and each root's graph
    /// is the range scan filtered to `primary_root == root` — so a nested scope
    /// is a disjoint graph, never swallowed by its host.
    pub roots: BTreeMap<PathBuf, RootMeta>,
    /// The folders the client actually opened (their client-spelling paths).
    ///
    /// Client folders declare *visibility*; markers declare *structure*
    /// (decision 019 clause 7). A folder is an entry point into whatever scope
    /// covers it, not a root of its own. The active [`Self::roots`] are derived
    /// from this set (each folder's covering marker plus its nested markers), so
    /// removing a folder deregisters exactly the scopes no remaining folder
    /// keeps visible.
    pub client_folders: BTreeSet<PathBuf>,
    /// Nested `.git` non-root environments (a submodule or vendored repo without
    /// a `.lattice.toml`), by their client-spelling directory (decision 019
    /// resolution 2). These are *not* scopes: excluded from every host scope's
    /// scan and membership, and never eagerly indexed (Lattice does not read a
    /// foreign repo's graph). A document behind one is rootless — opened directly
    /// it serves document-scoped features and structural under defaults (051
    /// semantics) — and a link resolving into one crosses a boundary. Derived
    /// from the open folders alongside [`Self::roots`].
    pub git_boundaries: BTreeSet<PathBuf>,
    /// Diagnostics last published to the client, keyed by document URI.
    ///
    /// Used to suppress redundant `publishDiagnostics` notifications and to
    /// detect which files an edit moved: a file is only re-published when its
    /// materialized vector changes, and only re-materialized when its Lattice
    /// vector changes (issue 013 — publication diffing, then ticket perf 02's
    /// materialization cache). Only non-empty entries are stored, so an absent
    /// entry means the client currently holds no diagnostics for that URI.
    /// Besides the indexed markdown set, the cache admits one per-root
    /// `.lattice.toml` URI — the config channel (decision 023 clause 4) —
    /// cleared like any absent file on config delete or root deregistration,
    /// and force-invalidated on every marker event (the config is unsynced,
    /// so no `didOpen` boundary ever resets its record).
    pub published: HashMap<String, PublishedDiagnostics>,
    /// Absolute paths of documents the client currently holds a buffer for —
    /// live between `textDocument/didOpen` and `textDocument/didClose`.
    ///
    /// Under decision 024 this is **client state, not arbitration**: it records
    /// which documents the editor owns a buffer for, and it decides exactly two
    /// things. (1) Clause 1's materialize-before-write: a disk write landing on
    /// an open document with no overlay entry must first move the pre-update
    /// saved parse into the overlay, because the client still holds that text.
    /// (2) Whether an uncovered document survives a workspace-folder removal.
    /// No watched event is ever dropped on account of it — the saved store's
    /// writers are unconditional — and the dirty set it used to pair with is
    /// retired outright (017 §3 superseded).
    ///
    /// # Keying (issue 069)
    ///
    /// Keyed by the **decoded path** [`uri_to_path`] yields, exactly like the
    /// document store — never by the raw URI string. Two components may spell
    /// one file differently (one percent-encodes a space, the other does not),
    /// so a URI-keyed set is spelling-dependent and misses silently. Deciding
    /// identity once, at the boundary, removes the dependence structurally. It
    /// is also the premise decision 024's buffer locality rests on — one buffer
    /// per document — which URI-spelling aliasing would break at the level of
    /// identity.
    ///
    /// **Out of scope:** symlink and case-insensitivity aliasing. Two paths
    /// that differ by a symlink hop, or only in case on a case-insensitive
    /// filesystem, are still two keys here. Collapsing them means
    /// `fs::canonicalize`, a semantic change — it resolves symlinks (which
    /// issue 047 deliberately keeps distinct from the client's folder spelling)
    /// and fails on a path not yet on disk (a `didOpen` for a new file) — so it
    /// is deferred to its own decision. This keying fixes **spelling** aliasing
    /// (percent-encoding variants) only.
    pub open_documents: HashSet<PathBuf>,
    /// A borrowable default configuration for rootless single-file views
    /// (issue 051): a document outside every root parses and serves its
    /// document-scoped features under defaults, with the graph tier inert.
    pub default_config: Config,
}

impl Workspaces {
    /// An empty store with no roots and no documents.
    pub fn new() -> Self {
        Self {
            store: DocumentStore::new(),
            roots: BTreeMap::new(),
            client_folders: BTreeSet::new(),
            git_boundaries: BTreeSet::new(),
            published: HashMap::new(),
            open_documents: HashSet::new(),
            default_config: Config::default(),
        }
    }

    /// Create from the initial set of workspace folders.
    pub fn from_params(params: &lsp::InitializeParams) -> Self {
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
    /// document-scoped under defaults (051 semantics).
    ///
    /// The gate is what a boundary *means*, not a blanket veto (issue 052): a
    /// boundary excludes a subtree from its **host's** graph; it never vetoes a
    /// scope registered at or inside the boundary itself. So it fires only when
    /// the deepest covering registered root is strictly **above** a covering
    /// boundary — `deepest` and every covering `g` are ancestors of `abs`, hence
    /// comparable, so `!deepest.starts_with(g)` is exactly "`g` lies strictly
    /// below `deepest`". A submodule opened directly as its own client folder
    /// therefore keeps the fallback scope its direct entry granted it, while the
    /// host's documents still see it as foreign — the entry-point independence
    /// decision 019 claims.
    pub fn deepest_root_for(&self, abs: &Path) -> Option<PathBuf> {
        let deepest = self
            .roots
            .keys()
            .filter(|root| abs.starts_with(root))
            .max_by_key(|root| root.components().count())?;
        let gated = self
            .git_boundaries
            .iter()
            .any(|g| abs.starts_with(g) && !deepest.starts_with(g));
        (!gated).then(|| deepest.clone())
    }

    /// The absolute paths of every document under `root` (either tier), by
    /// range scan.
    pub fn document_keys_under(&self, root: &Path) -> Vec<PathBuf> {
        self.store.keys_under(root)
    }

    /// The configuration a document with the given primary root parses under:
    /// the root's config, or the rootless default (issue 051).
    pub fn config_for(&self, primary: Option<&Path>) -> &Config {
        Self::config_of(&self.roots, &self.default_config, primary)
    }

    /// [`Self::config_for`] over the two fields it actually reads, so a caller
    /// that also holds `&mut self.store` can borrow them disjointly.
    pub fn config_of<'a>(
        roots: &'a BTreeMap<PathBuf, RootMeta>,
        default_config: &'a Config,
        primary: Option<&Path>,
    ) -> &'a Config {
        primary
            .and_then(|root| roots.get(root))
            .map_or(default_config, |meta| &meta.config)
    }

    /// The strictly-deeper scope boundaries nested inside `root`: every
    /// registered scope root `root` is a proper ancestor of, plus every nested
    /// `.git` non-root environment beneath it (decision 019), each tagged with
    /// its kind. A link resolving into one of these has crossed a boundary
    /// ([`WorkspaceLike::crosses_boundary`]), and the tag is what lets the
    /// diagnostic say *which kind* of foreign territory it landed in.
    ///
    /// Both chains are strictly deeper: `root` is never a boundary of its own
    /// view. That matters for a directly-opened submodule, which is a registered
    /// root **and** a `.git` boundary — without the exclusion its own view would
    /// declare every one of its documents out of scope. A directory in both
    /// chains is emitted once, as [`BoundaryKind::Git`]: membership in
    /// `git_boundaries` is exactly "carries `.git`, carries no marker"
    /// (`collect_git_boundaries` recurses through marker scopes rather than
    /// recording them), so the git tag is the true one.
    pub fn boundaries_under(&self, root: &Path) -> Vec<Boundary> {
        self.roots
            .keys()
            .filter(|other| {
                other.as_path() != root
                    && other.starts_with(root)
                    && !self.git_boundaries.contains(other.as_path())
            })
            .map(|other| Boundary {
                path: other.clone(),
                kind: BoundaryKind::Scope,
            })
            .chain(
                self.git_boundaries
                    .iter()
                    .filter(|g| g.as_path() != root && g.starts_with(root))
                    .map(|g| Boundary {
                        path: g.clone(),
                        kind: BoundaryKind::Git,
                    }),
            )
            .collect()
    }

    /// Wrap a per-root file map in the view the shared pipeline consumes.
    ///
    /// Membership is always the range scan tightened to `primary_root == root`
    /// (decision 019, ticket server 10's anticipated filter): a document under a
    /// strictly-deeper boundary belongs to that nested scope's graph, not this
    /// one, so the two scopes are disjoint — the host never sees the nested
    /// scope's files, and vice versa. Which *copy* of each document the map
    /// holds is the caller's declaration (decision 024 clause 9: diagnostics
    /// read perspective, edits and reads read current).
    pub fn view_over<'a>(
        &'a self,
        root: &Path,
        files: BTreeMap<PathBuf, &'a FileData>,
    ) -> WorkspaceView<'a> {
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

    /// The **saved world** for one root: every document at its last-committed
    /// content. This is what every document other than the perspective's focus
    /// is judged against, and what the `[[override]]` aggregate adjudicates over
    /// — the conformance surface `lattice lint` shares (decision 024 clause 8).
    pub fn saved_view(&self, root: &Path) -> WorkspaceView<'_> {
        self.view_over(root, self.store.saved_files(root))
    }

    /// The **perspective** one document's own rows are computed under: the
    /// saved world with `focus`'s buffer swapped in (decision 024's headline).
    ///
    /// A no-op for a document with no overlay entry, which is what makes the
    /// merge free for every undiverged document.
    pub fn perspective_view(&self, root: &Path, focus: &Path) -> WorkspaceView<'_> {
        self.view_over(root, self.store.perspective_files(root, focus))
    }

    /// Every document of one root at its **current** text — buffers where they
    /// exist, saved copies elsewhere.
    ///
    /// The read and edit surfaces' view (decision 024 clause 9). An edit
    /// computed against saved coordinates and applied to a diverged buffer
    /// lands in the wrong place, and a hover or semantic token must describe
    /// the text on screen.
    pub fn current_view(&self, root: &Path) -> WorkspaceView<'_> {
        self.view_over(root, self.store.current_files(root))
    }

    /// Build a single-file view over one rootless document (issue 051): its
    /// parent directory is the view root and its file name the sole key, exactly
    /// as the old single-file `Workspace` was shaped, so document-scoped
    /// features resolve identically without a workspace.
    pub fn single_file_view<'a>(&'a self, abs: &Path, doc: &'a Document) -> WorkspaceView<'a> {
        let root = match (abs.parent(), abs.file_name()) {
            (Some(parent), Some(_)) => parent.to_path_buf(),
            _ => PathBuf::new(),
        };
        let mut files = BTreeMap::new();
        files.insert(document_rel(abs, None), &doc.data);
        WorkspaceView::new(root, &self.default_config, false, files, Vec::new())
    }

    // --- Document resolution ---

    /// Resolve a URI to the view and relative path for its **diagnostic** tier:
    /// its own perspective under the deepest covering root, or `None` for a
    /// rootless or unindexed document (which publishes nothing — issue 051).
    pub fn resolve(&self, uri: &str) -> Option<(WorkspaceView<'_>, PathBuf)> {
        let abs = uri_to_path(uri);
        let root = self.store.current(&abs)?.primary_root.clone()?;
        let rel = abs.strip_prefix(&root).ok()?.to_path_buf();
        Some((self.perspective_view(&root, &abs), rel))
    }

    /// Resolve a URI to the view and relative path that serve its
    /// **document-scoped** features (semantic tokens, folding, symbols, hover,
    /// formatting, document links, completion, navigation, …) and its edit
    /// surfaces.
    ///
    /// Both read **current** text (decision 024 clause 9): the client applies a
    /// `WorkspaceEdit` to the buffers it holds, and a position-bearing answer
    /// must be anchored in the text on screen.
    ///
    /// A single direct path lookup: a rooted document resolves against its
    /// root's current view; a rootless document (issue 051) against a
    /// single-file view.
    pub fn resolve_document(&self, uri: &str) -> Option<(WorkspaceView<'_>, PathBuf)> {
        let abs = uri_to_path(uri);
        let doc = self.store.current(&abs)?;
        match doc.primary_root.as_ref() {
            Some(root) => {
                let rel = abs.strip_prefix(root).ok()?.to_path_buf();
                Some((self.current_view(root), rel))
            }
            None => Some((self.single_file_view(&abs, doc), document_rel(&abs, None))),
        }
    }

    // --- Overlay-store writers: didOpen / didChange / didClose -------------

    /// Seed the buffer copy for a `didOpen` (decision 024 clause 3).
    ///
    /// A `didOpen` is a **claim, not a source**: it asserts "here is what I am
    /// holding", never "here is what is on disk", so it never writes the saved
    /// store. This is what kills issue 067's read-then-`didOpen` race by
    /// construction — a client that read a file before an external edit and
    /// opened it afterwards makes a stale claim that can mislead exactly one
    /// document's rows.
    ///
    /// Divergence is decided against the **saved copy**, which is exactly the
    /// question the sharing rule asks ("can this document share the saved
    /// parse?"). A document with no saved copy — gitignored, outside every
    /// root, or not yet written to disk — always materializes.
    pub fn open_buffer(&mut self, uri: &str, content: &str) {
        let abs = uri_to_path(uri);
        if self.store.source(&abs, Tier::Saved) == Some(content) {
            // Undiverged: store nothing and share the saved parse. One parse,
            // not two, so a client holding hundreds of unmodified documents
            // open does not double the store.
            self.store.open_buffer(&abs, None);
            return;
        }
        let doc = self.parse_buffer(&abs, content);
        self.store.open_buffer(&abs, Some(doc));
        self.recompute_structural(&abs);
    }

    /// Materialize (or replace) the buffer copy for a `didChange`.
    ///
    /// Membership never moves: a `didChange` targets an already-open document
    /// and the saved store is untouched, so no other document's bare-path
    /// existence answer can flip. Only this document's own cache is owed.
    pub fn change_buffer(&mut self, uri: &str, content: &str) {
        let abs = uri_to_path(uri);
        let doc = self.parse_buffer(&abs, content);
        self.store.change_buffer(&abs, doc);
        self.recompute_structural(&abs);
    }

    /// Drop the buffer copy at `didClose`. The saved store never held buffer
    /// content, so there is nothing to revert (decision 024 clause 4).
    pub fn close_buffer(&mut self, abs: &Path) {
        self.store.close_buffer(abs);
    }

    /// Parse buffer `content` for `abs` under its placement's config.
    pub fn parse_buffer(&self, abs: &Path, content: &str) -> Document {
        let primary = self.deepest_root_for(abs);
        // Links classify against the absolute path (root-free), so the config
        // affects only the frontmatter predicate check.
        let config = self.config_for(primary.as_deref());
        Document {
            data: parse_content(content, abs, config),
            primary_root: primary,
        }
    }

    // --- Saved-store writers: didSave / watched files ----------------------

    /// Commit a `didSave` carrying `includeText` — the one seam between the
    /// tiers (decision 024 clause 1). The saved copy becomes the buffer's
    /// content and the overlay entry is dropped.
    pub fn commit_save(&mut self, uri: &str, content: &str) {
        let abs = uri_to_path(uri);
        let primary = self.deepest_root_for(&abs);
        let data = {
            let config = Self::config_of(&self.roots, &self.default_config, primary.as_deref());
            parse_content(content, &abs, config)
        };
        match self.store.commit_save(&abs, primary, data) {
            DiskUpdate::Membership => self.recompute_all_structural(),
            DiskUpdate::Content | DiskUpdate::Untouched => self.recompute_structural(&abs),
        }
    }

    /// The absent-`includeText` `didSave` fallback: drop the overlay and
    /// re-read disk into the saved store.
    pub fn commit_save_from_disk(&mut self, abs: &Path) {
        let primary = self.deepest_root_for(abs);
        let update = {
            let config = Self::config_of(&self.roots, &self.default_config, primary.as_deref());
            self.store
                .commit_save_from_disk(abs, primary.clone(), &|content| {
                    parse_content(content, abs, config)
                })
        };
        self.settle(abs, update);
    }

    /// Reconcile the saved copy to disk and settle the structural debt in one
    /// step. Every production caller splits the two — the watched-files batch
    /// folds N debts into one sweep (issue 063), and `didClose` inspects the
    /// [`DiskUpdate`] to decide whether it caught a watcher miss — so this
    /// survives as the tests' one-shot convenience.
    #[cfg(test)]
    pub fn update_from_disk(&mut self, abs: &Path) {
        let update = self.apply_from_disk(abs);
        self.settle(abs, update);
    }

    /// Pay the structural debt one saved-store write left.
    pub fn settle(&mut self, abs: &Path, update: DiskUpdate) {
        match update {
            DiskUpdate::Membership => self.recompute_all_structural(),
            DiskUpdate::Content => self.recompute_structural(abs),
            DiskUpdate::Untouched => {}
        }
    }

    /// Reconcile the saved copy to disk without recomputing any structural
    /// cache, returning the debt the caller owes ([`DiskUpdate`]).
    ///
    /// **Unconditional** (decision 024, issue 067): no buffer-wins drop, no
    /// dirty check. The saved store never held buffer content, so a watched
    /// event has nothing to clobber — it only ever refreshes disk truth, and
    /// an open document's own rows keep reading its overlay. Where the document
    /// is open and undiverged, the store first materializes the pre-update
    /// saved parse into the overlay (clause 1), so the client's text is never
    /// silently replaced by content it never sent.
    ///
    /// Folding the recompute out of the per-file apply is what keeps a bulk
    /// `didChangeWatchedFiles` batch `O(batch + workspace)` instead of
    /// `O(batch × workspace)` (issue 063).
    pub fn apply_from_disk(&mut self, abs: &Path) -> DiskUpdate {
        let primary = self.deepest_root_for(abs);
        let open = self.open_documents.contains(abs);
        let config = Self::config_of(&self.roots, &self.default_config, primary.as_deref());
        self.store
            .apply_from_disk(abs, open, primary.clone(), &|content| {
                parse_content(content, abs, config)
            })
    }

    /// Drop a rootless single-file document (issue 051) — used on `didClose`,
    /// when the editor discards a buffer that has no disk-backed root to revert
    /// to. A no-op for a rooted or unindexed URI.
    pub fn remove_single_file(&mut self, uri: &str) {
        let abs = uri_to_path(uri);
        if self
            .store
            .current(&abs)
            .is_some_and(|doc| doc.primary_root.is_none())
        {
            self.store.close_buffer(&abs);
            self.store.evict_saved(&abs);
        }
    }

    /// Re-key the document store for one `oldUri -> newUri` rename the client
    /// has just performed (`workspace/didRenameFiles` — decision 020 clause 2),
    /// **without a rescan**.
    ///
    /// The move engine's text edits were already applied to buffers by the
    /// client before it renamed on disk, so the content at the new key is
    /// correct in **both** tiers — re-keying just moves the parsed entries
    /// from the old absolute path to the new one and re-derives each under its
    /// (possibly changed) primary root, rather than re-reading a whole scope.
    /// A file rename moves the single entry; a directory rename moves every
    /// document under the old prefix. The `open_documents` set and the per-URI
    /// `published` cache are re-keyed alongside so client state and publication
    /// diffing follow the file.
    ///
    /// Re-deriving (not a bare key swap) is required because a document parses
    /// relative to its root, and the link classification / structural existence
    /// checks read the new coordinate; each tier's text is preserved verbatim.
    ///
    /// Returns the old URIs that held a published diagnostic set, so the caller
    /// can send each an explicit empty publish — the re-publish diff iterates
    /// the *current* store and never revisits a vanished key.
    pub fn rekey_rename(&mut self, old_abs: &Path, new_abs: &Path) -> Vec<String> {
        let mut cleared = Vec::new();
        for (old_key, new_key) in self.store.rekey(old_abs, new_abs) {
            // Follow the client-state and publication-diff records to the new
            // key so an open renamed file stays open and its stale publication
            // under the old URI is cleared.
            if self.open_documents.remove(&old_key) {
                self.open_documents.insert(new_key.clone());
            }
            let old_uri = path_to_uri(&old_key);
            if self.published.remove(&old_uri).is_some() {
                cleared.push(old_uri);
            }
            // Re-derive each tier's parse under the destination's primary root
            // — placement and coordinate change, content does not.
            let primary = self.deepest_root_for(&new_key);
            self.store.set_primary_root(&new_key, primary.as_deref());
            for tier in TIERS {
                let Some(source) = self.store.source(&new_key, tier).map(str::to_string) else {
                    continue;
                };
                let data = {
                    let config =
                        Self::config_of(&self.roots, &self.default_config, primary.as_deref());
                    parse_content(&source, &new_key, config)
                };
                self.store.reparse_in_place(&new_key, tier, data);
            }
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
    /// re-root within one config cannot change them, and each tier's
    /// `FileData` — the buffer copy included — is preserved untouched without
    /// touching disk or the parser. One parse-time derivation, however, *is*
    /// config-sensitive: `FileData::backlink_diagnostics`, the frontmatter
    /// unknown-predicate check, reads the predicate vocabulary. A re-root that
    /// crosses a scope boundary (a live split/merge, or a folder add/remove
    /// over a marker scope) changes the effective config, so when the predicate
    /// vocabulary actually differs this re-derives every tier's parse. The
    /// root-dependent structural cache is refreshed by the caller's
    /// `recompute_all_structural` afterward.
    pub fn refresh_placement(&mut self, abs: &Path) {
        let new_primary = self.deepest_root_for(abs);
        let Some(old_primary) = self.store.current(abs).map(|doc| doc.primary_root.clone()) else {
            return;
        };
        if old_primary == new_primary {
            return;
        }
        // Does the re-root change the predicate vocabulary the parse-time
        // backlink check reads? Only then must each tier be re-derived.
        let reparse = self.config_for(old_primary.as_deref()).predicates
            != self.config_for(new_primary.as_deref()).predicates;
        self.store.set_primary_root(abs, new_primary.as_deref());
        if reparse {
            self.reparse_in_place(abs);
        }
    }

    /// Re-derive every stored copy of a document from its **own** source under
    /// its current primary root's config (placement unchanged) — used by a
    /// config reload (ticket server 08), which changes the config every owned
    /// document parses under while preserving membership and open buffers.
    ///
    /// The re-derivation survives ticket server 11's placement/reparse split
    /// because the config still feeds one *parse-time* derivation: the
    /// frontmatter backlink-predicate check (`FileData::backlink_diagnostics`),
    /// which flags an unknown predicate against the config vocabulary and
    /// records its line. Link classification, by contrast, is config- and
    /// root-free, so a mere placement change routes through `refresh_placement`
    /// instead (no reparse).
    pub fn reparse_in_place(&mut self, abs: &Path) {
        let primary = self.store.primary_root(abs);
        for tier in TIERS {
            let Some(source) = self.store.source(abs, tier).map(str::to_string) else {
                continue;
            };
            let data = {
                let config = Self::config_of(&self.roots, &self.default_config, primary.as_deref());
                parse_content(&source, abs, config)
            };
            self.store.reparse_in_place(abs, tier, data);
        }
    }

    // --- Structural cache maintenance ---

    /// Recompute a document's cached structural diagnostics in **every** tier it
    /// has a copy in, against its primary root's membership and config.
    ///
    /// Each tier reads the membership its own consumer is judged against: the
    /// saved copy sees the saved world alone, so its rows are exactly what
    /// `lattice lint` computes on the same disk state (the conformance
    /// invariant); the overlay copy sees the same saved world **plus itself**,
    /// because a document is always a member of its own perspective — that is
    /// what makes a buffer-only file lint itself while staying invisible to
    /// every other document until the first save (decision 024's notes).
    ///
    /// A rootless document is left empty (issue 051 — single-file documents
    /// carry no workspace-tier verdicts).
    pub fn recompute_structural(&mut self, abs: &Path) {
        for tier in TIERS {
            let computed = {
                let Some(doc) = self.store.tier(abs, tier) else {
                    continue;
                };
                let Some(root) = doc.primary_root.clone() else {
                    self.store.set_caches(
                        abs,
                        tier,
                        Vec::new(),
                        crate::structural::FileSuppressions::default(),
                    );
                    continue;
                };
                let rel = abs.strip_prefix(&root).unwrap_or(abs).to_path_buf();
                let config = Self::config_of(&self.roots, &self.default_config, Some(&root));
                // Membership under the primary root, by range scan through the
                // saved store: a bare-path target `t` exists iff `root/t` is a
                // saved document *of this scope* — `primary_root == root`
                // (decision 019). A document under a strictly-deeper boundary
                // lives in a nested scope, so it is not a member here.
                let file_exists = |target: &Path| {
                    let key = root.join(target);
                    self.store.is_saved_member(&key, &root)
                        || (tier == Tier::Overlay && key.as_path() == abs)
                };
                compute_structural(&doc.data, &rel, config, &file_exists)
            };
            self.store.set_caches(abs, tier, computed.0, computed.1);
        }
    }

    /// Recompute the structural cache for every document. Required on a
    /// membership change: adding or removing one file can flip a bare-path
    /// existence answer in any document that shares a root.
    pub fn recompute_all_structural(&mut self) {
        // Count full sweeps so tests can pin which store mutations pay the
        // O(workspace) cost (a rootless open must not). Compiled out of
        // release builds.
        #[cfg(test)]
        STRUCTURAL_SWEEP_COUNT.with(|count| count.set(count.get() + 1));

        for abs in self.store.all_keys() {
            self.recompute_structural(&abs);
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
    /// root. Both tiers re-root together, so an open document keeps the buffer
    /// it is holding across the change and no orphaned entry remains.
    pub fn add_folder(&mut self, uri: &str) {
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
    pub fn register_scope(&mut self, scope_root: &Path) {
        if self.roots.contains_key(scope_root) {
            return;
        }
        // The holding scan (decision 023 addendum): a broken config refuses
        // the one-shot CLI, but the server registers the scope anyway —
        // config-independent features serve, and the load error is published
        // on the config URI instead of gating the whole root.
        let Ok(ws) = Workspace::scan_recording_config_error(scope_root) else {
            return;
        };
        let parts = ws.into_parts();
        self.roots.insert(
            scope_root.to_path_buf(),
            RootMeta {
                canonical_root: parts.root,
                config: parts.config,
                // With no last-good to hold, a config broken at registration
                // leaves `config` a fabricated default: nothing computed
                // under it may publish (issue 065).
                config_committed: parts.config_error.is_none(),
                config_error: parts.config_error,
                has_config: parts.has_config,
                verdicts: OverrideVerdicts::default(),
            },
        );
        for (rel, data) in parts.files {
            let key = scope_root.join(&rel);
            self.store.seed_scan(
                key,
                Document {
                    data,
                    primary_root: Some(scope_root.to_path_buf()),
                },
            );
        }
        // Recurse only into nested `.lattice.toml` scopes — those are graphs. A
        // nested `.git`-only environment is not a scope (decision 019 resolution
        // 2): it is left unscanned and tracked as a boundary by
        // `rebuild_git_boundaries`, so a foreign repo is never indexed.
        for nested in discover_scope_boundaries(scope_root) {
            if nested.kind == BoundaryKind::Scope {
                self.register_scope(&nested.path);
            }
        }
    }

    /// Recompute the nested `.git` non-root environments visible through the open
    /// client folders (decision 019 resolution 2), walking each folder's scope
    /// tree without parsing. Rebuilt whenever the folder set or scope structure
    /// changes, so a `.git` boundary no folder keeps visible drops out.
    pub fn rebuild_git_boundaries(&mut self) {
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
    pub fn collect_git_boundaries(scope_root: &Path, out: &mut BTreeSet<PathBuf>) {
        for boundary in discover_scope_boundaries(scope_root) {
            match boundary.kind {
                BoundaryKind::Scope => Self::collect_git_boundaries(&boundary.path, out),
                BoundaryKind::Git => {
                    out.insert(boundary.path);
                }
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
    /// re-rooted onto the covering scope with no dark window (its buffer rides
    /// along), reparsing across a config boundary via `refresh_placement`.
    pub fn remove_folder(&mut self, uri: &str) {
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
            .store
            .current_documents()
            .into_iter()
            .filter(|(_, doc)| {
                doc.primary_root
                    .as_ref()
                    .is_some_and(|root| stale.contains(root))
            })
            .map(|(abs, _)| abs.to_path_buf())
            .collect();
        for key in affected {
            self.reroot_or_evict(&key);
        }
        self.recompute_all_structural();
    }

    /// Re-root a document whose covering scope just changed, or evict its saved
    /// copy when nothing covers it any more and the client holds no buffer for
    /// it. An open document always survives — rootless, or re-rooted onto the
    /// covering scope — so there is no dark window.
    pub fn reroot_or_evict(&mut self, key: &Path) {
        let uncovered = self.deepest_root_for(key).is_none();
        if uncovered && !self.open_documents.contains(key) {
            self.store.evict_saved(key);
        } else {
            self.refresh_placement(key);
        }
    }

    /// The scope roots visible through the currently-open client folders: each
    /// folder's covering marker plus every strictly-deeper marker beneath it
    /// (decision 019 clause 7). Recomputed on a folder removal to deregister
    /// scopes no surviving folder keeps visible.
    pub fn active_scope_roots(&self) -> BTreeSet<PathBuf> {
        let mut active = BTreeSet::new();
        for folder in &self.client_folders {
            let covering = find_scope_root(folder).unwrap_or_else(|| folder.clone());
            Self::collect_scope_tree(&covering, &mut active);
        }
        active
    }

    /// Add `scope_root` and every strictly-deeper marker scope beneath it to
    /// `out`, walking client-spelling directories on disk.
    ///
    /// Discriminates exactly as `register_scope` and `collect_git_boundaries`
    /// do: only a `.lattice.toml`-bearing boundary is an active nested scope
    /// root. A `.git`-only boundary is a non-root environment (decision 019
    /// resolution 2) — never registered by the walk-down, so reporting it
    /// "active" would only keep a *directly-opened* submodule's root alive after
    /// its own folder closed, and the boundary gate could never be restored
    /// (issue 052). The seed is unconditional: a directly-opened submodule is
    /// legitimately its own active root, entering as the seed of its own
    /// folder's walk rather than as a nested boundary of the host's.
    pub fn collect_scope_tree(scope_root: &Path, out: &mut BTreeSet<PathBuf>) {
        if !out.insert(scope_root.to_path_buf()) {
            return;
        }
        for nested in discover_scope_boundaries(scope_root) {
            if nested.kind == BoundaryKind::Scope {
                Self::collect_scope_tree(&nested.path, out);
            }
        }
    }

    // --- Live split / merge (decision 019 clause 6) ---

    /// The client-key of the scope root registered at directory `dir`, matched
    /// by its own key or its canonical scan path — so a marker reported under a
    /// symlinked spelling (issue 047) still resolves to the workspace it belongs
    /// to (issue 050). Unlike a prefix match, this is exact: it names the scope
    /// *at* `dir`, not a scope that merely contains it.
    pub fn registered_root_at(&self, dir: &Path) -> Option<PathBuf> {
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
    pub fn handle_marker_event(&mut self, marker_uri: &str) -> bool {
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
    /// Open buffers are preserved (both tiers re-root together); only the re-rooted documents
    /// reparse, and then only across a config boundary that changes the predicate
    /// vocabulary — every other document is untouched. The host's now-crossing
    /// plain links resurface as steering errors, and its mentions into the split
    /// subtree as stale references, both computed by the next publish's collect
    /// (no reparse of the host's documents).
    pub fn split_scope(&mut self, marker_dir: &Path) {
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
    pub fn merge_scope(&mut self, scope_root: &Path) {
        if self.roots.remove(scope_root).is_none() {
            return;
        }
        self.rebuild_git_boundaries();
        for key in self.document_keys_under(scope_root) {
            self.reroot_or_evict(&key);
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
    pub fn reload_root_config(&mut self, root: &Path) {
        let Some(meta) = self.roots.get_mut(root) else {
            return;
        };
        let canonical = meta.canonical_root.clone();
        meta.has_config = canonical.join(".lattice.toml").is_file();
        match Config::load(&canonical) {
            Ok(config) => {
                meta.config = config;
                meta.config_error = None;
                meta.config_committed = true;
            }
            Err(e) => {
                // A failed commitment changes nothing (decision 023 addendum,
                // issue 065): the previous valid config keeps governing
                // adjudication — or, with no last-good, the root stays
                // serving config-independent features only. The error is
                // recorded for the config channel to publish; the held
                // config did not change, so no reparse is owed.
                tracing::warn!(root = %canonical.display(), "config reload error, holding last-good: {e}");
                meta.config_error = Some(e);
                return;
            }
        }

        let owned: Vec<PathBuf> = self
            .store
            .current_documents()
            .into_iter()
            .filter(|(_, doc)| doc.primary_root.as_deref() == Some(root))
            .map(|(abs, _)| abs.to_path_buf())
            .collect();
        for abs in &owned {
            self.reparse_in_place(abs);
        }
        self.recompute_all_structural();
    }
}

/// The path a document parses relative to: its path under `primary` for a
/// rooted document, or its file name (matching the old single-file `Workspace`)
/// when rootless.
pub fn document_rel(abs: &Path, primary: Option<&Path>) -> PathBuf {
    primary.map_or_else(
        || match (abs.parent(), abs.file_name()) {
            (Some(_), Some(name)) => PathBuf::from(name),
            _ => abs.to_path_buf(),
        },
        |root| abs.strip_prefix(root).unwrap_or(abs).to_path_buf(),
    )
}
