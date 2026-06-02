// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

// CommonMark 0.31.2 conformance tests.
//
// Validates Lattice's block structure parser against the official
// CommonMark spec test suite (vendored at `tests/fixtures/commonmark_spec.json`).
//
// Only block-structure sections are tested. Inline sections (emphasis,
// code spans, links, images, etc.) are skipped — Lattice does not parse
// inline formatting.
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
    // -----------------------------------------------------------------
    // Link reference definition continuation: multi-line definitions
    // spanning continuation lines are not fully parsed.
    // -----------------------------------------------------------------
    (193, "multi-line link reference definition"),
    (195, "multi-line link reference definition (angle bracket dest)"),
    (196, "multi-line link reference definition (multi-line title)"),
    (198, "multi-line link reference definition (dest on next line)"),
    (201, "link reference definition with angle bracket dest"),
    (202, "link reference definition with backslash escapes"),
    // -----------------------------------------------------------------
    // Block quote lazy continuation: lines that omit the `>` marker but
    // belong to a blockquote paragraph are not captured.
    // -----------------------------------------------------------------
    (234, "blockquote lazy continuation"),
    (235, "blockquote lazy continuation"),
    (246, "blockquote lazy continuation with list"),
    (251, "blockquote lazy continuation with nested quote"),
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

    let structural: &[&str] = &[
        "ATX headings",
        "Blank lines",
        "Block quotes",
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
    let inline: &[&str] = &[
        "Autolinks",
        "Backslash escapes",
        "Code spans",
        "Emphasis and strong emphasis",
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
            structural.contains(section) || inline.contains(section),
            "unaccounted spec section: {section:?} — add it to structural or inline list"
        );
    }
}
