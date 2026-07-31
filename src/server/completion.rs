// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! `textDocument/completion` — the server side of decision 007.
//!
//! The trigger character is ignored: the surface under the cursor and the
//! partial word being typed are both recovered from the line prefix by
//! [`crate::completion`], so a client that completes on demand and one that
//! completes on every keystroke get the same answer. This module is the glue —
//! it detects the site, refuses one inside a code span, block, or math node,
//! and turns each surface (path, fragment, predicate, reference label,
//! footnote) into candidates against the **current** view.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::block::{ElementKind, Heading, HeadingId, Tree, normalize_label};
use crate::completion::Context as CompletionContext;
use crate::config::{Config, FragmentAlgorithm};
use crate::lsp;
use crate::workspace::WorkspaceView;

use super::helpers::{byte_offset_to_lsp_position, line_byte_range, lsp_position_to_byte_offset};
use super::workspaces::Workspaces;

// ---------------------------------------------------------------------------
// Completion (decision 007, ticket integration 14)
// ---------------------------------------------------------------------------

/// Build completion candidates for the construct under the cursor.
///
/// Returns `None` when the cursor is not in a completion site (prose) or sits
/// inside a code span, code block, or math node. Otherwise returns the
/// candidate list for the detected surface — possibly empty (e.g. a fragment
/// against a target that is not yet a resolvable file).
pub fn completion(
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
