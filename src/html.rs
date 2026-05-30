// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! HTML tag tokenizer and element vocabulary.
//!
//! Provides tag parsing, attribute extraction, autolink recognition,
//! and compile-time element sets for the block and inline parsers.

use crate::block::ElementKind;
use crate::span::Span;

// ---------------------------------------------------------------------------
// Element vocabulary (compile-time phf sets)
// ---------------------------------------------------------------------------

/// Void elements — must never have a closing tag.
pub static VOID_ELEMENTS: phf::Set<&str> = phf::phf_set! {
    "area", "base", "br", "col", "embed", "hr", "img", "input",
    "link", "meta", "source", "track", "wbr",
};

/// Block-level elements — for block-in-inline diagnostics.
pub static BLOCK_ELEMENTS: phf::Set<&str> = phf::phf_set! {
    "address", "article", "aside", "blockquote", "body", "canvas",
    "dd", "details", "dialog", "div", "dl", "dt", "fieldset",
    "figcaption", "figure", "footer", "form", "h1", "h2", "h3",
    "h4", "h5", "h6", "header", "hgroup", "hr", "li", "main",
    "menu", "nav", "noscript", "ol", "p", "pre", "search",
    "section", "summary", "table", "tbody", "td", "template",
    "tfoot", "th", "thead", "tr", "ul",
};

/// All standard HTML elements — for unknown element detection.
pub static ALL_ELEMENTS: phf::Set<&str> = phf::phf_set! {
    "a", "abbr", "address", "area", "article", "aside", "audio",
    "b", "base", "bdi", "bdo", "blockquote", "body", "br", "button",
    "canvas", "caption", "cite", "code", "col", "colgroup",
    "data", "datalist", "dd", "del", "details", "dfn", "dialog",
    "div", "dl", "dt",
    "em", "embed",
    "fieldset", "figcaption", "figure", "footer", "form",
    "h1", "h2", "h3", "h4", "h5", "h6", "head", "header", "hgroup",
    "hr", "html",
    "i", "iframe", "img", "input", "ins",
    "kbd",
    "label", "legend", "li", "link",
    "main", "map", "mark", "math", "menu", "meta", "meter",
    "nav", "noscript",
    "object", "ol", "optgroup", "option", "output",
    "p", "picture", "pre", "progress",
    "q",
    "rp", "rt", "ruby",
    "s", "samp", "script", "search", "section", "select", "slot",
    "small", "source", "span", "strong", "style", "sub", "summary",
    "sup", "svg",
    "table", "tbody", "td", "template", "textarea", "tfoot", "th",
    "thead", "time", "title", "tr", "track",
    "u", "ul",
    "var", "video",
    "wbr",
};

// ---------------------------------------------------------------------------
// Tag tokenization
// ---------------------------------------------------------------------------

/// A parsed HTML tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HtmlTag {
    /// `<tagname attr="value" ...>` or `<tagname ... />`
    Open {
        /// Lowercased tag name.
        name: String,
        /// Parsed attributes.
        attrs: Vec<Attribute>,
        /// Whether the tag is self-closing (`/>`).
        self_closing: bool,
        /// Byte length consumed from the input.
        len: usize,
    },
    /// `</tagname>`
    Close {
        /// Lowercased tag name.
        name: String,
        /// Byte length consumed from the input.
        len: usize,
    },
    /// `<!-- ... -->`
    Comment {
        /// Byte length consumed from the input.
        len: usize,
    },
}

/// An HTML attribute name-value pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribute {
    /// Lowercased attribute name.
    pub name: String,
    /// Attribute value (if present). `None` for boolean attributes.
    pub value: Option<String>,
    /// Span of the attribute name in the source.
    pub name_span: Span,
    /// Span of the attribute value in the source (content only, no quotes).
    pub value_span: Option<Span>,
}

/// Tokenize an HTML tag at the start of `text`.
///
/// `base` is the byte offset of `text[0]` in the full source, used for
/// span computation. Returns `None` if the text does not start with a
/// valid HTML tag.
pub fn tokenize_tag(text: &str, base: usize) -> Option<HtmlTag> {
    let bytes = text.as_bytes();
    if bytes.first() != Some(&b'<') {
        return None;
    }

    // Comment: <!-- ... -->
    if let Some(after) = text.strip_prefix("<!--") {
        let end = after.find("-->").map(|p| p + 7)?;
        return Some(HtmlTag::Comment { len: end });
    }

    // Close tag: </tagname>
    if bytes.get(1) == Some(&b'/') {
        return parse_close_tag(text, bytes);
    }

    // Open tag (including self-closing)
    parse_open_tag(text, bytes, base)
}

/// Parse a closing tag `</tagname>`.
fn parse_close_tag(text: &str, bytes: &[u8]) -> Option<HtmlTag> {
    let rest = &text[2..];
    if !rest.as_bytes().first().is_some_and(u8::is_ascii_alphabetic) {
        return None;
    }
    let name_end = rest
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '-')
        .unwrap_or(rest.len());
    let name = rest[..name_end].to_lowercase();
    let after_name = &bytes[2 + name_end..];

    // Skip whitespace, then expect >
    let ws = after_name
        .iter()
        .take_while(|b| b.is_ascii_whitespace())
        .count();
    if after_name.get(ws) != Some(&b'>') {
        return None;
    }

    Some(HtmlTag::Close {
        name,
        len: 2 + name_end + ws + 1,
    })
}

/// Parse an opening tag `<tagname ...>` or `<tagname ... />`.
fn parse_open_tag(text: &str, bytes: &[u8], base: usize) -> Option<HtmlTag> {
    let rest = &text[1..];
    if !rest.as_bytes().first().is_some_and(u8::is_ascii_alphabetic) {
        return None;
    }

    let name_end = rest
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '-')
        .unwrap_or(rest.len());
    let name = rest[..name_end].to_lowercase();

    let mut i = 1 + name_end;
    let mut attrs = Vec::new();

    loop {
        // Skip whitespace
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }

        if i >= bytes.len() {
            return None;
        }

        // Self-closing />
        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'>') {
            return Some(HtmlTag::Open {
                name,
                attrs,
                self_closing: true,
                len: i + 2,
            });
        }

        // End of tag >
        if bytes[i] == b'>' {
            return Some(HtmlTag::Open {
                name,
                attrs,
                self_closing: false,
                len: i + 1,
            });
        }

        // Parse attribute
        let attr = parse_attribute(text, bytes, &mut i, base)?;
        attrs.push(attr);
    }
}

/// Parse a single attribute, advancing `i` past it.
fn parse_attribute(text: &str, bytes: &[u8], i: &mut usize, base: usize) -> Option<Attribute> {
    let attr_name_start = *i;
    while *i < bytes.len()
        && bytes[*i] != b'='
        && bytes[*i] != b'>'
        && bytes[*i] != b'/'
        && !bytes[*i].is_ascii_whitespace()
    {
        *i += 1;
    }
    if *i == attr_name_start {
        return None;
    }
    let attr_name = text[attr_name_start..*i].to_lowercase();
    let name_span = Span::new(base + attr_name_start, base + *i);

    // Skip whitespace before potential =
    while *i < bytes.len() && bytes[*i].is_ascii_whitespace() {
        *i += 1;
    }

    if *i < bytes.len() && bytes[*i] == b'=' {
        *i += 1;
        // Skip whitespace after =
        while *i < bytes.len() && bytes[*i].is_ascii_whitespace() {
            *i += 1;
        }
        let (value, value_span) = parse_attribute_value(text, bytes, i, base)?;
        Some(Attribute {
            name: attr_name,
            value: Some(value),
            name_span,
            value_span: Some(value_span),
        })
    } else {
        // Boolean attribute
        Some(Attribute {
            name: attr_name,
            value: None,
            name_span,
            value_span: None,
        })
    }
}

/// Parse an attribute value (quoted or unquoted), advancing `i` past it.
fn parse_attribute_value(
    text: &str,
    bytes: &[u8],
    i: &mut usize,
    base: usize,
) -> Option<(String, Span)> {
    if *i >= bytes.len() {
        return None;
    }

    if bytes[*i] == b'"' || bytes[*i] == b'\'' {
        let quote = bytes[*i];
        *i += 1;
        let value_start = *i;
        while *i < bytes.len() && bytes[*i] != quote {
            *i += 1;
        }
        if *i >= bytes.len() {
            return None;
        }
        let value = text[value_start..*i].to_string();
        let span = Span::new(base + value_start, base + *i);
        *i += 1; // skip closing quote
        Some((value, span))
    } else {
        // Unquoted value
        let value_start = *i;
        while *i < bytes.len()
            && !bytes[*i].is_ascii_whitespace()
            && bytes[*i] != b'>'
            && bytes[*i] != b'/'
        {
            *i += 1;
        }
        if *i == value_start {
            return None;
        }
        let value = text[value_start..*i].to_string();
        let span = Span::new(base + value_start, base + *i);
        Some((value, span))
    }
}

// ---------------------------------------------------------------------------
// Autolinks
// ---------------------------------------------------------------------------

/// Try to parse a `CommonMark` autolink at the start of `text`.
///
/// `text` must start with `<`. Returns `(url, byte_length)` on success.
pub fn try_autolink(text: &str) -> Option<(String, usize)> {
    let bytes = text.as_bytes();
    if bytes.first() != Some(&b'<') {
        return None;
    }

    // Find the closing >
    let close = bytes[1..].iter().position(|&b| b == b'>')?;
    let inner = &text[1..=close];

    // Must not contain spaces, <, or line endings
    if inner
        .bytes()
        .any(|b| b == b' ' || b == b'<' || b == b'\n' || b == b'\r')
    {
        return None;
    }

    // URI autolink: scheme:content
    if let Some(colon) = inner.find(':') {
        let scheme = &inner[..colon];
        if !scheme.is_empty()
            && scheme.as_bytes()[0].is_ascii_alphabetic()
            && scheme
                .bytes()
                .skip(1)
                .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'.' || b == b'-')
        {
            return Some((inner.to_string(), close + 2));
        }
    }

    // Email autolink: local@domain
    if let Some(at) = inner.find('@') {
        let local = &inner[..at];
        let domain = &inner[at + 1..];
        if !local.is_empty()
            && !domain.is_empty()
            && domain.contains('.')
            && local
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".!#$%&'*+/=?^_`{|}~-".contains(&b))
            && is_valid_email_domain(domain)
        {
            return Some((format!("mailto:{inner}"), close + 2));
        }
    }

    None
}

/// Validate an email domain (simplified).
fn is_valid_email_domain(domain: &str) -> bool {
    domain.split('.').all(|part| {
        !part.is_empty()
            && part.len() <= 63
            && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    })
}

// ---------------------------------------------------------------------------
// Tag-to-ElementKind mapping
// ---------------------------------------------------------------------------

/// Map an HTML tag name to its `ElementKind`.
///
/// Returns `None` for tags that don't map to a structural element
/// (inline formatting like `<em>`, `<strong>`, `<span>`, etc.).
pub fn tag_to_element_kind(name: &str) -> Option<ElementKind> {
    match name {
        "blockquote" => Some(ElementKind::QuoteBlock),
        "hr" => Some(ElementKind::Rules),
        "h1" => Some(ElementKind::Heading { level: 1 }),
        "h2" => Some(ElementKind::Heading { level: 2 }),
        "h3" => Some(ElementKind::Heading { level: 3 }),
        "h4" => Some(ElementKind::Heading { level: 4 }),
        "h5" => Some(ElementKind::Heading { level: 5 }),
        "h6" => Some(ElementKind::Heading { level: 6 }),
        "p" => Some(ElementKind::Paragraph),
        "pre" => Some(ElementKind::CodeBlock),
        "ul" => Some(ElementKind::List {
            ordered: false,
            start: 0,
            tight: true,
        }),
        "ol" => Some(ElementKind::List {
            ordered: true,
            start: 1,
            tight: true,
        }),
        "li" => Some(ElementKind::ListItem { task: None }),
        "table" => Some(ElementKind::Table {
            alignments: Vec::new(),
        }),
        "tr" => Some(ElementKind::TableRow { header: false }),
        "th" | "td" => Some(ElementKind::TableCell),
        "img" | "iframe" => Some(ElementKind::Image {
            url: String::new(),
            title: String::new(),
        }),
        "video" => Some(ElementKind::Video {
            url: String::new(),
            title: String::new(),
        }),
        "audio" => Some(ElementKind::Audio {
            url: String::new(),
            title: String::new(),
        }),
        "input" | "select" | "textarea" => Some(ElementKind::FormControl),
        "details" => Some(ElementKind::Details),
        "summary" => Some(ElementKind::DetailsSummary),
        "div" | "section" | "article" | "aside" | "nav" | "main" | "header" | "footer"
        | "figure" | "figcaption" | "form" | "fieldset" | "dialog" | "address" | "hgroup" => {
            Some(ElementKind::Container)
        }
        _ => None,
    }
}

/// Whether an HTML tag creates a container scope (push on open, pop on close).
pub fn is_html_container(name: &str) -> bool {
    matches!(
        name,
        "blockquote"
            | "div"
            | "section"
            | "article"
            | "aside"
            | "nav"
            | "main"
            | "header"
            | "footer"
            | "details"
            | "summary"
            | "ul"
            | "ol"
            | "li"
            | "table"
            | "tbody"
            | "thead"
            | "tfoot"
            | "tr"
            | "th"
            | "td"
            | "form"
            | "fieldset"
            | "figure"
            | "figcaption"
            | "dialog"
            | "address"
            | "hgroup"
            | "dl"
            | "dd"
            | "dt"
    )
}

/// Extract `href` and `title` from an `<a>` tag's attributes.
pub fn extract_link_attrs(attrs: &[Attribute]) -> (String, String) {
    let mut href = String::new();
    let mut title = String::new();
    for attr in attrs {
        match attr.name.as_str() {
            "href" => {
                if let Some(v) = &attr.value {
                    href.clone_from(v);
                }
            }
            "title" => {
                if let Some(v) = &attr.value {
                    title.clone_from(v);
                }
            }
            _ => {}
        }
    }
    (href, title)
}

/// Extract `src`, `alt`, and `title` from an `<img>` tag's attributes.
pub fn extract_image_attrs(attrs: &[Attribute]) -> (String, String) {
    let mut src = String::new();
    let mut title = String::new();
    for attr in attrs {
        match attr.name.as_str() {
            "src" => {
                if let Some(v) = &attr.value {
                    src.clone_from(v);
                }
            }
            "title" => {
                if let Some(v) = &attr.value {
                    title.clone_from(v);
                }
            }
            _ => {}
        }
    }
    (src, title)
}

/// Known admonition class names.
static ADMONITION_CLASSES: phf::Set<&str> = phf::phf_set! {
    "note", "tip", "warning", "caution", "important",
};

/// Extract an admonition type from a container element's `class` attribute.
///
/// Returns the uppercased admonition type if the class list contains a
/// known admonition keyword (e.g. `"warning"` → `Some("WARNING")`).
pub fn extract_admonition_class(attrs: &[Attribute]) -> Option<String> {
    for attr in attrs {
        if attr.name == "class"
            && let Some(value) = &attr.value
        {
            for cls in value.split_whitespace() {
                if ADMONITION_CLASSES.contains(cls) {
                    return Some(cls.to_uppercase());
                }
            }
        }
    }
    None
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
    use super::*;

    // --- Element sets ---

    #[test]
    fn void_elements_contains_expected() {
        for tag in &["area", "br", "hr", "img", "input", "meta", "wbr"] {
            assert!(VOID_ELEMENTS.contains(tag), "{tag} should be void");
        }
    }

    #[test]
    fn void_elements_excludes_non_void() {
        for tag in &["div", "span", "p", "a"] {
            assert!(!VOID_ELEMENTS.contains(tag), "{tag} should not be void");
        }
    }

    #[test]
    fn block_elements_contains_expected() {
        for tag in &["div", "p", "blockquote", "table", "ul", "ol", "li"] {
            assert!(BLOCK_ELEMENTS.contains(tag), "{tag} should be block");
        }
    }

    #[test]
    fn all_elements_contains_standard() {
        for tag in &["a", "div", "span", "img", "table", "video", "svg"] {
            assert!(ALL_ELEMENTS.contains(tag), "{tag} should be known");
        }
    }

    #[test]
    fn all_elements_excludes_unknown() {
        assert!(
            !ALL_ELEMENTS.contains("foo"),
            "foo should not be a known element"
        );
        assert!(
            !ALL_ELEMENTS.contains("blink"),
            "blink should not be a known element"
        );
    }

    // --- Tag tokenization ---

    #[test]
    fn open_tag_simple() {
        let tag = tokenize_tag("<div>", 0).expect("should parse");
        assert_eq!(
            tag,
            HtmlTag::Open {
                name: "div".into(),
                attrs: vec![],
                self_closing: false,
                len: 5,
            },
            "simple open tag"
        );
    }

    #[test]
    fn open_tag_with_attributes() {
        let tag = tokenize_tag(r#"<a href="url" title="pred">"#, 0).expect("should parse");
        match tag {
            HtmlTag::Open {
                name, attrs, len, ..
            } => {
                assert_eq!(name, "a", "tag name");
                assert_eq!(attrs.len(), 2, "two attributes");
                assert_eq!(attrs[0].name, "href", "first attr name");
                assert_eq!(attrs[0].value.as_deref(), Some("url"), "first attr value");
                assert_eq!(attrs[1].name, "title", "second attr name");
                assert_eq!(attrs[1].value.as_deref(), Some("pred"), "second attr value");
                assert_eq!(len, 27, "consumed length");
            }
            _ => panic!("expected open tag"),
        }
    }

    #[test]
    fn open_tag_self_closing() {
        let tag = tokenize_tag("<br/>", 0).expect("should parse");
        assert_eq!(
            tag,
            HtmlTag::Open {
                name: "br".into(),
                attrs: vec![],
                self_closing: true,
                len: 5,
            },
            "self-closing tag"
        );
    }

    #[test]
    fn open_tag_self_closing_with_space() {
        let tag = tokenize_tag("<img src=\"x\" />", 0).expect("should parse");
        match tag {
            HtmlTag::Open {
                self_closing, name, ..
            } => {
                assert!(self_closing, "should be self-closing");
                assert_eq!(name, "img", "tag name");
            }
            _ => panic!("expected open tag"),
        }
    }

    #[test]
    fn close_tag() {
        let tag = tokenize_tag("</div>", 0).expect("should parse");
        assert_eq!(
            tag,
            HtmlTag::Close {
                name: "div".into(),
                len: 6,
            },
            "close tag"
        );
    }

    #[test]
    fn close_tag_with_whitespace() {
        let tag = tokenize_tag("</div  >", 0).expect("should parse");
        assert_eq!(
            tag,
            HtmlTag::Close {
                name: "div".into(),
                len: 8,
            },
            "close tag with whitespace"
        );
    }

    #[test]
    fn comment() {
        let tag = tokenize_tag("<!-- hello -->", 0).expect("should parse");
        assert_eq!(tag, HtmlTag::Comment { len: 14 }, "comment");
    }

    #[test]
    fn case_insensitive_tag_name() {
        let tag = tokenize_tag("<DIV>", 0).expect("should parse");
        match tag {
            HtmlTag::Open { name, .. } => assert_eq!(name, "div", "should lowercase"),
            _ => panic!("expected open tag"),
        }
    }

    #[test]
    fn boolean_attribute() {
        let tag = tokenize_tag("<input disabled>", 0).expect("should parse");
        match tag {
            HtmlTag::Open { attrs, .. } => {
                assert_eq!(attrs.len(), 1, "one attribute");
                assert_eq!(attrs[0].name, "disabled", "attr name");
                assert!(attrs[0].value.is_none(), "boolean has no value");
            }
            _ => panic!("expected open tag"),
        }
    }

    #[test]
    fn single_quoted_attribute() {
        let tag = tokenize_tag("<a href='url'>", 0).expect("should parse");
        match tag {
            HtmlTag::Open { attrs, .. } => {
                assert_eq!(
                    attrs[0].value.as_deref(),
                    Some("url"),
                    "single-quoted value"
                );
            }
            _ => panic!("expected open tag"),
        }
    }

    #[test]
    fn unquoted_attribute() {
        let tag = tokenize_tag("<a href=url>", 0).expect("should parse");
        match tag {
            HtmlTag::Open { attrs, .. } => {
                assert_eq!(attrs[0].value.as_deref(), Some("url"), "unquoted value");
            }
            _ => panic!("expected open tag"),
        }
    }

    #[test]
    fn unterminated_tag_returns_none() {
        assert!(tokenize_tag("<div", 0).is_none(), "unterminated tag");
    }

    #[test]
    fn not_a_tag() {
        assert!(tokenize_tag("hello", 0).is_none(), "no leading <");
        assert!(tokenize_tag("<123>", 0).is_none(), "digit after <");
    }

    #[test]
    fn attribute_spans() {
        let tag = tokenize_tag(r#"<a href="url">"#, 10).expect("should parse");
        match tag {
            HtmlTag::Open { attrs, .. } => {
                assert_eq!(attrs[0].name_span, Span::new(13, 17), "name span at offset");
                assert_eq!(
                    attrs[0].value_span,
                    Some(Span::new(19, 22)),
                    "value span at offset"
                );
            }
            _ => panic!("expected open tag"),
        }
    }

    // --- Autolinks ---

    #[test]
    fn autolink_uri() {
        let (url, len) = try_autolink("<https://example.com>").expect("should parse");
        assert_eq!(url, "https://example.com", "URI autolink");
        assert_eq!(len, 21, "consumed length");
    }

    #[test]
    fn autolink_mailto() {
        let (url, len) = try_autolink("<mailto:user@example.com>").expect("should parse");
        assert_eq!(url, "mailto:user@example.com", "mailto autolink");
        assert_eq!(len, 25, "consumed length");
    }

    #[test]
    fn autolink_email() {
        let (url, len) = try_autolink("<user@example.com>").expect("should parse");
        assert_eq!(url, "mailto:user@example.com", "email autolink");
        assert_eq!(len, 18, "consumed length");
    }

    #[test]
    fn autolink_not_tag() {
        // Should be autolink, not tag
        let result = try_autolink("<http://example.com>");
        assert!(result.is_some(), "URI with :// should be autolink");
    }

    #[test]
    fn autolink_with_spaces_rejected() {
        assert!(
            try_autolink("<not a url>").is_none(),
            "spaces disqualify autolinks"
        );
    }

    // --- Tag-to-ElementKind mapping ---

    #[test]
    fn blockquote_maps_to_quoteblock() {
        assert_eq!(
            tag_to_element_kind("blockquote"),
            Some(ElementKind::QuoteBlock),
            "blockquote → QuoteBlock"
        );
    }

    #[test]
    fn headings_map_to_heading() {
        for level in 1..=6 {
            let tag = format!("h{level}");
            assert_eq!(
                tag_to_element_kind(&tag),
                Some(ElementKind::Heading { level }),
                "{tag} → Heading"
            );
        }
    }

    #[test]
    fn hr_maps_to_rules() {
        assert_eq!(
            tag_to_element_kind("hr"),
            Some(ElementKind::Rules),
            "hr → Rules"
        );
    }

    #[test]
    fn div_maps_to_container() {
        assert_eq!(
            tag_to_element_kind("div"),
            Some(ElementKind::Container),
            "div → Container"
        );
    }

    #[test]
    fn details_maps_to_details() {
        assert_eq!(
            tag_to_element_kind("details"),
            Some(ElementKind::Details),
            "details → Details"
        );
    }

    #[test]
    fn inline_tag_returns_none() {
        assert_eq!(tag_to_element_kind("em"), None, "inline tags return None");
        assert_eq!(
            tag_to_element_kind("strong"),
            None,
            "inline tags return None"
        );
    }

    #[test]
    fn container_check() {
        assert!(is_html_container("div"), "div is container");
        assert!(is_html_container("blockquote"), "blockquote is container");
        assert!(is_html_container("details"), "details is container");
        assert!(!is_html_container("p"), "p is not container");
        assert!(!is_html_container("hr"), "hr is not container");
        assert!(!is_html_container("img"), "img is not container");
    }

    // --- Media/form element mapping ---

    #[test]
    fn video_maps_to_video() {
        assert!(
            matches!(
                tag_to_element_kind("video"),
                Some(ElementKind::Video { .. })
            ),
            "video → Video"
        );
    }

    #[test]
    fn audio_maps_to_audio() {
        assert!(
            matches!(
                tag_to_element_kind("audio"),
                Some(ElementKind::Audio { .. })
            ),
            "audio → Audio"
        );
    }

    #[test]
    fn iframe_maps_to_image() {
        assert!(
            matches!(
                tag_to_element_kind("iframe"),
                Some(ElementKind::Image { .. })
            ),
            "iframe → Image"
        );
    }

    #[test]
    fn input_maps_to_form_control() {
        assert_eq!(
            tag_to_element_kind("input"),
            Some(ElementKind::FormControl),
            "input → FormControl"
        );
    }

    #[test]
    fn select_maps_to_form_control() {
        assert_eq!(
            tag_to_element_kind("select"),
            Some(ElementKind::FormControl),
            "select → FormControl"
        );
    }

    // --- Admonition class extraction ---

    #[test]
    fn extract_admonition_class_warning() {
        let attrs = vec![Attribute {
            name: "class".into(),
            value: Some("warning".into()),
            name_span: Span::new(0, 5),
            value_span: Some(Span::new(7, 14)),
        }];
        assert_eq!(
            extract_admonition_class(&attrs),
            Some("WARNING".to_string()),
            "warning class → WARNING"
        );
    }

    #[test]
    fn extract_admonition_class_mixed() {
        let attrs = vec![Attribute {
            name: "class".into(),
            value: Some("custom note extra".into()),
            name_span: Span::new(0, 5),
            value_span: Some(Span::new(7, 24)),
        }];
        assert_eq!(
            extract_admonition_class(&attrs),
            Some("NOTE".to_string()),
            "note in mixed classes → NOTE"
        );
    }

    #[test]
    fn extract_admonition_class_none() {
        let attrs = vec![Attribute {
            name: "class".into(),
            value: Some("fancy-box".into()),
            name_span: Span::new(0, 5),
            value_span: Some(Span::new(7, 16)),
        }];
        assert_eq!(
            extract_admonition_class(&attrs),
            None,
            "unknown class → None"
        );
    }

    // --- Link/image attribute extraction ---

    #[test]
    fn extract_link_attrs_works() {
        let attrs = vec![
            Attribute {
                name: "href".into(),
                value: Some("path.md".into()),
                name_span: Span::new(0, 4),
                value_span: Some(Span::new(6, 13)),
            },
            Attribute {
                name: "title".into(),
                value: Some("references".into()),
                name_span: Span::new(15, 20),
                value_span: Some(Span::new(22, 32)),
            },
        ];
        let (href, title) = extract_link_attrs(&attrs);
        assert_eq!(href, "path.md", "href extracted");
        assert_eq!(title, "references", "title extracted");
    }
}
