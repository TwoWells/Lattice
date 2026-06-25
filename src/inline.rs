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

use crate::block::{
    Diagnostic, DiagnosticLevel, ElementKind, NodeId, Syntax, Tree, normalize_label,
};
use crate::html::{self, HtmlTag};
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
    // Idempotent: the pass appends inline children and definition diagnostics,
    // so running it twice would duplicate them. `parse_tree_with_entries` runs
    // it once; any later call is a no-op.
    if tree.inlines_parsed() {
        return;
    }
    tree.mark_inlines_parsed();

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
                        level: DiagnosticLevel::Warning,
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
                        level: DiagnosticLevel::Warning,
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
                level: DiagnosticLevel::Warning,
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
                level: DiagnosticLevel::Warning,
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

    // Precompute `[` -> matching `]` in a single pass so the scanner never
    // re-scans for a closing bracket. Without this, a degenerate run like
    // `[[[[...` drives quadratic backtracking through `find_matching_bracket`.
    let bracket_matches = precompute_bracket_matches(bytes);

    // Start byte (within `bytes`) of the line currently being scanned, used
    // to bound per-line inline work.
    let mut line_start = 0;

    // Emphasis / strong / strikethrough delimiter runs found in top-level text
    // (i.e. not inside a code span, link destination, autolink, or raw HTML).
    // Matched into runs after the scan completes; offsets here are relative to
    // `text`, not `base`.
    let mut delimiters: Vec<DelimRun> = Vec::new();

    while i < bytes.len() {
        // Per-line inline-scan cap: bytes past the limit on a single line are
        // treated as plain text. A degenerate line (e.g. unmatched `$` runs)
        // cannot drive quadratic inline scanning. Block structure was already
        // recognized at line start, so only inline detection is affected.
        if i - line_start >= crate::limits::MAX_INLINE_LINE_BYTES {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            // Consume the line ending and reset the line so the next line is
            // scanned from its start. Without resetting `line_start` the cap
            // would stay tripped and the loop would spin on the newline.
            if i < bytes.len() {
                line_start = i + 1;
                i += 1;
            }
            continue;
        }

        match bytes[i] {
            b'\n' => {
                line_start = i + 1;
                i += 1;
            }
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
                if let Some((end, url, title)) = try_parse_bracket_element(
                    text,
                    bytes,
                    i + 1,
                    &bracket_matches,
                    ref_defs,
                    diagnostics,
                    base,
                ) {
                    tree.add_child(
                        parent,
                        crate::block::classify_media(url, title),
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
                                level: DiagnosticLevel::Error,
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
                } else if let Some((end, url, title)) = try_parse_bracket_element(
                    text,
                    bytes,
                    i,
                    &bracket_matches,
                    ref_defs,
                    diagnostics,
                    base,
                ) {
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
            b'<' => {
                let remaining = &text[i..];
                // Try autolink first (short-circuit before tag parsing).
                if let Some((url, len)) = html::try_autolink(remaining) {
                    tree.add_child(
                        parent,
                        ElementKind::Link {
                            url,
                            title: String::new(),
                        },
                        Syntax::Html,
                        Span::new(base + i, base + i + len),
                    );
                    i += len;
                } else if let Some(tag) = html::tokenize_tag(remaining, base + i) {
                    match tag {
                        HtmlTag::Open {
                            ref name,
                            ref attrs,
                            len,
                            ..
                        } if name == "a" => {
                            let (href, title) = html::extract_link_attrs(attrs);
                            // Find closing </a> to determine full span.
                            let tag_end = i + len;
                            let close_end = find_inline_close_tag(&text[tag_end..], "a")
                                .map_or(tag_end, |cl| tag_end + cl);
                            tree.add_child(
                                parent,
                                ElementKind::Link { url: href, title },
                                Syntax::Html,
                                Span::new(base + i, base + close_end),
                            );
                            i = close_end;
                        }
                        HtmlTag::Open {
                            ref name,
                            ref attrs,
                            len,
                            ..
                        } if name == "img" => {
                            let (url, title) = html::extract_image_attrs(attrs);
                            tree.add_child(
                                parent,
                                ElementKind::Image { url, title },
                                Syntax::Html,
                                Span::new(base + i, base + i + len),
                            );
                            i += len;
                        }
                        HtmlTag::Open { ref attrs, len, .. } => {
                            // A non-`<a>`/`<img>` open tag is otherwise folded
                            // into text. Materialize a generic raw-HTML node
                            // only when it carries an anchor `id`, so the tag is
                            // visible to the same `Syntax::Html` surface that
                            // `Tree::anchors()` and the structural duplicate-id
                            // pass already walk — a mid-paragraph `<span id="x">`
                            // becomes a resolvable `#x` target (issue 026).
                            if attrs.iter().any(|a| {
                                a.name == "id" && a.value.as_deref().is_some_and(|v| !v.is_empty())
                            }) {
                                tree.add_child(
                                    parent,
                                    ElementKind::InlineHtml,
                                    Syntax::Html,
                                    Span::new(base + i, base + i + len),
                                );
                            }
                            i += len;
                        }
                        HtmlTag::Close { len, .. } | HtmlTag::Comment { len, .. } => {
                            i += len;
                        }
                    }
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
            b'*' | b'_' | b'~' => {
                let marker = bytes[i];
                let run_len = count_char(bytes, i, marker);
                delimiters.push(classify_delim_run(text, i, run_len, marker));
                i += run_len;
            }
            _ => {
                i += 1;
            }
        }
    }

    match_emphasis(base, &delimiters, tree, parent);
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
// Emphasis / strong / strikethrough (CommonMark + GFM flanking)
// ---------------------------------------------------------------------------

/// A run of identical emphasis delimiter characters found in top-level text.
///
/// Offsets are relative to the host text slice. `can_open` / `can_close`
/// encode the `CommonMark` delimiter-run flanking rules (with the GFM tilde
/// rules for `~`), precomputed from the characters surrounding the run so the
/// matching pass is a pure stack walk.
struct DelimRun {
    /// Delimiter character: `*`, `_`, or `~`.
    marker: u8,
    /// Start offset of the run within the host text.
    start: usize,
    /// Number of delimiter characters remaining in the run. Consumed from the
    /// inner edges as the run is matched, so a long run can pair more than once.
    count: usize,
    /// End offset of the run within the host text (`start + original length`).
    end: usize,
    /// Whether this run can open emphasis (left-flanking, with the `_`/`~`
    /// refinements).
    can_open: bool,
    /// Whether this run can close emphasis (right-flanking, with the `_`/`~`
    /// refinements).
    can_close: bool,
}

/// Classify a delimiter run for `CommonMark` / GFM flanking.
///
/// Reads the Unicode characters immediately before and after the run (the
/// start and end of the host text count as whitespace, per `CommonMark`) and
/// derives left/right-flanking and the `can_open` / `can_close` predicates,
/// including the intraword-`_` restriction and the GFM tilde rules.
fn classify_delim_run(text: &str, start: usize, len: usize, marker: u8) -> DelimRun {
    let end = start + len;
    let before = text[..start].chars().next_back();
    let after = text[end..].chars().next();

    let before_ws = before.is_none_or(is_unicode_whitespace);
    let after_ws = after.is_none_or(is_unicode_whitespace);
    let before_punct = before.is_some_and(is_punct);
    let after_punct = after.is_some_and(is_punct);

    // CommonMark left-flanking: not followed by whitespace, and either not
    // followed by punctuation or (followed by punctuation and preceded by
    // whitespace or punctuation).
    let left_flanking = !after_ws && (!after_punct || before_ws || before_punct);
    // CommonMark right-flanking: not preceded by whitespace, and either not
    // preceded by punctuation or (preceded by punctuation and followed by
    // whitespace or punctuation).
    let right_flanking = !before_ws && (!before_punct || after_ws || after_punct);

    let (can_open, can_close) = match marker {
        // `_` carries the intraword restriction: it may only open when it is
        // left-flanking and either not right-flanking or preceded by
        // punctuation, and symmetrically for closing.
        b'_' => (
            left_flanking && (!right_flanking || before_punct),
            right_flanking && (!left_flanking || after_punct),
        ),
        // `*` and `~` (GFM strikethrough) use the plain flanking predicates.
        _ => (left_flanking, right_flanking),
    };

    DelimRun {
        marker,
        start,
        count: len,
        end,
        can_open,
        can_close,
    }
}

/// `CommonMark` "Unicode whitespace": a space, tab, newline, carriage return,
/// form feed, or any Unicode whitespace character.
fn is_unicode_whitespace(c: char) -> bool {
    c.is_whitespace()
}

/// `CommonMark` "punctuation": an ASCII punctuation character or any Unicode
/// punctuation/symbol character.
fn is_punct(c: char) -> bool {
    c.is_ascii_punctuation() || (!c.is_alphanumeric() && !c.is_whitespace() && !c.is_control())
}

/// Find the nearest opener (scanning back from `close_idx`) that can pair with
/// the closer at `close_idx`, honouring the marker, flanking, multiple-of-3,
/// and GFM tilde length rules. Returns the opener index, or `None` if no run
/// before the closer is a compatible partner.
fn find_opener(runs: &[DelimRun], remaining: &[usize], close_idx: usize) -> Option<usize> {
    let closer = &runs[close_idx];
    let mut open_idx = close_idx;
    while open_idx > 0 {
        open_idx -= 1;
        let candidate = &runs[open_idx];
        if candidate.marker != closer.marker || !candidate.can_open || remaining[open_idx] == 0 {
            continue;
        }
        if closer.marker == b'~' {
            // GFM strikethrough: opener and closer must be the same original
            // length, and only runs of one or two tildes are delimiters.
            if candidate.count != closer.count || closer.count > 2 {
                continue;
            }
            return Some(open_idx);
        }
        // CommonMark rule 9/10: when one of the two runs can both open and
        // close, the sum of their *original* lengths must not be a multiple of
        // 3 unless both lengths are themselves multiples of 3.
        if (candidate.can_close || closer.can_open)
            && (candidate.count + closer.count).is_multiple_of(3)
            && (!candidate.count.is_multiple_of(3) || !closer.count.is_multiple_of(3))
        {
            continue;
        }
        return Some(open_idx);
    }
    None
}

/// Match collected delimiter runs into strong / emphasis / strikethrough nodes
/// and attach them to `parent`.
///
/// Implements the `CommonMark` emphasis delimiter-stack algorithm for `*` / `_`
/// (pairing `**`/`__` into [`ElementKind::Strong`] and a single delimiter into
/// [`ElementKind::Emphasis`]), plus the GFM strikethrough rule for `~` (a run
/// of exactly one or two tildes, matched against an equal-length closer). The
/// span of each emitted run covers both delimiters and their content; offsets
/// in `runs` are relative to the host text, so each span is shifted by `base`.
fn match_emphasis(base: usize, runs: &[DelimRun], tree: &mut Tree, parent: NodeId) {
    // Working copy of the remaining delimiter counts, consumed as runs pair.
    let mut remaining: Vec<usize> = runs.iter().map(|r| r.count).collect();
    // Emitted spans, collected then sorted so attachment order is deterministic
    // (outermost-first by start) regardless of the inside-out matching order.
    let mut emitted: Vec<(Span, ElementKind)> = Vec::new();

    // Walk closers left-to-right; for each, repeatedly scan back for the
    // nearest compatible opener and pair until the closer is exhausted (the
    // classic delimiter-stack walk — a long run pairs more than once).
    for close_idx in 0..runs.len() {
        let closer = &runs[close_idx];
        if !closer.can_close {
            continue;
        }

        while remaining[close_idx] > 0 {
            let opener = find_opener(runs, &remaining, close_idx);
            let Some(open_idx) = opener else {
                break;
            };

            // CommonMark "process emphasis": once an opener and closer are
            // paired, every delimiter run strictly between them is removed from
            // the stack — it is now enclosed by the pair and can never match a
            // delimiter outside it. Omitting this lets an intervening run of a
            // *different* marker mis-pair across the boundary, emphasizing text
            // that the spec leaves literal (e.g. the inner `_` in `*foo _bar*`
            // or the inner `*` in `*foo __bar *baz bim__ bam*`).
            for slot in &mut remaining[open_idx + 1..close_idx] {
                *slot = 0;
            }

            if closer.marker == b'~' {
                // GFM strikethrough: only runs of length 1 or 2 are delimiters,
                // and an opener pairs with a closer of the *same* original
                // length. `find_opener` already filtered to an equal-length,
                // <=2 partner, so emit and consume both whole runs.
                let span = Span::new(base + runs[open_idx].start, base + closer.end);
                remaining[open_idx] = 0;
                remaining[close_idx] = 0;
                emitted.push((span, ElementKind::Strikethrough));
                continue;
            }

            // `*` / `_`: consume two delimiters for strong, otherwise one for
            // emphasis, from the inner edges of each run.
            let use_two = remaining[open_idx] >= 2 && remaining[close_idx] >= 2;
            let take = if use_two { 2 } else { 1 };
            let open_inner = runs[open_idx].start + (remaining[open_idx] - take);
            let close_inner = closer.start + (closer.count - remaining[close_idx]) + take;
            let span = Span::new(base + open_inner, base + close_inner);
            remaining[open_idx] -= take;
            remaining[close_idx] -= take;
            let kind = if use_two {
                ElementKind::Strong
            } else {
                ElementKind::Emphasis
            };
            emitted.push((span, kind));
        }
    }

    // Attach outermost-first so the wellformedness invariant sees a stable,
    // deterministic order (a node's siblings need not be disjoint, only
    // contained in the shared parent).
    emitted.sort_by(|a, b| a.0.start.cmp(&b.0.start).then(b.0.end.cmp(&a.0.end)));
    for (span, kind) in emitted {
        tree.add_child(parent, kind, Syntax::Markdown, span);
    }
}

// ---------------------------------------------------------------------------
// Backtick / inline code helpers
// ---------------------------------------------------------------------------

/// Count consecutive occurrences of `ch` starting at `pos`.
pub fn count_char(bytes: &[u8], pos: usize, ch: u8) -> usize {
    bytes[pos..].iter().take_while(|&&b| b == ch).count()
}

/// Find closing backtick sequence of exactly `count` backticks.
pub fn find_closing_backticks(bytes: &[u8], start: usize, count: usize) -> Option<usize> {
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

/// Precompute, for every `[` in `bytes`, the index of its matching `]`.
///
/// One left-to-right pass using a stack, with the same backslash-escape and
/// backtick-span skipping rules as [`find_matching_bracket`]. The result for
/// any given `[` is identical to calling `find_matching_bracket` at that
/// position, but the whole table is built in O(n) so the inline scanner never
/// re-scans — eliminating quadratic backtracking on inputs like `[[[[...`.
///
/// Entries are `None` for byte positions that are not an unmatched-then-closed
/// `[` (including non-`[` bytes and `[` with no matching `]`).
fn precompute_bracket_matches(bytes: &[u8]) -> Vec<Option<usize>> {
    let mut matches = vec![None; bytes.len()];
    let mut stack: Vec<usize> = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        match bytes[i] {
            b'\\' if i + 1 < bytes.len() => {
                i += 2;
            }
            b'`' => {
                let ticks = count_char(bytes, i, b'`');
                if let Some(end) = find_closing_backticks(bytes, i + ticks, ticks) {
                    i = end;
                } else {
                    i += ticks;
                }
            }
            b'[' => {
                stack.push(i);
                i += 1;
            }
            b']' => {
                if let Some(open) = stack.pop() {
                    matches[open] = Some(i);
                }
                i += 1;
            }
            _ => {
                i += 1;
            }
        }
    }

    matches
}

/// Find the `]` that matches the `[` at `start`.
///
/// Handles nested brackets, backslash escapes, and backtick spans.
pub fn find_matching_bracket(bytes: &[u8], start: usize) -> Option<usize> {
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
    bracket_matches: &[Option<usize>],
    ref_defs: &mut HashMap<String, RefDef>,
    diagnostics: &mut Vec<Diagnostic>,
    base: usize,
) -> Option<(usize, String, String)> {
    let close = bracket_matches[bracket_pos]?;
    let after = close + 1;

    if after < bytes.len() && bytes[after] == b'(' {
        // Inline destination: [text](url "title")
        let result = parse_inline_dest(text, after);
        if result.is_none() {
            diagnostics.push(Diagnostic {
                level: DiagnosticLevel::Error,
                span: Span::new(base + bracket_pos, base + after + 1),
                message: "malformed link: unclosed or invalid destination".to_string(),
            });
        }
        result
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
                    level: DiagnosticLevel::Error,
                    span: Span::new(base + bracket_pos, base + after + 2),
                    message: format!("undefined reference `{label}`"),
                });
                None
            }
        } else {
            // Full reference: [text][label]
            let ref_close = bracket_matches[after]?;
            let label_text = &text[after + 1..ref_close];
            let label = normalize_label(label_text);
            if let Some(def) = ref_defs.get_mut(&label) {
                def.used = true;
                Some((ref_close + 1, def.url.clone(), def.title.clone()))
            } else {
                diagnostics.push(Diagnostic {
                    level: DiagnosticLevel::Error,
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
                // A bare destination never contains a line ending. CommonMark
                // allows the title to follow on the next line (separated by up
                // to one line ending), so the destination ends here regardless
                // of paren depth — otherwise the newline and the title text get
                // swallowed into the URL.
                b'\n' | b'\r' => break,
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
// Inline HTML close tag search
// ---------------------------------------------------------------------------

/// Find the end of a closing tag `</name>` within inline text.
///
/// Returns the byte offset past the `>` relative to `text`, or `None`
/// if no matching close tag is found.
fn find_inline_close_tag(text: &str, tag: &str) -> Option<usize> {
    let mut search = 0;
    while let Some(lt) = text[search..].find("</") {
        let abs = search + lt;
        if let Some(HtmlTag::Close { ref name, len, .. }) = html::tokenize_tag(&text[abs..], 0)
            && name == tag
        {
            return Some(abs + len);
        }
        search = abs + 2;
    }
    None
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
    use crate::block::{ElementKind, Node, NodeId, Syntax, Tree, parse_tree};

    /// Parse source with no frontmatter.
    fn parse(source: &str) -> Tree {
        parse_tree(source, None)
    }

    /// Helper: collect children of the root.
    fn root_children(tree: &Tree) -> Vec<NodeId> {
        tree.children(tree.root()).to_vec()
    }

    /// Helper: assert a node is a specific kind and return it.
    fn assert_kind<'a>(tree: &'a Tree, id: NodeId, expected: &ElementKind) -> &'a Node {
        let node = tree.node(id);
        assert_eq!(
            &node.kind, expected,
            "node {id} should be {expected:?}, got {:?}",
            node.kind
        );
        node
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
    fn inline_link_title_on_continuation_line() {
        // CommonMark separates the title from the destination by spaces, tabs,
        // and up to one line ending. The bare destination must stop at the
        // newline and not swallow it plus the title text. Regression for a
        // false `link target does not exist` on multi-line links.
        let tree = parse("see ([18](18_commonmark_conformance.md\n\"references\")) ok\n");
        let links = find_nodes(&tree, |k| matches!(k, ElementKind::Link { .. }));
        assert_eq!(links.len(), 1, "should find one link");
        match &tree.node(links[0]).kind {
            ElementKind::Link { url, title } => {
                assert_eq!(
                    url, "18_commonmark_conformance.md",
                    "destination stops at the line ending, not swallowing the title"
                );
                assert_eq!(
                    title, "references",
                    "title parsed from the continuation line"
                );
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

    #[test]
    fn reference_def_multiline_title_in_blockquote() {
        // A definition whose title continues on the next line, inside a block
        // quote: each continuation line keeps its `>` marker, which the run
        // collector strips before joining.
        let tree = parse("> [ref]: /url\n> \"pred\"\n");
        let defs = find_nodes(&tree, |k| matches!(k, ElementKind::ReferenceDef { .. }));
        assert_eq!(defs.len(), 1, "multi-line ref def inside a block quote");
        match &tree.node(defs[0]).kind {
            ElementKind::ReferenceDef { url, title, .. } => {
                assert_eq!(url, "/url", "url from the first quoted line");
                assert_eq!(title, "pred", "title from the second quoted line");
            }
            other => panic!("expected ReferenceDef, got {other:?}"),
        }
        // The definition is nested inside the block quote scope.
        let quote = root_children(&tree)
            .into_iter()
            .find(|&id| matches!(tree.node(id).kind, ElementKind::QuoteBlock))
            .expect("a block quote at the root");
        assert!(
            tree.children(quote).contains(&defs[0]),
            "ref def should be a child of the block quote"
        );
    }

    #[test]
    fn reference_def_multiline_label() {
        // A link label may span a line ending; the run collector joins the
        // lines and the label normalizes to a single space-collapsed string.
        let tree = parse("[foo\nbar]: /url\n\n[foo bar]\n");
        let defs = find_nodes(&tree, |k| matches!(k, ElementKind::ReferenceDef { .. }));
        assert_eq!(defs.len(), 1, "definition with a wrapped label");
        match &tree.node(defs[0]).kind {
            ElementKind::ReferenceDef { label, url, .. } => {
                assert_eq!(label, "foo bar", "label collapses the line ending");
                assert_eq!(url, "/url", "url after the wrapped label");
            }
            other => panic!("expected ReferenceDef, got {other:?}"),
        }
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

    // ===================================================================
    // Inline HTML
    // ===================================================================

    // --- Autolinks ---

    #[test]
    fn inline_autolink_uri() {
        let tree = parse("See <https://example.com> for info.\n");
        let links = find_nodes(&tree, |k| matches!(k, ElementKind::Link { .. }));
        assert_eq!(links.len(), 1, "should find one autolink");
        match &tree.node(links[0]).kind {
            ElementKind::Link { url, title } => {
                assert_eq!(url, "https://example.com", "autolink URL");
                assert!(title.is_empty(), "autolink has no title");
            }
            other => panic!("expected Link, got {other:?}"),
        }
        assert_eq!(
            tree.node(links[0]).syntax,
            Syntax::Html,
            "autolink has Html syntax"
        );
    }

    #[test]
    fn inline_autolink_email() {
        let tree = parse("Mail <user@example.com> now.\n");
        let links = find_nodes(&tree, |k| matches!(k, ElementKind::Link { .. }));
        assert_eq!(links.len(), 1, "should find one email autolink");
        match &tree.node(links[0]).kind {
            ElementKind::Link { url, .. } => {
                assert_eq!(url, "mailto:user@example.com", "email autolink URL");
            }
            other => panic!("expected Link, got {other:?}"),
        }
    }

    #[test]
    fn autolink_not_confused_with_tags() {
        let tree = parse("See <http://example.com> here.\n");
        let links = find_nodes(&tree, |k| matches!(k, ElementKind::Link { .. }));
        assert_eq!(links.len(), 1, "URI autolink parsed as link, not tag");
    }

    #[test]
    fn email_autolink_passes_inline_fidelity() {
        // Regression (fuzz_parse_tree, ticket 22): email autolinks synthesize a
        // `mailto:` scheme that is absent from the source. The inline content-
        // fidelity invariant must not flag the synthesized prefix as encoding
        // corruption — only the address after it is sliced from the source.
        let tree = parse("Reach <a@b.co> or <x.y@sub.example.com> today.\n");
        crate::invariants::assert_tree_wellformed(&tree);
        crate::invariants::assert_inline_resource_fidelity(&tree);
    }

    // --- Inline <a> tags ---

    #[test]
    fn inline_a_tag_as_link() {
        let tree = parse(r#"Click <a href="path.md" title="references">here</a>."#);
        let links = find_nodes(&tree, |k| matches!(k, ElementKind::Link { .. }));
        assert_eq!(links.len(), 1, "should find one link from <a>");
        match &tree.node(links[0]).kind {
            ElementKind::Link { url, title } => {
                assert_eq!(url, "path.md", "<a> href");
                assert_eq!(title, "references", "<a> title");
            }
            other => panic!("expected Link, got {other:?}"),
        }
        assert_eq!(
            tree.node(links[0]).syntax,
            Syntax::Html,
            "<a> link has Html syntax"
        );
    }

    #[test]
    fn inline_a_tag_same_as_markdown_link() {
        let md_tree = parse("[text](path.md \"references\")\n");
        let html_tree = parse(r#"<a href="path.md" title="references">text</a>"#);

        let md_links = find_nodes(&md_tree, |k| matches!(k, ElementKind::Link { .. }));
        let html_links = find_nodes(&html_tree, |k| matches!(k, ElementKind::Link { .. }));

        assert_eq!(md_links.len(), 1, "one markdown link");
        assert_eq!(html_links.len(), 1, "one HTML link");

        // Same kind (same url and title).
        assert_eq!(
            md_tree.node(md_links[0]).kind,
            html_tree.node(html_links[0]).kind,
            "same Link kind"
        );
    }

    // --- Inline <img> tags ---

    #[test]
    fn inline_img_tag_as_image() {
        let tree = parse(r#"<img src="photo.jpg" title="caption" />"#);
        let images = find_nodes(&tree, |k| matches!(k, ElementKind::Image { .. }));
        assert_eq!(images.len(), 1, "should find one image from <img>");
        match &tree.node(images[0]).kind {
            ElementKind::Image { url, title } => {
                assert_eq!(url, "photo.jpg", "<img> src");
                assert_eq!(title, "caption", "<img> title");
            }
            other => panic!("expected Image, got {other:?}"),
        }
    }

    // --- Inline HTML tags are skipped ---

    #[test]
    fn inline_html_comment_skipped() {
        let tree = parse("text <!-- comment --> more\n");
        let links = find_nodes(&tree, |k| matches!(k, ElementKind::Link { .. }));
        assert!(links.is_empty(), "comments produce no links");
    }

    #[test]
    fn inline_formatting_tags_skipped() {
        let tree = parse("Some <em>emphasized</em> text.\n");
        // em has no structural mapping, so it's skipped.
        let children = root_children(&tree);
        assert_eq!(children.len(), 1, "one paragraph");
        assert_kind(&tree, children[0], &ElementKind::Paragraph);
    }

    #[test]
    fn inline_formatting_tag_without_id_still_skipped() {
        // Issue 026: a non-`<a>`/`<img>` inline tag is materialized *only* when
        // it bears an anchor `id`. A plain `<span>` carries none, so it is still
        // folded into text — no `InlineHtml` node.
        let tree = parse("Some <span>styled</span> text.\n");
        let inline_html = find_nodes(&tree, |k| matches!(k, ElementKind::InlineHtml));
        assert!(
            inline_html.is_empty(),
            "an id-less inline tag produces no InlineHtml node: {inline_html:?}"
        );
    }

    #[test]
    fn inline_span_id_materializes_inline_html_node() {
        // Issue 026: a mid-paragraph id-bearing inline tag becomes an
        // `InlineHtml` node on the `Syntax::Html` surface, so `Tree::anchors()`
        // and the structural duplicate-id pass both see it.
        let tree = parse("text <span id=\"x\"></span> text\n");
        let inline_html = find_nodes(&tree, |k| matches!(k, ElementKind::InlineHtml));
        assert_eq!(
            inline_html.len(),
            1,
            "one InlineHtml node for the id-bearing span: {inline_html:?}"
        );
        let node = tree.node(inline_html[0]);
        assert_eq!(
            node.syntax,
            Syntax::Html,
            "the materialized inline node carries Syntax::Html"
        );
        // The node spans exactly the open tag (`<span id="x">`), not the close.
        let raw = &tree.source()[node.span.start..node.span.end];
        assert_eq!(
            raw, "<span id=\"x\">",
            "the InlineHtml span covers the open tag only"
        );
        // The new node must preserve the tree-wellformedness invariant.
        crate::invariants::assert_tree_wellformed(&tree);
    }

    // --- Pathological inline input (ticket 20) ---

    /// Generous per-thread **CPU-time** bound: catches quadratic backtracking
    /// while remaining immune to scheduling delay. Because CPU time accrues
    /// only while the thread is actually executing, it excludes time spent
    /// descheduled, so cross-process contention (e.g. a concurrent full-suite
    /// run saturating every core) cannot inflate it — unlike a wall-clock
    /// bound. A linear parse burns ~constant CPU regardless of load; a
    /// quadratic regression burns orders of magnitude more (seconds → minutes).
    /// The bound is set generously so slower CI hardware still clears it
    /// (GitHub-hosted runners are markedly slower per core than the self-hosted
    /// box this was originally tuned on — the dollar-run case measured ~5.8s
    /// there), while genuine quadratic blowup never could.
    const INLINE_SLOW_BOUND: std::time::Duration = std::time::Duration::from_secs(20);

    #[test]
    fn unclosed_bracket_run_is_not_quadratic() {
        // `[[[[...` with no closing bracket. Each `[` previously triggered an
        // O(n) forward scan; the precomputed match table makes the whole scan
        // linear.
        let source = format!("{}\n", "[".repeat(200_000));
        let start = cpu_time::ThreadTime::now();
        let tree = parse(&source);
        let elapsed = start.elapsed();
        assert!(
            elapsed < INLINE_SLOW_BOUND,
            "unclosed bracket run must scan linearly, took {elapsed:?}"
        );
        let links = find_nodes(&tree, |k| matches!(k, ElementKind::Link { .. }));
        assert!(links.is_empty(), "unclosed brackets are not links");
    }

    #[test]
    fn open_brackets_then_single_close_is_not_quadratic() {
        // `[[[[...]` — one closing bracket at the end. Depth-counting bracket
        // matching from each `[` would be O(n^2); the precompute keeps it
        // linear.
        let source = format!("{}]\n", "[".repeat(200_000));
        let start = cpu_time::ThreadTime::now();
        let _tree = parse(&source);
        let elapsed = start.elapsed();
        assert!(
            elapsed < INLINE_SLOW_BOUND,
            "bracket run with one close must scan linearly, took {elapsed:?}"
        );
    }

    #[test]
    fn long_backtick_run_is_not_quadratic() {
        // A long run of backticks with no matching closer.
        let source = format!("{}\n", "`".repeat(200_000));
        let start = cpu_time::ThreadTime::now();
        let _tree = parse(&source);
        let elapsed = start.elapsed();
        assert!(
            elapsed < INLINE_SLOW_BOUND,
            "long backtick run must scan linearly, took {elapsed:?}"
        );
    }

    #[test]
    fn unmatched_dollar_run_is_not_quadratic() {
        // `$a $a $a ...` — each unmatched `$` would scan to end of line; the
        // per-line inline cap bounds the work.
        let source = format!("{}\n", "$a ".repeat(100_000));
        let start = cpu_time::ThreadTime::now();
        let _tree = parse(&source);
        let elapsed = start.elapsed();
        assert!(
            elapsed < INLINE_SLOW_BOUND,
            "unmatched dollar run must not be quadratic, took {elapsed:?}"
        );
    }

    #[test]
    fn brackets_still_match_after_precompute() {
        // The precompute must preserve normal link detection, including nested
        // brackets in the link text.
        let tree = parse("[a [nested] b](url \"references\")\n");
        let links = find_nodes(&tree, |k| matches!(k, ElementKind::Link { .. }));
        assert_eq!(links.len(), 1, "nested-bracket link text still parses");
        match &tree.node(links[0]).kind {
            ElementKind::Link { url, title } => {
                assert_eq!(url, "url", "link url preserved");
                assert_eq!(title, "references", "link title preserved");
            }
            other => panic!("expected Link, got {other:?}"),
        }
    }

    #[test]
    fn overlong_line_truncates_inline_scan() {
        // A link past the per-line inline cap is treated as plain text, but a
        // link near the line start is still detected.
        let filler = "x".repeat(crate::limits::MAX_INLINE_LINE_BYTES + 100);
        let source = format!("[early](a.md) {filler}[late](b.md)\n");
        let tree = parse(&source);
        let urls: Vec<String> = find_nodes(&tree, |k| matches!(k, ElementKind::Link { .. }))
            .iter()
            .filter_map(|&id| match &tree.node(id).kind {
                ElementKind::Link { url, .. } => Some(url.clone()),
                _ => None,
            })
            .collect();
        assert!(
            urls.iter().any(|u| u == "a.md"),
            "link before the cap is detected: {urls:?}"
        );
        assert!(
            urls.iter().all(|u| u != "b.md"),
            "link past the cap is treated as text: {urls:?}"
        );
    }

    // ===================================================================
    // Emphasis / strong / strikethrough (ticket 26)
    // ===================================================================

    /// Collect the source slices of every node of a given emphasis kind.
    fn emphasis_slices(tree: &Tree, want: &ElementKind) -> Vec<String> {
        tree.nodes()
            .iter()
            .filter(|n| &n.kind == want)
            .map(|n| tree.source()[n.span.start..n.span.end].to_string())
            .collect()
    }

    #[test]
    fn strong_double_asterisk() {
        let tree = parse("a **bold** b\n");
        assert_eq!(
            emphasis_slices(&tree, &ElementKind::Strong),
            vec!["**bold**".to_string()],
            "double-asterisk strong run covers both delimiters"
        );
        assert!(
            emphasis_slices(&tree, &ElementKind::Emphasis).is_empty(),
            "no emphasis run for a pure strong run"
        );
    }

    #[test]
    fn strong_double_underscore() {
        let tree = parse("a __bold__ b\n");
        assert_eq!(
            emphasis_slices(&tree, &ElementKind::Strong),
            vec!["__bold__".to_string()],
            "double-underscore strong run"
        );
    }

    #[test]
    fn emphasis_intraword_asterisk() {
        // `*` has no intraword restriction: `a*b*c` emphasizes `b`.
        let tree = parse("a*b*c\n");
        assert_eq!(
            emphasis_slices(&tree, &ElementKind::Emphasis),
            vec!["*b*".to_string()],
            "intraword asterisk emphasis is recognized"
        );
    }

    #[test]
    fn emphasis_intraword_underscore_suppressed() {
        // `_` carries the intraword restriction: `foo_bar_baz` is not emphasis.
        let tree = parse("foo_bar_baz\n");
        assert!(
            emphasis_slices(&tree, &ElementKind::Emphasis).is_empty(),
            "intraword underscore must not emphasize"
        );
        assert!(
            emphasis_slices(&tree, &ElementKind::Strong).is_empty(),
            "intraword underscore must not strong-emphasize"
        );
    }

    #[test]
    fn strikethrough_double_tilde() {
        let tree = parse("a ~~struck~~ b\n");
        assert_eq!(
            emphasis_slices(&tree, &ElementKind::Strikethrough),
            vec!["~~struck~~".to_string()],
            "double-tilde strikethrough run"
        );
    }

    #[test]
    fn strikethrough_single_tilde() {
        let tree = parse("a ~one~ b\n");
        assert_eq!(
            emphasis_slices(&tree, &ElementKind::Strikethrough),
            vec!["~one~".to_string()],
            "single-tilde GFM strikethrough run"
        );
    }

    #[test]
    fn tilde_left_flanking_only_is_not_strikethrough() {
        // The headline correctness case (ticket 26): in `~89 of ~162` the second
        // `~` is preceded by whitespace, so it is left-flanking only and cannot
        // close. cmark-gfm produces no strikethrough; Lattice must match.
        let tree = parse("~89 of ~162\n");
        assert!(
            emphasis_slices(&tree, &ElementKind::Strikethrough).is_empty(),
            "left-flanking-only single tildes must not form a strikethrough run: {:?}",
            emphasis_slices(&tree, &ElementKind::Strikethrough)
        );
        // No diagnostic is emitted for emphasis (styling-only).
        assert!(
            tree.diagnostics().is_empty(),
            "emphasis recognition emits no diagnostics: {:?}",
            tree.diagnostics()
        );
    }

    #[test]
    fn emphasis_emits_no_diagnostics() {
        let tree = parse("**bold** and *em* and ~~strike~~ and a*b*c here\n");
        assert!(
            tree.diagnostics().is_empty(),
            "emphasis runs are styling-only and emit no diagnostics: {:?}",
            tree.diagnostics()
        );
    }

    #[test]
    fn emphasis_nested_strong_in_emphasis() {
        // `***foo***` pairs into a strong run inside an emphasis run, both
        // children of the paragraph. The inner `<strong>` spans `**foo**`; the
        // outer `<em>` wraps it, so its span runs from the outermost `*` to the
        // outermost `*` — the whole `***foo***`, matching cmark-gfm.
        let tree = parse("***foo***\n");
        let strong = emphasis_slices(&tree, &ElementKind::Strong);
        let em = emphasis_slices(&tree, &ElementKind::Emphasis);
        assert_eq!(strong, vec!["**foo**".to_string()], "inner strong run");
        assert_eq!(
            em,
            vec!["***foo***".to_string()],
            "outer emphasis run wraps the strong run"
        );
    }

    #[test]
    fn emphasis_intervening_delim_removed_underscore() {
        // CommonMark example 469: `*foo _bar* baz_`. The `*` pair closes first,
        // enclosing the inner `_`; that `_` is then removed from the stack and
        // the trailing `_` finds no opener. Expected `<em>foo _bar</em> baz_`:
        // exactly one emphasis run, covering `*foo _bar*`, with no strong run and
        // the second `_` left literal. Before the intervening-delimiter removal,
        // the two `_` mis-paired and the whole line was emphasized.
        let tree = parse("*foo _bar* baz_\n");
        assert_eq!(
            emphasis_slices(&tree, &ElementKind::Emphasis),
            vec!["*foo _bar*".to_string()],
            "the `*` pair encloses the inner `_`, which cannot then pair with the trailing `_`"
        );
        assert!(
            emphasis_slices(&tree, &ElementKind::Strong).is_empty(),
            "no strong run forms in `*foo _bar* baz_`"
        );
    }

    #[test]
    fn emphasis_intervening_delim_removed_asterisk() {
        // CommonMark example 470: `*foo __bar *baz bim__ bam*`. The `__` pair
        // closes first, enclosing the inner `*baz`; that `*` is removed from the
        // stack, so the final `*` pairs with the leading `*`. Expected
        // `<em>foo <strong>bar *baz bim</strong> bam</em>`: one emphasis run over
        // the whole line and one strong run over `__bar *baz bim__`, with the
        // inner `*` left literal.
        let tree = parse("*foo __bar *baz bim__ bam*\n");
        assert_eq!(
            emphasis_slices(&tree, &ElementKind::Emphasis),
            vec!["*foo __bar *baz bim__ bam*".to_string()],
            "the outer `*` pair wraps the whole run once the inner `*` is enclosed by the `__` pair"
        );
        assert_eq!(
            emphasis_slices(&tree, &ElementKind::Strong),
            vec!["__bar *baz bim__".to_string()],
            "the `__` pair strongs `bar *baz bim`, leaving the inner `*` literal"
        );
    }

    #[test]
    fn emphasis_spans_pass_fidelity_invariant() {
        // Every recognized run must clear the shared span-fidelity invariant,
        // including runs adjacent to multi-byte characters.
        let tree = parse("**café** and *résumé* and ~~naïve~~ and ~89 of ~162\n");
        crate::invariants::assert_tree_wellformed(&tree);
        crate::invariants::assert_emphasis_span_fidelity(&tree);
    }
}
