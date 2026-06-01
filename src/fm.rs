// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Format-neutral frontmatter types and helpers.
//!
//! Defines the shared types consumed by tree construction, backlink
//! extraction, and predicate validation. The YAML, TOML, and JSON
//! frontmatter parsers all produce these types.

use crate::span::Span;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Parsed frontmatter block with span information.
#[derive(Debug)]
pub struct FrontmatterBlock {
    /// Full range including delimiters (`---`, `+++`, or `{`...`}`).
    pub span: Span,
    /// Content between delimiters.
    #[allow(dead_code, reason = "used by tree construction ticket 06a")]
    pub content_span: Span,
    /// Top-level entries.
    pub entries: Vec<FmNode>,
    /// Parse diagnostics (errors and warnings).
    pub diagnostics: Vec<FmDiagnostic>,
}

/// A node in the frontmatter tree.
#[derive(Debug)]
pub enum FmNode {
    /// A key-value mapping entry.
    Mapping {
        /// The mapping key.
        key: ScalarSpan,
        /// The mapping value.
        value: FmValue,
        /// Span covering the full key-value pair.
        span: Span,
    },
    /// A sequence item (`- value` in YAML, array element in TOML).
    SequenceItem {
        /// The item value.
        value: FmValue,
        /// Span covering the item.
        #[allow(dead_code, reason = "used by tree construction ticket 06a")]
        span: Span,
    },
}

/// A frontmatter value.
#[derive(Debug)]
pub enum FmValue {
    /// A scalar value (plain, quoted, or null).
    Scalar(ScalarSpan),
    /// A block sequence (list of `FmNode::SequenceItem`).
    Sequence(Vec<FmNode>),
    /// A block mapping (list of `FmNode::Mapping`).
    Mapping(Vec<FmNode>),
    /// An inline flow sequence (`[a, b, c]`).
    FlowSequence {
        /// Span of the entire flow sequence including brackets.
        #[allow(dead_code, reason = "used by tree construction ticket 06a")]
        span: Span,
        /// Scalar items.
        items: Vec<ScalarSpan>,
    },
    /// An inline flow mapping (`{a: b, c: d}`).
    FlowMapping {
        /// Span of the entire flow mapping including braces.
        #[allow(dead_code, reason = "used by tree construction ticket 06a")]
        span: Span,
        /// Key-value pairs.
        #[allow(dead_code, reason = "used by tree construction ticket 06a")]
        entries: Vec<(ScalarSpan, ScalarSpan)>,
    },
    /// A block scalar (literal `|` or folded `>`).
    BlockScalar {
        /// Span of the entire block scalar content.
        #[allow(dead_code, reason = "used by tree construction ticket 06a")]
        span: Span,
    },
}

/// A scalar with its source span and resolved text.
#[derive(Debug)]
pub struct ScalarSpan {
    /// Byte range in the original source.
    pub span: Span,
    /// Resolved text content.
    pub text: String,
}

/// Severity of a frontmatter diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FmSeverity {
    /// A hard parse error.
    Error,
    /// A warning (e.g. unsupported feature that was skipped).
    #[allow(dead_code, reason = "used by structural diagnostics ticket 07")]
    Warning,
}

/// A diagnostic emitted during frontmatter parsing.
#[derive(Debug)]
pub struct FmDiagnostic {
    /// Location in the source.
    pub span: Span,
    /// Severity level.
    pub severity: FmSeverity,
    /// Human-readable message.
    pub message: String,
}

// ---------------------------------------------------------------------------
// BOM stripping
// ---------------------------------------------------------------------------

/// UTF-8 byte order mark.
pub const BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

/// Strip a UTF-8 BOM at byte 0, returning the remainder and the byte offset.
pub fn strip_bom(source: &str) -> (&str, usize) {
    if source.as_bytes().starts_with(BOM) {
        (&source[3..], 3)
    } else {
        (source, 0)
    }
}

// ---------------------------------------------------------------------------
// Backlink extraction helper
// ---------------------------------------------------------------------------

/// Extract backlinks from a parsed frontmatter block.
///
/// Walks the tree looking for a top-level `backlinks` key whose value
/// is a mapping of predicate → list of paths. Returns the backlinks map
/// and any entries that don't match the expected shape.
pub fn extract_backlinks(
    block: &FrontmatterBlock,
    source: &str,
) -> std::collections::HashMap<String, Vec<String>> {
    let mut backlinks = std::collections::HashMap::new();

    for entry in &block.entries {
        if let FmNode::Mapping { key, value, .. } = entry {
            if key.text != "backlinks" {
                continue;
            }

            let FmValue::Mapping(predicates) = value else {
                break;
            };

            for pred_entry in predicates {
                let FmNode::Mapping {
                    key: pred_key,
                    value: pred_value,
                    ..
                } = pred_entry
                else {
                    continue;
                };

                let mut paths = Vec::new();

                match pred_value {
                    FmValue::Sequence(items) => {
                        for item in items {
                            if let FmNode::SequenceItem {
                                value: FmValue::Scalar(s),
                                ..
                            } = item
                            {
                                paths.push(s.text.clone());
                            }
                        }
                    }
                    FmValue::FlowSequence { items, .. } => {
                        for item in items {
                            paths.push(item.text.clone());
                        }
                    }
                    _ => {}
                }

                backlinks.insert(pred_key.text.clone(), paths);
            }

            break;
        }
    }

    let _ = source; // reserved for future span-based extraction
    backlinks
}

/// Find the 1-based line number for a top-level key in the frontmatter.
///
/// Searches for the `backlinks` → predicate key and returns its line
/// number in the original source.
pub fn find_predicate_line(block: &FrontmatterBlock, predicate: &str, source: &str) -> usize {
    for entry in &block.entries {
        if let FmNode::Mapping { key, value, .. } = entry {
            if key.text != "backlinks" {
                continue;
            }

            let FmValue::Mapping(predicates) = value else {
                break;
            };

            for pred_entry in predicates {
                if let FmNode::Mapping { key: pred_key, .. } = pred_entry
                    && pred_key.text == predicate
                {
                    return byte_offset_to_line(source, pred_key.span.start);
                }
            }
        }
    }

    // Fallback: line 1 (the opening delimiter).
    1
}

/// Convert a byte offset to a 1-based line number.
pub fn byte_offset_to_line(source: &str, offset: usize) -> usize {
    source[..offset.min(source.len())]
        .bytes()
        .filter(|&b| b == b'\n')
        .count()
        + 1
}
