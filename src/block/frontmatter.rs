// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Frontmatter-entry expansion: projecting parsed frontmatter into tree nodes.
//!
//! The frontmatter itself is parsed by [`crate::fm`] (and the format-specific
//! [`crate::yaml`] / [`crate::toml`] / [`crate::json`] readers). This module is
//! the one place its result becomes *tree* — a `FrontmatterKey` node per
//! top-level key and a `FrontmatterMap` node per nested mapping, recursively —
//! so the document-symbol surface can show frontmatter structure without
//! knowing anything about the frontmatter grammar.

use super::parser::TreeBuilder;
use super::{ElementKind, NodeId, Syntax};
// ---------------------------------------------------------------------------
// Frontmatter tree expansion
// ---------------------------------------------------------------------------

/// Expand frontmatter entries into `FrontmatterKey` and `FrontmatterMap` child nodes.
pub fn expand_frontmatter_entries(
    builder: &mut TreeBuilder<'_>,
    parent_id: NodeId,
    syntax: Syntax,
    entries: &[crate::fm::FmNode],
) {
    for entry in entries {
        let crate::fm::FmNode::Mapping { key, value, span } = entry else {
            continue;
        };

        match value {
            crate::fm::FmValue::Mapping(children) => {
                let map_id = builder.add_node(
                    ElementKind::FrontmatterMap {
                        key: key.text.clone(),
                    },
                    syntax,
                    *span,
                    Some(parent_id),
                );
                expand_frontmatter_entries(builder, map_id, syntax, children);
            }
            _ => {
                builder.add_node(
                    ElementKind::FrontmatterKey {
                        key: key.text.clone(),
                        leaf_count: fm_leaf_count(value),
                    },
                    syntax,
                    *span,
                    Some(parent_id),
                );
            }
        }
    }
}

/// Count the number of leaf items in a frontmatter value.
///
/// Block sequences and flow sequences return their item count.
/// Scalars and other values return 0 (no list structure).
fn fm_leaf_count(value: &crate::fm::FmValue) -> usize {
    match value {
        crate::fm::FmValue::Sequence(items) => items.len(),
        crate::fm::FmValue::FlowSequence { items, .. } => items.len(),
        _ => 0,
    }
}
