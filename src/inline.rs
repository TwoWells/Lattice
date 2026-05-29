// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Inline parser for links, images, footnote references, and opaque spans.
//!
//! Second pass over the completed block tree. Scans [`Paragraph`] and
//! [`Heading`] nodes for inline elements and adds them as children.
//!
//! [`Paragraph`]: crate::block::ElementKind::Paragraph
//! [`Heading`]: crate::block::ElementKind::Heading

use std::collections::HashMap;

use crate::block::{Diagnostic, ElementKind, NodeId, Syntax, Tree, normalize_label};
use crate::span::Span;

// ---------------------------------------------------------------------------
// Reference definition data
// ---------------------------------------------------------------------------

/// Collected reference definition for lookup during inline parsing.
struct RefDef {
    url: String,
    title: String,
    node_id: NodeId,
    used: bool,
}

// ---------------------------------------------------------------------------
// File extensions for import directives
// ---------------------------------------------------------------------------

const IMPORT_EXTENSIONS: &[&str] = &[".json", ".md", ".toml", ".txt", ".xml", ".yaml", ".yml"];

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Parse inline elements in all `Paragraph` and `Heading` nodes.
///
/// Collects reference and footnote definitions, then scans inline hosts
/// for links, images, footnote references, code spans, math spans, and
/// import directives. Emits diagnostics for undefined references,
/// unused definitions, and duplicate definitions.
pub fn parse_inlines(tree: &mut Tree) {
    let source = tree.source().to_string();

    let mut ref_defs: HashMap<String, RefDef> = HashMap::new();
    let mut footnote_defs: HashMap<String, (NodeId, bool)> = HashMap::new();
    let mut hosts: Vec<(NodeId, Span)> = Vec::new();
    let mut diagnostics: Vec<Diagnostic> = Vec::new();

    for (id, node) in tree.nodes().iter().enumerate() {
        match &node.kind {
            ElementKind::ReferenceDef { label, url, title } => {
                if ref_defs.contains_key(label) {
                    diagnostics.push(Diagnostic {
                        span: node.span,
                        message: format!("duplicate reference definition `{label}`"),
                    });
                } else {
                    ref_defs.insert(
                        label.clone(),
                        RefDef {
                            url: url.clone(),
                            title: title.clone(),
                            node_id: id,
                            used: false,
                        },
                    );
                }
            }
            ElementKind::FootnoteDef { label } => {
                if footnote_defs.contains_key(label) {
                    diagnostics.push(Diagnostic {
                        span: node.span,
                        message: format!("duplicate footnote definition `{label}`"),
                    });
                } else {
                    footnote_defs.insert(label.clone(), (id, false));
                }
            }
            ElementKind::Paragraph | ElementKind::Heading { .. } | ElementKind::TableCell => {
                hosts.push((id, node.span));
            }
            _ => {}
        }
    }

    for (host_id, host_span) in hosts {
        scan_inlines(
            &source,
            host_span,
            tree,
            host_id,
            &mut ref_defs,
            &mut footnote_defs,
            &mut diagnostics,
        );
    }

    // Unused reference definitions
    for (label, def) in &ref_defs {
        if !def.used {
            let span = tree.node(def.node_id).span;
            diagnostics.push(Diagnostic {
                span,
                message: format!("unused reference definition `{label}`"),
            });
        }
    }

    // Unused footnote definitions
    for (label, &(node_id, used)) in &footnote_defs {
        if !used {
            let span = tree.node(node_id).span;
            diagnostics.push(Diagnostic {
                span,
                message: format!("unused footnote definition `{label}`"),
            });
        }
    }

    for diag in diagnostics {
        tree.add_diagnostic(diag);
    }
}

// ---------------------------------------------------------------------------
// Inline scanner
// ---------------------------------------------------------------------------

/// Scan a single inline host for links, images, and other inline elements.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "scanner state is threaded through all callers; single match loop"
)]
fn scan_inlines(
    source: &str,
    host_span: Span,
    tree: &mut Tree,
    parent: NodeId,
    ref_defs: &mut HashMap<String, RefDef>,
    footnote_defs: &mut HashMap<String, (NodeId, bool)>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let text = &source[host_span.start..host_span.end];
    let bytes = text.as_bytes();
    let base = host_span.start;
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            b'\\' if i + 1 < bytes.len() && bytes[i + 1].is_ascii_punctuation() => {
                i += 2;
            }
            b'`' => {
                let tick_count = count_char(bytes, i, b'`');
                if let Some(end) = find_closing_backticks(bytes, i + tick_count, tick_count) {
                    tree.add_child(
                        parent,
                        ElementKind::InlineCode,
                        Syntax::Markdown,
                        Span::new(base + i, base + end),
                    );
                    i = end;
                } else {
                    i += tick_count;
                }
            }
            b'$' => {
                if let Some(end) = try_parse_inline_math(bytes, i) {
                    tree.add_child(
                        parent,
                        ElementKind::InlineMath,
                        Syntax::Markdown,
                        Span::new(base + i, base + end),
                    );
                    i = end;
                } else {
                    i += 1;
                }
            }
            b'!' if i + 1 < bytes.len() && bytes[i + 1] == b'[' => {
                if let Some((end, url, title)) =
                    try_parse_bracket_element(text, bytes, i + 1, ref_defs, diagnostics, base)
                {
                    tree.add_child(
                        parent,
                        ElementKind::Image { url, title },
                        Syntax::Markdown,
                        Span::new(base + i, base + end),
                    );
                    i = end;
                } else {
                    i += 1;
                }
            }
            b'[' => {
                if i + 1 < bytes.len() && bytes[i + 1] == b'^' {
                    if let Some((end, label)) = try_parse_footnote_ref(bytes, i) {
                        if let Some((_, used)) = footnote_defs.get_mut(&label) {
                            *used = true;
                        } else {
                            diagnostics.push(Diagnostic {
                                span: Span::new(base + i, base + end),
                                message: format!("undefined footnote `{label}`"),
                            });
                        }
                        tree.add_child(
                            parent,
                            ElementKind::FootnoteRef { label },
                            Syntax::Markdown,
                            Span::new(base + i, base + end),
                        );
                        i = end;
                    } else {
                        i += 1;
                    }
                } else if let Some((end, url, title)) =
                    try_parse_bracket_element(text, bytes, i, ref_defs, diagnostics, base)
                {
                    tree.add_child(
                        parent,
                        ElementKind::Link { url, title },
                        Syntax::Markdown,
                        Span::new(base + i, base + end),
                    );
                    i = end;
                } else {
                    i += 1;
                }
            }
            b'@' if is_word_boundary(bytes, i) => {
                if let Some((end, path)) = try_parse_import(text, i) {
                    tree.add_child(
                        parent,
                        ElementKind::Import { path },
                        Syntax::Markdown,
                        Span::new(base + i, base + end),
                    );
                    i = end;
                } else {
                    i += 1;
                }
            }
            _ => {
                i += 1;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helper: word boundary check
// ---------------------------------------------------------------------------

/// Check if position `i` is at a word boundary (start of text or preceded
/// by whitespace).
fn is_word_boundary(bytes: &[u8], i: usize) -> bool {
    i == 0 || bytes[i - 1].is_ascii_whitespace()
}

// ---------------------------------------------------------------------------
// Backtick / inline code helpers
// ---------------------------------------------------------------------------

/// Count consecutive occurrences of `ch` starting at `pos`.
fn count_char(bytes: &[u8], pos: usize, ch: u8) -> usize {
    bytes[pos..].iter().take_while(|&&b| b == ch).count()
}

/// Find closing backtick sequence of exactly `count` backticks.
fn find_closing_backticks(bytes: &[u8], start: usize, count: usize) -> Option<usize> {
    let mut i = start;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            let found = count_char(bytes, i, b'`');
            if found == count {
                return Some(i + found);
            }
            i += found;
        } else {
            i += 1;
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Bracket matching
// ---------------------------------------------------------------------------

/// Find the `]` that matches the `[` at `start`.
///
/// Handles nested brackets, backslash escapes, and backtick spans.
fn find_matching_bracket(bytes: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    let mut depth: usize = 1;

    while i < bytes.len() && depth > 0 {
        match bytes[i] {
            b'\\' if i + 1 < bytes.len() => {
                i += 2;
            }
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
            b'`' => {
                let ticks = count_char(bytes, i, b'`');
                if let Some(end) = find_closing_backticks(bytes, i + ticks, ticks) {
                    i = end;
                } else {
                    i += ticks;
                }
            }
            _ => {
                i += 1;
            }
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Inline math
// ---------------------------------------------------------------------------

/// Try to parse an inline math span starting at `$`.
///
/// GitHub rules: opening `$` not followed by whitespace, closing `$` not
/// preceded by whitespace, content not empty.
fn try_parse_inline_math(bytes: &[u8], pos: usize) -> Option<usize> {
    if pos + 1 >= bytes.len() {
        return None;
    }

    let next = bytes[pos + 1];
    if next == b' ' || next == b'\t' || next == b'\n' || next == b'\r' || next == b'$' {
        return None;
    }

    let mut i = pos + 1;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() {
            i += 2;
            continue;
        }
        if bytes[i] == b'$' {
            let prev = bytes[i - 1];
            if prev == b' ' || prev == b'\t' || prev == b'\n' || prev == b'\r' {
                i += 1;
                continue;
            }
            return Some(i + 1);
        }
        i += 1;
    }

    None
}

// ---------------------------------------------------------------------------
// Footnote references
// ---------------------------------------------------------------------------

/// Try to parse a footnote reference `[^label]` at `pos`.
fn try_parse_footnote_ref(bytes: &[u8], pos: usize) -> Option<(usize, String)> {
    if pos + 2 >= bytes.len() || bytes[pos] != b'[' || bytes[pos + 1] != b'^' {
        return None;
    }

    let start = pos + 2;
    let mut i = start;
    while i < bytes.len() && bytes[i] != b']' && bytes[i] != b'\n' && bytes[i] != b'[' {
        i += 1;
    }

    if i >= bytes.len() || bytes[i] != b']' || i == start {
        return None;
    }

    let label = std::str::from_utf8(&bytes[start..i]).ok()?;
    Some((i + 1, label.to_string()))
}

// ---------------------------------------------------------------------------
// Link / image bracket elements
// ---------------------------------------------------------------------------

/// Try to parse a link or image starting at the `[` position.
///
/// Handles inline `[text](url "title")`, full reference `[text][label]`,
/// collapsed reference `[text][]`, and shortcut reference `[text]`.
fn try_parse_bracket_element(
    text: &str,
    bytes: &[u8],
    bracket_pos: usize,
    ref_defs: &mut HashMap<String, RefDef>,
    diagnostics: &mut Vec<Diagnostic>,
    base: usize,
) -> Option<(usize, String, String)> {
    let close = find_matching_bracket(bytes, bracket_pos)?;
    let after = close + 1;

    if after < bytes.len() && bytes[after] == b'(' {
        // Inline destination: [text](url "title")
        parse_inline_dest(text, after)
    } else if after < bytes.len() && bytes[after] == b'[' {
        if after + 1 < bytes.len() && bytes[after + 1] == b']' {
            // Collapsed reference: [text][]
            let label_text = &text[bracket_pos + 1..close];
            let label = normalize_label(label_text);
            if let Some(def) = ref_defs.get_mut(&label) {
                def.used = true;
                Some((after + 2, def.url.clone(), def.title.clone()))
            } else {
                diagnostics.push(Diagnostic {
                    span: Span::new(base + bracket_pos, base + after + 2),
                    message: format!("undefined reference `{label}`"),
                });
                None
            }
        } else {
            // Full reference: [text][label]
            let ref_close = find_matching_bracket(bytes, after)?;
            let label_text = &text[after + 1..ref_close];
            let label = normalize_label(label_text);
            if let Some(def) = ref_defs.get_mut(&label) {
                def.used = true;
                Some((ref_close + 1, def.url.clone(), def.title.clone()))
            } else {
                diagnostics.push(Diagnostic {
                    span: Span::new(base + bracket_pos, base + ref_close + 1),
                    message: format!("undefined reference `{label}`"),
                });
                None
            }
        }
    } else {
        // Shortcut reference: [label] — only resolves if def exists
        let label_text = &text[bracket_pos + 1..close];
        let label = normalize_label(label_text);
        if let Some(def) = ref_defs.get_mut(&label) {
            def.used = true;
            Some((close + 1, def.url.clone(), def.title.clone()))
        } else {
            None
        }
    }
}

/// Parse an inline link destination: `(url "title")`.
fn parse_inline_dest(text: &str, paren_pos: usize) -> Option<(usize, String, String)> {
    let bytes = text.as_bytes();
    if paren_pos >= bytes.len() || bytes[paren_pos] != b'(' {
        return None;
    }

    let mut i = paren_pos + 1;
    skip_spaces(bytes, &mut i);

    if i >= bytes.len() {
        return None;
    }

    // Empty destination: ()
    if bytes[i] == b')' {
        return Some((i + 1, String::new(), String::new()));
    }

    // Parse URL
    let (url, url_end) = parse_dest_url(text, bytes, i)?;
    i = url_end;

    // Skip whitespace (including newlines for multi-line titles)
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }

    if i >= bytes.len() {
        return None;
    }

    // Parse optional title or closing paren
    let title = match bytes[i] {
        b'"' | b'\'' => {
            let (t, end) = parse_quoted_title(text, bytes, i, bytes[i])?;
            i = end;
            t
        }
        b')' => String::new(),
        _ => return None,
    };

    skip_spaces(bytes, &mut i);

    if i >= bytes.len() || bytes[i] != b')' {
        return None;
    }

    Some((i + 1, url, title))
}

/// Parse the URL portion of an inline link destination.
fn parse_dest_url(text: &str, bytes: &[u8], start: usize) -> Option<(String, usize)> {
    if bytes[start] == b'<' {
        // Angle-bracket URL
        let inner_start = start + 1;
        let mut j = inner_start;
        while j < bytes.len() && bytes[j] != b'>' && bytes[j] != b'\n' {
            if bytes[j] == b'\\' && j + 1 < bytes.len() {
                j += 2;
            } else {
                j += 1;
            }
        }
        if j >= bytes.len() || bytes[j] != b'>' {
            return None;
        }
        Some((text[inner_start..j].to_string(), j + 1))
    } else {
        let mut i = start;
        let mut paren_depth: i32 = 0;
        while i < bytes.len() {
            match bytes[i] {
                b' ' | b'\t' | b')' if paren_depth == 0 => break,
                b'(' => {
                    paren_depth += 1;
                    i += 1;
                }
                b')' => {
                    paren_depth -= 1;
                    i += 1;
                }
                b'\\' if i + 1 < bytes.len() => {
                    i += 2;
                }
                _ => {
                    i += 1;
                }
            }
        }
        Some((text[start..i].to_string(), i))
    }
}

/// Parse a quoted title delimited by `quote` (`"` or `'`).
fn parse_quoted_title(text: &str, bytes: &[u8], pos: usize, quote: u8) -> Option<(String, usize)> {
    let start = pos + 1;
    let mut j = start;
    while j < bytes.len() && bytes[j] != quote {
        if bytes[j] == b'\\' && j + 1 < bytes.len() {
            j += 2;
        } else {
            j += 1;
        }
    }
    if j >= bytes.len() {
        return None;
    }
    Some((text[start..j].to_string(), j + 1))
}

/// Skip ASCII spaces and tabs.
fn skip_spaces(bytes: &[u8], i: &mut usize) {
    while *i < bytes.len() && (bytes[*i] == b' ' || bytes[*i] == b'\t') {
        *i += 1;
    }
}

// ---------------------------------------------------------------------------
// Import directives
// ---------------------------------------------------------------------------

/// Try to parse an `@path` import directive at `pos`.
fn try_parse_import(text: &str, pos: usize) -> Option<(usize, String)> {
    let rest = &text[pos + 1..];

    let word_end = rest
        .find(|c: char| {
            c.is_whitespace() || matches!(c, ',' | ';' | ':' | '!' | '?' | ')' | ']' | '"' | '\'')
        })
        .unwrap_or(rest.len());

    if word_end == 0 {
        return None;
    }

    let path = &rest[..word_end];

    // Reject absolute and home-relative paths
    if path.starts_with('/') || path.starts_with('~') {
        return None;
    }

    if !IMPORT_EXTENSIONS.iter().any(|ext| path.ends_with(ext)) {
        return None;
    }

    Some((pos + 1 + word_end, path.to_string()))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests use expect and panic for clarity"
)]
mod tests {
    use crate::block::{ElementKind, NodeId, Tree, parse_tree};

    /// Parse source with no frontmatter.
    fn parse(source: &str) -> Tree {
        parse_tree(source, None)
    }

    /// Collect children of the root.
    fn root_children(tree: &Tree) -> Vec<NodeId> {
        tree.children(tree.root()).to_vec()
    }

    /// Find all nodes of a given kind.
    fn find_nodes(tree: &Tree, pred: fn(&ElementKind) -> bool) -> Vec<NodeId> {
        tree.nodes()
            .iter()
            .enumerate()
            .filter(|(_, n)| pred(&n.kind))
            .map(|(id, _)| id)
            .collect()
    }

    // --- Inline links ---

    #[test]
    fn inline_link_basic() {
        let tree = parse("[text](url)\n");
        let links = find_nodes(&tree, |k| matches!(k, ElementKind::Link { .. }));
        assert_eq!(links.len(), 1, "should find one link");
        match &tree.node(links[0]).kind {
            ElementKind::Link { url, title } => {
                assert_eq!(url, "url", "link url");
                assert!(title.is_empty(), "no title");
            }
            other => panic!("expected Link, got {other:?}"),
        }
    }

    #[test]
    fn inline_link_with_title() {
        let tree = parse("[text](url \"predicate\")\n");
        let links = find_nodes(&tree, |k| matches!(k, ElementKind::Link { .. }));
        assert_eq!(links.len(), 1, "should find one link");
        match &tree.node(links[0]).kind {
            ElementKind::Link { url, title } => {
                assert_eq!(url, "url", "link url");
                assert_eq!(title, "predicate", "link title");
            }
            other => panic!("expected Link, got {other:?}"),
        }
    }

    #[test]
    fn inline_link_angle_bracket_url() {
        let tree = parse("[text](<url with spaces>)\n");
        let links = find_nodes(&tree, |k| matches!(k, ElementKind::Link { .. }));
        assert_eq!(links.len(), 1, "should find one link");
        match &tree.node(links[0]).kind {
            ElementKind::Link { url, .. } => {
                assert_eq!(url, "url with spaces", "angle-bracket url");
            }
            other => panic!("expected Link, got {other:?}"),
        }
    }

    #[test]
    fn inline_link_angle_bracket_with_title() {
        let tree = parse("[text](<url with spaces> \"pred\")\n");
        let links = find_nodes(&tree, |k| matches!(k, ElementKind::Link { .. }));
        assert_eq!(links.len(), 1, "should find one link");
        match &tree.node(links[0]).kind {
            ElementKind::Link { url, title } => {
                assert_eq!(url, "url with spaces", "angle-bracket url");
                assert_eq!(title, "pred", "title with angle bracket url");
            }
            other => panic!("expected Link, got {other:?}"),
        }
    }

    #[test]
    fn inline_link_empty_dest() {
        let tree = parse("[text]()\n");
        let links = find_nodes(&tree, |k| matches!(k, ElementKind::Link { .. }));
        assert_eq!(links.len(), 1, "should find one link");
        match &tree.node(links[0]).kind {
            ElementKind::Link { url, title } => {
                assert!(url.is_empty(), "empty url");
                assert!(title.is_empty(), "empty title");
            }
            other => panic!("expected Link, got {other:?}"),
        }
    }

    // --- Reference links ---

    #[test]
    fn reference_link_full() {
        let tree = parse("[text][ref]\n\n[ref]: url \"title\"\n");
        let links = find_nodes(&tree, |k| matches!(k, ElementKind::Link { .. }));
        assert_eq!(links.len(), 1, "should find one link");
        match &tree.node(links[0]).kind {
            ElementKind::Link { url, title } => {
                assert_eq!(url, "url", "resolved url");
                assert_eq!(title, "title", "resolved title");
            }
            other => panic!("expected Link, got {other:?}"),
        }
    }

    #[test]
    fn reference_link_collapsed() {
        let tree = parse("[ref][]\n\n[ref]: url \"title\"\n");
        let links = find_nodes(&tree, |k| matches!(k, ElementKind::Link { .. }));
        assert_eq!(links.len(), 1, "should find one link");
        match &tree.node(links[0]).kind {
            ElementKind::Link { url, title } => {
                assert_eq!(url, "url", "resolved url");
                assert_eq!(title, "title", "resolved title");
            }
            other => panic!("expected Link, got {other:?}"),
        }
    }

    #[test]
    fn reference_link_shortcut() {
        let tree = parse("[ref]\n\n[ref]: url \"title\"\n");
        let links = find_nodes(&tree, |k| matches!(k, ElementKind::Link { .. }));
        assert_eq!(links.len(), 1, "should find one link");
        match &tree.node(links[0]).kind {
            ElementKind::Link { url, title } => {
                assert_eq!(url, "url", "resolved url");
                assert_eq!(title, "title", "resolved title");
            }
            other => panic!("expected Link, got {other:?}"),
        }
    }

    #[test]
    fn reference_link_case_insensitive() {
        let tree = parse("[text][REF]\n\n[ref]: url\n");
        let links = find_nodes(&tree, |k| matches!(k, ElementKind::Link { .. }));
        assert_eq!(links.len(), 1, "case-insensitive label match");
    }

    // --- Undefined and unused references ---

    #[test]
    fn undefined_reference_produces_diagnostic() {
        let tree = parse("[text][missing]\n");
        let links = find_nodes(&tree, |k| matches!(k, ElementKind::Link { .. }));
        assert!(links.is_empty(), "undefined ref should not produce link");
        assert!(
            tree.diagnostics()
                .iter()
                .any(|d| d.message.contains("undefined reference")),
            "should emit undefined reference diagnostic"
        );
    }

    #[test]
    fn undefined_collapsed_reference_produces_diagnostic() {
        let tree = parse("[missing][]\n");
        assert!(
            tree.diagnostics()
                .iter()
                .any(|d| d.message.contains("undefined reference")),
            "should emit undefined reference diagnostic for collapsed"
        );
    }

    #[test]
    fn unused_reference_def_produces_diagnostic() {
        let tree = parse("[ref]: url \"title\"\n");
        assert!(
            tree.diagnostics()
                .iter()
                .any(|d| d.message.contains("unused reference definition")),
            "should emit unused ref def diagnostic"
        );
    }

    #[test]
    fn duplicate_reference_def_produces_diagnostic() {
        let tree = parse("[ref]: url1\n[ref]: url2\n");
        assert!(
            tree.diagnostics()
                .iter()
                .any(|d| d.message.contains("duplicate reference definition")),
            "should emit duplicate ref def diagnostic"
        );
    }

    // --- Images ---

    #[test]
    fn inline_image() {
        let tree = parse("![alt](image.png \"caption\")\n");
        let images = find_nodes(&tree, |k| matches!(k, ElementKind::Image { .. }));
        assert_eq!(images.len(), 1, "should find one image");
        match &tree.node(images[0]).kind {
            ElementKind::Image { url, title } => {
                assert_eq!(url, "image.png", "image url");
                assert_eq!(title, "caption", "image title");
            }
            other => panic!("expected Image, got {other:?}"),
        }
    }

    #[test]
    fn reference_image() {
        let tree = parse("![alt][img]\n\n[img]: image.png\n");
        let images = find_nodes(&tree, |k| matches!(k, ElementKind::Image { .. }));
        assert_eq!(images.len(), 1, "should find one image");
        match &tree.node(images[0]).kind {
            ElementKind::Image { url, .. } => {
                assert_eq!(url, "image.png", "resolved image url");
            }
            other => panic!("expected Image, got {other:?}"),
        }
    }

    // --- Footnotes ---

    #[test]
    fn footnote_reference() {
        let tree = parse("See[^note] for details.\n\n[^note]: The explanation.\n");
        let refs = find_nodes(&tree, |k| matches!(k, ElementKind::FootnoteRef { .. }));
        assert_eq!(refs.len(), 1, "should find one footnote ref");
        match &tree.node(refs[0]).kind {
            ElementKind::FootnoteRef { label } => {
                assert_eq!(label, "note", "footnote label");
            }
            other => panic!("expected FootnoteRef, got {other:?}"),
        }
    }

    #[test]
    fn footnote_def_is_container() {
        let tree = parse("[^note]: The explanation.\n");
        let defs = find_nodes(&tree, |k| matches!(k, ElementKind::FootnoteDef { .. }));
        assert_eq!(defs.len(), 1, "should find one footnote def");
        let children = tree.children(defs[0]);
        assert!(!children.is_empty(), "footnote def should have children");
    }

    #[test]
    fn undefined_footnote_produces_diagnostic() {
        let tree = parse("See [^missing] here.\n");
        assert!(
            tree.diagnostics()
                .iter()
                .any(|d| d.message.contains("undefined footnote")),
            "should emit undefined footnote diagnostic"
        );
    }

    #[test]
    fn unused_footnote_def_produces_diagnostic() {
        let tree = parse("[^unused]: Content.\n");
        assert!(
            tree.diagnostics()
                .iter()
                .any(|d| d.message.contains("unused footnote definition")),
            "should emit unused footnote diagnostic"
        );
    }

    // --- Backslash escapes ---

    #[test]
    fn backslash_escaped_bracket_not_link() {
        let tree = parse("\\[not a link](url)\n");
        let links = find_nodes(&tree, |k| matches!(k, ElementKind::Link { .. }));
        assert!(links.is_empty(), "escaped bracket should not start a link");
    }

    // --- Inline code skipping ---

    #[test]
    fn inline_code_skips_link() {
        let tree = parse("`[not a link](url)`\n");
        let links = find_nodes(&tree, |k| matches!(k, ElementKind::Link { .. }));
        assert!(links.is_empty(), "link inside code should be skipped");
        let codes = find_nodes(&tree, |k| matches!(k, ElementKind::InlineCode));
        assert_eq!(codes.len(), 1, "should find inline code span");
    }

    #[test]
    fn double_backtick_code_span() {
        let tree = parse("``code with ` backtick``\n");
        let codes = find_nodes(&tree, |k| matches!(k, ElementKind::InlineCode));
        assert_eq!(codes.len(), 1, "should find one code span");
    }

    // --- Inline math ---

    #[test]
    fn inline_math_skips_link() {
        let tree = parse("$f = [g](h)$\n");
        let links = find_nodes(&tree, |k| matches!(k, ElementKind::Link { .. }));
        assert!(links.is_empty(), "link inside math should be skipped");
        let maths = find_nodes(&tree, |k| matches!(k, ElementKind::InlineMath));
        assert_eq!(maths.len(), 1, "should find inline math span");
    }

    #[test]
    fn dollar_fifty_not_math() {
        let tree = parse("Costs $50 today.\n");
        let maths = find_nodes(&tree, |k| matches!(k, ElementKind::InlineMath));
        assert!(maths.is_empty(), "$50 should not trigger math");
    }

    #[test]
    fn math_opening_dollar_not_followed_by_whitespace() {
        let tree = parse("$ not math$\n");
        let maths = find_nodes(&tree, |k| matches!(k, ElementKind::InlineMath));
        assert!(maths.is_empty(), "space after opening $ is not math");
    }

    #[test]
    fn math_closing_dollar_not_preceded_by_whitespace() {
        let tree = parse("$not math $\n");
        let maths = find_nodes(&tree, |k| matches!(k, ElementKind::InlineMath));
        assert!(maths.is_empty(), "space before closing $ is not math");
    }

    // --- Import directives ---

    #[test]
    fn import_directive_markdown() {
        let tree = parse("@./AGENTS.md\n");
        let imports = find_nodes(&tree, |k| matches!(k, ElementKind::Import { .. }));
        assert_eq!(imports.len(), 1, "should find one import");
        match &tree.node(imports[0]).kind {
            ElementKind::Import { path } => {
                assert_eq!(path, "./AGENTS.md", "import path");
            }
            other => panic!("expected Import, got {other:?}"),
        }
    }

    #[test]
    fn import_directive_requires_word_boundary() {
        let tree = parse("user@./path.md\n");
        let imports = find_nodes(&tree, |k| matches!(k, ElementKind::Import { .. }));
        assert!(imports.is_empty(), "@ after non-whitespace is not import");
    }

    #[test]
    fn import_directive_unknown_extension() {
        let tree = parse("@./diagram.png\n");
        let imports = find_nodes(&tree, |k| matches!(k, ElementKind::Import { .. }));
        assert!(imports.is_empty(), "unknown extension not recognized");
    }

    #[test]
    fn import_directive_absolute_path_rejected() {
        let tree = parse("@/home/user/file.md\n");
        let imports = find_nodes(&tree, |k| matches!(k, ElementKind::Import { .. }));
        assert!(imports.is_empty(), "absolute path rejected");
    }

    #[test]
    fn import_directive_no_slash() {
        let tree = parse("@README.md\n");
        let imports = find_nodes(&tree, |k| matches!(k, ElementKind::Import { .. }));
        assert_eq!(imports.len(), 1, "import without slash");
    }

    // --- Reference definitions (block level) ---

    #[test]
    fn reference_def_recognized() {
        let tree = parse("[ref]: url \"title\"\n");
        let defs = find_nodes(&tree, |k| matches!(k, ElementKind::ReferenceDef { .. }));
        assert_eq!(defs.len(), 1, "should find one ref def");
        match &tree.node(defs[0]).kind {
            ElementKind::ReferenceDef { label, url, title } => {
                assert_eq!(label, "ref", "label");
                assert_eq!(url, "url", "url");
                assert_eq!(title, "title", "title");
            }
            other => panic!("expected ReferenceDef, got {other:?}"),
        }
    }

    #[test]
    fn reference_def_no_title() {
        let tree = parse("[ref]: url\n");
        let defs = find_nodes(&tree, |k| matches!(k, ElementKind::ReferenceDef { .. }));
        assert_eq!(defs.len(), 1, "should find one ref def");
        match &tree.node(defs[0]).kind {
            ElementKind::ReferenceDef { title, .. } => {
                assert!(title.is_empty(), "no title");
            }
            other => panic!("expected ReferenceDef, got {other:?}"),
        }
    }

    #[test]
    fn reference_def_angle_bracket_url() {
        let tree = parse("[ref]: <url with spaces> \"title\"\n");
        let defs = find_nodes(&tree, |k| matches!(k, ElementKind::ReferenceDef { .. }));
        assert_eq!(defs.len(), 1, "should find one ref def");
        match &tree.node(defs[0]).kind {
            ElementKind::ReferenceDef { url, .. } => {
                assert_eq!(url, "url with spaces", "angle-bracket url");
            }
            other => panic!("expected ReferenceDef, got {other:?}"),
        }
    }

    #[test]
    fn reference_def_does_not_interrupt_paragraph() {
        let tree = parse("Some text.\n[ref]: url\n");
        let defs = find_nodes(&tree, |k| matches!(k, ElementKind::ReferenceDef { .. }));
        assert!(
            defs.is_empty(),
            "ref def should not interrupt paragraph: found {defs:?}"
        );
        let paras = find_nodes(&tree, |k| matches!(k, ElementKind::Paragraph));
        assert_eq!(paras.len(), 1, "should be one paragraph");
    }

    #[test]
    fn reference_def_multiline_title() {
        let tree = parse("[ref]: url\n\"title on next line\"\n");
        let defs = find_nodes(&tree, |k| matches!(k, ElementKind::ReferenceDef { .. }));
        assert_eq!(defs.len(), 1, "should find one ref def");
        match &tree.node(defs[0]).kind {
            ElementKind::ReferenceDef { url, title, .. } => {
                assert_eq!(url, "url", "url");
                assert_eq!(title, "title on next line", "multi-line title");
            }
            other => panic!("expected ReferenceDef, got {other:?}"),
        }
    }

    #[test]
    fn reference_def_multiline_title_single_quoted() {
        let tree = parse("[ref]: url\n'single quoted'\n");
        let defs = find_nodes(&tree, |k| matches!(k, ElementKind::ReferenceDef { .. }));
        assert_eq!(defs.len(), 1, "should find one ref def");
        match &tree.node(defs[0]).kind {
            ElementKind::ReferenceDef { title, .. } => {
                assert_eq!(title, "single quoted", "single-quoted multi-line title");
            }
            other => panic!("expected ReferenceDef, got {other:?}"),
        }
    }

    #[test]
    fn reference_def_multiline_title_resolves() {
        let tree = parse("[text][ref]\n\n[ref]: url\n\"pred\"\n");
        let links = find_nodes(&tree, |k| matches!(k, ElementKind::Link { .. }));
        assert_eq!(links.len(), 1, "should find one link");
        match &tree.node(links[0]).kind {
            ElementKind::Link { url, title } => {
                assert_eq!(url, "url", "resolved url");
                assert_eq!(title, "pred", "resolved multi-line title");
            }
            other => panic!("expected Link, got {other:?}"),
        }
    }

    #[test]
    fn reference_def_multiline_non_title_stays_separate() {
        let tree = parse("[ref]: url\nNot a title\n");
        let defs = find_nodes(&tree, |k| matches!(k, ElementKind::ReferenceDef { .. }));
        assert_eq!(defs.len(), 1, "should find one ref def");
        match &tree.node(defs[0]).kind {
            ElementKind::ReferenceDef { title, .. } => {
                assert!(title.is_empty(), "non-title line should not be consumed");
            }
            other => panic!("expected ReferenceDef, got {other:?}"),
        }
        let paras = find_nodes(&tree, |k| matches!(k, ElementKind::Paragraph));
        assert_eq!(paras.len(), 1, "non-title line should be a paragraph");
    }

    // --- Nested brackets ---

    #[test]
    fn nested_brackets_in_link_text() {
        let tree = parse("[text [with] brackets](url)\n");
        let links = find_nodes(&tree, |k| matches!(k, ElementKind::Link { .. }));
        assert_eq!(links.len(), 1, "nested brackets in text should work");
    }

    // --- Footnote def continuation ---

    #[test]
    fn footnote_def_with_continuation() {
        let tree = parse("[^note]: First.\n    Second.\n");
        let defs = find_nodes(&tree, |k| matches!(k, ElementKind::FootnoteDef { .. }));
        assert_eq!(defs.len(), 1, "should find one footnote def");
        let children = tree.children(defs[0]);
        assert_eq!(
            children.len(),
            2,
            "footnote def with continuation has two children"
        );
    }

    // --- Link in heading ---

    #[test]
    fn link_in_heading() {
        let tree = parse("## [Link](url \"pred\")\n");
        let links = find_nodes(&tree, |k| matches!(k, ElementKind::Link { .. }));
        assert_eq!(links.len(), 1, "should find link in heading");
        match &tree.node(links[0]).kind {
            ElementKind::Link { url, title } => {
                assert_eq!(url, "url", "link url");
                assert_eq!(title, "pred", "link title");
            }
            other => panic!("expected Link, got {other:?}"),
        }
        // Link should be child of heading
        let heading_id = root_children(&tree)[0];
        assert!(
            tree.children(heading_id).contains(&links[0]),
            "link should be child of heading"
        );
    }

    // --- Multiple links ---

    #[test]
    fn multiple_links_in_paragraph() {
        let tree = parse("[a](url1) and [b](url2)\n");
        let links = find_nodes(&tree, |k| matches!(k, ElementKind::Link { .. }));
        assert_eq!(links.len(), 2, "should find two links");
    }

    // --- Link with single-quoted title ---

    #[test]
    fn link_with_single_quoted_title() {
        let tree = parse("[text](url 'title')\n");
        let links = find_nodes(&tree, |k| matches!(k, ElementKind::Link { .. }));
        assert_eq!(links.len(), 1, "should find one link");
        match &tree.node(links[0]).kind {
            ElementKind::Link { title, .. } => {
                assert_eq!(title, "title", "single-quoted title");
            }
            other => panic!("expected Link, got {other:?}"),
        }
    }

    // --- Shortcut reference not-a-link ---

    #[test]
    fn shortcut_ref_without_def_is_not_link() {
        let tree = parse("[just text]\n");
        let links = find_nodes(&tree, |k| matches!(k, ElementKind::Link { .. }));
        assert!(
            links.is_empty(),
            "shortcut ref without definition is not a link"
        );
        assert!(
            tree.diagnostics().is_empty(),
            "no diagnostic for unmatched shortcut ref"
        );
    }
}
