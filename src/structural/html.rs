// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Raw-HTML diagnostics.
//!
//! Markdown documents carry HTML, and HTML has its own well-formedness rules
//! that the block parser records but does not judge. This module does the
//! judging: markdown left inside an opaque HTML block (where no renderer will
//! process it), required attributes missing from a tag that needs them, a block
//! element nested inside an inline one, and a child under a parent that cannot
//! hold it.

use std::collections::HashMap;
use std::path::Path;

use crate::block::{self, ElementKind, Syntax, Tree};
use crate::html;
use crate::validation::{Diagnostic, Severity};

// ---------------------------------------------------------------------------
// HTML diagnostics
// ---------------------------------------------------------------------------

/// Emit HTML-specific diagnostics from tree structure.
pub fn emit_html_diagnostics(tree: &Tree, rel_path: &Path, out: &mut Vec<Diagnostic>) {
    let source = tree.source();
    let mut seen_ids: HashMap<String, usize> = HashMap::new();

    for node in tree.nodes() {
        // Check both structural HTML nodes (Syntax::Html) and opaque HTML blocks.
        let is_html_node = node.syntax == Syntax::Html;
        let is_html_block = matches!(node.kind, ElementKind::HtmlBlock);
        if !is_html_node && !is_html_block {
            continue;
        }

        let raw = &source[node.span.start..node.span.end];
        let line = block::byte_offset_to_line(source, node.span.start);

        // For HtmlBlock, try the first line's tag.
        let first_line = if is_html_block {
            raw.lines().next().unwrap_or("").trim()
        } else {
            raw.trim()
        };
        let Some(tag) = html::tokenize_tag(first_line, node.span.start) else {
            continue;
        };

        match tag {
            html::HtmlTag::Open {
                ref name,
                ref attrs,
                self_closing,
                ..
            } => {
                if self_closing && !html::VOID_ELEMENTS.contains(name.as_str()) {
                    out.push(Diagnostic {
                        file: rel_path.to_path_buf(),
                        line,
                        severity: Severity::Warning,
                        message: format!("self-closing non-void tag `<{name}/>`"),
                        span: Some(node.span),
                    });
                }

                if !html::ALL_ELEMENTS.contains(name.as_str()) {
                    out.push(Diagnostic {
                        file: rel_path.to_path_buf(),
                        line,
                        severity: Severity::Info,
                        message: format!("unknown HTML element `<{name}>`"),
                        span: Some(node.span),
                    });
                }

                for attr in attrs {
                    if let Some(ref val) = attr.value
                        && attr.name == "id"
                        && !val.is_empty()
                    {
                        if let Some(&first_line) = seen_ids.get(val) {
                            out.push(Diagnostic {
                                file: rel_path.to_path_buf(),
                                line,
                                severity: Severity::Error,
                                message: format!(
                                    "duplicate `id` attribute `{val}` (first at line {first_line})",
                                ),
                                span: Some(node.span),
                            });
                        } else {
                            seen_ids.insert(val.clone(), line);
                        }
                    }
                }

                check_required_attrs(name, attrs, rel_path, line, out);
                check_block_in_inline(tree, node, name, rel_path, line, out);
                check_invalid_parent(tree, node, name, rel_path, line, out);
            }
            html::HtmlTag::Close { .. } | html::HtmlTag::Comment { .. } => {}
        }
    }
}

/// Check for markdown-like content inside opaque HTML blocks.
///
/// When HTML block content has no blank lines, markdown syntax won't be
/// parsed — headings, links, and lists render as literal text.
pub fn check_markdown_in_opaque_html(tree: &Tree, rel_path: &Path, out: &mut Vec<Diagnostic>) {
    let source = tree.source();

    for node in tree.nodes() {
        if !matches!(node.kind, ElementKind::HtmlBlock) {
            continue;
        }

        let raw = &source[node.span.start..node.span.end];
        let lines: Vec<&str> = raw.lines().collect();

        // Skip if there are blank lines (markdown is parsed after blank lines).
        if lines.iter().any(|l| l.trim().is_empty()) {
            continue;
        }

        // Check non-tag lines for markdown syntax.
        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            // Skip the first and last lines (likely HTML tags).
            if i == 0 || (i == lines.len() - 1 && trimmed.starts_with("</")) {
                continue;
            }

            let has_markdown = trimmed.starts_with('#')
                || trimmed.starts_with("- ")
                || trimmed.starts_with("* ")
                || trimmed.contains("](");

            if has_markdown {
                let line_start = node.span.start
                    + raw
                        .match_indices('\n')
                        .take(i)
                        .last()
                        .map_or(0, |(idx, _)| idx + 1);
                let line_num = block::byte_offset_to_line(source, line_start);
                out.push(Diagnostic {
                    file: rel_path.to_path_buf(),
                    line: line_num,
                    severity: Severity::Warning,
                    message:
                        "markdown syntax inside HTML block without blank lines will not be parsed"
                            .to_string(),
                    span: None,
                });
                // One diagnostic per HTML block is enough.
                break;
            }
        }
    }
}

/// Check for missing required attributes on HTML elements.
///
/// An `<a>` carrying `id` or `name` (and no `href`) is a valid explicit
/// anchor *target*, not a link *source* — the standard GFM idiom for a stable
/// `#fragment` (issue 025). Such a tag legitimately omits `href`, so it is not
/// flagged. An `<a>` with neither `href` nor an anchor-defining attribute is
/// still flagged.
fn check_required_attrs(
    tag: &str,
    attrs: &[html::Attribute],
    rel_path: &Path,
    line: usize,
    out: &mut Vec<Diagnostic>,
) {
    // A target `<a>` (bearing `id`/`name`) does not require `href`.
    if tag == "a" && attrs.iter().any(|a| a.name == "id" || a.name == "name") {
        return;
    }

    let required: &[&str] = match tag {
        "img" => &["alt"],
        "a" => &["href"],
        _ => return,
    };

    for &attr_name in required {
        if !attrs.iter().any(|a| a.name == attr_name) {
            out.push(Diagnostic {
                file: rel_path.to_path_buf(),
                line,
                severity: Severity::Warning,
                message: format!("`<{tag}>` missing required attribute `{attr_name}`"),
                // No node in scope here; fall back to a whole-line range.
                span: None,
            });
        }
    }
}

/// Check if a block element is nested inside an inline element context.
fn check_block_in_inline(
    tree: &Tree,
    node: &block::Node,
    tag: &str,
    rel_path: &Path,
    line: usize,
    out: &mut Vec<Diagnostic>,
) {
    if !html::BLOCK_ELEMENTS.contains(tag) {
        return;
    }

    let mut current = node.parent;
    while let Some(pid) = current {
        let parent = tree.node(pid);
        if parent.syntax == Syntax::Html {
            let parent_raw = &tree.source()[parent.span.start..parent.span.end];
            let parent_trimmed = parent_raw.trim();
            if let Some(html::HtmlTag::Open { ref name, .. }) =
                html::tokenize_tag(parent_trimmed, 0)
                && !html::BLOCK_ELEMENTS.contains(name.as_str())
                && !html::VOID_ELEMENTS.contains(name.as_str())
            {
                out.push(Diagnostic {
                    file: rel_path.to_path_buf(),
                    line,
                    severity: Severity::Error,
                    message: format!("block element `<{tag}>` inside inline element `<{name}>`"),
                    span: Some(node.span),
                });
                return;
            }
        }
        current = parent.parent;
    }
}

/// Check if an element has a valid parent (e.g., `<tr>` must be inside `<table>`).
fn check_invalid_parent(
    tree: &Tree,
    node: &block::Node,
    tag: &str,
    rel_path: &Path,
    line: usize,
    out: &mut Vec<Diagnostic>,
) {
    let required_parents: &[&str] = match tag {
        "tr" | "thead" | "tbody" | "tfoot" | "caption" | "colgroup" | "col" => &["table"],
        "td" | "th" => &["table", "tr"],
        "li" => &["ul", "ol", "menu"],
        "summary" => &["details"],
        "option" | "optgroup" => &["select", "datalist"],
        _ => return,
    };

    let mut current = node.parent;
    while let Some(pid) = current {
        let parent = tree.node(pid);
        if parent.syntax == Syntax::Html {
            let parent_raw = &tree.source()[parent.span.start..parent.span.end];
            let parent_trimmed = parent_raw.trim();
            if let Some(html::HtmlTag::Open { ref name, .. }) =
                html::tokenize_tag(parent_trimmed, 0)
                && required_parents.contains(&name.as_str())
            {
                return;
            }
        }
        match &parent.kind {
            ElementKind::Table { .. } if required_parents.contains(&"table") => return,
            ElementKind::List { ordered: true, .. } if required_parents.contains(&"ol") => return,
            ElementKind::List { ordered: false, .. } if required_parents.contains(&"ul") => return,
            ElementKind::Details if required_parents.contains(&"details") => return,
            _ => {}
        }
        current = parent.parent;
    }

    out.push(Diagnostic {
        file: rel_path.to_path_buf(),
        line,
        severity: Severity::Error,
        message: format!(
            "`<{tag}>` requires parent {}",
            required_parents
                .iter()
                .map(|p| format!("`<{p}>`"))
                .collect::<Vec<_>>()
                .join(" or ")
        ),
        span: Some(node.span),
    });
}
