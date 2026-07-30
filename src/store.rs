// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! The two-tier document store (decision 024, issue 067).
//!
//! Two stores, one writer channel each, and the module boundary is what makes
//! that a property rather than a convention: both maps are private to this
//! module, so nothing outside it can write either one except through a method
//! named for the channel that owns it.
//!
//! - The **saved store** holds every indexed document — disk truth. Its
//!   writers are the initial scan ([`DocumentStore::seed_scan`]), watched-file
//!   events and the `didClose` audit ([`DocumentStore::apply_from_disk`]), and
//!   `didSave` ([`DocumentStore::commit_save`] /
//!   [`DocumentStore::commit_save_from_disk`]). Every one of them is
//!   **unconditional**: there is no buffer-wins drop and no dirty set, because
//!   there is no longer a collision to arbitrate.
//! - The **overlay store** holds buffer copies for *open* documents only. Its
//!   writers are `didOpen` ([`DocumentStore::open_buffer`]) and `didChange`
//!   ([`DocumentStore::change_buffer`]); `didClose`
//!   ([`DocumentStore::close_buffer`]) drops the entry.
//!
//! `didSave` is the one seam between them: it commits the buffer's content into
//! the saved store and drops the overlay entry. That is the only path by which
//! buffer content ever becomes saved content.
//!
//! Both maps are keyed by **canonical path** (issue 069), which is what makes
//! "one buffer per document" architectural rather than assumed: the overlay is
//! a map, so two URI spellings of one file cannot become two buffers.
//!
//! # Sharing, and the one case it does not cover
//!
//! An open document whose buffer matches its saved copy stores **no** overlay
//! entry and shares the saved parse — one parse, not two, so a client holding
//! hundreds of unmodified documents open does not double the store. The single
//! case sharing does not cover is a disk write landing on such a document: the
//! saved copy must move (unconditionally), but the client still holds the old
//! text, so the pre-update saved parse is **moved into the overlay before the
//! write** ([`DocumentStore::materialize_before_write`]). An absent overlay
//! entry may never silently re-point at content the client never sent.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::structural::FileSuppressions;
use crate::validation::Diagnostic;
use crate::workspace::FileData;

/// A single parsed document: its [`FileData`] plus the deepest scope root that
/// covers it (ticket server 10).
///
/// The same shape serves both tiers. In the saved store it is disk truth; in
/// the overlay it is the client's buffer. `primary_root` is kept in step across
/// both copies by [`DocumentStore::set_primary_root`].
pub struct Document {
    /// Parsed data, always parsed relative to `primary_root` (or the file name
    /// when rootless), so its link classification matches how the owning root
    /// resolves it.
    pub data: FileData,
    /// The deepest workspace root whose path covers this document, or `None`
    /// when the document lies outside every folder — a rootless single-file
    /// document (issue 051). `None` documents stay diagnostic-quiet: they are
    /// absent from every root's range scan, so the publish tier never sees
    /// them, while their document-scoped features still resolve by direct path
    /// lookup.
    pub primary_root: Option<PathBuf>,
}

/// Which tier a maintenance operation targets.
///
/// Maintenance — the structural-cache refresh, a placement flip, a reparse
/// under a changed config — re-derives state from content that is already
/// stored. It is deliberately *not* a channel write: it never moves content
/// between tiers and never introduces content from outside, so exposing it
/// per-tier does not weaken the single-writer discipline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// The saved store — disk truth.
    Saved,
    /// The overlay store — open documents' buffers.
    Overlay,
}

/// Every tier, in the order maintenance passes walk them.
pub const TIERS: [Tier; 2] = [Tier::Saved, Tier::Overlay];

/// The structural-cache debt one saved-store write leaves its caller.
///
/// [`DocumentStore::apply_from_disk`] mutates the store without paying any
/// structural recompute, so the watched-files batch handler can fold N debts
/// into one sweep (issue 063).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskUpdate {
    /// The saved store did not change: the path is neither on disk nor
    /// indexed, or the on-disk bytes already match the saved content.
    Untouched,
    /// An existing document's saved content was replaced in place. Only its
    /// own structural cache is owed — no membership changed.
    Content,
    /// A document joined or left the saved store. Any sibling's bare-path
    /// existence answer may have flipped, so a full-workspace sweep is owed.
    Membership,
}

/// The server's two-tier document store: disk truth and open buffers, keyed by
/// canonical path.
///
/// Root membership is derived by range scan —
/// `saved.range(root..).take_while(|(p, _)| p.starts_with(root))` — which is
/// component-wise (via `Path::starts_with`), so a `Lattice` folder never
/// captures a `LatticeInternal` sibling the way a string-keyed range would
/// (ticket server 10).
pub struct DocumentStore {
    /// Every indexed document, at its last-committed (disk) content.
    saved: BTreeMap<PathBuf, Document>,
    /// Buffer copies for open documents that diverge from their saved copy.
    /// An open document whose buffer matches the saved copy is absent here and
    /// shares the saved parse.
    overlay: BTreeMap<PathBuf, Document>,
}

impl DocumentStore {
    /// An empty store.
    pub const fn new() -> Self {
        Self {
            saved: BTreeMap::new(),
            overlay: BTreeMap::new(),
        }
    }

    // --- Reads -------------------------------------------------------------

    /// The document's **current** copy: its buffer if it has one, else its
    /// saved copy. This is what every read surface and every edit surface
    /// reads (decision 024 clause 9) — the client applies edits to the buffers
    /// it holds, and a hover or a semantic token must describe the text on
    /// screen.
    pub fn current(&self, abs: &Path) -> Option<&Document> {
        self.overlay.get(abs).or_else(|| self.saved.get(abs))
    }

    /// One tier's copy of `abs`.
    pub fn tier(&self, abs: &Path, tier: Tier) -> Option<&Document> {
        match tier {
            Tier::Saved => self.saved.get(abs),
            Tier::Overlay => self.overlay.get(abs),
        }
    }

    /// Whether the client holds a diverged buffer for `abs`.
    pub fn has_overlay(&self, abs: &Path) -> bool {
        self.overlay.contains_key(abs)
    }

    /// How many distinct documents the store holds across both tiers. A
    /// document with both a saved and a buffer copy counts once — the two are
    /// copies of one document, not two documents.
    ///
    /// The one-buffer-per-document premise decision 024 rests on is exactly
    /// this count staying at one per file however a document arrived, so this
    /// exists to be asserted (issue 069's URI-spelling aliasing is the way it
    /// would break).
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.saved.len()
            + self
                .overlay
                .keys()
                .filter(|abs| !self.saved.contains_key(abs.as_path()))
                .count()
    }

    /// The deepest root covering `abs`, as recorded on whichever copy exists.
    pub fn primary_root(&self, abs: &Path) -> Option<PathBuf> {
        self.current(abs).and_then(|doc| doc.primary_root.clone())
    }

    /// Every document in the store, at its current text, keyed by absolute
    /// path and ordered by it — the read surface for workspace-wide features
    /// (workspace symbols, find-references, call hierarchy).
    pub fn current_documents(&self) -> Vec<(&Path, &Document)> {
        let mut keys: Vec<&Path> = self
            .saved
            .keys()
            .chain(self.overlay.keys())
            .map(PathBuf::as_path)
            .collect();
        keys.sort_unstable();
        keys.dedup();
        keys.into_iter()
            .filter_map(|abs| self.current(abs).map(|doc| (abs, doc)))
            .collect()
    }

    /// Every key in the store (both tiers), ordered.
    pub fn all_keys(&self) -> Vec<PathBuf> {
        let mut keys: Vec<PathBuf> = self
            .saved
            .keys()
            .chain(self.overlay.keys())
            .cloned()
            .collect();
        keys.sort_unstable();
        keys.dedup();
        keys
    }

    /// Every key at or under `root`, in either tier.
    pub fn keys_under(&self, root: &Path) -> Vec<PathBuf> {
        let mut keys: Vec<PathBuf> = self
            .saved
            .range(root.to_path_buf()..)
            .take_while(|(abs, _)| abs.starts_with(root))
            .chain(
                self.overlay
                    .range(root.to_path_buf()..)
                    .take_while(|(abs, _)| abs.starts_with(root)),
            )
            .map(|(abs, _)| abs.clone())
            .collect();
        keys.sort_unstable();
        keys.dedup();
        keys
    }

    /// Every saved document whose deepest covering root is `root`, keyed by
    /// path relative to it — the membership the whole workspace is judged
    /// against (decision 024's saved world).
    pub fn saved_files(&self, root: &Path) -> BTreeMap<PathBuf, &FileData> {
        self.saved
            .range(root.to_path_buf()..)
            .take_while(|(abs, _)| abs.starts_with(root))
            .filter(|(_, doc)| doc.primary_root.as_deref() == Some(root))
            .filter_map(|(abs, doc)| {
                abs.strip_prefix(root)
                    .ok()
                    .map(|rel| (rel.to_path_buf(), &doc.data))
            })
            .collect()
    }

    /// The saved world with `focus`'s buffer swapped in — the **perspective**
    /// a document's own rows are computed under (decision 024 clause 8).
    ///
    /// A `focus` with no overlay entry yields exactly [`Self::saved_files`], so
    /// the merge is a no-op for every undiverged document — which is what makes
    /// the sharing rule pay off twice. A `focus` that exists *only* as a buffer
    /// (opened on a path absent from disk) is inserted: it is a member of its
    /// own perspective, and of no other document's.
    pub fn perspective_files(&self, root: &Path, focus: &Path) -> BTreeMap<PathBuf, &FileData> {
        let mut files = self.saved_files(root);
        if let Some(doc) = self.overlay.get(focus)
            && doc.primary_root.as_deref() == Some(root)
            && let Ok(rel) = focus.strip_prefix(root)
        {
            files.insert(rel.to_path_buf(), &doc.data);
        }
        files
    }

    /// Every document of `root` at its **current** text — buffers where they
    /// exist, saved copies elsewhere.
    ///
    /// This is the view edit surfaces compute spans against (decision 024
    /// clause 9) and the one read surfaces resolve through. It is deliberately
    /// *not* what diagnostics read: a `WorkspaceEdit` is consumed synchronously
    /// by the client that owns those buffers, whereas a diagnostic persists.
    pub fn current_files(&self, root: &Path) -> BTreeMap<PathBuf, &FileData> {
        let mut files = self.saved_files(root);
        for (abs, doc) in self
            .overlay
            .range(root.to_path_buf()..)
            .take_while(|(abs, _)| abs.starts_with(root))
        {
            if doc.primary_root.as_deref() == Some(root)
                && let Ok(rel) = abs.strip_prefix(root)
            {
                files.insert(rel.to_path_buf(), &doc.data);
            }
        }
        files
    }

    /// The absolute paths of the overlay entries owned by `root` — the
    /// documents whose rows the publish pass must compute under their own
    /// perspective rather than from the saved world.
    pub fn overlay_keys_of_root(&self, root: &Path) -> Vec<PathBuf> {
        self.overlay
            .range(root.to_path_buf()..)
            .take_while(|(abs, _)| abs.starts_with(root))
            .filter(|(_, doc)| doc.primary_root.as_deref() == Some(root))
            .map(|(abs, _)| abs.clone())
            .collect()
    }

    /// Whether `abs` is a **saved** member of `root`'s graph — the membership
    /// oracle the bare-path existence check reads.
    ///
    /// Saved-world membership, deliberately: a document that exists only as an
    /// unsaved buffer satisfies no other document's reference until the first
    /// save (decision 024's notes).
    pub fn is_saved_member(&self, abs: &Path, root: &Path) -> bool {
        self.saved
            .get(abs)
            .is_some_and(|doc| doc.primary_root.as_deref() == Some(root))
    }

    /// One tier's stored source text.
    pub fn source(&self, abs: &Path, tier: Tier) -> Option<&str> {
        self.tier(abs, tier).map(|doc| doc.data.tree.source())
    }

    // --- Saved-store writers: scan / disk / didSave ------------------------

    /// Fold one scanned document into the saved store, **upsert-if-absent**
    /// (the initial scan and every scope registration).
    ///
    /// An occupied entry — a document a sibling scope already holds — keeps its
    /// content and the fresh parse is dropped; the provisional primary root is
    /// corrected by the caller's placement pass.
    pub fn seed_scan(&mut self, abs: PathBuf, doc: Document) {
        self.saved.entry(abs).or_insert(doc);
    }

    /// Reconcile the saved copy of `abs` to disk — re-read and re-parse it, or
    /// drop it if it is gone — **unconditionally**.
    ///
    /// This is the watched-files writer and the `didClose` audit's applier.
    /// There is no open-document check and no dirty check: the saved store
    /// never held buffer content, so there is nothing to clobber. What `open`
    /// *does* control is clause 1's materialize-before-write — see
    /// [`Self::materialize_before_write`].
    ///
    /// Re-reading bytes identical to the saved content reports
    /// [`DiskUpdate::Untouched`] — nothing reparsed, nothing owed — so a
    /// watcher echo of content the server already holds costs one read.
    pub fn apply_from_disk(
        &mut self,
        abs: &Path,
        open: bool,
        primary: Option<PathBuf>,
        parse: &dyn Fn(&str) -> FileData,
    ) -> DiskUpdate {
        if !abs.is_file() {
            return self.retire_saved(abs, open);
        }
        let Ok(content) = std::fs::read_to_string(abs) else {
            // Exists but unreadable: drop it so no stale content lingers.
            return self.retire_saved(abs, open);
        };
        let existed = match self.saved.get(abs) {
            Some(doc) if doc.data.tree.source() == content => return DiskUpdate::Untouched,
            Some(_) => true,
            None => false,
        };
        self.materialize_before_write(abs, open);
        self.saved.insert(
            abs.to_path_buf(),
            Document {
                data: parse(&content),
                primary_root: primary,
            },
        );
        if existed {
            DiskUpdate::Content
        } else {
            DiskUpdate::Membership
        }
    }

    /// Commit a `didSave`'s `includeText` payload into the saved store and drop
    /// the overlay entry — the one seam between the tiers (decision 024
    /// clause 1).
    ///
    /// The notification's text is byte-identical to what the client just wrote,
    /// so no disk read is owed and the buffer is no longer divergent: after the
    /// commit the client's buffer *is* the saved copy, so it shares it.
    pub fn commit_save(
        &mut self,
        abs: &Path,
        primary: Option<PathBuf>,
        data: FileData,
    ) -> DiskUpdate {
        let existed = self.saved.contains_key(abs);
        self.overlay.remove(abs);
        self.saved.insert(
            abs.to_path_buf(),
            Document {
                data,
                primary_root: primary,
            },
        );
        if existed {
            DiskUpdate::Content
        } else {
            DiskUpdate::Membership
        }
    }

    /// The absent-`includeText` `didSave` fallback: drop the overlay entry,
    /// then re-read disk into the saved store.
    ///
    /// The overlay is dropped *first* and the disk apply runs as if the
    /// document were closed, so clause 1's materialize-before-write does not
    /// fire: a save means the buffer was just flushed, so the client's buffer
    /// and disk agree and there is nothing to preserve.
    pub fn commit_save_from_disk(
        &mut self,
        abs: &Path,
        primary: Option<PathBuf>,
        parse: &dyn Fn(&str) -> FileData,
    ) -> DiskUpdate {
        self.overlay.remove(abs);
        self.apply_from_disk(abs, false, primary, parse)
    }

    /// Evict a saved document that no longer belongs to any visible scope (a
    /// workspace folder was removed, or a nested scope merged away). Returns
    /// whether anything was removed.
    pub fn evict_saved(&mut self, abs: &Path) -> bool {
        self.saved.remove(abs).is_some()
    }

    // --- Overlay-store writers: didOpen / didChange / didClose -------------

    /// Seed the buffer copy at `didOpen` (decision 024 clause 3: a `didOpen` is
    /// a *claim*, not a source — it never writes the saved store).
    ///
    /// `data` is `None` when the buffer matches the saved copy: the document
    /// stores no overlay entry and shares the saved parse. Passing `None` also
    /// clears any stale entry, so a re-open with the saved text re-shares.
    pub fn open_buffer(&mut self, abs: &Path, data: Option<Document>) {
        match data {
            Some(doc) => {
                self.overlay.insert(abs.to_path_buf(), doc);
            }
            None => {
                self.overlay.remove(abs);
            }
        }
    }

    /// Materialize (or replace) the buffer copy at `didChange`.
    pub fn change_buffer(&mut self, abs: &Path, doc: Document) {
        self.overlay.insert(abs.to_path_buf(), doc);
    }

    /// Drop the buffer copy at `didClose`. Returns whether one existed.
    ///
    /// This is the whole of `didClose`'s store duty: the saved store never held
    /// buffer content, so there is nothing to revert (decision 024 clause 4).
    /// The audit that follows is the caller's, and it compares disk against the
    /// *saved* copy — never against the buffer, which is being discarded and
    /// has authority over nothing.
    pub fn close_buffer(&mut self, abs: &Path) -> bool {
        self.overlay.remove(abs).is_some()
    }

    // --- Maintenance (both tiers) -----------------------------------------

    /// Flip a document's recorded placement in both tiers.
    pub fn set_primary_root(&mut self, abs: &Path, primary: Option<&Path>) {
        for map in [&mut self.saved, &mut self.overlay] {
            if let Some(doc) = map.get_mut(abs) {
                doc.primary_root = primary.map(Path::to_path_buf);
            }
        }
    }

    /// Replace one tier's parse with a re-derivation of its **own** stored
    /// source under a changed config (a config hot-reload, a re-root that
    /// crosses a predicate vocabulary).
    ///
    /// Not a channel write: the content is the content already stored, so this
    /// cannot move buffer bytes into the saved store or vice versa.
    pub fn reparse_in_place(&mut self, abs: &Path, tier: Tier, data: FileData) {
        let map = match tier {
            Tier::Saved => &mut self.saved,
            Tier::Overlay => &mut self.overlay,
        };
        if let Some(doc) = map.get_mut(abs) {
            doc.data = data;
        }
    }

    /// Refresh one tier's cached structural diagnostics and suppression ledger.
    pub fn set_caches(
        &mut self,
        abs: &Path,
        tier: Tier,
        structural: Vec<Diagnostic>,
        suppressions: FileSuppressions,
    ) {
        let map = match tier {
            Tier::Saved => &mut self.saved,
            Tier::Overlay => &mut self.overlay,
        };
        if let Some(doc) = map.get_mut(abs) {
            doc.data.structural = structural;
            doc.data.suppressions = suppressions;
        }
    }

    /// Re-key every document at or under `old_abs` onto `new_abs`, in both
    /// tiers, preserving each copy's content and tier verbatim
    /// (`workspace/didRenameFiles` — decision 020 clause 2).
    ///
    /// A pure key move: the caller re-derives each moved document under its new
    /// coordinate afterwards through [`Self::reparse_in_place`]. Returns the
    /// `(old, new)` key pairs so the caller can follow its own per-path state.
    pub fn rekey(&mut self, old_abs: &Path, new_abs: &Path) -> Vec<(PathBuf, PathBuf)> {
        let moved: Vec<PathBuf> = self
            .all_keys()
            .into_iter()
            .filter(|abs| abs.starts_with(old_abs))
            .collect();
        let mut pairs = Vec::with_capacity(moved.len());
        for old_key in moved {
            let new_key = if old_key == old_abs {
                new_abs.to_path_buf()
            } else {
                old_key
                    .strip_prefix(old_abs)
                    .map_or_else(|_| old_key.clone(), |suffix| new_abs.join(suffix))
            };
            for map in [&mut self.saved, &mut self.overlay] {
                if let Some(doc) = map.remove(&old_key) {
                    map.insert(new_key.clone(), doc);
                }
            }
            pairs.push((old_key, new_key));
        }
        pairs
    }

    // --- Internals ---------------------------------------------------------

    /// Decision 024 clause 1's materialize-before-write.
    ///
    /// A disk write landing on an **open** document with **no** overlay entry
    /// first moves the pre-update saved parse into the overlay: the saved copy
    /// must advance (that clause is unconditional), but the client still holds
    /// the old text and owns its buffer. Moving the existing parse rather than
    /// re-parsing makes the rule free — the pre-update saved parse is exactly
    /// what the overlay needs.
    ///
    /// A no-op for a closed document (nothing holds the old text) and for one
    /// that already has an overlay entry (its buffer is already materialized).
    fn materialize_before_write(&mut self, abs: &Path, open: bool) {
        if !open || self.overlay.contains_key(abs) {
            return;
        }
        if let Some(prev) = self.saved.remove(abs) {
            self.overlay.insert(abs.to_path_buf(), prev);
        }
    }

    /// Drop `abs` from the saved store (it is gone from disk, or unreadable),
    /// materializing an open document's buffer first.
    fn retire_saved(&mut self, abs: &Path, open: bool) -> DiskUpdate {
        if !self.saved.contains_key(abs) {
            return DiskUpdate::Untouched;
        }
        self.materialize_before_write(abs, open);
        self.saved.remove(abs);
        DiskUpdate::Membership
    }
}
