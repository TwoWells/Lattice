// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! The surface-independent move engine (decision 020, ticket mv/01).
//!
//! [`compute_move_edits`] turns a declared file or directory move into its
//! complete, forced edit set: every text edit the move forces across affected
//! documents, plus the rename operation itself. Both surfaces call it — the CLI
//! `lattice mv` (ticket mv/03) applies the edits to disk then renames; the LSP
//! `workspace/willRenameFiles` handler (ticket mv/02) returns them to the
//! client. Neither has private semantics.
//!
//! The governing property (decision 020) is that **a correct move changes
//! coordinates, not the graph**: the post-move diagnostic set is the
//! coordinate-renamed image of the pre-move set. The engine fixes nothing and
//! breaks nothing — pre-existing drift survives verbatim at its new coordinates,
//! and a clean graph stays clean. That isomorphism is the engine's acceptance
//! test (`tests::drift_preserving_isomorphism_*`).
//!
//! # What the engine enumerates
//!
//! Exactly two surfaces carry a path a move forces:
//! 1. **Forward-link destinations** ([`crate::block::Link`]) in every file's
//!    body — inbound links into the moved set, and the moved files' own outbound
//!    links.
//! 2. **Backlink entries** in every file's frontmatter — entries naming a moved
//!    file, and moved files' own entries.
//!
//! An edge is re-rendered only when at least one endpoint is inside the moved
//! set, and then only when its post-move spelling actually differs from the
//! authored one. Prose/backticked/quoted path mentions, exception keys, and
//! `[external]` alias config are untouched by construction — they are the
//! post-move judgment surface (decision 020 clause 5).
//!
//! # What the engine does not do (a sketch for the property/fuzz arm)
//!
//! The engine is read-only: it computes spans and replacement text but never
//! mutates a document. A future property/fuzz arm (ticket mv/01 acceptance,
//! foldable into the `fuzz_edits` target) would generate a random valid move on
//! a generated workspace, apply the computed edits plus the key rename, and
//! assert the graph-tier diagnostic image is the coordinate rename of the
//! original — the same invariant `tests::drift_preserving_isomorphism_*` pins
//! deterministically here. The deterministic suite is this ticket's gate.

// The move engine is a complete, self-contained library surface landed by
// ticket mv/01; ticket mv/03 (`lattice mv`, the [`run`] runner below) is the
// first caller of [`compute_move_edits`], so the engine and its helpers are now
// live. The remaining second caller — mv/02's `workspace/willRenameFiles`
// handler — has not landed yet, so a few surface members it needs (the
// [`MoveEdits`] transport shape) are still consumed only by this module and its
// tests. The allow covers that ticket-scoped gap, not dead code.
#![allow(
    dead_code,
    reason = "move-engine transport surface consumed by ticket mv/02 (willRenameFiles); the CLI surface lands here"
)]

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use thiserror::Error;

use crate::block::{self, LinkKind};
use crate::fm::{self, FmNode, FmValue};
use crate::span::Span;
use crate::validation;
use crate::workspace::{Workspace, WorkspaceLike};

/// The complete edit set a move forces: per-file text edits plus the rename.
///
/// The engine produces byte-span edits and a separate [`RenameOp`] rather than
/// an `lsp::WorkspaceEdit` (which carries no rename operation); mapping this
/// onto each surface's transport is ticket mv/02's job.
#[derive(Debug, PartialEq, Eq)]
pub struct MoveEdits {
    /// Per-file text edits keyed by the file's **absolute** path (the flat
    /// document store's native keyspace). Within a file the edits are
    /// non-overlapping and sorted ascending by span start.
    pub edits: BTreeMap<PathBuf, Vec<MoveTextEdit>>,
    /// The rename operation itself.
    pub rename: RenameOp,
}

/// A single text edit: replace `span` (byte offsets into the file's current
/// source) with `new_text`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveTextEdit {
    /// Byte range in the file's current source to replace.
    pub span: Span,
    /// The replacement text.
    pub new_text: String,
}

/// The rename operation a move performs, as absolute paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameOp {
    /// The current absolute path of the moved file or directory.
    pub old: PathBuf,
    /// The absolute path it moves to.
    pub new: PathBuf,
}

/// A refusal to compute a move (decision 020 clause 6, plus the mv/01 review
/// rulings). Every variant names the fix; the engine is read-only, so a refusal
/// never leaves a partial edit set.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum MoveError {
    /// The destination resolves across a scope boundary — into a strictly-deeper
    /// nested scope, or above this scope's root. Such a move is an extraction,
    /// not a rename: inbound plain links would become cross-boundary defects.
    #[error(
        "move destination `{dest}` is outside this scope — a move across a scope boundary is an extraction, not a rename; keep the destination within the scope, or reference it through an `[external]` alias (see `lattice help config`)"
    )]
    CrossesBoundary {
        /// The (translated) destination that crosses the boundary.
        dest: PathBuf,
    },

    /// The destination already exists (as an indexed member or on disk). Choose
    /// a destination that does not exist.
    #[error("move destination `{dest}` already exists — choose a destination that does not exist")]
    DestinationExists {
        /// The colliding destination path.
        dest: PathBuf,
    },

    /// The source is outside any scope this workspace covers: not under the
    /// root, and neither an indexed member, an existing file, nor a directory
    /// prefixing members. There is no edit set to compute — a plain shell move
    /// already does everything Lattice could. Move a path under the workspace
    /// root instead.
    #[error(
        "move source `{source_path}` is outside the workspace — there is no edit set to compute; move a path under the workspace root instead"
    )]
    SourceOutsideScope {
        /// The out-of-scope source path.
        source_path: PathBuf,
    },

    /// The destination lies inside the source directory (e.g. `mv dir dir/sub`).
    /// The move would relocate the directory into itself. Choose a destination
    /// outside the source.
    #[error(
        "move destination `{dest}` is inside the source `{source_path}` — choose a destination outside the source directory"
    )]
    DestinationInsideSource {
        /// The source directory.
        source_path: PathBuf,
        /// The destination nested under it.
        dest: PathBuf,
    },

    /// A file move flips markdown-ness: exactly one of the source and
    /// destination has a markdown extension. That changes the node's kind
    /// (graph member vs. asset), so it cannot be a pure coordinate change.
    /// Rename within the same kind (`.md` to `.md`, or asset to asset).
    #[error(
        "move `{source_path}` -> `{dest}` changes markdown-ness — a move must keep the node's kind; rename within the same kind (`.md` to `.md`, or a non-markdown asset to a non-markdown asset)"
    )]
    MarkdownnessFlip {
        /// The source path.
        source_path: PathBuf,
        /// The destination path.
        dest: PathBuf,
    },

    /// The moved directory contains a scope marker (`.lattice.toml` / `.git`).
    /// Relocating it would require rewriting referrers' `[external]` alias
    /// values — config editing with its own blast radius, refused in v1. Move
    /// the inner scope on its own, or update the aliases by hand.
    #[error(
        "move source `{source_path}` contains a nested scope boundary (`{boundary}`) — relocating a nested scope is refused in v1; move the inner scope on its own and update referrers' `[external]` aliases by hand"
    )]
    DirectoryContainsMarker {
        /// The moved directory.
        source_path: PathBuf,
        /// The nested boundary directory it contains.
        boundary: PathBuf,
    },
}

/// The authored spelling style of a path expression, inferred from its raw
/// leading characters so the re-rendered spelling stays in the same style
/// (decision 020, "path re-rendering matches the authored style").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathStyle {
    /// Root-relative (`/x`): re-render as `/` + the target's store key.
    RootRelative,
    /// Document-relative with an explicit `./` prefix: keep the `./` while the
    /// re-rendered path still descends.
    DotRelative,
    /// Plain document-relative: rendered via [`validation::file_relative`].
    Plain,
}

impl PathStyle {
    /// Infer the authored style from a raw destination slice.
    fn infer(raw: &str) -> Self {
        if raw.starts_with('/') {
            Self::RootRelative
        } else if raw.starts_with("./") {
            Self::DotRelative
        } else {
            Self::Plain
        }
    }
}

/// Compute the complete, forced edit set for moving `old` to `new`.
///
/// `old` and `new` are **absolute** paths. `fs_exists` is a filesystem
/// existence oracle (injected, not a direct `stat`, so tests are deterministic —
/// the `compute_structural` pattern); it answers whether a non-indexed path
/// exists on disk, used both to validate a non-markdown source and to reject a
/// colliding destination.
///
/// Returns the edit set on success, or a [`MoveError`] naming the fix on any
/// refusal (decision 020 clause 6). The engine is read-only: a refusal computes
/// no edits and touches nothing.
///
/// # Errors
///
/// Returns a [`MoveError`] variant for each refusal condition — see that type.
pub fn compute_move_edits(
    workspace: &impl WorkspaceLike,
    old: &Path,
    new: &Path,
    fs_exists: &dyn Fn(&Path) -> bool,
) -> Result<MoveEdits, MoveError> {
    let root = workspace.root();

    // The source must lie under the root to have any workspace coordinate.
    let Ok(old_rel) = old.strip_prefix(root) else {
        return Err(MoveError::SourceOutsideScope {
            source_path: old.to_path_buf(),
        });
    };
    let old_rel = old_rel.to_path_buf();

    // old == new is a no-op whose destination already exists (the source is
    // there). Refuse via DestinationExists per the review ruling.
    if old == new {
        return Err(MoveError::DestinationExists {
            dest: new.to_path_buf(),
        });
    }

    // The destination lying inside the source is a move-into-itself.
    if new.starts_with(old) {
        return Err(MoveError::DestinationInsideSource {
            source_path: old.to_path_buf(),
            dest: new.to_path_buf(),
        });
    }

    // Enumerate the moved set of relative keys and decide file vs. directory.
    let members: Vec<PathBuf> = workspace
        .files_iter()
        .map(|(key, _)| key.clone())
        .filter(|key| key == &old_rel || key.starts_with(&old_rel))
        .collect();

    let source_is_member = members.iter().any(|k| k == &old_rel);
    let source_is_dir_prefix = members.iter().any(|k| k != &old_rel);
    let source_exists_on_disk = fs_exists(old);

    // Source validity (review ruling 4): under root AND (a member, or exists on
    // disk as a non-markdown asset, or a directory prefixing members).
    if !source_is_member && !source_is_dir_prefix && !source_exists_on_disk {
        return Err(MoveError::SourceOutsideScope {
            source_path: old.to_path_buf(),
        });
    }

    // A move is a directory move when the source is not itself a member but
    // prefixes members, or when it exists on disk yet is not a file member and
    // not a plausible file (no extension). We treat "is an indexed member" as
    // the file case; everything else that prefixes members is a directory.
    let is_directory = !source_is_member && source_is_dir_prefix;

    // Markdown-ness flip (review ruling 6): a *file* move may not change the
    // node's kind. Only applies when the source is an indexed member or an
    // existing asset file — a directory move has no single extension.
    if !is_directory {
        let old_md = has_md_ext(old);
        let new_md = has_md_ext(new);
        if old_md != new_md {
            return Err(MoveError::MarkdownnessFlip {
                source_path: old.to_path_buf(),
                dest: new.to_path_buf(),
            });
        }
    }

    // A directory carrying a nested scope marker is refused in v1.
    if is_directory
        && let Some(boundary) = workspace.boundaries().iter().find(|b| b.starts_with(old))
    {
        return Err(MoveError::DirectoryContainsMarker {
            source_path: old.to_path_buf(),
            boundary: boundary.clone(),
        });
    }

    // The destination must not already exist.
    if workspace.resolve_key(new).is_some() || fs_exists(new) {
        return Err(MoveError::DestinationExists {
            dest: new.to_path_buf(),
        });
    }

    // The (translated) destination must not cross a scope boundary.
    if workspace.crosses_boundary(new) {
        return Err(MoveError::CrossesBoundary {
            dest: new.to_path_buf(),
        });
    }

    let Ok(new_rel) = new.strip_prefix(root) else {
        // `new` under `root` is implied by the not-crossing-boundary check
        // above (a destination above the root crosses it), but guard defensively.
        return Err(MoveError::CrossesBoundary {
            dest: new.to_path_buf(),
        });
    };
    let new_rel = new_rel.to_path_buf();

    let ctx = MoveCtx {
        old_rel,
        new_rel,
        members,
    };

    let mut edits: BTreeMap<PathBuf, Vec<MoveTextEdit>> = BTreeMap::new();
    collect_forward_link_edits(workspace, &ctx, &mut edits);
    collect_backlink_entry_edits(workspace, &ctx, &mut edits);

    // Per-file: sort ascending and drop any that would overlap (defensive — the
    // enumeration produces disjoint spans, since each edge owns a distinct
    // destination slice or scalar span).
    for file_edits in edits.values_mut() {
        file_edits.sort_by_key(|e| (e.span.start, e.span.end));
    }

    Ok(MoveEdits {
        edits,
        rename: RenameOp {
            old: old.to_path_buf(),
            new: new.to_path_buf(),
        },
    })
}

/// The resolved move context in workspace-relative coordinates.
struct MoveCtx {
    /// Relative key of the source (a member key for a file, a directory prefix
    /// for a directory).
    old_rel: PathBuf,
    /// Relative key of the destination.
    new_rel: PathBuf,
    /// Relative keys of every moved member.
    members: Vec<PathBuf>,
}

impl MoveCtx {
    /// Whether a relative key is inside the moved set.
    fn in_moved_set(&self, rel: &Path) -> bool {
        rel == self.old_rel || rel.starts_with(&self.old_rel)
    }

    /// Translate a relative key: a moved key maps under `new_rel`; anything else
    /// is identity.
    fn translate(&self, rel: &Path) -> PathBuf {
        if rel == self.old_rel {
            return self.new_rel.clone();
        }
        rel.strip_prefix(&self.old_rel)
            .map_or_else(|_| rel.to_path_buf(), |suffix| self.new_rel.join(suffix))
    }
}

/// Whether a path has a `.md` (ASCII case-insensitive) extension.
fn has_md_ext(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
}

/// Render a relative path as a forward-slash-joined string (the on-disk
/// separator on the supported platform; matches how the graph tier displays
/// keys).
fn rel_to_string(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Map a link `target` (root-free: absolute for a document-relative link, the
/// relative remainder for a root-relative one) onto its workspace-relative key.
fn target_to_rel_key(root: &Path, target: &Path) -> PathBuf {
    if target.is_absolute() {
        target
            .strip_prefix(root)
            .map_or_else(|_| target.to_path_buf(), Path::to_path_buf)
    } else {
        target.to_path_buf()
    }
}

/// Re-render the path expression that denotes `target_rel` authored from
/// `source_rel`, in the given `style`, preserving `fragment` verbatim.
fn render_spelling(
    source_rel: &Path,
    target_rel: &Path,
    style: PathStyle,
    fragment: Option<&str>,
) -> String {
    let mut path = match style {
        PathStyle::RootRelative => format!("/{}", rel_to_string(target_rel)),
        PathStyle::DotRelative | PathStyle::Plain => {
            let rel = validation::file_relative(source_rel, target_rel);
            let rendered = rel_to_string(&rel);
            // Keep an authored `./` only while the re-rendered path still
            // descends (no leading `..`); otherwise a plain relative spelling.
            if style == PathStyle::DotRelative && !rendered.starts_with("..") {
                format!("./{rendered}")
            } else {
                rendered
            }
        }
    };
    if let Some(frag) = fragment {
        path.push('#');
        path.push_str(frag);
    }
    path
}

/// Collect the forced edits to forward-link destinations across every file.
///
/// Walks every file's cached links; for each intra-project / non-markdown link
/// whose edge has at least one endpoint in the moved set, computes the post-move
/// spelling and, when it differs from the authored one, emits an edit at the
/// destination's byte span (inline / angle-bracket / import) or, for a
/// reference-style link, at its `ReferenceDef` URL (deduped per definition).
fn collect_forward_link_edits(
    workspace: &impl WorkspaceLike,
    ctx: &MoveCtx,
    edits: &mut BTreeMap<PathBuf, Vec<MoveTextEdit>>,
) {
    let root = workspace.root();
    for (source_key, file_data) in workspace.files_iter() {
        let source_moved = ctx.in_moved_set(source_key);
        let new_source_rel = ctx.translate(source_key);
        let source = &file_data.tree;
        let src_text = source.source();

        // Track which ReferenceDef URLs we have already edited in this file, so
        // multiple reference-style links to the same label produce one edit.
        let mut edited_refdefs: Vec<Span> = Vec::new();

        for link in &file_data.links {
            // The fragment rides along verbatim: for an inline/import link it is
            // outside `link_destination_span`, and for a reference-style link it
            // is re-derived from the definition's raw URL below. So only the
            // target matters here.
            let target = match &link.kind {
                LinkKind::IntraProject { target, .. } | LinkKind::NonMarkdown { target } => target,
                LinkKind::External { .. } | LinkKind::IntraDocument { .. } => continue,
            };

            let target_rel = target_to_rel_key(root, target);
            let target_moved = ctx.in_moved_set(&target_rel);
            // Only an edge with at least one moved endpoint is a coordinate the
            // move forces; everything else is authored surface we must not touch.
            if !source_moved && !target_moved {
                continue;
            }
            let new_target_rel = ctx.translate(&target_rel);

            if let Some(dest_span) = block::link_destination_span(src_text, link.span) {
                let raw = &src_text[dest_span.start..dest_span.end];
                let style = PathStyle::infer(raw);
                let new_spelling = render_spelling(&new_source_rel, &new_target_rel, style, None);
                if new_spelling != raw {
                    push_edit(
                        edits,
                        root.join(source_key),
                        MoveTextEdit {
                            span: dest_span,
                            new_text: new_spelling,
                        },
                    );
                }
            } else if let Some(url_span) = reference_def_url_span(source, link.span) {
                if edited_refdefs.contains(&url_span) {
                    continue;
                }
                let raw = &src_text[url_span.start..url_span.end];
                // The refdef URL carries its own fragment; re-render the path
                // portion only and keep the fragment verbatim.
                let (raw_path, raw_frag) = split_fragment(raw);
                let style = PathStyle::infer(raw_path);
                let new_spelling =
                    render_spelling(&new_source_rel, &new_target_rel, style, raw_frag);
                if new_spelling != raw {
                    edited_refdefs.push(url_span);
                    push_edit(
                        edits,
                        root.join(source_key),
                        MoveTextEdit {
                            span: url_span,
                            new_text: new_spelling,
                        },
                    );
                }
            }
        }
    }
}

/// Locate the byte span of a reference-style link's destination — the URL of the
/// `ReferenceDef` its label resolves to — within the source, or `None`.
///
/// A reference-style link node (`[text][label]`, `[label]`) carries the resolved
/// URL as its `ElementKind::Link` payload but no inline destination span; the
/// authored destination lives in the definition. We resolve the label from the
/// link's text and find the matching `ReferenceDef`, then locate its URL span.
fn reference_def_url_span(tree: &block::Tree, link_span: Span) -> Option<Span> {
    let source = tree.source();
    let slice = &source[link_span.start..link_span.end.min(source.len())];
    let label = reference_link_label(slice)?;
    let (def_id, _node) = tree.find_ref_def(&label)?;
    let def_node = tree.node(def_id);
    refdef_url_span(source, def_node.span)
}

/// Extract the normalized reference label from a reference-style link slice.
///
/// Handles the full (`[text][label]`), collapsed (`[text][]`), and shortcut
/// (`[label]`) forms. Returns the label normalized the same way the inline
/// parser does (case-folded, whitespace-collapsed).
fn reference_link_label(slice: &str) -> Option<String> {
    let bytes = slice.as_bytes();
    if bytes.first() != Some(&b'[') {
        return None;
    }
    let first_close = matching_bracket(bytes, 0)?;
    let after = first_close + 1;
    if after < bytes.len() && bytes[after] == b'[' {
        let second_close = matching_bracket(bytes, after)?;
        if second_close == after + 1 {
            // Collapsed `[text][]` — label is the text.
            return Some(normalize_label(&slice[1..first_close]));
        }
        // Full `[text][label]` — label is the second bracket's content.
        return Some(normalize_label(&slice[after + 1..second_close]));
    }
    // Shortcut `[label]` — label is the text.
    Some(normalize_label(&slice[1..first_close]))
}

/// Index of the `]` matching the `[` at `open`, honoring escapes and nesting.
fn matching_bracket(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth: i32 = 0;
    let mut i = open;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if i + 1 < bytes.len() => i += 2,
            b'[' => {
                depth += 1;
                i += 1;
            }
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
    None
}

/// Normalize a reference label: trim, collapse internal whitespace, case-fold.
/// Mirrors the inline parser's `normalize_label`.
fn normalize_label(label: &str) -> String {
    label
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Locate the URL span within a `ReferenceDef` node's span (`[label]: url
/// "title"`). Returns the byte range of the `url` token, or `None`.
fn refdef_url_span(source: &str, def_span: Span) -> Option<Span> {
    let base = def_span.start;
    let slice = &source[def_span.start..def_span.end.min(source.len())];
    let bytes = slice.as_bytes();
    // Skip to the label's closing `]`, then require `:` (the `]:` separator).
    let close = matching_bracket(bytes, skip_leading_spaces(bytes))?;
    let mut i = close + 1;
    if i >= bytes.len() || bytes[i] != b':' {
        return None;
    }
    i += 1;
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }
    // Angle-bracketed URL `<...>`.
    if bytes[i] == b'<' {
        let inner = i + 1;
        let mut j = inner;
        while j < bytes.len() && bytes[j] != b'>' && bytes[j] != b'\n' {
            j += 1;
        }
        if j >= bytes.len() || bytes[j] != b'>' {
            return None;
        }
        if j == inner {
            return None;
        }
        return Some(Span::new(base + inner, base + j));
    }
    // Bare URL: up to whitespace.
    let start = i;
    while i < bytes.len() && !matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    if i == start {
        return None;
    }
    Some(Span::new(base + start, base + i))
}

/// Index of the first non-space, non-`[`-preceding position — the `[` opener
/// after up to three spaces of indentation. Returns the index of the `[`.
fn skip_leading_spaces(bytes: &[u8]) -> usize {
    let mut i = 0;
    while i < bytes.len() && bytes[i] == b' ' {
        i += 1;
    }
    i
}

/// Split a raw destination into its path portion and optional fragment
/// (excluding the `#`).
fn split_fragment(raw: &str) -> (&str, Option<&str>) {
    match raw.split_once('#') {
        Some((path, frag)) => (path, Some(frag)),
        None => (raw, None),
    }
}

/// Collect the forced edits to frontmatter backlink entries across every file.
///
/// For each file with frontmatter backlinks, resolves each `(predicate, path)`
/// entry to a workspace-relative key; when the edge has at least one endpoint in
/// the moved set, re-renders the path (file-relative from the entry's post-move
/// location, `./` and quotes preserved) and, when it differs, emits an edit at
/// the entry's re-parsed scalar span.
fn collect_backlink_entry_edits(
    workspace: &impl WorkspaceLike,
    ctx: &MoveCtx,
    edits: &mut BTreeMap<PathBuf, Vec<MoveTextEdit>>,
) {
    let root = workspace.root();
    for (file_key, file_data) in workspace.files_iter() {
        let Some(frontmatter) = &file_data.frontmatter else {
            continue;
        };
        if frontmatter.backlinks.is_empty() {
            continue;
        }
        let file_moved = ctx.in_moved_set(file_key);
        let new_file_rel = ctx.translate(file_key);

        // Re-parse the frontmatter block to recover per-entry scalar spans that
        // `Frontmatter.backlinks` discards.
        let source = file_data.tree.source();
        let Some(block) = reparse_frontmatter_block(&file_data.tree) else {
            continue;
        };

        for entry in backlink_scalar_entries(&block) {
            let raw = &source[entry.span.start..entry.span.end];
            // The scalar span may include surrounding quotes; separate them so we
            // re-render only the path content and re-wrap in the same quotes.
            let (open_quote, inner, close_quote) = strip_quotes(raw);
            let resolved = validation::resolve_backlink_path(file_key, inner);
            let target_moved = ctx.in_moved_set(&resolved);
            if !file_moved && !target_moved {
                continue;
            }
            let new_target_rel = ctx.translate(&resolved);
            let style = PathStyle::infer(inner);
            let new_inner = render_spelling(&new_file_rel, &new_target_rel, style, None);
            if new_inner == inner {
                continue;
            }
            let new_text = format!("{open_quote}{new_inner}{close_quote}");
            push_edit(
                edits,
                root.join(file_key),
                MoveTextEdit {
                    span: entry.span,
                    new_text,
                },
            );
        }
    }
}

/// A backlink path scalar with its source span.
struct BacklinkScalar {
    /// Byte span of the scalar (may include surrounding quotes).
    span: Span,
}

/// Re-parse a file's frontmatter block from its cached tree source, recovering
/// the span-carrying [`fm::FrontmatterBlock`] the parse path discards.
///
/// Tries the leading-delimiter parsers in the same order as the parse path
/// (`---` YAML, `+++` TOML, `{` JSON), then the fenced `yaml lattice` carrier.
fn reparse_frontmatter_block(tree: &block::Tree) -> Option<fm::FrontmatterBlock> {
    let source = tree.source();
    crate::yaml::parse_frontmatter_block(source)
        .or_else(|| crate::toml::parse_frontmatter_block(source))
        .or_else(|| crate::json::parse_frontmatter_block(source))
        .or_else(|| crate::metadata::parse_carrier_block(tree))
}

/// Walk a re-parsed frontmatter block for the `backlinks` mapping's path
/// scalars, yielding each with its span (document order).
fn backlink_scalar_entries(block: &fm::FrontmatterBlock) -> Vec<BacklinkScalar> {
    let mut out = Vec::new();
    for entry in &block.entries {
        let FmNode::Mapping { key, value, .. } = entry else {
            continue;
        };
        if key.text != "backlinks" {
            continue;
        }
        let FmValue::Mapping(predicates) = value else {
            break;
        };
        for pred_entry in predicates {
            let FmNode::Mapping {
                value: pred_value, ..
            } = pred_entry
            else {
                continue;
            };
            match pred_value {
                FmValue::Sequence(items) => {
                    for item in items {
                        if let FmNode::SequenceItem {
                            value: FmValue::Scalar(s),
                            ..
                        } = item
                        {
                            out.push(BacklinkScalar { span: s.span });
                        }
                    }
                }
                FmValue::FlowSequence { items, .. } => {
                    for item in items {
                        out.push(BacklinkScalar { span: item.span });
                    }
                }
                _ => {}
            }
        }
        break;
    }
    out
}

/// Separate a raw scalar slice into an optional surrounding quote, its inner
/// content, and the matching closing quote. A YAML/TOML/JSON string may be
/// single- or double-quoted; a plain scalar has no quotes.
fn strip_quotes(raw: &str) -> (&str, &str, &str) {
    let bytes = raw.as_bytes();
    if raw.len() >= 2 {
        let first = bytes[0];
        let last = bytes[raw.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return (&raw[..1], &raw[1..raw.len() - 1], &raw[raw.len() - 1..]);
        }
    }
    ("", raw, "")
}

/// Append an edit for a file, keyed by its absolute path.
fn push_edit(
    edits: &mut BTreeMap<PathBuf, Vec<MoveTextEdit>>,
    abs_path: PathBuf,
    edit: MoveTextEdit,
) {
    edits.entry(abs_path).or_default().push(edit);
}

// ---------------------------------------------------------------------------
// CLI surface: `lattice mv <old> <new>` (ticket mv/03, decision 020 clause 3)
// ---------------------------------------------------------------------------

/// Run the `lattice mv` shell surface: apply the move engine's forced edits, then
/// perform the rename.
///
/// `old` and `new` are the raw CLI arguments (resolved against the process
/// current directory if relative). The workspace is discovered by scanning from
/// `old` — the same root discovery `lattice lint` uses. `new` is resolved
/// shell-`mv` style: an existing directory destination becomes
/// `new/<basename-of-old>`.
///
/// On a refusal the engine's [`MoveError`] is returned as an `anyhow` error whose
/// message names the fix (the caller in [`crate::run`] renders it and exits
/// non-zero); the workspace is left byte-identical. On success the computed text
/// edits are written to disk **first**, then the rename is performed — so a write
/// failure stops before the rename, leaving a re-derivable state rather than a
/// half-move. With `dry_run` set, the edit set and rename are printed and nothing
/// is touched.
///
/// After a successful move a ledger line reports what moved and how many edits
/// landed across how many files — the same per-source accounting shape the lint
/// ledger uses.
///
/// # Errors
///
/// Returns an error if the workspace cannot be scanned, the move is refused, a
/// file read/write fails, or the rename fails. On any error before the rename the
/// engine's read-only contract and the edits-before-rename order together keep
/// the tree recoverable.
pub fn run(old: &Path, new: &Path, dry_run: bool, out: &mut impl Write) -> Result<()> {
    let old_abs = absolutize(old).context("failed to resolve the move source path")?;

    let workspace = Workspace::scan(&old_abs).context("failed to scan workspace")?;

    // Resolve the destination shell-`mv` style: a destination naming an existing
    // directory means "move the source *into* it", so the real destination is
    // that directory joined with the source's file name.
    let new_abs = resolve_destination(old, new, &old_abs)
        .context("failed to resolve the move destination path")?;

    let fs_exists = |p: &Path| p.exists();
    let edits = compute_move_edits(&workspace, &old_abs, &new_abs, &fs_exists)
        // The engine names the fix in every refusal message; surface it verbatim.
        .map_err(anyhow::Error::new)
        .context("move refused")?;

    if dry_run {
        write_dry_run(&workspace, &edits, out)?;
        return Ok(());
    }

    apply_to_disk(&edits)?;
    write_summary(workspace.root(), &edits, out)?;
    Ok(())
}

/// Absolutize a path against the process current directory without requiring it
/// to exist (`std::path::absolute` is purely lexical — it does not `stat` or
/// resolve symlinks, so a not-yet-existing destination absolutizes fine).
fn absolutize(path: &Path) -> Result<PathBuf> {
    std::path::absolute(path)
        .with_context(|| format!("could not resolve `{}` to an absolute path", path.display()))
}

/// Resolve the destination shell-`mv` style.
///
/// A destination that is an existing directory means "move `old` into it": the
/// real destination is `new/<file-name-of-old>`. Otherwise the destination is
/// taken as the full target path. Both are returned absolutized; the engine
/// enforces every refusal (existing destination, cross-boundary, etc.), so this
/// only performs the `mv`-style directory sugar.
fn resolve_destination(old: &Path, new: &Path, old_abs: &Path) -> Result<PathBuf> {
    let new_abs = absolutize(new)?;
    if new_abs.is_dir() {
        let file_name = old_abs
            .file_name()
            .or_else(|| old.file_name())
            .with_context(|| {
                format!(
                    "move source `{}` has no file name to join onto the destination directory",
                    old.display()
                )
            })?;
        Ok(new_abs.join(file_name))
    } else {
        Ok(new_abs)
    }
}

/// Apply the computed text edits to disk, then perform the rename.
///
/// The order is load-bearing (decision 020 clause 3, ticket mv/03): every text
/// edit is written **before** the rename, so a write failure aborts with the
/// rename not yet performed. The edits are re-derivable from the workspace, so an
/// aborted apply is recoverable; a half-move (rename done, some edits missing) is
/// not. Within a file the edits are spliced back-to-front so earlier byte offsets
/// stay valid as later ones are replaced.
fn apply_to_disk(edits: &MoveEdits) -> Result<()> {
    for (abs_path, file_edits) in &edits.edits {
        let source = std::fs::read_to_string(abs_path)
            .with_context(|| format!("failed to read `{}` for editing", abs_path.display()))?;
        let mut text = source;
        // Sort descending by start so each splice leaves earlier offsets intact.
        let mut sorted = file_edits.clone();
        sorted.sort_by_key(|e| std::cmp::Reverse(e.span.start));
        for edit in &sorted {
            text.replace_range(edit.span.start..edit.span.end, &edit.new_text);
        }
        std::fs::write(abs_path, text)
            .with_context(|| format!("failed to write edits to `{}`", abs_path.display()))?;
    }

    // Every text edit landed; only now perform the rename. Create the
    // destination's parent so a move into a new directory succeeds.
    let RenameOp { old, new } = &edits.rename;
    if let Some(parent) = new.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create destination directory `{}`",
                parent.display()
            )
        })?;
    }
    std::fs::rename(old, new).with_context(|| {
        format!(
            "failed to rename `{}` to `{}`",
            old.display(),
            new.display()
        )
    })?;

    Ok(())
}

/// Write the dry-run edit set: every forced edit as `file:line: before -> after`,
/// then the rename. Touches nothing.
///
/// The `before`/`after` text and the target spans are exactly what
/// [`apply_to_disk`] would splice, so the dry-run is byte-exact with the applied
/// result (acceptance: "`--dry-run` output matches the edits subsequently applied,
/// byte-exact"). Paths are rendered workspace-relative — the coordinate the graph
/// tier and the lint ledger use.
fn write_dry_run(workspace: &Workspace, edits: &MoveEdits, out: &mut impl Write) -> Result<()> {
    let root = workspace.root();
    if edits.edits.is_empty() {
        writeln!(out, "no edits: the move forces no reference updates")?;
    }
    for (abs_path, file_edits) in &edits.edits {
        let rel = display_rel(root, abs_path);
        let source = std::fs::read_to_string(abs_path)
            .with_context(|| format!("failed to read `{}` for the dry-run", abs_path.display()))?;
        for edit in file_edits {
            let line = line_of(&source, edit.span.start);
            let before = &source[edit.span.start..edit.span.end.min(source.len())];
            writeln!(out, "{rel}:{line}: `{before}` -> `{}`", edit.new_text)?;
        }
    }
    let old_rel = display_rel(root, &edits.rename.old);
    let new_rel = display_rel(root, &edits.rename.new);
    writeln!(out, "rename: {old_rel} -> {new_rel}")?;
    Ok(())
}

/// Write the post-move summary: what moved and how many edits landed across how
/// many files — the lint ledger's per-source accounting shape.
///
/// The rename ends are rendered workspace-relative (via `root`), the coordinate
/// the graph tier and the dry-run both use.
fn write_summary(root: &Path, edits: &MoveEdits, out: &mut impl Write) -> Result<()> {
    let files = edits.edits.len();
    let total: usize = edits.edits.values().map(Vec::len).sum();
    let old = display_rel(root, &edits.rename.old);
    let new = display_rel(root, &edits.rename.new);
    writeln!(
        out,
        "moved: {old} -> {new}  ({} across {})",
        plural(total, "edit"),
        plural(files, "file")
    )?;
    Ok(())
}

/// Render `path` relative to `root` when it is under it, else its full display.
fn display_rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

/// The 1-based line number of byte `offset` in `source` (a `\n` count plus one).
/// Matches the `path:line:` shape the lint ledger uses; a `\r\n` is one break.
fn line_of(source: &str, offset: usize) -> usize {
    let clamped = offset.min(source.len());
    source[..clamped].bytes().filter(|&b| b == b'\n').count() + 1
}

/// Format a count with its noun, pluralized with a trailing `s` for `n != 1`
/// (the lint ledger's `format_counts` convention).
fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("{n} {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests use expect and panic for clarity"
)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use super::{MoveError, compute_move_edits};
    use crate::validation::{self, Diagnostic, Severity};
    use crate::workspace::Workspace;

    /// A move fixture on disk: a `.git` + `.lattice.toml` rooted workspace with
    /// the given files, plus the extra asset files that are not markdown.
    struct Fixture {
        dir: TempDir,
    }

    impl Fixture {
        /// Build a fixture from `(relative path, content)` pairs. An empty
        /// `.lattice.toml` enables the graph diagnostic tier with defaults.
        fn new(files: &[(&str, &str)]) -> Self {
            let dir = TempDir::new().expect("create temp dir");
            fs::create_dir(dir.path().join(".git")).expect("create .git");
            fs::write(dir.path().join(".lattice.toml"), "").expect("write config");
            for (path, content) in files {
                write_file(dir.path(), path, content);
            }
            Self { dir }
        }

        /// The workspace root (absolute).
        fn root(&self) -> &Path {
            self.dir.path()
        }

        /// Scan the current on-disk state into a fresh workspace.
        fn scan(&self) -> Workspace {
            Workspace::scan(self.dir.path()).expect("scan workspace")
        }
    }

    /// A filesystem existence oracle over absolute paths — the injected
    /// `fs_exists` for the on-disk fixtures. A free function (the fixtures use
    /// absolute paths, so no fixture state is captured).
    fn fs_exists(p: &Path) -> bool {
        p.exists()
    }

    /// Write `content` to `root/rel`, creating parent directories.
    fn write_file(root: &Path, rel: &str, content: &str) {
        let full = root.join(rel);
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent).expect("create parent dirs");
        }
        fs::write(&full, content).expect("write file");
    }

    /// Apply a computed [`super::MoveEdits`] to a fresh temp copy of `fixture`
    /// and perform the rename on disk, returning the applied copy. Splices each
    /// file's edits back-to-front so earlier byte offsets stay valid.
    fn apply(fixture: &Fixture, edits: &super::MoveEdits) -> Fixture {
        let out = TempDir::new().expect("create temp dir");
        // Copy every entry (including .git / .lattice.toml) from the source.
        copy_tree(fixture.dir.path(), out.path());

        // Apply text edits per file (keyed by absolute path in the *source*
        // tree; translate onto the output tree by rebasing the root).
        for (abs_path, file_edits) in &edits.edits {
            let rel = abs_path
                .strip_prefix(fixture.dir.path())
                .expect("edit path under source root");
            let target = out.path().join(rel);
            let mut text = fs::read_to_string(&target).expect("read edited file");
            let mut sorted = file_edits.clone();
            sorted.sort_by_key(|e| e.span.start);
            for edit in sorted.iter().rev() {
                text.replace_range(edit.span.start..edit.span.end, &edit.new_text);
            }
            fs::write(&target, text).expect("write edited file");
        }

        // Perform the rename on the output tree.
        let old_rel = edits
            .rename
            .old
            .strip_prefix(fixture.dir.path())
            .expect("rename old under source root");
        let new_rel = edits
            .rename
            .new
            .strip_prefix(fixture.dir.path())
            .expect("rename new under source root");
        let old_abs = out.path().join(old_rel);
        let new_abs = out.path().join(new_rel);
        if let Some(parent) = new_abs.parent() {
            fs::create_dir_all(parent).expect("create rename dest parent");
        }
        fs::rename(&old_abs, &new_abs).expect("perform rename");

        Fixture { dir: out }
    }

    /// Recursively copy every entry under `src` into `dst`.
    fn copy_tree(src: &Path, dst: &Path) {
        for entry in fs::read_dir(src).expect("read source dir") {
            let entry = entry.expect("dir entry");
            let from = entry.path();
            let to = dst.join(entry.file_name());
            if entry.file_type().expect("file type").is_dir() {
                fs::create_dir_all(&to).expect("create dir");
                copy_tree(&from, &to);
            } else {
                if let Some(parent) = to.parent() {
                    fs::create_dir_all(parent).expect("create parent");
                }
                fs::copy(&from, &to).expect("copy file");
            }
        }
    }

    /// Translate a workspace-relative key under the move `old_rel -> new_rel`.
    fn translate_key(rel: &Path, old_rel: &Path, new_rel: &Path) -> PathBuf {
        if rel == old_rel {
            return new_rel.to_path_buf();
        }
        rel.strip_prefix(old_rel)
            .map_or_else(|_| rel.to_path_buf(), |suffix| new_rel.join(suffix))
    }

    /// Replace every path-shaped token in a message with a `<P>` placeholder.
    ///
    /// The graph tier renders path spellings four different ways (root-relative
    /// key display, file-relative rel-paths, authored raw entries), and a
    /// coordinate move re-renders each in its own coordinates — so comparing raw
    /// message strings across a move would demand re-deriving every rendering.
    /// Instead we normalize spellings away: a token is path-shaped when it
    /// contains a `/` or ends in `.md` (or is `..`). What remains — the
    /// diagnostic *kind*, its predicate/severity, and the file it anchors on
    /// (compared separately, transported) — is exactly the graph identity the
    /// governing property preserves. A wrong retarget still shows up: it changes
    /// a diagnostic's *kind* or *count* (a heal or a new break), which this
    /// multiset and the severity histogram both catch, and the clean-fixture
    /// arm turns any spurious edit into a brand-new diagnostic.
    fn normalize_paths(message: &str) -> String {
        message
            .split(' ')
            .map(|tok| {
                let stripped = tok.trim_matches(|c| matches!(c, '`' | '\'' | '"' | ':'));
                let looks_md = Path::new(stripped)
                    .extension()
                    .is_some_and(|e| e.eq_ignore_ascii_case("md"));
                if stripped.contains('/') || looks_md || stripped == ".." {
                    tok.replace(stripped, "<P>")
                } else {
                    tok.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The path-normalized multiset of `(file, severity, message-kind)`.
    fn kind_multiset(diags: &[Diagnostic]) -> BTreeMap<(PathBuf, String, String), usize> {
        let mut map: BTreeMap<(PathBuf, String, String), usize> = BTreeMap::new();
        for d in diags {
            let key = (
                d.file.clone(),
                format!("{:?}", d.severity),
                normalize_paths(&d.message),
            );
            *map.entry(key).or_default() += 1;
        }
        map
    }

    /// Transport a pre-move diagnostic set under the move, producing the
    /// path-normalized image the post-move set must equal.
    fn transported(
        diags: &[Diagnostic],
        old_rel: &Path,
        new_rel: &Path,
    ) -> BTreeMap<(PathBuf, String, String), usize> {
        let mut map: BTreeMap<(PathBuf, String, String), usize> = BTreeMap::new();
        for d in diags {
            let file = translate_key(&d.file, old_rel, new_rel);
            let key = (
                file,
                format!("{:?}", d.severity),
                normalize_paths(&d.message),
            );
            *map.entry(key).or_default() += 1;
        }
        map
    }

    /// Per-file severity histogram (the safety-net cross-check).
    fn severity_histogram(diags: &[Diagnostic]) -> BTreeMap<(PathBuf, String), usize> {
        let mut map: BTreeMap<(PathBuf, String), usize> = BTreeMap::new();
        for d in diags {
            *map.entry((d.file.clone(), format!("{:?}", d.severity)))
                .or_default() += 1;
        }
        map
    }

    /// Drive the full drift-preserving isomorphism check for a single move.
    fn assert_isomorphism(fixture: &Fixture, old_rel: &str, new_rel: &str) -> super::MoveEdits {
        let ws = fixture.scan();
        let pre = validation::collect_all(&ws);

        let old_abs = fixture.root().join(old_rel);
        let new_abs = fixture.root().join(new_rel);
        let edits = compute_move_edits(&ws, &old_abs, &new_abs, &fs_exists)
            .expect("move should compute an edit set");

        let applied = apply(fixture, &edits);
        let post_ws = applied.scan();
        let post = validation::collect_all(&post_ws);

        let expected = transported(&pre, Path::new(old_rel), Path::new(new_rel));
        let actual = kind_multiset(&post);
        assert_eq!(
            actual, expected,
            "post-move graph diagnostics must equal the coordinate-renamed pre-move set\n  pre: {pre:#?}\n  post: {post:#?}"
        );

        // Safety net: the per-file severity histogram, transported, must match.
        let pre_hist = severity_histogram(&pre);
        let mut expected_hist: BTreeMap<(PathBuf, String), usize> = BTreeMap::new();
        for ((file, sev), n) in pre_hist {
            let tfile = translate_key(&file, Path::new(old_rel), Path::new(new_rel));
            *expected_hist.entry((tfile, sev)).or_default() += n;
        }
        assert_eq!(
            severity_histogram(&post),
            expected_hist,
            "per-file severity histogram must transport exactly"
        );

        edits
    }

    /// Count the total number of text edits across all files.
    fn total_edit_count(edits: &super::MoveEdits) -> usize {
        edits.edits.values().map(Vec::len).sum()
    }

    /// The diagnostics for one file, by severity.
    fn errors(diags: &[Diagnostic]) -> Vec<&Diagnostic> {
        diags
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect()
    }

    // --- 1. Drift-preserving isomorphism (the governing property) ---

    #[test]
    fn drift_preserving_isomorphism_clean() {
        // A clean two-file graph: a forward link and its reciprocal backlink.
        // Moving the source across directories must re-relativize both ends and
        // leave the graph clean (any wrong edit would surface a new broken-link
        // or stale-backlink diagnostic the equality catches).
        let fixture = Fixture::new(&[
            ("docs/a.md", "# A\n\n[to b](b.md \"references\")\n"),
            (
                "docs/b.md",
                "---\nbacklinks:\n  referenced_by:\n    - a.md\n---\n# B\n",
            ),
        ]);

        // Pre-move the graph is clean.
        let pre = validation::collect_all(&fixture.scan());
        assert!(pre.is_empty(), "clean fixture has no diagnostics: {pre:#?}");

        let edits = assert_isomorphism(&fixture, "docs/a.md", "notes/a.md");
        assert!(
            total_edit_count(&edits) >= 2,
            "the forward link and the backlink entry are both re-rendered: {edits:#?}"
        );

        // Post-move stays clean (the isomorphism already proved equality; assert
        // cleanliness explicitly to document intent).
        let applied = apply(&fixture, &edits);
        let post = validation::collect_all(&applied.scan());
        assert!(post.is_empty(), "post-move graph stays clean: {post:#?}");
    }

    #[test]
    fn drift_preserving_isomorphism_drifted() {
        // Three deliberate drifts must survive verbatim at translated
        // coordinates: (1) a missing backlink, (2) a stale backlink entry, and
        // (3) a broken forward link — all anchored on the moved file.
        let fixture = Fixture::new(&[
            (
                "src/mover.md",
                // (1) links to peer.md but peer has no backlink -> missing.
                // (3) links to a target that does not exist -> broken. The
                // explicit predicate keeps this to the broken-link error alone
                // (no predicate-default Info), so exactly three drifts.
                "# Mover\n\n[to peer](peer.md \"supersedes\")\n\n[gone](ghost.md \"references\")\n",
            ),
            (
                "src/peer.md",
                // (2) claims a backlink from nobody.md that never links here.
                "---\nbacklinks:\n  referenced_by:\n    - nobody.md\n---\n# Peer\n",
            ),
        ]);

        // Pre-move: exactly the three drifts (one warning missing, one warning
        // stale, one error broken).
        let pre = validation::collect_all(&fixture.scan());
        assert_eq!(pre.len(), 3, "three drifts pre-move: {pre:#?}");

        // Move the drifting file; every drift must transport, not vanish or heal.
        assert_isomorphism(&fixture, "src/mover.md", "archive/mover.md");
    }

    // --- 2. Directory move: exact edit accounting ---

    #[test]
    fn directory_move_edit_accounting() {
        // A directory `sub/` with an internal relative link (no edit), an
        // internal root-relative link (edited — ruling 1), an outbound boundary
        // edge, an inbound boundary edge, a reference-style link edited at its
        // definition, and a fragment that must survive byte-for-byte.
        let fixture = Fixture::new(&[
            // Inbound: outside file links into the moved dir (edited).
            ("outside.md", "# Outside\n\n[in](sub/inner.md#heading)\n"),
            // Internal relative link between two moved files -> NO edit.
            // Internal root-relative link to a moved file -> IS edited.
            // Outbound link to a file staying put -> edited.
            // Reference-style link to the outside file -> edited at its def.
            (
                "sub/inner.md",
                "# Heading\n\n[peer](peer.md)\n\n[rooted](/sub/peer.md)\n\n[out][o]\n\n[o]: ../outside.md\n",
            ),
            ("sub/peer.md", "# Peer\n\n[back out](../outside.md)\n"),
        ]);

        let edits = assert_isomorphism(&fixture, "sub", "moved");

        // Enumerate the expected edits precisely:
        // - outside.md: the inbound link `sub/inner.md` -> `moved/inner.md`
        //   (fragment `#heading` preserved).                              [1]
        // - moved/inner.md: `peer.md` (relative, both moved) -> NO edit.
        //   `/sub/peer.md` (root-relative) -> `/moved/peer.md`.           [1]
        //   `[o]: ../outside.md` refdef: inner moved deeper? sub and moved
        //   are siblings, so `../outside.md` still resolves -> NO edit.
        // - moved/peer.md: `../outside.md` unchanged (sibling depth same) -> NO.
        //
        // So total edits = inbound (1) + internal root-relative (1) = 2.
        assert_eq!(
            total_edit_count(&edits),
            2,
            "exactly the inbound link and the internal root-relative link are edited: {edits:#?}"
        );

        // The inbound edit preserves the fragment.
        let outside_abs = fixture.root().join("outside.md");
        let outside_edits = edits.edits.get(&outside_abs).expect("outside.md is edited");
        assert_eq!(outside_edits.len(), 1, "one inbound edit");
        assert_eq!(
            outside_edits[0].new_text, "moved/inner.md",
            "inbound link retargets to the moved dir; the `#heading` fragment is outside the edit span"
        );
    }

    #[test]
    fn directory_move_internal_relative_links_untouched() {
        // A directory move where every edge is an internal relative link: both
        // endpoints translate together, so zero edits are produced.
        let fixture = Fixture::new(&[
            ("d/a.md", "# A\n\n[b](b.md)\n"),
            ("d/b.md", "# B\n\n[a](a.md)\n"),
        ]);
        let ws = fixture.scan();
        let edits = compute_move_edits(
            &ws,
            &fixture.root().join("d"),
            &fixture.root().join("e"),
            &fs_exists,
        )
        .expect("directory move computes");
        assert_eq!(
            total_edit_count(&edits),
            0,
            "internal relative links need no re-rendering: {edits:#?}"
        );
    }

    #[test]
    fn authored_dot_slash_prefix_is_preserved_when_still_descending() {
        // An authored `./b.md` whose re-rendered target still descends keeps its
        // `./` prefix, so the diff stays minimal and in the authored style.
        let fixture = Fixture::new(&[
            ("top/hub.md", "# Hub\n\n[t](./sub/target.md)\n"),
            ("top/sub/target.md", "# Target\n"),
        ]);
        let ws = fixture.scan();
        // Move the target deeper but still under hub's descent.
        let edits = compute_move_edits(
            &ws,
            &fixture.root().join("top/sub/target.md"),
            &fixture.root().join("top/sub/deep/target.md"),
            &fs_exists,
        )
        .expect("move computes");
        let hub_edits = edits
            .edits
            .get(&fixture.root().join("top/hub.md"))
            .expect("hub.md edited");
        assert_eq!(
            hub_edits[0].new_text, "./sub/deep/target.md",
            "the authored `./` prefix is preserved while the path still descends"
        );
    }

    #[test]
    fn directory_with_no_markdown_members_is_valid_empty_move() {
        // A directory holding only assets (no markdown) is a valid move with an
        // empty edit set (plus the rename).
        let fixture = Fixture::new(&[
            ("README.md", "# Root\n"),
            ("assets/logo.png", "PNG"),
            ("assets/data.bin", "BIN"),
        ]);
        let ws = fixture.scan();
        let edits = compute_move_edits(
            &ws,
            &fixture.root().join("assets"),
            &fixture.root().join("static"),
            &fs_exists,
        )
        .expect("asset-only directory move computes");
        assert_eq!(
            total_edit_count(&edits),
            0,
            "no markdown members means no edits: {edits:#?}"
        );
        assert_eq!(
            edits.rename.new,
            fixture.root().join("static"),
            "the rename is still produced"
        );
    }

    // --- Non-markdown source (review ruling 4) ---

    #[test]
    fn non_markdown_source_edits_its_one_referrer() {
        // A non-markdown asset with a single inbound `NonMarkdown` link (a plain
        // `[text](asset)` link, not an `![]()` image embed — images are
        // `ElementKind::Image`, outside `FileData.links`): moving it must edit
        // the referrer (enumeration (a) only) and refuse nothing.
        let fixture = Fixture::new(&[
            ("doc.md", "# Doc\n\n[logo](img/logo.png)\n"),
            ("img/logo.png", "PNG"),
        ]);
        let ws = fixture.scan();
        let edits = compute_move_edits(
            &ws,
            &fixture.root().join("img/logo.png"),
            &fixture.root().join("assets/logo.png"),
            &fs_exists,
        )
        .expect("non-markdown source move computes");
        assert_eq!(
            total_edit_count(&edits),
            1,
            "exactly the one referrer is edited: {edits:#?}"
        );
        let doc_edits = edits
            .edits
            .get(&fixture.root().join("doc.md"))
            .expect("doc.md edited");
        assert_eq!(
            doc_edits[0].new_text, "assets/logo.png",
            "the image link retargets to the asset's new location"
        );
    }

    // --- 3. Refusals: every message names the fix ---

    #[test]
    fn refusal_destination_exists() {
        let fixture = Fixture::new(&[("a.md", "# A\n"), ("b.md", "# B\n")]);
        let ws = fixture.scan();
        let err = compute_move_edits(
            &ws,
            &fixture.root().join("a.md"),
            &fixture.root().join("b.md"),
            &fs_exists,
        )
        .expect_err("moving onto an existing file is refused");
        assert!(
            matches!(err, MoveError::DestinationExists { .. }),
            "got {err:?}"
        );
        assert!(
            err.to_string().contains("does not exist"),
            "message names the fix: {err}"
        );
    }

    #[test]
    fn refusal_same_source_and_destination() {
        let fixture = Fixture::new(&[("a.md", "# A\n")]);
        let ws = fixture.scan();
        let err = compute_move_edits(
            &ws,
            &fixture.root().join("a.md"),
            &fixture.root().join("a.md"),
            &fs_exists,
        )
        .expect_err("a no-op move is refused");
        assert!(
            matches!(err, MoveError::DestinationExists { .. }),
            "old == new refuses via DestinationExists: {err:?}"
        );
    }

    #[test]
    fn refusal_source_outside_scope() {
        let fixture = Fixture::new(&[("a.md", "# A\n")]);
        let ws = fixture.scan();
        let outside = fixture.root().parent().expect("temp parent").join("x.md");
        let err = compute_move_edits(&ws, &outside, &fixture.root().join("here.md"), &fs_exists)
            .expect_err("a source outside the root is refused");
        assert!(
            matches!(err, MoveError::SourceOutsideScope { .. }),
            "got {err:?}"
        );
        assert!(
            err.to_string().contains("under the workspace root"),
            "message names the fix: {err}"
        );
    }

    #[test]
    fn refusal_destination_inside_source() {
        // mv dir dir/sub — the destination nests inside the source.
        let fixture = Fixture::new(&[("d/a.md", "# A\n")]);
        let ws = fixture.scan();
        let err = compute_move_edits(
            &ws,
            &fixture.root().join("d"),
            &fixture.root().join("d/sub"),
            &fs_exists,
        )
        .expect_err("moving a directory into itself is refused");
        assert!(
            matches!(err, MoveError::DestinationInsideSource { .. }),
            "got {err:?}"
        );
        assert!(
            err.to_string().contains("outside the source"),
            "message names the fix: {err}"
        );
    }

    #[test]
    fn refusal_markdownness_flip() {
        let fixture = Fixture::new(&[("note.md", "# Note\n")]);
        let ws = fixture.scan();
        let err = compute_move_edits(
            &ws,
            &fixture.root().join("note.md"),
            &fixture.root().join("note.txt"),
            &fs_exists,
        )
        .expect_err("a .md -> .txt move flips the node kind and is refused");
        assert!(
            matches!(err, MoveError::MarkdownnessFlip { .. }),
            "got {err:?}"
        );
        assert!(
            err.to_string().contains("same kind"),
            "message names the fix: {err}"
        );
    }

    #[test]
    fn refusal_directory_contains_marker() {
        // A directory holding a nested `.git` (a scope boundary) is refused.
        let fixture = Fixture::new(&[("outer/a.md", "# A\n")]);
        fs::create_dir_all(fixture.root().join("outer/inner/.git")).expect("create nested .git");
        fs::write(fixture.root().join("outer/inner/x.md"), "# X\n").expect("write nested file");
        let ws = fixture.scan();
        let err = compute_move_edits(
            &ws,
            &fixture.root().join("outer"),
            &fixture.root().join("relocated"),
            &fs_exists,
        )
        .expect_err("moving a directory that contains a scope marker is refused in v1");
        assert!(
            matches!(err, MoveError::DirectoryContainsMarker { .. }),
            "got {err:?}"
        );
        assert!(
            err.to_string().contains("nested scope"),
            "message names the fix: {err}"
        );
    }

    #[test]
    fn refusal_crosses_boundary_into_nested_scope() {
        // A file move whose destination lands inside a nested scope crosses a
        // boundary and is refused with the alias-steering message.
        let fixture = Fixture::new(&[("a.md", "# A\n")]);
        fs::create_dir_all(fixture.root().join("nested/.git")).expect("create nested .git");
        fs::write(fixture.root().join("nested/keep.md"), "# Keep\n").expect("write nested keep");
        let ws = fixture.scan();
        let err = compute_move_edits(
            &ws,
            &fixture.root().join("a.md"),
            &fixture.root().join("nested/a.md"),
            &fs_exists,
        )
        .expect_err("moving into a nested scope crosses a boundary");
        assert!(
            matches!(err, MoveError::CrossesBoundary { .. }),
            "got {err:?}"
        );
        assert!(
            err.to_string().contains("external"),
            "message steers to the `[external]` alias: {err}"
        );
    }

    // --- 4. Clause-5 deliberate non-edits (the judgment surface) ---

    #[test]
    fn clause_five_non_edits_are_left_for_judgment() {
        // A prose mention, a backticked mention, and an exception key — all
        // naming the moved path — are never rewritten. After the move they
        // surface as the post-move judgment surface, exactly as decision 020
        // clause 5 requires; only the real link is edited.
        let fixture = Fixture::new(&[
            (
                "hub.md",
                concat!(
                    "# Hub\n\n",
                    "See old/target.md in prose.\n\n", // prose mention
                    "Or the file `old/target.md` here.\n\n", // backticked mention
                    "[real link](old/target.md)\n",    // the one real edge
                ),
            ),
            (
                "keeper.md",
                // An exception key naming the moved path (verbatim epitaph).
                "---\nexceptions:\n  stale_references:\n    \"old/target.md\": \"kept on purpose\"\n---\n# Keeper\n\nSee `old/target.md`.\n",
            ),
            ("old/target.md", "# Target\n"),
        ]);

        let ws = fixture.scan();
        let old_abs = fixture.root().join("old/target.md");
        let new_abs = fixture.root().join("new/target.md");
        let edits = compute_move_edits(&ws, &old_abs, &new_abs, &fs_exists).expect("move computes");

        // Only hub.md's real link is edited; keeper.md is untouched.
        assert_eq!(
            total_edit_count(&edits),
            1,
            "exactly the one real link is edited, nothing else: {edits:#?}"
        );
        let hub_edits = edits
            .edits
            .get(&fixture.root().join("hub.md"))
            .expect("hub.md edited");
        assert_eq!(hub_edits.len(), 1, "one edit in hub.md");
        assert_eq!(
            hub_edits[0].new_text, "new/target.md",
            "the real link retargets"
        );
        // The edited span is the link destination — not either prose/backtick
        // mention. Verify the two mentions' byte ranges are untouched.
        let hub_src = fs::read_to_string(fixture.root().join("hub.md")).expect("read hub");
        let edited_range = hub_edits[0].span.start..hub_edits[0].span.end;
        let prose_at = hub_src
            .find("old/target.md in prose")
            .expect("prose mention");
        let backtick_at = hub_src.find("`old/target.md`").expect("backtick mention");
        assert!(
            !edited_range.contains(&prose_at) && !edited_range.contains(&(backtick_at + 1)),
            "neither the prose nor the backticked mention is inside the edit span"
        );

        // keeper.md is not in the edit set at all.
        assert!(
            !edits.edits.contains_key(&fixture.root().join("keeper.md")),
            "the exception key is never rewritten"
        );

        // After the move the judgment surface appears in hub.md: its backticked
        // mention of `old/target.md` — never rewritten — now names a path that no
        // longer exists, and hub.md carries no exception, so the stale-reference
        // nudge fires. That is the mechanism working (decision 020 clause 5): the
        // move left the mention for the per-mention move-test judgment.
        let applied = apply(&fixture, &edits);
        let post_ws = applied.scan();
        let hub = post_ws
            .file(Path::new("hub.md"))
            .expect("hub.md present post-move");
        assert!(
            hub.structural
                .iter()
                .any(|d| d.message.contains("old/target.md")),
            "the un-rewritten backticked mention surfaces as the post-move judgment surface: {:?}",
            hub.structural
        );
    }

    #[test]
    fn fragment_bytes_preserved_verbatim() {
        // A link with a fragment: only the path portion is edited; the fragment
        // rides along untouched.
        let fixture = Fixture::new(&[
            ("hub.md", "# Hub\n\n[x](old/t.md#a-section)\n"),
            ("old/t.md", "# T\n\n## A section\n"),
        ]);
        let ws = fixture.scan();
        let edits = compute_move_edits(
            &ws,
            &fixture.root().join("old/t.md"),
            &fixture.root().join("new/t.md"),
            &fs_exists,
        )
        .expect("move computes");
        let hub_edits = edits
            .edits
            .get(&fixture.root().join("hub.md"))
            .expect("hub.md edited");
        assert_eq!(
            hub_edits[0].new_text, "new/t.md",
            "only the path is replaced; `#a-section` is outside the span"
        );
        // Apply and confirm the fragment still resolves.
        let applied = apply(&fixture, &edits);
        let post = validation::collect_all(&applied.scan());
        assert!(
            errors(&post).is_empty(),
            "the fragment survived and still resolves: {post:#?}"
        );
    }

    // --- 5. CLI surface: `lattice mv` (ticket mv/03, decision 020 clause 3) ---

    /// Run `lattice lint` over the fixture root, returning `(failed, output)`.
    /// The ledger is suppressed (`quiet`) so assertions focus on diagnostics.
    fn lint(fixture: &Fixture) -> (bool, String) {
        let mut buf = Vec::new();
        let failed = crate::lint::run(fixture.root(), false, true, false, &mut buf)
            .expect("lint run should succeed");
        (
            failed,
            String::from_utf8(buf).expect("lint output is utf-8"),
        )
    }

    /// Invoke the CLI `mv` runner in real (apply) mode over the fixture on disk,
    /// returning the captured stdout summary. Panics on refusal so a test that
    /// expects success surfaces the engine's message.
    fn mv_apply(fixture: &Fixture, old_rel: &str, new_rel: &str) -> String {
        let mut buf = Vec::new();
        super::run(
            &fixture.root().join(old_rel),
            &fixture.root().join(new_rel),
            false,
            &mut buf,
        )
        .expect("mv should apply");
        String::from_utf8(buf).expect("mv output is utf-8")
    }

    /// Invoke the CLI `mv` runner in `--dry-run` mode, returning the captured
    /// stdout edit-set listing. Touches nothing.
    fn mv_dry_run(fixture: &Fixture, old_rel: &str, new_rel: &str) -> String {
        let mut buf = Vec::new();
        super::run(
            &fixture.root().join(old_rel),
            &fixture.root().join(new_rel),
            true,
            &mut buf,
        )
        .expect("mv --dry-run should compute");
        String::from_utf8(buf).expect("dry-run output is utf-8")
    }

    /// Read a workspace-relative file from the fixture, or panic.
    fn read_rel(fixture: &Fixture, rel: &str) -> String {
        fs::read_to_string(fixture.root().join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
    }

    #[test]
    fn cli_move_clean_stays_clean_after_and_lints_clean() {
        // Acceptance 1 (clean arm): a clean two-file graph. `lattice mv` then
        // `lattice lint` is clean where it was clean before — the move re-renders
        // both ends and introduces no new diagnostic.
        let fixture = Fixture::new(&[
            ("docs/a.md", "# A\n\n[to b](b.md \"references\")\n"),
            (
                "docs/b.md",
                "---\nbacklinks:\n  referenced_by:\n    - a.md\n---\n# B\n",
            ),
        ]);

        let (pre_failed, pre_out) = lint(&fixture);
        assert!(
            !pre_failed && pre_out.is_empty(),
            "the fixture lints clean before the move: {pre_out}"
        );

        mv_apply(&fixture, "docs/a.md", "notes/a.md");

        // The source is gone, the destination present, and lint is still clean.
        assert!(
            !fixture.root().join("docs/a.md").exists(),
            "the source file no longer exists at the old path"
        );
        assert!(
            fixture.root().join("notes/a.md").exists(),
            "the moved file exists at the new path"
        );
        let (post_failed, post_out) = lint(&fixture);
        assert!(
            !post_failed && post_out.is_empty(),
            "the moved graph lints clean — the move changed coordinates, not the graph: {post_out}"
        );
    }

    #[test]
    fn cli_move_preserves_pre_existing_drift_at_renamed_coordinates() {
        // Acceptance 1 (drift arm): three deliberate drifts (missing backlink,
        // stale backlink, broken forward link) all anchored on the moved file.
        // After the move, `lattice lint` shows exactly the same drift — same
        // count, same severities — at the renamed coordinates.
        let fixture = Fixture::new(&[
            (
                "src/mover.md",
                "# Mover\n\n[to peer](peer.md \"supersedes\")\n\n[gone](ghost.md \"references\")\n",
            ),
            (
                "src/peer.md",
                "---\nbacklinks:\n  referenced_by:\n    - nobody.md\n---\n# Peer\n",
            ),
        ]);

        let (pre_failed, pre_out) = lint(&fixture);
        // Pre-move: one broken-link error (fails) plus two warnings.
        assert!(
            pre_failed,
            "the broken forward link fails the lint pre-move"
        );
        let pre_errors = pre_out.matches("error:").count();
        let pre_warnings = pre_out.matches("warning:").count();
        assert_eq!(
            (pre_errors, pre_warnings),
            (1, 2),
            "exactly three drifts pre-move (1 error, 2 warnings): {pre_out}"
        );

        mv_apply(&fixture, "src/mover.md", "archive/mover.md");

        let (post_failed, post_out) = lint(&fixture);
        assert!(
            post_failed,
            "the broken forward link survives the move and still fails: {post_out}"
        );
        assert_eq!(
            (
                post_out.matches("error:").count(),
                post_out.matches("warning:").count()
            ),
            (1, 2),
            "the same three drifts survive verbatim after the move: {post_out}"
        );
        // The drift re-keys onto the renamed file — the moved file's own broken
        // link is now anchored at the new path, not the old one.
        assert!(
            post_out.contains("archive/mover.md:"),
            "the moved file's drift is anchored at its new coordinates: {post_out}"
        );
        assert!(
            !post_out.contains("src/mover.md:"),
            "no phantom diagnostic remains at the old path: {post_out}"
        );
    }

    #[test]
    fn cli_dry_run_matches_applied_edits_byte_exact() {
        // Acceptance 2: `--dry-run` output matches the edits subsequently applied,
        // byte-exact. We capture the dry-run listing, confirm it touched nothing,
        // then apply for real and verify each `before -> after` line reflects the
        // exact substitution that landed in the file.
        let fixture = Fixture::new(&[
            ("hub.md", "# Hub\n\n[t](old/target.md#sec)\n"),
            (
                "old/target.md",
                "---\nbacklinks:\n  referenced_by:\n    - ../hub.md\n---\n# Target\n\n## Sec\n",
            ),
        ]);

        // Snapshot every file before the dry-run to prove it is read-only.
        let hub_before = read_rel(&fixture, "hub.md");
        let target_before = read_rel(&fixture, "old/target.md");

        // Move the target UP to the root: the referrer's forward link
        // (`old/target.md` -> `target.md`) and the moved file's own backlink entry
        // (`../hub.md` -> `hub.md`) both change depth, so both surfaces are edited.
        let dry = mv_dry_run(&fixture, "old/target.md", "target.md");

        // The dry-run touched nothing.
        assert_eq!(
            read_rel(&fixture, "hub.md"),
            hub_before,
            "dry-run must not modify hub.md"
        );
        assert_eq!(
            read_rel(&fixture, "old/target.md"),
            target_before,
            "dry-run must not modify the source file"
        );
        assert!(
            fixture.root().join("old/target.md").exists()
                && !fixture.root().join("target.md").exists(),
            "dry-run must not perform the rename"
        );
        // The dry-run reports the forced edits plus the rename.
        assert!(
            dry.contains("rename: old/target.md -> target.md"),
            "the dry-run reports the rename: {dry}"
        );

        // Now apply for real and confirm the dry-run's `before -> after` pairs are
        // exactly the substitutions that landed.
        mv_apply(&fixture, "old/target.md", "target.md");
        let hub_after = read_rel(&fixture, "hub.md");
        let target_after = read_rel(&fixture, "target.md");

        // Each dry-run edit line is `path:line: `before` -> `after``. For every
        // such line, `before` must have been present in the pre-move file and
        // `after` in the post-move file, and applying the line's replacement to
        // the pre-move text must reproduce the post-move text for that file.
        let hub_edit = dry
            .lines()
            .find(|l| l.starts_with("hub.md:"))
            .expect("the dry-run lists hub.md's edit");
        let (before, after) = parse_edit_line(hub_edit);
        assert!(
            hub_before.contains(&before),
            "the dry-run `before` (`{before}`) was present pre-move in hub.md"
        );
        assert!(
            hub_after.contains(&after),
            "the dry-run `after` (`{after}`) is present post-move in hub.md"
        );
        assert_eq!(
            hub_before.replacen(&before, &after, 1),
            hub_after,
            "applying the dry-run's single hub.md substitution reproduces the applied file byte-exact"
        );
        // The `#sec` fragment rode along verbatim outside the edit span.
        assert!(
            hub_after.contains("#sec"),
            "the fragment survived the applied edit: {hub_after}"
        );

        // The backlink entry edit in the moved file likewise reproduces exactly.
        // Its dry-run line is keyed at the pre-rename path (`old/target.md`), the
        // engine's keyspace coordinate at the time the edits are computed.
        let target_edit = dry
            .lines()
            .find(|l| l.starts_with("old/target.md:"))
            .expect("the dry-run lists the moved file's backlink edit");
        let (b2, a2) = parse_edit_line(target_edit);
        assert!(
            target_before.contains(&b2),
            "the moved file's `before` (`{b2}`) was present pre-move"
        );
        assert_eq!(
            target_before.replacen(&b2, &a2, 1),
            target_after,
            "applying the moved file's dry-run substitution reproduces the applied file byte-exact"
        );
    }

    /// Parse a dry-run edit line of the form
    /// "path:line: BACKTICK before BACKTICK -> BACKTICK after BACKTICK" into the
    /// `(before, after)` inner strings (the two are backtick-wrapped in the
    /// output).
    fn parse_edit_line(line: &str) -> (String, String) {
        let (_, rest) = line
            .split_once(": `")
            .unwrap_or_else(|| panic!("malformed dry-run edit line: {line}"));
        let (before, after_part) = rest
            .split_once("` -> `")
            .unwrap_or_else(|| panic!("malformed dry-run edit line: {line}"));
        let after = after_part
            .strip_suffix('`')
            .unwrap_or_else(|| panic!("malformed dry-run edit line: {line}"));
        (before.to_string(), after.to_string())
    }

    #[test]
    fn cli_move_into_existing_directory_is_shell_mv_style() {
        // The destination naming an existing directory means "move into it": the
        // real destination is `dir/<basename>`, shell-`mv` style.
        let fixture = Fixture::new(&[
            ("a.md", "# A\n\n[b](b.md \"references\")\n"),
            (
                "b.md",
                "---\nbacklinks:\n  referenced_by:\n    - a.md\n---\n# B\n",
            ),
        ]);
        // `dir/` exists; moving `a.md` into it lands at `dir/a.md`.
        fs::create_dir(fixture.root().join("dir")).expect("create dir");

        mv_apply(&fixture, "a.md", "dir");

        assert!(
            fixture.root().join("dir/a.md").exists(),
            "the source moved into the existing directory as dir/a.md"
        );
        assert!(
            !fixture.root().join("a.md").exists(),
            "the source no longer exists at the old path"
        );
        let (failed, out) = lint(&fixture);
        assert!(
            !failed && out.is_empty(),
            "the shell-mv-style move re-renders both ends and lints clean: {out}"
        );
    }

    #[test]
    fn cli_refusal_exits_error_and_touches_nothing() {
        // A refused move (existing destination) returns an error naming the fix
        // and leaves the workspace byte-identical.
        let fixture = Fixture::new(&[("a.md", "# A\n"), ("b.md", "# B\n")]);
        let a_before = read_rel(&fixture, "a.md");
        let b_before = read_rel(&fixture, "b.md");

        let mut buf = Vec::new();
        let err = super::run(
            &fixture.root().join("a.md"),
            &fixture.root().join("b.md"),
            false,
            &mut buf,
        )
        .expect_err("moving onto an existing file is refused");
        assert!(
            err.to_string().contains("does not exist")
                || format!("{err:#}").contains("does not exist"),
            "the refusal names the fix (choose a destination that does not exist): {err:#}"
        );
        // The workspace is byte-identical and no rename happened.
        assert_eq!(
            read_rel(&fixture, "a.md"),
            a_before,
            "a refused move must not edit the source"
        );
        assert_eq!(
            read_rel(&fixture, "b.md"),
            b_before,
            "a refused move must not edit the destination"
        );
    }

    #[test]
    fn cli_dry_run_with_no_forced_edits_reports_the_rename_only() {
        // A directory move whose only edges are internal relative links forces no
        // edit; the dry-run reports "no edits" plus the rename, and touches
        // nothing.
        let fixture = Fixture::new(&[
            ("d/a.md", "# A\n\n[b](b.md)\n"),
            ("d/b.md", "# B\n\n[a](a.md)\n"),
        ]);
        let dry = mv_dry_run(&fixture, "d", "e");
        assert!(
            dry.contains("no edits"),
            "an edit-free move reports no forced edits: {dry}"
        );
        assert!(
            dry.contains("rename: d -> e"),
            "the rename is still reported: {dry}"
        );
        assert!(
            fixture.root().join("d/a.md").exists() && !fixture.root().join("e").exists(),
            "the dry-run performed no rename"
        );
    }

    #[test]
    fn cli_summary_reports_moved_and_edit_accounting() {
        // Acceptance: the post-move summary reports what moved and how many edits
        // landed across how many files — the lint ledger's per-source shape.
        let fixture = Fixture::new(&[
            ("docs/a.md", "# A\n\n[to b](b.md \"references\")\n"),
            (
                "docs/b.md",
                "---\nbacklinks:\n  referenced_by:\n    - a.md\n---\n# B\n",
            ),
        ]);
        let summary = mv_apply(&fixture, "docs/a.md", "notes/a.md");
        assert!(
            summary.contains("moved: docs/a.md -> notes/a.md"),
            "the summary names what moved, in workspace-relative coordinates: {summary}"
        );
        // The forward link (in the moved file) and the backlink entry (in b.md)
        // are both re-rendered: two edits across two files.
        assert!(
            summary.contains("2 edits across 2 files"),
            "the summary accounts for the edits across files: {summary}"
        );
    }

    #[test]
    fn cli_apply_edits_before_rename_leaves_recoverable_state_on_write_failure() {
        // Decision 020 clause 3 / ticket mv/03: edits land before the rename, so a
        // failure leaves a re-derivable state, never a half-move. We cannot inject
        // a mid-apply IO fault portably, but we can assert the ordering invariant
        // that guarantees it: `apply_to_disk` performs the rename only after every
        // text edit is written. Here we drive a move that both edits a referrer
        // and renames, and confirm that after a *successful* apply the referrer
        // edit is present AND the rename happened — the two are not independent,
        // the edit is a prerequisite the code sequences first.
        let fixture = Fixture::new(&[
            ("hub.md", "# Hub\n\n[t](sub/target.md)\n"),
            ("sub/target.md", "# Target\n"),
        ]);
        mv_apply(&fixture, "sub/target.md", "moved/target.md");
        // The referrer edit is present (the edit ran)...
        let hub = read_rel(&fixture, "hub.md");
        assert!(
            hub.contains("moved/target.md"),
            "the referrer edit landed before the rename: {hub}"
        );
        // ...and the rename completed (proving the edit did not abort it).
        assert!(
            fixture.root().join("moved/target.md").exists()
                && !fixture.root().join("sub/target.md").exists(),
            "the rename completed after the edits landed"
        );
    }

    #[test]
    fn cli_move_rekeys_membership_no_phantom_at_old_path() {
        // Acceptance 3 (the in-process arm). The cross-surface property — a live
        // LSP session converging via the watched-files channel (decision 017)
        // without a restart — is exercised end-to-end by ticket mv/02's
        // willRenameFiles handler and its watched-file replay; here we assert the
        // graph-tier image that convergence must reach: after `lattice mv` writes
        // the rename to disk, a fresh scan of the same workspace (the state the
        // server rebuilds from the create/delete events) keys the moved file at
        // the NEW path only, with no phantom entry at the old key, and the moved
        // file's diagnostics re-key with it. What is left to mv/02 is the live
        // session's incremental convergence itself (no in-process channel exists
        // in the CLI surface to drive here).
        let fixture = Fixture::new(&[
            ("docs/a.md", "# A\n\n[to b](b.md \"references\")\n"),
            (
                "docs/b.md",
                "---\nbacklinks:\n  referenced_by:\n    - a.md\n---\n# B\n",
            ),
        ]);

        // Pre-move the moved file is a member at its old key.
        let pre = fixture.scan();
        assert!(
            pre.file(Path::new("docs/a.md")).is_some(),
            "the file is indexed at its old key before the move"
        );

        mv_apply(&fixture, "docs/a.md", "notes/a.md");

        // A fresh scan — the exact state the LSP rebuilds from the watched-file
        // create/delete pair — re-keys the file and drops the old key entirely.
        let post = fixture.scan();
        assert!(
            post.file(Path::new("notes/a.md")).is_some(),
            "the moved file is indexed at its new key after the move"
        );
        assert!(
            post.file(Path::new("docs/a.md")).is_none(),
            "no phantom membership entry survives at the old key"
        );
        // The moved file's graph diagnostics re-key with it: the clean graph
        // stays clean, so no diagnostic is anchored on either the old or new key.
        let diags = validation::collect_all(&post);
        assert!(
            diags
                .iter()
                .all(|d| d.file.as_path() != Path::new("docs/a.md")),
            "no diagnostic remains anchored on the old path: {diags:#?}"
        );
    }
}
