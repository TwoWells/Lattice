// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Consumer helpers: turning parsed nodes into the answers the graph needs.
//!
//! The tree records what the source *says*; this module decides what it
//! *means* to a workspace. Four families:
//!
//! - **Link classification.** Is a destination external, an embed, an import,
//!   or an intra-project path — and what does it resolve to relative to the
//!   document. The external test is by URI grammar, not a scheme allowlist
//!   (issue 071), with the one deliberate boundary at `C:\notes.md`.
//! - **Slug algorithms.** The three heading-anchor conventions (`github`,
//!   `gitlab`, `vscode`) and their per-document deduplication, which
//!   `[policy] fragments` pins a workspace to.
//! - **Bare path detection.** Recognizing an unmarked path in prose, which is
//!   the raw material of the structural dark-matter diagnostics.
//! - **Text helpers.** Code-span stripping and offset→line conversion the
//!   above need.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use crate::span::Span;

use super::{BarePath, ElementKind, Link, LinkKind};

// ---------------------------------------------------------------------------
// Consumer helpers
// ---------------------------------------------------------------------------

/// Normalize a path by resolving `.` and `..` components without touching
/// the filesystem.
pub fn normalize_path(path: &Path) -> PathBuf {
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

/// Check whether a URL is external — anything that is not a workspace path.
///
/// Recognition is by *grammar*, not by a list of known schemes (issue 071): a
/// destination carrying any URI scheme is a URI, so it is never resolved
/// against the source document. A prefix list answers the question backwards —
/// every scheme nobody enumerated (`data:`, `tel:`, `sms:`, `ftp:`, `file:`,
/// the `javascript:` an author quotes as an example) is diagnosed as a missing
/// file, and a base64-inlined `![](data:image/png;base64,…)` embed is a common,
/// sanctioned way to write a self-contained document.
///
/// A protocol-relative URL (`//host/path`) is external too: a renderer
/// resolves it against the current scheme and host, never against the
/// repository root, so it must not be read as a root-relative workspace path
/// (issue 028).
///
/// See [`has_uri_scheme`] for the scheme grammar and the one boundary it
/// decides — `C:\notes.md`.
pub fn is_external(url: &str) -> bool {
    url.starts_with("//") || has_uri_scheme(url)
}

/// Whether `url` opens with an RFC 3986 scheme — `ALPHA *( ALPHA / DIGIT / "+"
/// / "-" / "." ) ":"` — requiring at least two scheme characters.
///
/// The grammar is the same production [`crate::html::try_autolink`] uses to
/// tell a URI autolink from an email one, so a destination and an autolink
/// agree on what a scheme is. The run must start at byte 0: a `/` (or any other
/// character outside the scheme set) before the `:` breaks it, which is what
/// keeps `docs/a:b.md` and `12:30` out of the external bucket.
///
/// **The two-character minimum is the deliberate boundary.** A single ALPHA
/// followed by `:` is read as a Windows drive letter — `C:\notes.md` is a path,
/// not a URI — because one-letter schemes are essentially nonexistent in the
/// wild while the drive spelling has real users. `CommonMark` draws the line in
/// the same place: its own absolute-URI production requires a scheme of 2–32
/// characters. Above that floor `CommonMark`'s reading governs, so a bare
/// `foo:bar` *is* a URI (that is how a browser resolves an `href`), not a
/// relative path that happens to contain a colon.
///
/// No upper bound is imposed: `CommonMark`'s 32-character cap is an autolink
/// restriction, not an RFC 3986 one, and a longer scheme run is still not a
/// workspace path.
pub fn has_uri_scheme(url: &str) -> bool {
    // ASCII-only comparisons: a multi-byte character's bytes are all >= 0x80,
    // so none of them can match the scheme set or the terminating `:`, and the
    // run simply stops there.
    let Some((first, rest)) = url.as_bytes().split_first() else {
        return false;
    };
    if !first.is_ascii_alphabetic() {
        return false;
    }
    let tail = rest
        .iter()
        .take_while(|&&b| b.is_ascii_alphanumeric() || b == b'+' || b == b'-' || b == b'.')
        .count();
    tail >= 1 && rest.get(tail) == Some(&b':')
}

/// Resolve a link target path string against the source document's path.
///
/// `doc_path` is the document's **absolute** path in production (its key in the
/// server's flat store, or `root.join(rel)` for the CLI's owning workspace), so
/// a document-relative target resolves to an absolute path that encodes *no*
/// workspace root — the coordinate move of decision 019 clause 8. A root
/// re-enters only where a target is matched or displayed.
///
/// A leading single `/` is **root-relative**: GitHub and web renderers resolve
/// `/foo.md` against the repository (workspace) root, not the filesystem root
/// (issue 028). The root is not known at parse time, so such a target keeps its
/// deferred form — the leading `/` is stripped and the relative remainder is
/// stored verbatim, to be joined onto whichever root matches it at query time.
/// Stripping the `/` also keeps it inside the workspace: it can never escape to
/// an absolute filesystem path. The result is normalized in both cases.
///
/// The two forms are self-describing by absoluteness: a document-relative
/// target is absolute (given an absolute `doc_path`), a root-relative remainder
/// is relative. `WorkspaceLike` uses exactly this distinction to map a target
/// back onto its stored key.
pub fn resolve_target_path(path_str: &str, doc_path: &Path) -> PathBuf {
    // `//host/...` is handled as external before this point; a single leading
    // `/` here is unambiguously root-relative — strip it and keep the relative
    // remainder for query-time root resolution. Otherwise resolve against the
    // source document's parent directory (absolute in production).
    path_str.strip_prefix('/').map_or_else(
        || {
            let parent = doc_path.parent().unwrap_or_else(|| Path::new(""));
            normalize_path(&parent.join(path_str))
        },
        |rooted| normalize_path(Path::new(rooted)),
    )
}

/// Split a URL into path and optional fragment.
pub fn split_url_fragment(url: &str) -> (&str, Option<String>) {
    match url.split_once('#') {
        Some((path, frag)) => (path, Some(frag.to_string())),
        None => (url, None),
    }
}

/// Check whether a path has a `.md` extension.
pub fn is_markdown_ext(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "md")
}

/// Video file extensions.
static VIDEO_EXTENSIONS: phf::Set<&str> = phf::phf_set! {
    "mp4", "webm", "ogv", "mov", "avi", "mkv",
};

/// Audio file extensions.
static AUDIO_EXTENSIONS: phf::Set<&str> = phf::phf_set! {
    "mp3", "wav", "ogg", "flac", "aac", "m4a", "opus",
};

/// Classify an image URL into `Image`, `Video`, or `Audio` based on
/// file extension. Falls back to `Image` for unknown extensions.
pub fn classify_media(url: String, title: String) -> ElementKind {
    let path = url.split(['?', '#']).next().unwrap_or(&url);
    if let Some(ext) = path.rsplit('.').next() {
        let ext_lower = ext.to_lowercase();
        if VIDEO_EXTENSIONS.contains(ext_lower.as_str()) {
            return ElementKind::Video { url, title };
        }
        if AUDIO_EXTENSIONS.contains(ext_lower.as_str()) {
            return ElementKind::Audio { url, title };
        }
    }
    ElementKind::Image { url, title }
}

/// Classify a raw link URL and title into a [`Link`].
///
/// `doc_path` is the source document's absolute path (see
/// [`resolve_target_path`]); an [`LinkKind::IntraProject`] / [`LinkKind::NonMarkdown`]
/// `target` is therefore absolute for a document-relative link and a relative
/// remainder for a root-relative (`/x`) one.
pub fn classify_link(
    url: &str,
    title: &str,
    doc_path: &Path,
    line: usize,
    span: Span,
) -> Option<Link> {
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
        let target = resolve_target_path(path_str, doc_path);

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

    Some(Link { line, span, kind })
}

/// Classify an embed source URL (image / video / audio) into a [`Link`].
///
/// Mirrors [`classify_link`]'s resolution — same [`is_external`] oracle, same
/// [`resolve_target_path`] coordinates — but lands every in-project destination
/// in [`LinkKind::Embed`] regardless of extension: an embed asserts no relation,
/// so it never becomes an [`LinkKind::IntraProject`] edge with a predicate and a
/// backlink obligation. Returns `None` for an empty source and for a
/// fragment-only one (`![](#x)`), neither of which denotes a file.
pub fn classify_embed(url: &str, doc_path: &Path, line: usize, span: Span) -> Option<Link> {
    if url.is_empty() || url.starts_with('#') {
        return None;
    }

    let kind = if is_external(url) {
        LinkKind::External {
            url: url.to_string(),
        }
    } else {
        let (path_str, _fragment) = split_url_fragment(url);
        if path_str.is_empty() {
            return None;
        }
        LinkKind::Embed {
            target: resolve_target_path(path_str, doc_path),
        }
    };

    Some(Link { line, span, kind })
}

/// Classify an import directive path into a [`Link`].
pub fn classify_import(path: &str, doc_path: &Path, line: usize, span: Span) -> Link {
    let target = resolve_target_path(path, doc_path);
    let kind = if is_markdown_ext(&target) {
        LinkKind::IntraProject {
            target,
            fragment: None,
            predicate: "imports".to_string(),
            explicit_predicate: true,
        }
    } else {
        LinkKind::NonMarkdown { target }
    };
    Link { line, span, kind }
}

// --- Slug algorithms ---

/// GitHub heading slug ([github-slugger] compatible).
pub fn github_slug(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-' || *c == ' ')
        .map(|c| if c == ' ' { '-' } else { c })
        .collect()
}

/// GitLab heading slug.
pub fn gitlab_slug(text: &str) -> String {
    let raw: String = text
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-' || *c == ' ')
        .map(|c| if c == ' ' { '-' } else { c })
        .collect();

    collapse_hyphens(&raw).trim_matches('-').to_string()
}

/// VS Code heading slug.
pub fn vscode_slug(text: &str) -> String {
    let raw: String = text
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_whitespace() { '-' } else { c })
        .filter(|c| !is_vscode_punctuation(*c))
        .collect();

    raw.trim_matches('-').to_string()
}

pub fn collapse_hyphens(s: &str) -> String {
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

pub const fn is_vscode_punctuation(c: char) -> bool {
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

/// Tracks slug occurrences across a document for deduplication.
pub struct SlugCounts {
    github: HashMap<String, usize>,
    gitlab: HashMap<String, usize>,
    vscode: HashMap<String, usize>,
}

impl SlugCounts {
    pub fn new() -> Self {
        Self {
            github: HashMap::new(),
            gitlab: HashMap::new(),
            vscode: HashMap::new(),
        }
    }

    pub fn next_github(&mut self, text: &str) -> String {
        deduplicate(github_slug(text), &mut self.github)
    }

    pub fn next_gitlab(&mut self, text: &str) -> String {
        deduplicate(gitlab_slug(text), &mut self.gitlab)
    }

    pub fn next_vscode(&mut self, text: &str) -> String {
        deduplicate(vscode_slug(text), &mut self.vscode)
    }
}

/// Deduplicate a slug by appending `-1`, `-2`, etc. on collision.
pub fn deduplicate(base: String, slugs: &mut HashMap<String, usize>) -> String {
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

// --- Bare path detection ---

/// File extensions recognized in `@path` import directives.
pub const IMPORT_EXTENSIONS: &[&str] = &[".json", ".md", ".toml", ".txt", ".xml", ".yaml", ".yml"];

/// Check whether a string looks like a bare markdown path.
///
/// Scoped to `.md` only (issue 028): `.md` is the extension that forms a graph
/// edge, so it is the only intra-repo path-shape worth nudging into a link. A
/// trailing `#fragment` is stripped before the extension check, so
/// `foo.md#section` (a genuine anchored reference) is recognized just like
/// `foo.md`.
///
/// An external-namespace token (`{Name}/…`, issue 030) is the one exception to
/// the `.md` scope: it is recognized regardless of extension, so a cross-repo
/// directory or non-`.md` reference (`{Archive}/docs`, `{Archive}/schema.txt`)
/// is collected and existence-checked against its alias directory. The `.md`
/// rationale does not apply — an external reference never forms a graph edge
/// (decision 010), and the explicit `{Name}/` brace is a deliberate opt-in, not
/// the ambiguous prose mention the `.md` scope guards against.
///
/// Shapes that are not workspace paths are rejected outright: a `~`-leading
/// token (home-relative, out of the repo), a token containing `<` or `>` (a
/// placeholder), a token containing `*` (a glob), and a token containing an
/// ellipsis — `…` (U+2026) or `...` — which is documentation shorthand for "a
/// path of this shape" (e.g. the `{repo}/…` syntax this very tool teaches), not
/// a real file. These mirror the same exclusions in the prose path scan
/// ([`crate::structural`]) and apply to external tokens too — a `{Name}/…`
/// placeholder is exempt, while a concrete `{Name}/path` is resolved.
pub fn is_bare_path(s: &str) -> bool {
    let path = split_path_fragment(s).0;
    !is_import_directive(path)
        && !path.starts_with('~')
        && !path.contains('<')
        && !path.contains('>')
        && !path.contains('*')
        && !path.contains('…')
        && !path.contains("...")
        && path.contains('/')
        && (is_markdown_ext(Path::new(path)) || external_namespace(path).is_some())
}

/// Recognize an external-namespace reference of the form `{<identifier>}/rest`.
///
/// Returns `(alias, rest)` — the bare alias name (inside the braces) and the
/// path following the `}/` — when the token is shaped as an external reference
/// (issue 030, decision 010). This is the single recognizer shared by the bare
/// scanner ([`is_bare_path`]) and the prose/quoted/backtick scanners
/// ([`crate::structural`]), so the surfaces cannot drift. It is matched
/// **before** the normal dir/root resolution so the literal `{Name}` component
/// is never dir-joined and mis-flagged as a dangling intra-repo path, and
/// independently of the `.md` extension scope so a cross-repo directory or
/// non-`.md` file is recognized.
///
/// An identifier is one or more of `[A-Za-z0-9_-]`; the braces must wrap a
/// non-empty identifier and be immediately followed by `/` and a non-empty
/// remainder. `{}/x`, `{ }/x`, `{a b}/x`, a bare `{Name}` with no trailing `/`,
/// and `{Name}/` with no remainder are all rejected — they are not external
/// references and fall through to ordinary handling.
pub fn external_namespace(s: &str) -> Option<(&str, &str)> {
    let after_brace = s.strip_prefix('{')?;
    let close = after_brace.find('}')?;
    let alias = &after_brace[..close];
    let rest = after_brace[close + 1..].strip_prefix('/')?;
    if alias.is_empty()
        || rest.is_empty()
        || !alias
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return None;
    }
    Some((alias, rest))
}

/// Split a path-shaped token into its path and optional `#fragment`.
///
/// Mirrors the link-target classifier's fragment handling (issue 028): a
/// markdown link can target `path#fragment`, so the dark-matter scan must
/// strip the fragment before resolving the path part for existence.
pub fn split_path_fragment(s: &str) -> (&str, Option<&str>) {
    match s.split_once('#') {
        Some((path, frag)) => (path, Some(frag)),
        None => (s, None),
    }
}

/// Check whether a string is an `@path` import directive.
pub fn is_import_directive(s: &str) -> bool {
    let Some(path) = s.strip_prefix('@') else {
        return false;
    };
    is_import_path(path)
}

/// Check whether a path (after stripping `@`) looks like a relative import.
pub fn is_import_path(path: &str) -> bool {
    if path.starts_with('/') || path.starts_with('~') || path.is_empty() {
        return false;
    }
    IMPORT_EXTENSIONS.iter().any(|ext| path.ends_with(ext))
}

/// Scan a text segment for bare file paths.
///
/// Quote characters are deliberately **not** trimmed (issue 032): a quoted
/// dir-bearing token like `"docs/x.md"` is owned by the structural quoted-path
/// scanner ([`crate::structural`]), the sole owner of quoted content. Trimming
/// the quotes here would let the bare-path surface also claim the inner string,
/// double-emitting the stale-reference (or make-it-a-link) diagnostic. Leaving
/// the quotes attached makes the token fail the extension check, so the two
/// surfaces partition the text instead of overlapping. Only prose-adjacent
/// punctuation and bracketing are stripped.
pub fn scan_bare_paths_in_text(text: &str, base_line: usize, out: &mut Vec<BarePath>) {
    for (line_idx, line_text) in text.split('\n').enumerate() {
        for word in line_text.split_whitespace() {
            let cleaned = word
                .trim_start_matches(['(', '['])
                .trim_end_matches([',', '.', ';', ':', '!', '?', ')', ']']);

            if is_bare_path(cleaned) {
                // Store the fragment-stripped path so existence resolution and
                // the emitted message agree on the file the reference targets.
                let path = split_path_fragment(cleaned).0;
                out.push(BarePath {
                    line: base_line + line_idx,
                    path: path.to_string(),
                });
            }
        }
    }
}

// --- Text helpers ---

/// Convert a byte offset to a 1-based line number.
///
/// Recognizes `\n`, `\r\n`, and bare `\r` line endings (delegates to the
/// crate-wide counter in [`crate::fm`]).
pub fn byte_offset_to_line(content: &str, offset: usize) -> usize {
    crate::fm::byte_offset_to_line(content, offset)
}

/// Strip backtick-delimited code spans from text, keeping inner content.
pub fn strip_code_spans(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut result = String::with_capacity(text.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'`' {
            let tick_count = bytes[i..].iter().take_while(|&&b| b == b'`').count();
            if let Some(end) = find_code_span_close(bytes, i + tick_count, tick_count) {
                let inner = &text[i + tick_count..end];
                // CommonMark: strip one leading and one trailing space if both present
                // and content is not all spaces.
                let stripped = if inner.len() >= 2
                    && inner.starts_with(' ')
                    && inner.ends_with(' ')
                    && inner.trim().len() < inner.len()
                {
                    &inner[1..inner.len() - 1]
                } else {
                    inner
                };
                result.push_str(stripped);
                i = end + tick_count;
            } else {
                for _ in 0..tick_count {
                    result.push('`');
                }
                i += tick_count;
            }
        } else {
            let ch = text[i..].chars().next().unwrap_or(' ');
            result.push(ch);
            i += ch.len_utf8();
        }
    }

    result
}

/// Find closing backticks of exactly `count` length.
pub fn find_code_span_close(bytes: &[u8], start: usize, count: usize) -> Option<usize> {
    let mut i = start;
    while i < bytes.len() {
        if bytes[i] == b'`' {
            let n = bytes[i..].iter().take_while(|&&b| b == b'`').count();
            if n == count {
                return Some(i);
            }
            i += n;
        } else {
            i += 1;
        }
    }
    None
}

/// Compute the byte span of the text content inside an HTML heading tag.
///
/// Given `<h1>text</h1>` and its `base` offset in the source, returns the
/// span covering `text`.
pub fn html_heading_text_span(raw: &str, base: usize) -> Span {
    let start = raw.find('>').map_or(0, |i| i + 1);
    let end = raw.rfind("</").unwrap_or(raw.len());
    Span::new(base + start, base + end)
}

/// Extract display text from an HTML heading like `<h1>text</h1>`.
pub fn extract_html_heading_text(source: &str) -> String {
    // Strip the opening tag
    let after_open = source.find('>').map_or(source, |i| &source[i + 1..]);
    // Strip the closing tag
    let before_close = after_open
        .rfind("</")
        .map_or(after_open, |i| &after_open[..i]);
    // Join lines and trim
    before_close
        .lines()
        .map(str::trim)
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}
