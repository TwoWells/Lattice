// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Markdown document parsing.
//!
//! Extracts links with predicates and headings with anchor IDs from a
//! markdown document using `pulldown-cmark`.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// Result of parsing a markdown document.
#[derive(Debug)]
pub struct ParsedDocument {
    /// All links found in the document.
    pub links: Vec<Link>,
    /// All headings found in the document.
    pub headings: Vec<Heading>,
}

/// A link extracted from a markdown document.
#[derive(Debug)]
pub struct Link {
    /// 1-based line number in the source.
    pub line: usize,
    /// Classification and resolved details.
    pub kind: LinkKind,
}

/// Classification of a markdown link.
#[derive(Debug)]
pub enum LinkKind {
    /// External URL (`http://`, `https://`, `mailto:`).
    External {
        /// The raw URL.
        url: String,
    },
    /// Intra-document fragment-only link (`#section`).
    IntraDocument {
        /// Fragment without the leading `#`.
        fragment: String,
    },
    /// Link to a non-markdown file in the project.
    NonMarkdown {
        /// Resolved path to the target.
        target: PathBuf,
    },
    /// Intra-project link to a markdown file.
    IntraProject {
        /// Resolved path to the target `.md` file.
        target: PathBuf,
        /// Fragment (heading anchor), if any.
        fragment: Option<String>,
        /// Predicate from title text, or `"references"` if absent.
        predicate: String,
        /// Whether the predicate was explicitly set via title text.
        explicit_predicate: bool,
    },
}

/// A heading extracted from a markdown document.
#[derive(Debug)]
pub struct Heading {
    /// 1-based line number in the source.
    pub line: usize,
    /// Heading level (1–6).
    pub level: u8,
    /// Raw text content of the heading.
    pub text: String,
    /// Heading anchor ID.
    pub id: HeadingId,
}

/// How a heading's anchor ID was determined.
#[derive(Debug)]
pub enum HeadingId {
    /// Explicit `{#id}` attribute on the heading.
    Explicit(String),
    /// Computed slugs from the heading text.
    Computed {
        /// GitHub slug.
        github: String,
        /// GitLab slug.
        gitlab: String,
        /// VS Code slug.
        vscode: String,
    },
}

/// Parse a markdown document, extracting links and headings.
///
/// `file_path` is used to resolve relative link targets. Pass the
/// workspace-relative path so resolved targets are also workspace-relative.
pub fn parse_document(content: &str, file_path: &Path) -> ParsedDocument {
    let options = Options::ENABLE_HEADING_ATTRIBUTES;
    let parser = Parser::new_ext(content, options);

    let mut links = Vec::new();
    let mut headings = Vec::new();
    let mut slugs = SlugCounts::new();

    let mut in_heading = false;
    let mut heading_text = String::new();
    let mut heading_start: Option<(u8, Option<String>, usize)> = None;

    for (event, range) in parser.into_offset_iter() {
        match event {
            Event::Start(Tag::Link {
                dest_url, title, ..
            }) => {
                let line = byte_offset_to_line(content, range.start);
                if let Some(link) = build_link(&dest_url, &title, file_path, line) {
                    links.push(link);
                }
            }
            Event::Start(Tag::Heading { level, id, .. }) => {
                in_heading = true;
                heading_text.clear();
                heading_start = Some((level_to_u8(level), id.map(|s| s.to_string()), range.start));
            }
            Event::Text(text) | Event::Code(text) if in_heading => {
                heading_text.push_str(&text);
            }
            Event::SoftBreak | Event::HardBreak if in_heading => {
                heading_text.push(' ');
            }
            Event::End(TagEnd::Heading(_)) => {
                if let Some((level, explicit_id, offset)) = heading_start.take() {
                    let line = byte_offset_to_line(content, offset);
                    let text = heading_text.trim().to_string();
                    let id = explicit_id.map_or_else(
                        || HeadingId::Computed {
                            github: slugs.next_github(&text),
                            gitlab: slugs.next_gitlab(&text),
                            vscode: slugs.next_vscode(&text),
                        },
                        HeadingId::Explicit,
                    );
                    headings.push(Heading {
                        line,
                        level,
                        text,
                        id,
                    });
                }
                in_heading = false;
            }
            _ => {}
        }
    }

    ParsedDocument { links, headings }
}

// --- Link helpers ---

fn build_link(url: &str, title: &str, file_path: &Path, line: usize) -> Option<Link> {
    if url.is_empty() {
        return None;
    }

    let kind = if is_external(url) {
        LinkKind::External {
            url: url.to_string(),
        }
    } else if let Some(fragment) = url.strip_prefix('#') {
        LinkKind::IntraDocument {
            fragment: fragment.to_string(),
        }
    } else {
        let (path_str, fragment) = split_url_fragment(url);
        let parent = file_path.parent().unwrap_or_else(|| Path::new(""));
        let target = normalize_path(&parent.join(path_str));

        if is_markdown_ext(&target) {
            let explicit_predicate = !title.is_empty();
            let predicate = if explicit_predicate {
                title.to_string()
            } else {
                "references".to_string()
            };
            LinkKind::IntraProject {
                target,
                fragment,
                predicate,
                explicit_predicate,
            }
        } else {
            LinkKind::NonMarkdown { target }
        }
    };

    Some(Link { line, kind })
}

fn is_external(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://") || url.starts_with("mailto:")
}

fn split_url_fragment(url: &str) -> (&str, Option<String>) {
    match url.split_once('#') {
        Some((path, frag)) => (path, Some(frag.to_string())),
        None => (url, None),
    }
}

fn is_markdown_ext(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "md")
}

/// Normalize a path by resolving `.` and `..` components without
/// touching the filesystem.
fn normalize_path(path: &Path) -> PathBuf {
    let mut parts: Vec<Component<'_>> = Vec::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(parts.last(), Some(Component::Normal(_))) {
                    parts.pop();
                } else {
                    parts.push(c);
                }
            }
            _ => parts.push(c),
        }
    }
    parts.iter().collect()
}

// --- Heading helpers ---

fn level_to_u8(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

#[allow(
    clippy::naive_bytecount,
    reason = "not worth a dependency for line counting"
)]
fn byte_offset_to_line(content: &str, offset: usize) -> usize {
    let offset = offset.min(content.len());
    content.as_bytes()[..offset]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
        + 1
}

// --- Slug algorithms ---

/// GitHub heading slug ([github-slugger] compatible).
///
/// Keeps Unicode letters, numbers, underscores, hyphens, and spaces.
/// Spaces become hyphens.
///
/// [github-slugger]: https://github.com/Flet/github-slugger
fn github_slug(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-' || *c == ' ')
        .map(|c| if c == ' ' { '-' } else { c })
        .collect()
}

/// GitLab heading slug.
///
/// ASCII-only: strips all non-ASCII characters. Collapses consecutive
/// hyphens and trims leading/trailing hyphens.
fn gitlab_slug(text: &str) -> String {
    let raw: String = text
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-' || *c == ' ')
        .map(|c| if c == ' ' { '-' } else { c })
        .collect();

    collapse_hyphens(&raw).trim_matches('-').to_string()
}

/// VS Code heading slug.
///
/// Keeps most characters. Strips specific ASCII punctuation.
/// Whitespace becomes hyphens.
fn vscode_slug(text: &str) -> String {
    let raw: String = text
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_whitespace() { '-' } else { c })
        .filter(|c| !is_vscode_punctuation(*c))
        .collect();

    raw.trim_matches('-').to_string()
}

fn collapse_hyphens(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut prev_hyphen = false;
    for c in s.chars() {
        if c == '-' {
            if !prev_hyphen {
                result.push(c);
            }
            prev_hyphen = true;
        } else {
            result.push(c);
            prev_hyphen = false;
        }
    }
    result
}

const fn is_vscode_punctuation(c: char) -> bool {
    matches!(
        c,
        '[' | ']'
            | '!'
            | '"'
            | '#'
            | '$'
            | '%'
            | '&'
            | '\''
            | '('
            | ')'
            | '*'
            | '+'
            | ','
            | '.'
            | '/'
            | ':'
            | ';'
            | '<'
            | '='
            | '>'
            | '?'
            | '@'
            | '\\'
            | '^'
            | '{'
            | '|'
            | '}'
            | '~'
            | '`'
    )
}

// --- Slug deduplication ---

/// Tracks slug occurrences across a document for deduplication.
struct SlugCounts {
    github: HashMap<String, usize>,
    gitlab: HashMap<String, usize>,
    vscode: HashMap<String, usize>,
}

impl SlugCounts {
    fn new() -> Self {
        Self {
            github: HashMap::new(),
            gitlab: HashMap::new(),
            vscode: HashMap::new(),
        }
    }

    fn next_github(&mut self, text: &str) -> String {
        deduplicate(github_slug(text), &mut self.github)
    }

    fn next_gitlab(&mut self, text: &str) -> String {
        deduplicate(gitlab_slug(text), &mut self.gitlab)
    }

    fn next_vscode(&mut self, text: &str) -> String {
        deduplicate(vscode_slug(text), &mut self.vscode)
    }
}

/// Deduplicate a slug by appending `-1`, `-2`, etc. on collision.
///
/// Follows the [github-slugger] algorithm: if `base` is taken, increment
/// the counter on the original base and try `{base}-{n}` until unique.
///
/// [github-slugger]: https://github.com/Flet/github-slugger
fn deduplicate(base: String, slugs: &mut HashMap<String, usize>) -> String {
    let original = base.clone();
    let mut slug = base;
    while slugs.contains_key(&slug) {
        let count = slugs.entry(original.clone()).or_insert(0);
        *count += 1;
        slug = format!("{original}-{count}");
    }
    slugs.insert(slug.clone(), 0);
    slug
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::panic,
    reason = "tests use expect and panic for clarity"
)]
mod tests {
    use super::*;

    // --- Link extraction ---

    #[test]
    fn inline_link_with_predicate() {
        let doc = parse_document(
            r#"[Decision #26](decisions/26.md "supersedes")"#,
            Path::new("index.md"),
        );

        assert_eq!(doc.links.len(), 1, "should find one link");
        match &doc.links[0].kind {
            LinkKind::IntraProject {
                target,
                predicate,
                fragment,
                explicit_predicate,
            } => {
                assert_eq!(target, Path::new("decisions/26.md"), "target path");
                assert_eq!(predicate, "supersedes", "predicate");
                assert!(fragment.is_none(), "no fragment");
                assert!(explicit_predicate, "predicate was explicit");
            }
            other => panic!("expected IntraProject, got {other:?}"),
        }
    }

    #[test]
    fn reference_link_with_predicate() {
        let doc = parse_document(
            "See [the decision][dec26] for context.\n\n[dec26]: decisions/26.md \"supersedes\"\n",
            Path::new("index.md"),
        );

        assert_eq!(doc.links.len(), 1, "should find one link");
        match &doc.links[0].kind {
            LinkKind::IntraProject {
                target,
                predicate,
                explicit_predicate,
                ..
            } => {
                assert_eq!(target, Path::new("decisions/26.md"), "target");
                assert_eq!(predicate, "supersedes", "predicate from definition");
                assert!(explicit_predicate, "predicate was explicit");
            }
            other => panic!("expected IntraProject, got {other:?}"),
        }
    }

    #[test]
    fn default_predicate_when_no_title() {
        let doc = parse_document("[other doc](other.md)", Path::new("index.md"));

        assert_eq!(doc.links.len(), 1, "should find one link");
        match &doc.links[0].kind {
            LinkKind::IntraProject {
                predicate,
                explicit_predicate,
                ..
            } => {
                assert_eq!(predicate, "references", "default predicate");
                assert!(!explicit_predicate, "predicate was implicit");
            }
            other => panic!("expected IntraProject, got {other:?}"),
        }
    }

    #[test]
    fn link_with_fragment() {
        let doc = parse_document(
            r#"[section](other.md#context "supersedes")"#,
            Path::new("index.md"),
        );

        assert_eq!(doc.links.len(), 1, "should find one link");
        match &doc.links[0].kind {
            LinkKind::IntraProject {
                target,
                fragment,
                predicate,
                explicit_predicate,
            } => {
                assert_eq!(target, Path::new("other.md"), "target");
                assert_eq!(fragment.as_deref(), Some("context"), "fragment");
                assert_eq!(predicate, "supersedes", "predicate");
                assert!(explicit_predicate, "predicate was explicit");
            }
            other => panic!("expected IntraProject, got {other:?}"),
        }
    }

    // --- Link classification ---

    #[test]
    fn classify_external_urls() {
        let doc = parse_document(
            "[a](https://example.com) [b](http://example.com) [c](mailto:u@e.com)",
            Path::new("index.md"),
        );

        assert_eq!(doc.links.len(), 3, "should find three links");
        for link in &doc.links {
            assert!(
                matches!(link.kind, LinkKind::External { .. }),
                "should be external: {link:?}"
            );
        }
    }

    #[test]
    fn classify_intra_document_fragment() {
        let doc = parse_document("[context](#context)", Path::new("index.md"));

        assert_eq!(doc.links.len(), 1, "should find one link");
        match &doc.links[0].kind {
            LinkKind::IntraDocument { fragment } => {
                assert_eq!(fragment, "context", "fragment without #");
            }
            other => panic!("expected IntraDocument, got {other:?}"),
        }
    }

    #[test]
    fn classify_non_markdown_target() {
        let doc = parse_document("[diagram](architecture.png)", Path::new("index.md"));

        assert_eq!(doc.links.len(), 1, "should find one link");
        match &doc.links[0].kind {
            LinkKind::NonMarkdown { target } => {
                assert_eq!(target, Path::new("architecture.png"), "target");
            }
            other => panic!("expected NonMarkdown, got {other:?}"),
        }
    }

    #[test]
    fn resolve_relative_path() {
        let doc = parse_document(
            r#"[other](../api/endpoints.md "implements")"#,
            Path::new("docs/design/overview.md"),
        );

        assert_eq!(doc.links.len(), 1, "should find one link");
        match &doc.links[0].kind {
            LinkKind::IntraProject { target, .. } => {
                assert_eq!(target, Path::new("docs/api/endpoints.md"), "resolved path");
            }
            other => panic!("expected IntraProject, got {other:?}"),
        }
    }

    #[test]
    fn images_not_extracted_as_links() {
        let doc = parse_document(r#"![alt text](image.png "caption")"#, Path::new("index.md"));
        assert!(doc.links.is_empty(), "images should not produce links");
    }

    // --- Heading extraction ---

    #[test]
    fn heading_with_explicit_id() {
        let doc = parse_document("## My Heading {#custom-id}\n", Path::new("index.md"));

        assert_eq!(doc.headings.len(), 1, "should find one heading");
        let h = &doc.headings[0];
        assert_eq!(h.level, 2, "heading level");
        assert_eq!(h.text, "My Heading", "heading text");
        match &h.id {
            HeadingId::Explicit(id) => assert_eq!(id, "custom-id", "explicit id"),
            HeadingId::Computed { .. } => panic!("expected explicit id"),
        }
    }

    #[test]
    fn heading_computed_slugs() {
        let doc = parse_document("## Hello World!\n", Path::new("index.md"));

        assert_eq!(doc.headings.len(), 1, "should find one heading");
        match &doc.headings[0].id {
            HeadingId::Computed {
                github,
                gitlab,
                vscode,
            } => {
                assert_eq!(github, "hello-world", "github slug");
                assert_eq!(gitlab, "hello-world", "gitlab slug");
                assert_eq!(vscode, "hello-world", "vscode slug");
            }
            HeadingId::Explicit(_) => panic!("expected computed slugs"),
        }
    }

    #[test]
    fn slug_algorithm_differences() {
        // GitLab strips non-ASCII; GitLab collapses consecutive hyphens.
        let doc = parse_document("## Héllo & Wörld\n", Path::new("index.md"));

        assert_eq!(doc.headings.len(), 1, "should find one heading");
        match &doc.headings[0].id {
            HeadingId::Computed {
                github,
                gitlab,
                vscode,
            } => {
                // GitHub keeps Unicode, doesn't collapse hyphens
                assert_eq!(
                    github, "héllo--wörld",
                    "github keeps unicode, double hyphen"
                );
                // GitLab strips non-ASCII and collapses hyphens
                assert_eq!(gitlab, "hllo-wrld", "gitlab strips non-ascii, collapses");
                // VS Code keeps Unicode, doesn't collapse hyphens
                assert_eq!(
                    vscode, "héllo--wörld",
                    "vscode keeps unicode, double hyphen"
                );
            }
            HeadingId::Explicit(_) => panic!("expected computed slugs"),
        }
    }

    #[test]
    fn deduplicate_heading_slugs() {
        let doc = parse_document(
            "## Heading\n## Heading\n## Heading\n",
            Path::new("index.md"),
        );

        assert_eq!(doc.headings.len(), 3, "should find three headings");
        let slugs: Vec<&str> = doc
            .headings
            .iter()
            .map(|h| match &h.id {
                HeadingId::Computed { github, .. } => github.as_str(),
                HeadingId::Explicit(_) => panic!("expected computed"),
            })
            .collect();

        assert_eq!(
            slugs,
            vec!["heading", "heading-1", "heading-2"],
            "deduplicated slugs"
        );
    }

    #[test]
    fn heading_with_code() {
        let doc = parse_document("## The `Config` struct\n", Path::new("index.md"));

        assert_eq!(doc.headings.len(), 1, "should find one heading");
        assert_eq!(
            doc.headings[0].text, "The Config struct",
            "code spans included in heading text"
        );
    }

    // --- Line numbers ---

    #[test]
    fn line_numbers_correct() {
        let doc = parse_document(
            "first line\n\n[link](other.md)\n\n## Heading\n",
            Path::new("index.md"),
        );

        assert_eq!(doc.links.len(), 1, "one link");
        assert_eq!(doc.links[0].line, 3, "link on line 3");
        assert_eq!(doc.headings.len(), 1, "one heading");
        assert_eq!(doc.headings[0].line, 5, "heading on line 5");
    }

    // --- Slug unit tests ---

    #[test]
    fn github_slug_basic() {
        assert_eq!(github_slug("Hello World"), "hello-world", "basic");
        assert_eq!(github_slug("C++ Guide"), "c-guide", "strips non-word");
        assert_eq!(
            github_slug("under_score"),
            "under_score",
            "keeps underscore"
        );
    }

    #[test]
    fn gitlab_slug_basic() {
        assert_eq!(gitlab_slug("Hello World"), "hello-world", "basic");
        assert_eq!(gitlab_slug("A & B"), "a-b", "collapses hyphens");
        assert_eq!(gitlab_slug("-leading-"), "leading", "trims hyphens");
    }

    #[test]
    fn vscode_slug_basic() {
        assert_eq!(vscode_slug("Hello World"), "hello-world", "basic");
        assert_eq!(
            vscode_slug("under_score"),
            "under_score",
            "keeps underscore"
        );
        assert_eq!(
            vscode_slug("  padded  "),
            "padded",
            "trims whitespace and hyphens"
        );
    }

    #[test]
    fn deduplication_pathological() {
        // If "foo-1" is a natural slug and "foo" also appears,
        // the deduplication should not collide.
        let mut slugs = HashMap::new();

        let first = deduplicate("foo".to_string(), &mut slugs);
        assert_eq!(first, "foo", "first foo");

        let foo_1 = deduplicate("foo-1".to_string(), &mut slugs);
        assert_eq!(foo_1, "foo-1", "natural foo-1");

        let second = deduplicate("foo".to_string(), &mut slugs);
        // "foo-1" is taken by the natural slug, so should skip to "foo-2"
        assert_eq!(second, "foo-2", "dedup skips taken foo-1");
    }
}
