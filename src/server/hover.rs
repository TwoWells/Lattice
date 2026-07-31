// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! `textDocument/hover` — a preview of what a link points at.
//!
//! Reads the **current** view (decision 024 clause 9): a hover answers about
//! the text on screen, so it resolves through the buffer where one exists and
//! the saved copy everywhere else. Embeds are skipped rather than
//! matched-and-rejected — an embed asserts no relation, so it has no hover to
//! show, and letting one win the line would hide a real link's.

use crate::block::{LinkKind, content_lines};
use crate::lsp;
use crate::workspace::target_to_key;

use super::navigation::heading_matches_fragment;
use super::workspaces::Workspaces;

// ---------------------------------------------------------------------------
// Hover preview (ticket 10)
// ---------------------------------------------------------------------------

/// Show a preview of the link target on hover.
#[allow(
    clippy::cast_possible_truncation,
    reason = "line numbers in markdown files won't exceed u32::MAX"
)]
pub fn hover_preview(
    workspaces: &Workspaces,
    params: &lsp::TextDocumentPositionParams,
) -> Option<lsp::Hover> {
    let (workspace, rel_path) = workspaces.resolve_document(&params.text_document.uri)?;
    let file_data = workspace.file(&rel_path)?;
    let root = workspace.root();
    let file_links = file_data.tree.links(&root.join(&rel_path));

    // Find the link on the cursor's line. Embeds are skipped rather than
    // matched-and-rejected: an embed asserts no relation, so it has no hover to
    // show, and letting one win the line would hide the hover of a real link
    // sharing that line (issue 058).
    let cursor_line = params.position.line;
    let link = file_links.iter().find(|l| {
        l.line.saturating_sub(1) as u32 == cursor_line && !matches!(l.kind, LinkKind::Embed { .. })
    })?;

    let (target, fragment, predicate) = match &link.kind {
        LinkKind::IntraProject {
            target,
            fragment,
            predicate,
            ..
        } => (target.clone(), fragment.clone(), predicate.as_str()),
        LinkKind::NonMarkdown { target } => (target.clone(), None, "references"),
        // No hover for external or intra-document links, nor for embeds (which
        // the candidate filter above already excluded).
        LinkKind::External { .. } | LinkKind::IntraDocument { .. } | LinkKind::Embed { .. } => {
            return None;
        }
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
