// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Navigation: references, the four go-to surfaces, both hierarchies, and
//! document links.
//!
//! One family, because they all answer the same shape of question — *what is
//! related to the thing under the cursor, and where does it live* — off the
//! same two structures: the link graph a document's tree exposes, and the
//! backlink frontmatter validation reconciles against it. Declaration finds a
//! reference definition, definition follows a link (heading-precise where a
//! fragment names one), type definition and implementation walk the backlink
//! predicates, and the two hierarchies climb heading levels and link edges
//! respectively.
//!
//! `DocumentLink` is deliberately file-granularity: its `target` is a bare URI
//! with no position field, so cross-file links drop their fragment and
//! same-document anchors are skipped entirely. Heading-precise navigation is
//! go-to-definition's job instead.
//!
//! Every surface here reads the **current** view (decision 024 clause 9): a
//! position-bearing answer must be anchored in the text on screen.

use std::path::Path;

use crate::block::{ElementKind, Heading, HeadingId, LinkKind};
use crate::lsp;
use crate::uri::{path_to_uri, uri_to_path};
use crate::validation;
use crate::workspace::{FileData, WorkspaceView, target_to_key};

use super::helpers::{
    enclosing_heading, file_hierarchy_item, find_classified_link, heading_at_line,
    heading_index_at_line, heading_to_hierarchy_item, hierarchy_item_level, link_ref_label,
    lsp_position_to_byte_offset, ref_def_label_at_offset, source_line_at, span_to_lsp_range,
};
use super::workspaces::Workspaces;

// ---------------------------------------------------------------------------
// Find references (ticket 05)
// ---------------------------------------------------------------------------

/// Find all documents that link to the file or heading at the cursor,
/// or all call sites of a reference definition.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
pub fn find_references(
    workspaces: &Workspaces,
    params: &lsp::ReferenceParams,
) -> Vec<lsp::Location> {
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

    // Determine if the cursor is on a heading (to filter by fragment). The
    // cached extractions are the ones the rename engine resolves against, so
    // both surfaces answer over the same heading and anchor lists.
    let target_heading = heading_index_at_line(&file_data.headings, params.position.line);
    let algorithm = workspace.config().policy.fragments;

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
    //
    // A read surface reads **current** text (decision 024 clause 9's audit):
    // the reference list must name the link the user can see, and a location it
    // returns is resolved by the client against the buffer it holds. Whether
    // `find_references` should instead answer over the saved graph — so the
    // answer matches the committed world — is issue 072's question, left open
    // deliberately rather than settled here.
    let cursor_abs = uri_to_path(&params.text_document.uri);
    let cursor_root = workspaces.store.primary_root(&cursor_abs);
    for (abs, doc) in workspaces.store.current_documents() {
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
            // If cursor is on a heading, only match links whose fragment names
            // *that* heading. Resolution is the shared authority's (issue 072):
            // the same eligible slug forms the fragment diagnostic validates
            // against under the same `[policy] fragments` pin, the same
            // first-match-in-document-order heading identity the rename engine
            // retargets by, and the same explicit raw-HTML anchors — so a
            // spelling the configured renderer would not resolve is not listed
            // as a live edge, and an id an author pinned with `<a id>` is.
            if let Some(heading_index) = target_heading {
                let Some(frag) = fragment else {
                    continue;
                };
                if !crate::fragment::names_heading(
                    &file_data.headings,
                    &file_data.anchors,
                    file_data.tree.source(),
                    algorithm,
                    frag,
                    heading_index,
                ) {
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
///
/// The **lenient navigation** matcher: it accepts any of the three computed
/// conventions regardless of `[policy] fragments`, so jumping to a heading and
/// previewing it still work on a fragment the configured renderer would not
/// resolve — following a link physically, as plain navigation does. It is
/// deliberately *not* the resolver the graph surfaces use: whether a fragment
/// is a live edge is [`crate::fragment`]'s answer, shared by the fragment
/// diagnostic, the heading-rename engine, and `find_references` (issue 072).
pub fn heading_matches_fragment(heading: &Heading, fragment: &str) -> bool {
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
pub fn go_to_declaration(
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
pub fn go_to_definition(
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
        LinkKind::IntraProject { target, .. }
        | LinkKind::NonMarkdown { target }
        | LinkKind::Embed { target } => {
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
pub fn go_to_type_definition(
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
pub fn go_to_implementation(
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
pub fn prepare_type_hierarchy(
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
pub fn type_hierarchy_supertypes(
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
pub fn type_hierarchy_subtypes(
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
pub fn prepare_call_hierarchy(
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
pub fn call_hierarchy_incoming(
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
    let cursor_root = workspaces.store.primary_root(&cursor_abs);
    for (abs, doc) in workspaces.store.current_documents() {
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
pub fn call_hierarchy_outgoing(
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
pub fn document_links(workspaces: &Workspaces, uri: &str) -> Vec<lsp::DocumentLink> {
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
            LinkKind::IntraProject { target, .. }
            | LinkKind::NonMarkdown { target }
            | LinkKind::Embed { target } => path_to_uri(&root.join(target)),
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
