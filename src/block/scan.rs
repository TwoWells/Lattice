// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! The line scanner: what one line, looked at on its own, could be.
//!
//! Every function here is a *recognizer*. It takes a line (or a slice of one)
//! and answers a local question — is this an ATX heading, and at what level; is
//! this a fenced code opener; where does this link's destination run end; is
//! this a table delimiter row, and with what alignments. None of them own
//! state, none of them decide what the line *is*: that is the sibling
//! [`super::parser`]'s job, which holds the scope stack and the container
//! context a line has to be read against.
//!
//! Tab expansion leads, because it is the precondition for everything below:
//! `CommonMark` measures indentation in columns with a tab stop of 4, so a line
//! is expanded before it is measured and offsets are mapped back to the raw
//! bytes ([`expanded_to_raw`]) before any span is recorded.

use crate::html::{self, HtmlTag};
use crate::span::Span;

use super::{AtxId, NodeId, TableAlignment};

// ---------------------------------------------------------------------------
// Tab expansion
// ---------------------------------------------------------------------------

/// Expand tabs to spaces at the next tab stop (multiples of 4 columns).
///
/// Only expands tabs that appear in leading indentation — once a
/// non-whitespace character is seen, remaining tabs are preserved as-is
/// so that spans into the original source remain valid for content after
/// indentation.
pub fn expand_leading_tabs(line: &str) -> (String, Vec<TabMapping>) {
    let mut result = String::with_capacity(line.len());
    let mut mappings = Vec::new();
    let mut col = 0;
    let mut in_indent = true;

    for (byte_idx, ch) in line.char_indices() {
        if in_indent && ch == '\t' {
            let spaces = 4 - (col % 4);
            mappings.push(TabMapping {
                original_byte: byte_idx,
                num_spaces: spaces,
            });
            for _ in 0..spaces {
                result.push(' ');
            }
            col += spaces;
        } else {
            if ch != ' ' {
                in_indent = false;
            }
            result.push(ch);
            col += 1;
        }
    }

    (result, mappings)
}

/// Expand ALL tabs to spaces (not just leading ones).
///
/// Used for list marker recognition where tabs may appear after the
/// marker character (e.g. `-\t\tfoo`).
pub fn expand_all_tabs(line: &str) -> (String, Vec<TabMapping>) {
    let mut result = String::with_capacity(line.len());
    let mut mappings = Vec::new();
    let mut col = 0;

    for (byte_idx, ch) in line.char_indices() {
        if ch == '\t' {
            let spaces = 4 - (col % 4);
            mappings.push(TabMapping {
                original_byte: byte_idx,
                num_spaces: spaces,
            });
            for _ in 0..spaces {
                result.push(' ');
            }
            col += spaces;
        } else {
            result.push(ch);
            col += 1;
        }
    }

    (result, mappings)
}

/// Mapping from a tab character to its expansion.
#[derive(Debug)]
pub struct TabMapping {
    /// Byte offset of the tab in the original line.
    pub original_byte: usize,
    /// Number of spaces this tab expanded to.
    pub num_spaces: usize,
}

/// Map a column offset in a tab-expanded string back to the corresponding byte
/// offset in the original (pre-expansion) string.
///
/// Walks the raw line accumulating expanded columns until it reaches
/// `expanded_offset`. Because each character advances the byte index by its
/// UTF-8 width, the returned offset always lands on a char boundary even when
/// the indentation region contains multi-byte characters — e.g. a U+00A0
/// non-breaking space, which `str::trim` counts as whitespace, so an
/// all-whitespace continuation line can reach the slice path. A tab recorded in
/// `mappings` occupies its expanded `num_spaces` columns; every other character
/// (including a non-leading tab absent from `mappings`) is one column. With no
/// tabs and ASCII content this reduces to the identity `min(len)`.
pub fn expanded_to_raw(expanded_offset: usize, raw_line: &str, mappings: &[TabMapping]) -> usize {
    let mut col = 0;
    let mut mi = 0;
    for (byte_idx, ch) in raw_line.char_indices() {
        if col >= expanded_offset {
            return byte_idx;
        }
        // Mappings are in increasing byte order; skip any already passed.
        while mi < mappings.len() && mappings[mi].original_byte < byte_idx {
            mi += 1;
        }
        if ch == '\t' && mi < mappings.len() && mappings[mi].original_byte == byte_idx {
            col += mappings[mi].num_spaces;
            mi += 1;
        } else {
            col += 1;
        }
    }
    raw_line.len()
}

// ---------------------------------------------------------------------------
// Line classification helpers
// ---------------------------------------------------------------------------

/// Count leading spaces in a string (after tab expansion).
pub fn count_indent(line: &str) -> usize {
    line.bytes().take_while(|&b| b == b' ').count()
}

/// Strip a trailing `\n` or `\r\n` from a byte offset into source.
///
/// Returns the adjusted end offset with the line ending excluded.
#[allow(dead_code, reason = "used by consumer migration ticket 06")]
pub fn strip_trailing_newline(source: &str, end: usize) -> usize {
    let bytes = source.as_bytes();
    if end > 0 && bytes.get(end - 1) == Some(&b'\n') {
        if end > 1 && bytes.get(end - 2) == Some(&b'\r') {
            end - 2
        } else {
            end - 1
        }
    } else {
        end
    }
}

/// Normalize a reference label per `CommonMark` rules.
///
/// Case-fold (lowercase) and collapse consecutive whitespace to a single space.
pub fn normalize_label(label: &str) -> String {
    label
        .split_whitespace()
        .collect::<Vec<&str>>()
        .join(" ")
        .to_lowercase()
}

/// Skip ASCII spaces and tabs (not line endings) from `i`.
pub const fn skip_inline_ws(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    i
}

/// Consume a single line ending (`\n`, `\r\n`, or `\r`) at `i`, returning the
/// index just past it. If no line ending is present, returns `i` unchanged.
pub const fn consume_line_ending(bytes: &[u8], mut i: usize) -> usize {
    if i < bytes.len() && bytes[i] == b'\r' {
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b'\n' {
        i += 1;
    }
    i
}

/// Scan a link destination starting at `start`.
///
/// Either an angle-bracketed destination (`<...>`, single line) or a bare
/// sequence of non-whitespace, non-control characters. Backslash escapes are
/// skipped when locating the boundary. Returns the raw inner text and the
/// index just past the destination, or `None` if no destination is present.
pub fn scan_destination(s: &str, start: usize) -> Option<(String, usize)> {
    let bytes = s.as_bytes();
    let len = bytes.len();
    if bytes[start] == b'<' {
        let mut i = start + 1;
        while i < len {
            match bytes[i] {
                b'\\' if i + 1 < len && bytes[i + 1] < 0x80 => i += 2,
                b'>' => return Some((s[start + 1..i].to_string(), i + 1)),
                b'\n' | b'\r' | b'<' => return None,
                _ => i += 1,
            }
        }
        None
    } else {
        let mut i = start;
        while i < len {
            let b = bytes[i];
            if b == b'\\' && i + 1 < len && bytes[i + 1] < 0x80 {
                i += 2;
                continue;
            }
            if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' || b < 0x20 {
                break;
            }
            i += 1;
        }
        if i == start {
            None
        } else {
            Some((s[start..i].to_string(), i))
        }
    }
}

/// Scan a link title starting at its opening delimiter (`"`, `'`, or `(`).
///
/// Titles may span multiple lines (the caller never passes a buffer that
/// crosses a blank line, so an unterminated title correctly fails). Backslash
/// escapes are skipped. Returns the raw inner text and the index just past the
/// closing delimiter, or `None` if the title is not closed.
pub fn scan_title(s: &str, start: usize) -> Option<(String, usize)> {
    let bytes = s.as_bytes();
    let len = bytes.len();
    let close = match bytes[start] {
        b'"' => b'"',
        b'\'' => b'\'',
        b'(' => b')',
        _ => return None,
    };
    let open = bytes[start];
    let mut i = start + 1;
    while i < len {
        let b = bytes[i];
        if b == b'\\' && i + 1 < len && bytes[i + 1] < 0x80 {
            i += 2;
            continue;
        }
        if b == close {
            return Some((s[start + 1..i].to_string(), i + 1));
        }
        // An unescaped opening paren inside a `(...)` title is invalid.
        if open == b'(' && b == b'(' {
            return None;
        }
        i += 1;
    }
    None
}

/// Locate the byte span of a link's *destination* — the path-denoting text a
/// move must re-render — within the full document `source`, given the link
/// node's full span (as carried by [`Link::span`]).
///
/// The returned span covers only the path portion of the destination, up to but
/// never including a `#` fragment (fragment bytes ride along verbatim in a file
/// rename — decision 020 clause 4) or a title. The move engine (`crate::mv`)
/// splices the new spelling into exactly this range, leaving the surrounding
/// `[text]`, delimiters, title, and fragment byte-identical.
///
/// Handles the destination-bearing link syntaxes:
/// - inline `[text](dest "title")` — the bare `dest` run;
/// - angle-bracketed inline `[text](<dest> "title")` — the run *inside* the
///   angle brackets, so the edit stays between `<` and `>`;
/// - an `![alt](dest "title")` embed — the `dest` run after the `!`, in either
///   the bare or angle-bracketed form (issue 058);
/// - an `@dest` import directive — the path after the `@`;
/// - a raw-HTML `<a href="dest">…</a>` anchor — the `href` attribute value;
/// - a raw-HTML `<img>` / `<video>` / `<audio>` / `<iframe>` embed — the `src`
///   attribute value (issue 058).
///
/// Returns `None` for a reference-style link or embed (`[text][label]`,
/// `![alt][label]`, `[text][]`, `[label]`), whose destination lives in a
/// separate `ReferenceDef` node, not in the link span — the caller edits the
/// definition's URL via [`Tree::find_ref_def`] instead. Also `None` for an
/// autolink, for a fragment-only destination (`(#section)`, which denotes no
/// file at all), and for any link whose span does not carry an editable inline
/// destination.
#[must_use]
pub fn link_destination_span(source: &str, link_span: Span) -> Option<Span> {
    let run = destination_run_span(source, link_span)?;
    let path_len = path_portion_len(&source[run.start..run.end]);
    if path_len == 0 {
        return None;
    }
    Some(Span::new(run.start, run.start + path_len))
}

/// Locate the byte span of a link's *fragment* — the heading-denoting text after
/// the first `#` in its destination, excluding the `#` itself — within the full
/// document `source`, given the link node's full span (as carried by
/// [`Link::span`]).
///
/// The same extraction as [`link_destination_span`], one step further along the
/// destination run: where a file move re-renders the path portion and rides the
/// fragment along verbatim (decision 020 clause 4), a *heading* rename is the
/// mirror image — it re-renders the fragment and leaves the path byte-identical
/// (issue 057). The heading-rename engine (`crate::mv`) splices the new slug
/// into exactly this range, so the surrounding `[text]`, delimiters, path
/// spelling, and title are untouched.
///
/// Covers every syntax [`link_destination_span`] does — inline, angle-bracketed,
/// embed, `@import`, raw-HTML `href` / `src` — since both read the same
/// destination run. Returns `None` when the destination carries no `#` at all,
/// and (like [`link_destination_span`]) for a reference-style link or embed,
/// whose destination — fragment included — lives in its `ReferenceDef` URL.
#[must_use]
pub fn link_fragment_span(source: &str, link_span: Span) -> Option<Span> {
    let run = destination_run_span(source, link_span)?;
    let hash = source[run.start..run.end].find('#')?;
    Some(Span::new(run.start + hash + 1, run.end))
}

/// The byte length of a raw destination's path portion: everything before the
/// first `#`. Zero for a fragment-only destination.
pub fn path_portion_len(raw: &str) -> usize {
    raw.split('#').next().unwrap_or(raw).len()
}

/// Locate the byte span of a link's whole destination *run* — the path plus any
/// `#fragment`, excluding delimiters, title, and attribute quotes.
///
/// The shared spine of [`link_destination_span`] (which trims the run at the
/// `#`) and [`link_fragment_span`] (which takes the remainder after it), so the
/// path-axis and fragment-axis edit primitives cannot disagree about where a
/// destination begins and ends.
pub fn destination_run_span(source: &str, link_span: Span) -> Option<Span> {
    let start = link_span.start;
    let end = link_span.end.min(source.len());
    if start >= end {
        return None;
    }
    let slice = &source[start..end];
    let bytes = slice.as_bytes();

    match bytes[0] {
        b'@' => import_run_span(slice, start),
        b'<' => html_resource_attr_run_span(slice, start),
        b'[' => inline_link_run_span(slice, start),
        // An embed node's span opens on the `!`; the destination sits in the
        // `[alt](dest)` remainder, which is the plain inline link shape.
        b'!' if bytes.get(1) == Some(&b'[') => inline_link_run_span(&slice[1..], start + 1),
        _ => None,
    }
}

/// Destination run of an `@dest` import directive: the path after the `@`.
/// `base` is the byte offset of the slice's first byte in the full source.
pub fn import_run_span(slice: &str, base: usize) -> Option<Span> {
    // The import path is the whole slice after the leading `@` (the node span
    // ends exactly at the path's end — `try_parse_import` sets it there).
    let path = &slice[1..];
    if path.is_empty() {
        return None;
    }
    let dest_start = base + 1;
    Some(Span::new(dest_start, dest_start + path.len()))
}

/// Destination run of a raw-HTML resource reference — the `href` attribute of
/// an `<a>` anchor, or the `src` attribute of an `<img>` / `<video>` / `<audio>`
/// / `<iframe>` embed (issue 058). `base` is the byte offset of the slice's first
/// byte in the full source.
///
/// The node kind is not available here (the caller has only a span), so `href`
/// is tried first and `src` second. The two never compete: an `<a>` carries no
/// `src` and an embed tag carries no `href`, and a nested `<img src>` inside an
/// `<a href>`'s span is reached only after the anchor's own `href` has matched.
pub fn html_resource_attr_run_span(slice: &str, base: usize) -> Option<Span> {
    html_attr_value_run_span(slice, base, "href")
        .or_else(|| html_attr_value_run_span(slice, base, "src"))
}

/// Span of one named attribute's quoted value within a raw-HTML tag slice.
/// `base` is the byte offset of the slice's first byte in the full source.
pub fn html_attr_value_run_span(slice: &str, base: usize, attr: &str) -> Option<Span> {
    // Locate the attribute name, then its quoted value. Case-insensitive
    // attribute name, matching the tokenizer; the value delimiter is `"` or `'`.
    let lower = slice.to_ascii_lowercase();
    let mut search = 0;
    while let Some(rel) = lower[search..].find(attr) {
        let name_at = search + rel;
        let after = name_at + attr.len();
        // Skip optional whitespace, require `=`.
        let sbytes = slice.as_bytes();
        let mut i = after;
        while i < sbytes.len() && matches!(sbytes[i], b' ' | b'\t' | b'\n' | b'\r') {
            i += 1;
        }
        if i >= sbytes.len() || sbytes[i] != b'=' {
            search = after;
            continue;
        }
        i += 1;
        while i < sbytes.len() && matches!(sbytes[i], b' ' | b'\t' | b'\n' | b'\r') {
            i += 1;
        }
        if i >= sbytes.len() {
            return None;
        }
        let quote = sbytes[i];
        if quote != b'"' && quote != b'\'' {
            search = after;
            continue;
        }
        let value_start = i + 1;
        let mut j = value_start;
        while j < sbytes.len() && sbytes[j] != quote {
            j += 1;
        }
        if j >= sbytes.len() {
            return None;
        }
        return Some(Span::new(base + value_start, base + j));
    }
    None
}

/// Destination run of an inline `[text](dest …)` link — the bare or
/// angle-bracketed `dest` run. Returns `None` for a reference-style link (no
/// inline `(dest)`). `base` is the byte offset of the slice's first byte in the
/// full source.
pub fn inline_link_run_span(slice: &str, base: usize) -> Option<Span> {
    let bytes = slice.as_bytes();
    // Find the `]` closing the link text, then require `(` immediately after —
    // otherwise this is a reference-style link with no inline destination.
    let close = matching_link_text_close(bytes)?;
    let paren = close + 1;
    if paren >= bytes.len() || bytes[paren] != b'(' {
        return None;
    }
    let mut i = paren + 1;
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t') {
        i += 1;
    }
    if i >= bytes.len() {
        return None;
    }
    if bytes[i] == b'<' {
        // Angle-bracketed: edit inside the brackets.
        let inner_start = i + 1;
        let mut j = inner_start;
        while j < bytes.len() && bytes[j] != b'>' && bytes[j] != b'\n' {
            j += 1;
        }
        if j >= bytes.len() || bytes[j] != b'>' {
            return None;
        }
        return Some(Span::new(base + inner_start, base + j));
    }
    // Bare destination: scan to whitespace or the closing `)`, honoring nested
    // parens, mirroring `parse_dest_url`.
    let dest_start = i;
    let mut paren_depth: i32 = 0;
    while i < bytes.len() {
        match bytes[i] {
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
            b'\\' if i + 1 < bytes.len() => i += 2,
            _ => i += 1,
        }
    }
    if i == dest_start {
        return None;
    }
    Some(Span::new(base + dest_start, base + i))
}

/// Index of the `]` that closes the leading `[` of a link's text, honoring
/// backslash escapes and nested balanced brackets. `bytes[0]` must be `[`.
pub fn matching_link_text_close(bytes: &[u8]) -> Option<usize> {
    let mut depth: i32 = 0;
    let mut i = 0;
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

/// Recognize the opener of a reference-definition label: up to three spaces of
/// indentation, then a `[` that is not a footnote marker (`[^`). Returns the
/// byte index just past the `[`, or `None`. Shared by the cheap gate and the
/// full label scan — only their label-body handling differs.
pub fn refdef_label_open(bytes: &[u8]) -> Option<usize> {
    let len = bytes.len();
    let mut i = 0;
    while i < len && bytes[i] == b' ' {
        i += 1;
    }
    if i > 3 || i >= len || bytes[i] != b'[' {
        return None;
    }
    i += 1;
    // Footnote definitions (`[^...]`) are not reference definitions.
    if i < len && bytes[i] == b'^' {
        return None;
    }
    Some(i)
}

/// Cheap, allocation-free gate: could the first line begin a reference
/// definition? Examines only `line` (the candidate's first line).
///
/// Returns `true` when the line opens a label that either closes with `]:`
/// here, or stays open at the line end (a label may continue on the next
/// line). Returns `false` for ordinary bracketed text such as `[text][ref]`,
/// `[link](url)`, and shortcut references, so they never trigger run
/// collection. Being a fast pre-filter, it tolerates false positives (e.g.
/// `[]:`); the full scan rejects those.
pub fn first_line_opens_refdef(line: &str) -> bool {
    let bytes = line.as_bytes();
    let len = bytes.len();
    let Some(mut i) = refdef_label_open(bytes) else {
        return false;
    };

    loop {
        if i >= len {
            return true; // label still open at end of line — may continue
        }
        match bytes[i] {
            b'\\' if i + 1 < len && bytes[i + 1] < 0x80 => i += 2,
            b'\n' | b'\r' => return true, // label may continue on the next line
            b']' => return bytes.get(i + 1) == Some(&b':'),
            b'[' => return false, // unescaped `[` — not a label
            _ => i += 1,
        }
    }
}

/// Recognize a reference-definition label at the start of `s`.
///
/// The label runs to the first unescaped `]` and may span line endings (the
/// caller's buffer never crosses a blank line, so a label that would need one
/// fails to close); it must contain at least one non-whitespace character and
/// be at most 999 bytes. Returns the byte index just past the `:` and the raw
/// label text, or `None`.
pub fn scan_refdef_label(s: &str) -> Option<(usize, &str)> {
    let bytes = s.as_bytes();
    let len = bytes.len();

    // Label: up to the first unescaped `]`; no unescaped `[`; may span lines.
    let label_start = refdef_label_open(bytes)?;
    let mut i = label_start;
    loop {
        if i >= len {
            return None;
        }
        match bytes[i] {
            b'\\' if i + 1 < len && bytes[i + 1] < 0x80 => i += 2,
            b']' => break,
            b'[' => return None,
            _ => i += 1,
        }
    }
    let label = &s[label_start..i];
    if label.trim().is_empty() || label.len() > 999 {
        return None;
    }
    i += 1; // consume `]`
    if i >= len || bytes[i] != b':' {
        return None;
    }
    Some((i + 1, label))
}

/// Scan a single link reference definition from the start of `s`.
///
/// `s` is the joined content of consecutive non-blank lines (each retaining its
/// line ending). Implements the `CommonMark` grammar with multi-line
/// destinations and titles and backslash escapes, including the
/// through-destination fallback: when a title is started but cannot be
/// completed, a definition valid up through the destination still matches.
///
/// Returns `(consumed_bytes, label, url, title)` for the first definition, or
/// `None` if `s` does not begin with one. `consumed_bytes` always lands on a
/// line boundary (or the end of `s`). The label is normalized.
pub fn scan_one_refdef(s: &str) -> Option<(usize, String, String, String)> {
    let bytes = s.as_bytes();
    let len = bytes.len();

    let (mut i, label) = scan_refdef_label(s)?;

    // Whitespace (including up to one line ending) before the destination.
    i = skip_inline_ws(bytes, i);
    if i < len && (bytes[i] == b'\n' || bytes[i] == b'\r') {
        i = consume_line_ending(bytes, i);
        i = skip_inline_ws(bytes, i);
    }
    // A second line ending (blank line) means there is no destination.
    if i >= len || bytes[i] == b'\n' || bytes[i] == b'\r' {
        return None;
    }

    // Destination.
    let (url, dest_end) = scan_destination(s, i)?;

    // Through-destination checkpoint: spaces/tabs then a line ending (or EOF)
    // after the destination make the definition valid without a title.
    let after_dest_ws = skip_inline_ws(bytes, dest_end);
    let had_trailing_ws = after_dest_ws > dest_end;
    let ckpt_dest = if after_dest_ws >= len {
        Some(len)
    } else if bytes[after_dest_ws] == b'\n' || bytes[after_dest_ws] == b'\r' {
        Some(consume_line_ending(bytes, after_dest_ws))
    } else {
        None
    };

    // Locate a possible title: on the same line, or on the next line when the
    // destination is followed only by whitespace (one line ending).
    let mut title_pos = after_dest_ws;
    let mut title_sep_ok = had_trailing_ws;
    if ckpt_dest.is_some()
        && after_dest_ws < len
        && (bytes[after_dest_ws] == b'\n' || bytes[after_dest_ws] == b'\r')
    {
        let nl_end = consume_line_ending(bytes, after_dest_ws);
        let next = skip_inline_ws(bytes, nl_end);
        if next < len && bytes[next] != b'\n' && bytes[next] != b'\r' {
            title_pos = next;
            title_sep_ok = true;
        }
    }

    if title_sep_ok
        && title_pos < len
        && matches!(bytes[title_pos], b'"' | b'\'' | b'(')
        && let Some((title, title_end)) = scan_title(s, title_pos)
    {
        let after_title_ws = skip_inline_ws(bytes, title_end);
        let ckpt_title = if after_title_ws >= len {
            Some(len)
        } else if bytes[after_title_ws] == b'\n' || bytes[after_title_ws] == b'\r' {
            Some(consume_line_ending(bytes, after_title_ws))
        } else {
            None
        };
        if let Some(end) = ckpt_title {
            return Some((end, normalize_label(label), url, title));
        }
    }

    // Fall back to a definition through the destination only.
    ckpt_dest.map(|end| (end, normalize_label(label), url, String::new()))
}

/// Try to parse the start of a footnote definition.
///
/// Returns `Some(label)` if the line starts with `[^label]:`.
pub fn parse_footnote_def_start(line: &str) -> Option<String> {
    let trimmed = line.trim_start_matches(' ');
    let indent = line.len() - trimmed.len();
    if indent > 3 {
        return None;
    }

    let rest = trimmed.strip_prefix("[^")?;
    let label_end = rest.find(']')?;
    let label = &rest[..label_end];

    if label.is_empty() || label.contains('[') || label.contains(']') {
        return None;
    }

    let after_bracket = &rest[label_end + 1..];
    if !after_bracket.starts_with(':') {
        return None;
    }

    Some(label.to_string())
}

/// Check if a line is an ATX heading opener. Returns `Some(level)` if so.
pub fn atx_heading_level(line: &str) -> Option<u8> {
    let trimmed = line.trim_start_matches(' ');
    // Must have at most 3 leading spaces
    if line.len() - trimmed.len() > 3 {
        return None;
    }

    let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }

    // After the hashes, must be space, tab, or EOL (including newline)
    let after = &trimmed[hashes..];
    if after.is_empty()
        || after.starts_with(' ')
        || after.starts_with('\t')
        || after.starts_with('\n')
        || after.starts_with('\r')
    {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "hashes is in 1..=6, always fits in u8"
        )]
        return Some(hashes as u8);
    }

    None
}

/// Extract the text span and optional `{#id}` from an ATX heading line.
///
/// `line_start` is the byte offset of this line in the original source.
/// `original_line` is the raw line from the source (not tab-expanded).
pub fn extract_atx_content(original_line: &str, line_start: usize) -> (Span, Option<AtxId>) {
    let trimmed = original_line.trim_start_matches(' ');
    let leading_spaces = original_line.len() - trimmed.len();
    let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();

    // Content starts after hashes + optional single space
    let content_start_in_line = leading_spaces + hashes;
    let after_hashes = &original_line[content_start_in_line..];
    let content_offset = if after_hashes.starts_with(' ') {
        content_start_in_line + 1
    } else {
        content_start_in_line
    };

    let content = &original_line[content_offset..];

    // Strip trailing whitespace, then trailing `#` markers, then trailing whitespace
    let content = content.trim_end();
    let stripped_trailing_hashes = content.trim_end_matches('#');
    let content = if stripped_trailing_hashes.is_empty()
        || stripped_trailing_hashes.ends_with(' ')
        || stripped_trailing_hashes.ends_with('\t')
    {
        stripped_trailing_hashes.trim_end()
    } else {
        // The `#` chars are part of the content if not preceded by space
        content
    };

    // Check for `{#id}` attribute at the end
    let (text_content, id) = match content.rfind("{#") {
        Some(attr_start) if content.ends_with('}') => {
            let id_text = &content[attr_start + 2..content.len() - 1];
            let text_before = content[..attr_start].trim_end();

            // Calculate the id span in the original source
            let text_before_end = content_offset + attr_start + 2;
            let id_end = content_offset + content.len() - 1;
            let id_span = Span::new(line_start + text_before_end, line_start + id_end);

            (
                text_before,
                Some(AtxId {
                    id: id_text.to_string(),
                    span: id_span,
                }),
            )
        }
        _ => (content, None),
    };

    // Calculate text span in original source
    let text_byte_start = if text_content.is_empty() {
        content_offset
    } else {
        // Find where text_content starts in original_line via pointer arithmetic
        text_content.as_ptr() as usize - original_line.as_ptr() as usize
    };
    let text_byte_end = text_byte_start + text_content.len();

    (
        Span::new(line_start + text_byte_start, line_start + text_byte_end),
        id,
    )
}

/// Check if a line is a thematic break.
///
/// Three or more matching `*`, `-`, or `_` characters, each optionally
/// separated by spaces or tabs, with no other characters, and at most 3
/// leading spaces. A trailing line ending does not affect the result, so
/// callers may pass raw lines (including the `\n`) directly.
pub fn is_thematic_break(line: &str) -> bool {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return false;
    }

    // Spaces and tabs between markers, and any trailing line ending, are
    // not part of the break sequence.
    let stripped: String = trimmed
        .chars()
        .filter(|c| !matches!(c, ' ' | '\t' | '\n' | '\r'))
        .collect();
    if stripped.len() < 3 {
        return false;
    }

    let first = stripped.as_bytes()[0];
    matches!(first, b'*' | b'-' | b'_') && stripped.bytes().all(|b| b == first)
}

/// Check if a line is a setext heading underline. Returns `Some(level)`.
pub fn setext_level(line: &str) -> Option<u8> {
    let trimmed = line.trim_start_matches(' ');
    if line.len() - trimmed.len() > 3 {
        return None;
    }

    let trimmed = trimmed.trim_end();
    if trimmed.is_empty() {
        return None;
    }

    let first = trimmed.as_bytes()[0];
    if first == b'=' && trimmed.bytes().all(|b| b == b'=') {
        Some(1)
    } else if first == b'-' && trimmed.bytes().all(|b| b == b'-') {
        Some(2)
    } else {
        None
    }
}

/// Check if a line opens a fenced code block. Returns the fence character,
/// fence length, and info string if so.
pub fn fenced_code_open(line: &str) -> Option<(u8, usize, Option<String>)> {
    let trimmed = line.trim_start_matches(' ');
    let indent = line.len() - trimmed.len();
    if indent > 3 {
        return None;
    }

    let fence_char = trimmed.as_bytes().first().copied()?;
    if fence_char != b'`' && fence_char != b'~' {
        return None;
    }

    let fence_len = trimmed.bytes().take_while(|&b| b == fence_char).count();
    if fence_len < 3 {
        return None;
    }

    // Backtick fences cannot have backticks in the info string
    let info_part = trimmed[fence_len..].trim();
    if fence_char == b'`' && info_part.contains('`') {
        return None;
    }

    let info = if info_part.is_empty() {
        None
    } else {
        Some(info_part.to_string())
    };

    Some((fence_char, fence_len, info))
}

/// Check if a line closes a fenced code block.
pub fn fenced_code_close(line: &str, fence_char: u8, open_len: usize) -> bool {
    let trimmed = line.trim_start_matches(' ');
    let indent = line.len() - trimmed.len();
    if indent > 3 {
        return false;
    }

    let close_len = trimmed.bytes().take_while(|&b| b == fence_char).count();
    if close_len < open_len {
        return false;
    }

    // Nothing after the fence except whitespace
    trimmed[close_len..].trim().is_empty()
}

/// Check if a line opens a block math span (`$$`).
pub fn block_math_open(line: &str) -> bool {
    let trimmed = line.trim_start_matches(' ');
    let indent = line.len() - trimmed.len();
    if indent > 3 {
        return false;
    }

    if !trimmed.starts_with("$$") {
        return false;
    }

    // After `$$`, must be whitespace, newline, or EOL
    let after = &trimmed[2..];
    after.is_empty()
        || after.starts_with(' ')
        || after.starts_with('\t')
        || after.starts_with('\n')
        || after.starts_with('\r')
}

/// Check if a line closes a block math span (`$$`).
pub fn block_math_close(line: &str) -> bool {
    line.trim() == "$$"
}

/// `CommonMark` HTML block types 1–7.
///
/// Returns the type number (1–7) if the line starts an HTML block, or
/// `None` otherwise.
pub fn html_block_start(line: &str) -> Option<u8> {
    let trimmed = line.trim_start_matches(' ');
    let indent = line.len() - trimmed.len();
    if indent > 3 {
        return None;
    }

    if !trimmed.starts_with('<') {
        return None;
    }

    let lower = trimmed.to_lowercase();

    // Type 1: <pre, <script, <style, <textarea (case-insensitive)
    for tag in &["<pre", "<script", "<style", "<textarea"] {
        if lower.strip_prefix(tag).is_some_and(|after| {
            after.is_empty()
                || after.starts_with(' ')
                || after.starts_with('\t')
                || after.starts_with('>')
                || after.starts_with('\n')
                || after.starts_with('\r')
        }) {
            return Some(1);
        }
    }

    // Type 2: <!-- (HTML comment)
    if lower.starts_with("<!--") {
        return Some(2);
    }

    // Type 3: <? (processing instruction)
    if lower.starts_with("<?") {
        return Some(3);
    }

    // Type 4: <! followed by uppercase letter (declaration)
    if trimmed.len() >= 3
        && trimmed.as_bytes()[0] == b'<'
        && trimmed.as_bytes()[1] == b'!'
        && trimmed.as_bytes()[2].is_ascii_uppercase()
    {
        return Some(4);
    }

    // Type 5: <![CDATA[
    if lower.starts_with("<![cdata[") {
        return Some(5);
    }

    // Type 6: block-level HTML tags
    if extract_html_tag_name(trimmed).is_some_and(|name| is_block_html_tag(&name)) {
        return Some(6);
    }

    // Type 7: any other tag (open or closing), not starting a paragraph
    if is_html_tag_line(trimmed) {
        return Some(7);
    }

    None
}

/// Extract the tag name from an HTML-like line, lowercased.
pub fn extract_html_tag_name(line: &str) -> Option<String> {
    let rest = line.strip_prefix('<')?;
    let rest = rest.strip_prefix('/').unwrap_or(rest);

    let end = rest
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '-')
        .unwrap_or(rest.len());

    if end == 0 {
        return None;
    }

    Some(rest[..end].to_lowercase())
}

/// Check if a tag name is a block-level HTML tag per the `CommonMark` spec.
pub fn is_block_html_tag(name: &str) -> bool {
    matches!(
        name,
        "address"
            | "article"
            | "aside"
            | "base"
            | "basefont"
            | "blockquote"
            | "body"
            | "caption"
            | "center"
            | "col"
            | "colgroup"
            | "dd"
            | "details"
            | "dialog"
            | "dir"
            | "div"
            | "dl"
            | "dt"
            | "fieldset"
            | "figcaption"
            | "figure"
            | "footer"
            | "form"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "head"
            | "header"
            | "hr"
            | "html"
            | "iframe"
            | "legend"
            | "li"
            | "link"
            | "main"
            | "menu"
            | "menuitem"
            | "nav"
            | "noframes"
            | "ol"
            | "optgroup"
            | "option"
            | "p"
            | "param"
            | "search"
            | "section"
            | "summary"
            | "table"
            | "tbody"
            | "td"
            | "template"
            | "tfoot"
            | "th"
            | "thead"
            | "title"
            | "tr"
            | "track"
            | "ul"
    )
}

/// Check if a line looks like an HTML open or close tag (for type 7).
pub fn is_html_tag_line(line: &str) -> bool {
    if !line.starts_with('<') {
        return false;
    }

    let rest = &line[1..];
    let is_close = rest.starts_with('/');
    let rest = if is_close { &rest[1..] } else { rest };

    // Must start with an ASCII letter
    let first = rest.as_bytes().first().copied().unwrap_or(0);
    if !first.is_ascii_alphabetic() {
        return false;
    }

    // Tag name: letters, digits, hyphens
    let name_end = rest
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '-')
        .unwrap_or(rest.len());

    if name_end == 0 {
        return false;
    }

    let after_name = rest[name_end..].trim();

    // For close tags, must end with >
    if is_close {
        return after_name.is_empty() || after_name == ">";
    }

    // For open tags, the rest must be attributes and end with > or />
    after_name.is_empty()
        || after_name.ends_with('>')
        || after_name.ends_with("/>")
        || after_name.contains('>')
}

/// Check if a line ends an HTML block of the given type.
pub fn html_block_end(line: &str, html_type: u8) -> bool {
    let lower = line.to_lowercase();
    match html_type {
        1 => {
            lower.contains("</pre>")
                || lower.contains("</script>")
                || lower.contains("</style>")
                || lower.contains("</textarea>")
        }
        2 => lower.contains("-->"),
        3 => lower.contains("?>"),
        4 => lower.contains('>'),
        5 => lower.contains("]]>"),
        // Types 6 and 7 are terminated by a blank line, not by content
        _ => false,
    }
}

/// Whether the closing tag for `tag_name` appears on the same line after
/// the opening tag. `open_len` is the byte length of the opening tag
/// (from [`HtmlTag::Open::len`]).
pub fn has_close_on_same_line(line: &str, tag_name: &str, open_len: usize) -> bool {
    let mut rest = &line[open_len..];
    while let Some(idx) = rest.find("</") {
        if let Some(HtmlTag::Close { ref name, .. }) = html::tokenize_tag(&rest[idx..], 0)
            && name == tag_name
        {
            return true;
        }
        rest = &rest[idx + 2..];
    }
    false
}

/// Check if a line opens a `<pre><code>` block (case-insensitive).
pub fn is_pre_code_open(line: &str) -> bool {
    let lower = line.trim().to_lowercase();
    if let Some(after) = lower.strip_prefix("<pre>") {
        return after.trim_start().starts_with("<code");
    }
    // <pre followed by whitespace then > (e.g. <pre >) is also type 1,
    // but the <code> must follow the closing >.
    false
}

// ---------------------------------------------------------------------------
// List helpers
// ---------------------------------------------------------------------------

/// Information about a recognized list marker.
pub struct ListMarkerInfo {
    /// Whether this is an ordered list.
    pub ordered: bool,
    /// The marker character: bullet char (`-`, `*`, `+`) for unordered,
    /// or delimiter (`.`, `)`) for ordered.
    pub marker_char: u8,
    /// Start number for ordered lists, 0 for unordered.
    pub start: u32,
    /// Column where the marker starts (leading spaces).
    pub marker_indent: usize,
    /// Column where item content starts (after marker + spaces).
    pub content_column: usize,
    /// Byte offset into the line where content begins.
    pub content_offset: usize,
}

/// Recognize a list marker at the start of a (tab-expanded) line.
///
/// Returns `None` if the line doesn't start with a list marker, or if
/// the line is actually a thematic break.
pub fn recognize_list_marker(line: &str) -> Option<ListMarkerInfo> {
    let trimmed = line.trim_start_matches(' ');
    let indent = line.len() - trimmed.len();
    if indent > 3 || trimmed.is_empty() {
        return None;
    }

    // Reject thematic breaks — they take priority over list markers.
    let trimmed_end = trimmed.trim_end();
    if is_thematic_break(trimmed_end) {
        return None;
    }

    let first = trimmed.as_bytes()[0];

    if matches!(first, b'-' | b'*' | b'+') {
        let after_marker = &trimmed[1..];
        // Bare marker (nothing or only whitespace/newline after).
        if after_marker.is_empty() || after_marker.trim_end().is_empty() {
            return Some(ListMarkerInfo {
                ordered: false,
                marker_char: first,
                start: 0,
                marker_indent: indent,
                content_column: indent + 2,
                content_offset: line.len(),
            });
        }
        // Normal case: marker char + at least one space + content.
        if !after_marker.starts_with(' ') {
            return None;
        }
        let spaces_after = after_marker.len() - after_marker.trim_start_matches(' ').len();
        // If rest is blank, content column = marker pos + 2.
        // If > 4 spaces after marker with content, cap to marker + 1
        // (excess spaces become indented code within the item).
        let (content_column, content_offset) = if after_marker.trim().is_empty() {
            (indent + 2, line.len())
        } else if spaces_after > 4 {
            (indent + 2, indent + 2)
        } else {
            let cc = indent + 1 + spaces_after;
            (cc, cc)
        };
        Some(ListMarkerInfo {
            ordered: false,
            marker_char: first,
            start: 0,
            marker_indent: indent,
            content_column,
            content_offset,
        })
    } else if first.is_ascii_digit() {
        // Ordered: digits + delimiter (. or )) + at least one space.
        let digit_count = trimmed.bytes().take_while(u8::is_ascii_digit).count();
        if digit_count == 0 || digit_count > 9 {
            return None;
        }
        let after_digits = &trimmed[digit_count..];
        if after_digits.is_empty() {
            return None;
        }
        let delimiter = after_digits.as_bytes()[0];
        if !matches!(delimiter, b'.' | b')') {
            return None;
        }
        let after_delim = &after_digits[1..];
        let start: u32 = trimmed[..digit_count].parse().ok()?;
        let marker_width = digit_count + 1; // digits + delimiter
        // Bare ordered marker (nothing or only whitespace/newline after delimiter).
        if after_delim.is_empty() || after_delim.trim_end().is_empty() {
            return Some(ListMarkerInfo {
                ordered: true,
                marker_char: delimiter,
                start,
                marker_indent: indent,
                content_column: indent + marker_width + 1,
                content_offset: line.len(),
            });
        }
        // Normal case: delimiter + at least one space + content.
        if !after_delim.starts_with(' ') {
            return None;
        }
        let spaces_after = after_delim.len() - after_delim.trim_start_matches(' ').len();
        // If rest is blank, content column = marker + 1.
        // If > 4 spaces after delimiter with content, cap to marker + 1
        // (excess spaces become indented code within the item).
        let (content_column, content_offset) = if after_delim.trim().is_empty() {
            (indent + marker_width + 1, line.len())
        } else if spaces_after > 4 {
            let cc = indent + marker_width + 1;
            (cc, cc)
        } else {
            let cc = indent + marker_width + spaces_after;
            (cc, cc)
        };
        Some(ListMarkerInfo {
            ordered: true,
            marker_char: delimiter,
            start,
            marker_indent: indent,
            content_column,
            content_offset,
        })
    } else {
        None
    }
}

/// Recognize a task list item checkbox at the start of item content.
///
/// Returns `Some(false)` for `[ ] `, `Some(true)` for `[x] ` or `[X] `.
pub fn recognize_task(content: &str) -> Option<bool> {
    if content.starts_with("[ ] ") {
        Some(false)
    } else if content.starts_with("[x] ") || content.starts_with("[X] ") {
        Some(true)
    } else {
        None
    }
}

/// Tracking state for an open list on the scope stack.
pub struct ListContext {
    /// The `List` node ID in the tree.
    pub list_node: NodeId,
    /// The current `ListItem` node ID.
    pub item_node: NodeId,
    /// Marker character: bullet for unordered, delimiter for ordered.
    pub marker_char: u8,
    /// Whether this is an ordered list.
    pub ordered: bool,
    /// Column where item content starts (in stripped coordinates).
    pub content_column: usize,
    /// Cumulative indent from parent lists / blockquotes.
    /// The real marker indent in raw coordinates is `base_indent + marker_indent`.
    pub base_indent: usize,
    /// A blank line was seen in the current item.
    pub saw_blank: bool,
    /// Any blank line appeared between items (list is loose).
    pub loose: bool,
}

// ---------------------------------------------------------------------------
// Table helpers
// ---------------------------------------------------------------------------

/// Parse a GFM delimiter row and return per-column alignments.
///
/// A delimiter row consists of cells separated by pipes, where each cell
/// is optional `:`, at least one `-`, optional `:`, surrounded by optional
/// spaces. Returns `None` if the line is not a valid delimiter row or has
/// zero columns.
pub fn parse_delimiter_row(line: &str) -> Option<Vec<TableAlignment>> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Strip optional leading/trailing pipes.
    let inner = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let inner = inner.strip_suffix('|').unwrap_or(inner);

    if inner.trim().is_empty() {
        return None;
    }

    let mut alignments = Vec::new();
    for cell in inner.split('|') {
        let cell = cell.trim();
        if cell.is_empty() {
            return None;
        }
        let left = cell.starts_with(':');
        let right = cell.ends_with(':');
        let dashes = cell
            .trim_start_matches(':')
            .trim_end_matches(':')
            .trim_matches(' ');
        if dashes.is_empty() || !dashes.bytes().all(|b| b == b'-') {
            return None;
        }
        alignments.push(match (left, right) {
            (true, true) => TableAlignment::Center,
            (false, true) => TableAlignment::Right,
            _ => TableAlignment::Left,
        });
    }

    if alignments.is_empty() {
        None
    } else {
        Some(alignments)
    }
}

/// Split a table row into cell content spans, respecting backtick code spans.
///
/// Pipes inside backtick code spans do not split cells. Returns byte offsets
/// relative to `row_start` for each cell's trimmed content.
pub fn split_table_cells(line: &str, row_start: usize) -> Vec<Span> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    // Locate `trimmed` within `line`.
    let trim_offset = line.len() - line.trim_start().len();
    let inner_start_in_line = trim_offset;

    // Strip optional leading pipe.
    let (inner, inner_offset) = trimmed
        .strip_prefix('|')
        .map_or((trimmed, inner_start_in_line), |stripped| {
            (stripped, inner_start_in_line + 1)
        });

    // Strip optional trailing pipe.
    let inner = if inner.ends_with('|') && !inner.ends_with("\\|") {
        &inner[..inner.len() - 1]
    } else {
        inner
    };

    let bytes = inner.as_bytes();
    let mut cells = Vec::new();
    let mut cell_start = 0;
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'`' {
            // Skip a backtick code span. Per CommonMark a span opened by a run
            // of N backticks closes only on the next run of *exactly* N; a
            // longer inner run (e.g. ``` inside a `` span) is literal content
            // and must not be mistaken for the close. A plain substring search
            // for N backticks would match the first N of a longer run, desync,
            // and swallow `|` delimiters past the real close.
            let bt_count = crate::inline::count_char(bytes, i, b'`');
            let after = i + bt_count;
            i = crate::inline::find_closing_backticks(bytes, after, bt_count)
                .unwrap_or(bytes.len());
        } else if bytes[i] == b'\\' && i + 1 < bytes.len() && bytes[i + 1] == b'|' {
            // Escaped pipe — skip both characters.
            i += 2;
        } else if bytes[i] == b'|' {
            // Cell boundary.
            let raw = &inner[cell_start..i];
            let cell_trimmed = raw.trim();
            if cell_trimmed.is_empty() {
                cells.push(Span::new(
                    row_start + inner_offset + cell_start,
                    row_start + inner_offset + cell_start,
                ));
            } else {
                let leading = raw.len() - raw.trim_start().len();
                let s = cell_start + leading;
                let e = s + cell_trimmed.len();
                cells.push(Span::new(
                    row_start + inner_offset + s,
                    row_start + inner_offset + e,
                ));
            }
            cell_start = i + 1;
            i += 1;
        } else {
            i += 1;
        }
    }

    // Last cell after the final pipe.
    let raw = &inner[cell_start..];
    let cell_trimmed = raw.trim();
    if cell_trimmed.is_empty() {
        cells.push(Span::new(
            row_start + inner_offset + cell_start,
            row_start + inner_offset + cell_start,
        ));
    } else {
        let leading = raw.len() - raw.trim_start().len();
        let s = cell_start + leading;
        let e = s + cell_trimmed.len();
        cells.push(Span::new(
            row_start + inner_offset + s,
            row_start + inner_offset + e,
        ));
    }

    cells
}

/// Check if a line could be a table row (has at least one unescaped pipe
/// outside backtick code spans).
pub fn is_table_row(line: &str) -> bool {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            // Same exact-length backtick-run close as `split_table_cells`; a
            // longer inner run must not be mistaken for the closing run.
            let bt_count = crate::inline::count_char(bytes, i, b'`');
            let after = i + bt_count;
            i = crate::inline::find_closing_backticks(bytes, after, bt_count)
                .unwrap_or(bytes.len());
        } else if bytes[i] == b'\\' && i + 1 < bytes.len() && bytes[i + 1] == b'|' {
            i += 2;
        } else if bytes[i] == b'|' {
            return true;
        } else {
            i += 1;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Block quote helpers
// ---------------------------------------------------------------------------

/// Detect a GFM admonition marker in blockquote content.
///
/// Returns the admonition type (e.g. `NOTE`, `WARNING`) if the content
/// starts with `[!TYPE]`, or `None` otherwise.
pub fn detect_admonition(content: &str) -> Option<String> {
    let trimmed = content.trim();
    let after = trimmed.strip_prefix("[!")?;
    let end = after.find(']')?;
    let kind = &after[..end];
    if kind.is_empty() || !kind.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    // Must be the only content on the line (possibly followed by whitespace).
    let rest = after[end + 1..].trim();
    if rest.is_empty() {
        Some(kind.to_uppercase())
    } else {
        None
    }
}

/// Strip the leading `> ` or `>` from a block quote line.
///
/// Returns `Some((stripped_bytes, content))` where `stripped_bytes` is how
/// many bytes of the original line were consumed by the marker and
/// `content` is the remainder.
pub fn strip_blockquote_marker(line: &str) -> Option<(usize, &str)> {
    let trimmed = line.trim_start_matches(' ');
    let indent = line.len() - trimmed.len();
    if indent > 3 {
        return None;
    }

    let after_gt = trimmed.strip_prefix('>')?;
    Some(
        after_gt
            .strip_prefix(' ')
            .map_or((indent + 1, after_gt), |content| (indent + 2, content)),
    )
}
