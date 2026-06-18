// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

// CommonMark 0.31.2 conformance tests.
//
// Validates Lattice's block structure parser against the official
// CommonMark spec test suite (vendored at `tests/fixtures/commonmark_spec.json`).
//
// Block-structure sections are tested with the `Shape` comparator below. The
// "Emphasis and strong emphasis" section is additionally tested with the
// inline-aware emphasis-mask comparator (ticket 27). The remaining inline
// sections (code spans, links, images, etc.) are still skipped — Lattice does
// not model their content against the spec.
//
// Assertion model
// ---------------
// The spec defines input markdown and expected HTML output. Lattice does
// not produce HTML, so a translation layer parses expected HTML into a
// `Shape` tree representing block structure. The same shape is extracted
// from Lattice's `Tree`. Structural equivalence is then asserted:
//
// - Correct block element types and nesting.
// - Heading levels.
// - List properties (ordered, start number, tight/loose).
// - Block quote child structure.
//
// For emphasis, the `Shape` layer returns `None` (it has no inline variant), so
// a second comparator (see "Inline emphasis comparator" below) reduces both the
// expected HTML and Lattice's tree to a per-character bold/italic mask and
// compares those.
//
// Intentional deviations
// ----------------------
// Some spec examples produce different structure in Lattice because:
//
// - Lattice models HTML containers (`<div>`, `<details>`, etc.) as scoped
//   nodes rather than opaque HTML blocks.
// - Lattice handles some setext heading and lazy continuation edge cases
//   differently from the spec.
//
// These are documented in `DEVIATIONS` and skipped during the test run.

use super::{parse, ElementKind, NodeId, Tree};

// ---------------------------------------------------------------------------
// Spec example
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct SpecExample {
    markdown: String,
    html: String,
    example: u32,
    start_line: u32,
    section: String,
}

// ---------------------------------------------------------------------------
// Block shape for structural comparison
// ---------------------------------------------------------------------------

/// Simplified block structure for comparing spec expectations against
/// the parser's tree output.
#[derive(Debug, Clone, PartialEq)]
enum Shape {
    Heading(u8),
    ThematicBreak,
    Paragraph,
    CodeBlock,
    HtmlBlock,
    BlockQuote(Vec<Self>),
    List {
        ordered: bool,
        start: u32,
        tight: bool,
        items: Vec<Vec<Self>>,
    },
}

// ---------------------------------------------------------------------------
// Spec loading
// ---------------------------------------------------------------------------

fn load_spec() -> Vec<SpecExample> {
    let json = include_str!("../tests/fixtures/commonmark_spec.json");
    serde_json::from_str(json).expect("spec.json should parse as valid JSON")
}

fn section_examples<'a>(all: &'a [SpecExample], section: &str) -> Vec<&'a SpecExample> {
    all.iter().filter(|e| e.section == section).collect()
}

// ---------------------------------------------------------------------------
// Tree → Shape
// ---------------------------------------------------------------------------

fn tree_shapes(tree: &Tree) -> Vec<Shape> {
    children_shapes(tree, tree.root(), false)
}

fn children_shapes(tree: &Tree, parent: NodeId, tight: bool) -> Vec<Shape> {
    tree.children(parent)
        .iter()
        .filter_map(|&id| node_shape(tree, id, tight))
        .collect()
}

fn node_shape(tree: &Tree, id: NodeId, tight: bool) -> Option<Shape> {
    let node = tree.node(id);
    match &node.kind {
        ElementKind::Heading { level } => Some(Shape::Heading(*level)),
        ElementKind::Rules => Some(Shape::ThematicBreak),
        ElementKind::Paragraph => {
            // Tight list items omit paragraph wrappers in HTML output.
            if tight {
                None
            } else {
                Some(Shape::Paragraph)
            }
        }
        ElementKind::CodeBlock | ElementKind::Math => Some(Shape::CodeBlock),
        ElementKind::QuoteBlock | ElementKind::Admonition { .. } => {
            Some(Shape::BlockQuote(children_shapes(tree, id, false)))
        }
        ElementKind::List {
            ordered,
            start,
            tight: t,
        } => {
            let items = tree
                .children(id)
                .iter()
                .filter(|&&cid| matches!(tree.node(cid).kind, ElementKind::ListItem { .. }))
                .map(|&cid| children_shapes(tree, cid, *t))
                .collect();
            Some(Shape::List {
                ordered: *ordered,
                start: *start,
                tight: *t,
                items,
            })
        }
        // HTML elements that Lattice models as structured nodes but the
        // spec treats as opaque HTML blocks.
        ElementKind::HtmlBlock
        | ElementKind::Container
        | ElementKind::Details
        | ElementKind::DetailsSummary
        | ElementKind::FormControl
        | ElementKind::DefinitionList
        | ElementKind::DefinitionTerm
        | ElementKind::DefinitionDesc
        | ElementKind::Table { .. }
        | ElementKind::TableRow { .. }
        | ElementKind::TableCell => Some(Shape::HtmlBlock),
        // Nodes that produce no HTML output (ReferenceDef, FootnoteDef,
        // Frontmatter), and inline elements, list items, table parts —
        // not top-level block structure.
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// HTML → Shape
// ---------------------------------------------------------------------------

fn html_shapes(html: &str) -> Vec<Shape> {
    parse_html_blocks(html)
}

fn parse_html_blocks(html: &str) -> Vec<Shape> {
    let mut shapes = Vec::new();
    let mut pos = 0;

    while pos < html.len() {
        pos = skip_ws(html, pos);
        if pos >= html.len() {
            break;
        }

        if html.as_bytes()[pos] != b'<' {
            // Non-tag text (bare content in list items, etc.) — skip.
            pos = next_lt(html, pos);
            continue;
        }

        // Close tag → stop (we're inside a container scope).
        if html[pos..].starts_with("</") {
            break;
        }

        if let Some((shape, end)) = try_hr(html, pos)
            .or_else(|| try_heading(html, pos))
            .or_else(|| try_pre_code(html, pos))
            .or_else(|| try_paragraph(html, pos))
            .or_else(|| try_blockquote(html, pos))
            .or_else(|| try_list(html, pos))
        {
            shapes.push(shape);
            pos = end;
        } else if let Some(end) = skip_unknown_html(html, pos) {
            shapes.push(Shape::HtmlBlock);
            pos = end;
        } else {
            pos += 1;
        }
    }

    shapes
}

fn skip_ws(s: &str, start: usize) -> usize {
    s[start..]
        .find(|c: char| !c.is_ascii_whitespace())
        .map_or(s.len(), |i| start + i)
}

fn next_lt(s: &str, start: usize) -> usize {
    s[start..].find('<').map_or(s.len(), |i| start + i)
}

// --- Thematic break ---

fn try_hr(html: &str, pos: usize) -> Option<(Shape, usize)> {
    for pat in &["<hr />", "<hr/>", "<hr>"] {
        if html[pos..].starts_with(pat) {
            return Some((Shape::ThematicBreak, pos + pat.len()));
        }
    }
    None
}

// --- Heading ---

fn try_heading(html: &str, pos: usize) -> Option<(Shape, usize)> {
    let rest = &html[pos..];
    if rest.len() < 5 || !rest.starts_with("<h") {
        return None;
    }
    let lvl = rest.as_bytes()[2];
    if !(b'1'..=b'6').contains(&lvl) || rest.as_bytes()[3] != b'>' {
        return None;
    }
    let level = lvl - b'0';
    let close = format!("</h{level}>");
    let end = rest.find(&close)? + close.len();
    Some((Shape::Heading(level), pos + end))
}

// --- Code block (<pre><code>) ---

fn try_pre_code(html: &str, pos: usize) -> Option<(Shape, usize)> {
    // Only `<pre><code` — matches Lattice's `is_pre_code_open` behavior.
    // Bare `<pre>` (without `<code>`) is raw HTML, not a code block.
    if !html[pos..].starts_with("<pre><code") {
        return None;
    }
    let end = html[pos..].find("</pre>")? + "</pre>".len();
    Some((Shape::CodeBlock, pos + end))
}

// --- Paragraph ---

fn try_paragraph(html: &str, pos: usize) -> Option<(Shape, usize)> {
    let rest = &html[pos..];
    if !rest.starts_with("<p>") && !rest.starts_with("<p ") {
        return None;
    }
    let end = rest.find("</p>")? + "</p>".len();
    Some((Shape::Paragraph, pos + end))
}

// --- Block quote ---

fn try_blockquote(html: &str, pos: usize) -> Option<(Shape, usize)> {
    if !tag_opens_at(html, pos, "blockquote") {
        return None;
    }
    let open_end = pos + html[pos..].find('>')? + 1;
    let close_pos = find_close(html, open_end, "blockquote")?;
    let children = parse_html_blocks(&html[open_end..close_pos]);
    Some((
        Shape::BlockQuote(children),
        close_pos + "</blockquote>".len(),
    ))
}

// --- List ---

fn try_list(html: &str, pos: usize) -> Option<(Shape, usize)> {
    let (ordered, tag) = if tag_opens_at(html, pos, "ul") {
        (false, "ul")
    } else if tag_opens_at(html, pos, "ol") {
        (true, "ol")
    } else {
        return None;
    };

    let open_tag_end = html[pos..].find('>')?;
    let start_num = if ordered {
        parse_start_attr(&html[pos..pos + open_tag_end])
    } else {
        0
    };
    let open_end = pos + open_tag_end + 1;
    let close_pos = find_close(html, open_end, tag)?;
    let inner = &html[open_end..close_pos];
    let li_contents = extract_li_contents(inner);

    let item_shapes: Vec<Vec<Shape>> = li_contents
        .iter()
        .map(|c| parse_html_blocks(c))
        .collect();

    let tight = !item_shapes
        .iter()
        .any(|shapes| shapes.iter().any(|s| matches!(s, Shape::Paragraph)));

    let close_tag_len = tag.len() + 3; // "</" + tag + ">"
    Some((
        Shape::List {
            ordered,
            start: start_num,
            tight,
            items: item_shapes,
        },
        close_pos + close_tag_len,
    ))
}

fn parse_start_attr(tag_text: &str) -> u32 {
    tag_text
        .find("start=\"")
        .and_then(|i| {
            let rest = &tag_text[i + 7..];
            rest.find('"').and_then(|j| rest[..j].parse().ok())
        })
        .unwrap_or(1)
}

fn extract_li_contents(inner: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut pos = 0;

    while pos < inner.len() {
        pos = skip_ws(inner, pos);
        if pos >= inner.len() {
            break;
        }

        if !tag_opens_at(inner, pos, "li") {
            pos += 1;
            continue;
        }

        let Some(tag_end_rel) = inner[pos..].find('>') else {
            break;
        };
        let tag_end = pos + tag_end_rel + 1;

        let Some(close_pos) = find_close(inner, tag_end, "li") else {
            break;
        };

        items.push(inner[tag_end..close_pos].to_string());
        pos = close_pos + "</li>".len();
    }

    items
}

// --- Tag helpers ---

/// Check if a specific HTML tag opens at `pos` (e.g. `<ul>`, `<ul `, `<ul\n`).
///
/// Verifies the character after the tag name is a tag boundary (`>`, space,
/// newline, tab, or `/`), preventing false matches like `<link>` when
/// searching for `<li`.
fn tag_opens_at(html: &str, pos: usize, tag: &str) -> bool {
    let rest = &html[pos..];
    let prefix_len = tag.len() + 1; // '<' + tag name
    if rest.len() < prefix_len {
        return false;
    }
    if rest.as_bytes()[0] != b'<' || !rest[1..].starts_with(tag) {
        return false;
    }
    // Boundary check: character after tag name must end the tag name.
    if rest.len() == prefix_len {
        return true;
    }
    matches!(
        rest.as_bytes()[prefix_len],
        b'>' | b' ' | b'\n' | b'\t' | b'/'
    )
}

/// Find the matching close tag, tracking nesting depth.
fn find_close(html: &str, start: usize, tag: &str) -> Option<usize> {
    let close = format!("</{tag}>");
    let mut depth = 1u32;
    let mut pos = start;

    while pos < html.len() {
        if html[pos..].starts_with(&close) {
            depth -= 1;
            if depth == 0 {
                return Some(pos);
            }
            pos += close.len();
        } else if tag_opens_at(html, pos, tag) {
            depth += 1;
            // Skip past the open tag to avoid re-matching.
            pos = html[pos..].find('>').map_or(html.len(), |i| pos + i + 1);
        } else {
            pos += 1;
        }
    }

    None
}

/// Skip an unknown HTML element (comment, PI, CDATA, declaration, or
/// regular open/close tag pair). Returns the byte position after the element.
fn skip_unknown_html(html: &str, pos: usize) -> Option<usize> {
    let rest = &html[pos..];

    // Comment: <!-- ... -->
    if rest.starts_with("<!--") {
        return rest.find("-->").map(|i| pos + i + 3);
    }
    // Processing instruction: <? ... ?>
    if rest.starts_with("<?") {
        return rest.find("?>").map(|i| pos + i + 2);
    }
    // CDATA: <![CDATA[ ... ]]>
    if rest.starts_with("<![CDATA[") {
        return rest.find("]]>").map(|i| pos + i + 3);
    }
    // Declaration: <! ... >
    if rest.starts_with("<!") {
        return rest.find('>').map(|i| pos + i + 1);
    }
    // Must be a regular open tag.
    if rest.len() < 2 || !rest.as_bytes()[1].is_ascii_alphabetic() {
        return Some(pos + 1);
    }

    // Extract tag name.
    let name_end = rest[1..]
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '-')
        .map_or(rest.len(), |i| i + 1);
    let tag = &rest[1..name_end];
    if tag.is_empty() {
        return Some(pos + 1);
    }

    let gt = rest.find('>')?;
    let open_end = pos + gt + 1;

    // Self-closing: <tag ... />
    if rest[..=gt].ends_with("/>") {
        return Some(open_end);
    }

    // Find matching close tag or treat as standalone.
    find_close(html, open_end, tag)
        .map(|cp| cp + tag.len() + 3) // "</tag>"
        .or(Some(open_end))
}

// ---------------------------------------------------------------------------
// Inline emphasis comparator (ticket 27)
// ---------------------------------------------------------------------------
//
// The block-shape comparison above returns `None` for every inline element, so
// the "Emphasis and strong emphasis" section is invisible to it. This second,
// inline-aware comparator closes that gap.
//
// Both the expected HTML and Lattice's tree are reduced to the *same*
// representation: a per-content-character emphasis mask — a `Vec<(char, Mask)>`
// over the example's rendered text content, where `Mask` records which of
// bold / italic is active over that character. Comparing the two vectors checks
// both *which* characters survive as text content and *what* emphasis applies
// to each, in one shot.
//
// This reduction neutralizes parser 26's two structural mismatches against the
// spec uniformly:
//
//   - Delimiter-vs-content: Lattice spans cover the delimiters (`*bar*`), the
//     HTML `<em>` wraps only the content (`bar`). Both sides strip their markup,
//     so only the content text is compared.
//   - Flat-overlap-vs-nesting: parser 26 emits flat, *overlapping* sibling spans
//     (`***foo***` -> a `Strong` over `**foo**` and an `Emphasis` over the whole
//     run), while the HTML nests `<em><strong>`. ORing each span's modifier bit
//     over the characters it covers collapses both to the same per-character
//     mask — exactly the cut-point flatten the semantic-tokens layer (feat 15,
//     `collect_emphasis_regions`) already performs.
//
// CommonMark 0.31.2 has no strikethrough examples (strikethrough is GFM), so the
// comparator models only bold (`<strong>`) and italic (`<em>`).

/// Per-character emphasis state: bit 0 = bold, bit 1 = italic.
type Mask = u8;

const BOLD: Mask = 0b01;
const ITALIC: Mask = 0b10;

/// One rendered text character paired with the emphasis active over it.
type MaskedText = Vec<(char, Mask)>;

// --- HTML -> masked text ---

/// Reduce an example's expected HTML to its rendered text content, tagging each
/// character with the bold/italic emphasis active over it.
///
/// `<strong>`/`<em>` open and close tags drive the bold/italic depth; every
/// other tag (`<a>`, `<code>`, `<img>`, ...) is skipped while its inner text is
/// kept under the current mask. The handful of HTML entities the spec emits in
/// this section are decoded so the text aligns with Lattice's source bytes.
///
/// Only text *inside* a `<p>` block is collected: the inter-block `\n` the spec
/// renders between `</p>` and the next `<p>` is not content, and the tree side
/// (which walks paragraphs directly) emits nothing there either. A soft-break
/// `\n` inside a single paragraph stays, since it is inside the `<p>`.
fn html_emphasis_mask(html: &str) -> MaskedText {
    let bytes = html.as_bytes();
    let mut out: MaskedText = Vec::new();
    let mut bold = 0u32;
    let mut italic = 0u32;
    let mut in_para = false;
    let mut i = 0;

    while i < html.len() {
        if bytes[i] == b'<' {
            let rest = &html[i..];
            if rest.starts_with("<p>") || rest.starts_with("<p ") {
                in_para = true;
            } else if rest.starts_with("</p>") {
                in_para = false;
            }
            i = consume_emphasis_tag(html, i, &mut bold, &mut italic);
            continue;
        }
        if !in_para {
            i += 1;
            continue;
        }
        if bytes[i] == b'&'
            && let Some((ch, end)) = decode_entity(html, i)
        {
            out.push((ch, current_mask(bold, italic)));
            i = end;
            continue;
        }
        let ch = html[i..]
            .chars()
            .next()
            .expect("byte index is on a char boundary");
        out.push((ch, current_mask(bold, italic)));
        i += ch.len_utf8();
    }

    out
}

/// Mask from the current bold/italic depths.
const fn current_mask(bold: u32, italic: u32) -> Mask {
    let mut m = 0;
    if bold > 0 {
        m |= BOLD;
    }
    if italic > 0 {
        m |= ITALIC;
    }
    m
}

/// Consume one HTML tag at `pos`, adjusting the bold/italic depth for
/// `<strong>`/`<em>` (open and close), and skipping every other tag. Returns the
/// byte position just past the tag's `>` (or past `<` if the tag is unterminated).
fn consume_emphasis_tag(html: &str, pos: usize, bold: &mut u32, italic: &mut u32) -> usize {
    let rest = &html[pos..];
    let Some(gt) = rest.find('>') else {
        return pos + 1;
    };
    let inner = &rest[1..gt]; // between '<' and '>'
    match inner {
        "strong" => *bold += 1,
        "/strong" => *bold = bold.saturating_sub(1),
        "em" => *italic += 1,
        "/em" => *italic = italic.saturating_sub(1),
        _ => {}
    }
    pos + gt + 1
}

/// Decode the small set of HTML entities the emphasis examples emit. Returns the
/// decoded character and the byte position past the `;`, or `None` if `pos` does
/// not start a recognized entity.
fn decode_entity(html: &str, pos: usize) -> Option<(char, usize)> {
    for (name, ch) in &[
        ("&amp;", '&'),
        ("&lt;", '<'),
        ("&gt;", '>'),
        ("&quot;", '"'),
        ("&#39;", '\''),
    ] {
        if html[pos..].starts_with(name) {
            return Some((*ch, pos + name.len()));
        }
    }
    None
}

// --- Tree -> masked text ---

/// Reduce Lattice's tree for an example to the same per-character masked text:
/// the rendered content of every inline host (`Paragraph` / `Heading`), in
/// document order, with emphasis delimiters stripped and each surviving
/// character tagged with the union of `Strong`/`Emphasis` modifiers covering it.
///
/// A `*`/`_` byte is markup (and dropped) exactly when it is one of the
/// `open_len` outermost delimiters at the leading or trailing edge of some
/// emphasis span — `Strong` consumes two at each edge, `Emphasis` one. OR-ing
/// the modifier bit of every span that covers a surviving byte yields its mask,
/// matching the cut-point flatten used for semantic tokens.
fn tree_emphasis_mask(tree: &Tree) -> MaskedText {
    let source = tree.source();

    // Per-byte: OR of modifier bits of the emphasis spans covering it, and
    // whether the byte is a consumed edge delimiter (markup to drop).
    let mut mask = vec![0u8; source.len()];
    let mut is_markup = vec![false; source.len()];

    for node in tree.nodes() {
        let bit = match node.kind {
            ElementKind::Strong => BOLD,
            ElementKind::Emphasis => ITALIC,
            _ => continue,
        };
        let (start, end) = (node.span.start, node.span.end);
        for slot in &mut mask[start..end] {
            *slot |= bit;
        }
        // `Strong` consumes two delimiters at each edge, `Emphasis` one — the
        // outermost ones. Mark exactly those bytes as dropped markup.
        let open_len = if bit == BOLD { 2 } else { 1 };
        for off in 0..open_len {
            is_markup[start + off] = true;
            is_markup[end - 1 - off] = true;
        }
    }

    let mut out: MaskedText = Vec::new();
    for node in tree.nodes() {
        if !matches!(
            node.kind,
            ElementKind::Paragraph | ElementKind::Heading { .. }
        ) {
            continue;
        }
        // Collect this host's content, then trim its own leading/trailing
        // whitespace before appending. A paragraph node's span can carry a
        // trailing newline, and consecutive paragraphs (`a\n\nb`) must join with
        // nothing — matching the HTML, where the inter-`<p>` `\n` is not content.
        // A soft break *inside* one paragraph (`*foo\nbar*`) is interior to a
        // single node, so it survives the per-host trim.
        let (start, end) = (node.span.start, node.span.end);
        let mut host: MaskedText = Vec::new();
        let mut i = start;
        while i < end {
            if is_markup[i] {
                i += 1;
                continue;
            }
            let ch = source[i..]
                .chars()
                .next()
                .expect("byte index is on a char boundary");
            host.push((ch, mask[i]));
            i += ch.len_utf8();
        }
        out.extend(normalize_masked(&host));
    }

    out
}

/// Normalize masked text for comparison: trim leading/trailing whitespace-only
/// entries (paragraph edges) and collapse no characters in between. The HTML and
/// the tree can disagree on a single trailing `\n` inside a multi-paragraph
/// example's join, so this drops *only* outer whitespace, leaving interior text
/// and every emphasis mask intact.
fn normalize_masked(text: &MaskedText) -> MaskedText {
    let is_ws = |&(c, _): &(char, Mask)| c.is_whitespace();
    let start = text.iter().position(|e| !is_ws(e)).unwrap_or(text.len());
    let end = text
        .iter()
        .rposition(|e| !is_ws(e))
        .map_or(start, |p| p + 1);
    text[start..end].to_vec()
}

// ---------------------------------------------------------------------------
// Known deviations
// ---------------------------------------------------------------------------

/// Spec examples whose block structure differs in Lattice's parser.
///
/// Each entry is `(example_number, reason)`. Grouped by root cause.
const DEVIATIONS: &[(u32, &str)] = &[
    // -----------------------------------------------------------------
    // HTML container modeling: Lattice models known HTML container tags
    // (<div>, <table>, <nav>, etc.) as structured Container/Table nodes
    // with rich symbol support, rather than opaque HtmlBlock nodes. This
    // is intentional — it enables structural navigation and symbol
    // emission for HTML-heavy documents.
    // -----------------------------------------------------------------
    (148, "HTML container modeling (<table>)"),
    (149, "HTML container modeling (<div>)"),
    (150, "HTML container modeling (<div>)"),
    (151, "HTML container modeling (<div>)"),
    (155, "HTML container modeling (<div>, blank line boundary)"),
    (156, "HTML container modeling (<div>)"),
    (157, "HTML container modeling (<div>)"),
    (160, "HTML container modeling (<table>)"),
    (161, "HTML container modeling (<div>)"),
    (162, "HTML container modeling (<div>)"),
    (165, "HTML container modeling (<div>, self-closing)"),
    (167, "HTML container modeling (<div>)"),
    (168, "HTML container modeling (<nav>)"),
    (175, "HTML container modeling (<div> in list)"),
    (184, "HTML container modeling (<div>, indented)"),
    (186, "HTML container modeling (<div>, blockquote)"),
    (190, "HTML container modeling (<table>, blank lines)"),
    (191, "HTML container modeling (<div>, blank line split)"),
];

fn is_deviation(example: u32) -> bool {
    DEVIATIONS.iter().any(|(n, _)| *n == example)
}

// ---------------------------------------------------------------------------
// Test runner
// ---------------------------------------------------------------------------

fn run_section(section: &str) {
    let spec = load_spec();
    let examples = section_examples(&spec, section);
    assert!(
        !examples.is_empty(),
        "no spec examples found for section {section:?}"
    );

    let mut failures: Vec<String> = Vec::new();
    let mut skipped = 0u32;

    for ex in &examples {
        if is_deviation(ex.example) {
            skipped += 1;
            continue;
        }

        let tree = parse(&ex.markdown);
        let actual = tree_shapes(&tree);
        let expected = html_shapes(&ex.html);

        if actual != expected {
            failures.push(format!(
                "Example {} (spec line {}):\n\
                 \x20 expected: {expected:?}\n\
                 \x20 actual:   {actual:?}\n\
                 \x20 markdown: {:?}\n\
                 \x20 html:     {:?}",
                ex.example, ex.start_line, ex.markdown, ex.html,
            ));
        }
    }

    let total = examples.len();
    let passed = total - failures.len() - skipped as usize;

    assert!(
        failures.is_empty(),
        "{section}: {passed}/{total} passed, {skipped} skipped, {} failed:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

// ---------------------------------------------------------------------------
// Emphasis deviations (ticket 27)
// ---------------------------------------------------------------------------

/// Emphasis examples whose *text content* differs from Lattice's because they
/// exercise an inline feature Lattice deliberately does not model inside the
/// emphasis comparator (not a flanking disagreement — see ticket 27). Each entry
/// names the unmodeled feature, never a "which text is emphasized" mismatch.
const EMPHASIS_DEVIATIONS: &[(u32, &str)] = &[
    // Links / autolinks inside emphasis: the HTML renders the link text or the
    // autolink target, while Lattice keeps the literal `[..](..)` / `<..>`
    // bracket syntax in the paragraph text. The link parser is a separate inline
    // pass; this comparator reduces only emphasis, so the bracket text diverges.
    (404, "link inside emphasis (`[bar](/url)` -> `bar`)"),
    (419, "link with nested emphasis inside emphasis"),
    (422, "link inside strong"),
    (433, "link with nested emphasis inside strong"),
    (473, "emphasis delimiter inside a link label"),
    (474, "emphasis delimiter inside a link label"),
    (480, "autolink absorbs the trailing `**`"),
    (481, "autolink absorbs the trailing `__`"),
    // Code spans inside emphasis: the HTML renders the code content (`<code>*`),
    // while Lattice keeps the backticks. Code spans are a separate inline pass.
    (478, "code span inside emphasis (`` `*` `` -> `*`)"),
    (479, "code span inside emphasis (`` `_` `` -> `_`)"),
    // Raw inline HTML: the HTML round-trips the tag (with its quoted `*`/`_`),
    // while Lattice keeps the literal source. Raw HTML is not emphasis content.
    (475, "raw inline HTML (`<img .. title=\"*\"/>`)"),
    (476, "raw inline HTML (`<a href=\"**\">`)"),
    (477, "raw inline HTML (`<a href=\"__\">`)"),
    // Backslash escapes: the HTML drops the backslash (`\*` -> `*`), Lattice
    // keeps the source byte. Escapes are a separate inline concern.
    (437, "backslash-escaped delimiter (`\\*`)"),
    (440, "backslash-escaped delimiter (`\\*`)"),
    (449, "backslash-escaped delimiter (`\\_`)"),
    (452, "backslash-escaped delimiter (`\\_`)"),
];

fn is_emphasis_deviation(example: u32) -> bool {
    EMPHASIS_DEVIATIONS.iter().any(|(n, _)| *n == example)
}

/// Run the inline emphasis comparator over the "Emphasis and strong emphasis"
/// section: reduce both the expected HTML and Lattice's tree to a per-character
/// bold/italic mask and assert they match, skipping the documented
/// unmodeled-feature deviations.
fn run_emphasis_section() {
    let spec = load_spec();
    let examples = section_examples(&spec, "Emphasis and strong emphasis");
    assert!(
        !examples.is_empty(),
        "no spec examples found for the emphasis section"
    );

    let mut failures: Vec<String> = Vec::new();
    let mut skipped = 0u32;

    for ex in &examples {
        if is_emphasis_deviation(ex.example) {
            skipped += 1;
            continue;
        }

        let tree = parse(&ex.markdown);
        let actual = normalize_masked(&tree_emphasis_mask(&tree));
        let expected = normalize_masked(&html_emphasis_mask(&ex.html));

        if actual != expected {
            failures.push(format!(
                "Example {} (spec line {}):\n\
                 \x20 expected: {expected:?}\n\
                 \x20 actual:   {actual:?}\n\
                 \x20 markdown: {:?}\n\
                 \x20 html:     {:?}",
                ex.example, ex.start_line, ex.markdown, ex.html,
            ));
        }
    }

    let total = examples.len();
    let passed = total - failures.len() - skipped as usize;

    assert!(
        failures.is_empty(),
        "Emphasis and strong emphasis: {passed}/{total} passed, {skipped} skipped, {} failed:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

// ---------------------------------------------------------------------------
// Per-section tests
// ---------------------------------------------------------------------------

#[test]
fn spec_tabs() {
    run_section("Tabs");
}

#[test]
fn spec_precedence() {
    run_section("Precedence");
}

#[test]
fn spec_thematic_breaks() {
    run_section("Thematic breaks");
}

#[test]
fn spec_atx_headings() {
    run_section("ATX headings");
}

#[test]
fn spec_setext_headings() {
    run_section("Setext headings");
}

#[test]
fn spec_indented_code_blocks() {
    run_section("Indented code blocks");
}

#[test]
fn spec_fenced_code_blocks() {
    run_section("Fenced code blocks");
}

#[test]
fn spec_html_blocks() {
    run_section("HTML blocks");
}

#[test]
fn spec_link_reference_definitions() {
    run_section("Link reference definitions");
}

#[test]
fn spec_paragraphs() {
    run_section("Paragraphs");
}

#[test]
fn spec_blank_lines() {
    run_section("Blank lines");
}

#[test]
fn spec_block_quotes() {
    run_section("Block quotes");
}

#[test]
fn spec_list_items() {
    run_section("List items");
}

#[test]
fn spec_lists() {
    run_section("Lists");
}

#[test]
fn spec_emphasis() {
    run_emphasis_section();
}

// ---------------------------------------------------------------------------
// Section coverage
// ---------------------------------------------------------------------------

/// Verify every spec section is either tested or explicitly skipped.
#[test]
fn all_sections_covered() {
    let spec = load_spec();
    let mut sections: Vec<&str> = spec.iter().map(|e| e.section.as_str()).collect();
    sections.sort_unstable();
    sections.dedup();

    // Sections covered by a conformance test: block-structure sections via the
    // `Shape` comparator, plus "Emphasis and strong emphasis" via the inline
    // emphasis-mask comparator (ticket 27).
    let tested: &[&str] = &[
        "ATX headings",
        "Blank lines",
        "Block quotes",
        "Emphasis and strong emphasis",
        "Fenced code blocks",
        "HTML blocks",
        "Indented code blocks",
        "Link reference definitions",
        "List items",
        "Lists",
        "Paragraphs",
        "Precedence",
        "Setext headings",
        "Tabs",
        "Thematic breaks",
    ];
    // Inline sections Lattice does not model against the spec.
    let inline: &[&str] = &[
        "Autolinks",
        "Backslash escapes",
        "Code spans",
        "Entity and numeric character references",
        "Hard line breaks",
        "Images",
        "Inlines",
        "Links",
        "Raw HTML",
        "Soft line breaks",
        "Textual content",
    ];

    for section in &sections {
        assert!(
            tested.contains(section) || inline.contains(section),
            "unaccounted spec section: {section:?} — add it to tested or inline list"
        );
    }
}
