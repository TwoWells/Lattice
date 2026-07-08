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
// ticket mv/01; its two callers arrive in mv/02 (`workspace/willRenameFiles`)
// and mv/03 (`lattice mv`). Until then the public entry point and its helpers
// are exercised only by this module's own tests, so the non-test build sees
// them as unused — a deliberate, ticket-scoped state, not dead code.
#![allow(
    dead_code,
    reason = "move-engine surface consumed by tickets mv/02 and mv/03; landed complete here"
)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::block::{self, LinkKind};
use crate::fm::{self, FmNode, FmValue};
use crate::span::Span;
use crate::validation;
use crate::workspace::WorkspaceLike;

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
}
