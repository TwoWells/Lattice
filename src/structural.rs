// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Structural diagnostics — document quality checks that run unconditionally.
//!
//! These diagnostics validate the document as a well-formed markdown/HTML
//! artifact, independent of Lattice's predicate graph. They run on every
//! file regardless of whether `.lattice.toml` is present.

use std::collections::HashMap;
use std::path::Path;

use crate::block::{self, ElementKind, Syntax, Tree};
use crate::config::{BarePathPolicy, CodeBlockLanguagePolicy, Config};
use crate::html;
use crate::span::Span;
use crate::validation::{Diagnostic, Severity};

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Collect all structural diagnostics for a single file.
///
/// `rel_path` is the workspace-relative path, used for bare path existence
/// checks via `file_exists`. `config` controls severity for configurable
/// diagnostics (code block language, admonitions).
pub fn collect(
    tree: &Tree,
    rel_path: &Path,
    config: &Config,
    file_exists: &dyn Fn(&Path) -> bool,
) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let source = tree.source();

    emit_parser_diagnostics(tree, rel_path, &mut diagnostics);
    emit_heading_diagnostics(tree, rel_path, &mut diagnostics);
    emit_tree_bare_paths(tree, rel_path, config, file_exists, &mut diagnostics);
    emit_bare_path_diagnostics(
        tree,
        rel_path,
        config.policy.bare_paths,
        file_exists,
        &mut diagnostics,
    );
    emit_html_diagnostics(tree, rel_path, &mut diagnostics);
    check_markdown_in_opaque_html(tree, rel_path, &mut diagnostics);
    emit_code_block_diagnostics(tree, rel_path, config, &mut diagnostics);
    emit_image_diagnostics(tree, rel_path, &mut diagnostics);
    emit_trailing_whitespace_diagnostics(source, rel_path, tree, &mut diagnostics);
    emit_missing_blank_line_diagnostics(tree, rel_path, &mut diagnostics);

    diagnostics.sort_by(|a, b| a.file.cmp(&b.file).then(a.line.cmp(&b.line)));
    diagnostics
}

// ---------------------------------------------------------------------------
// Parser diagnostics
// ---------------------------------------------------------------------------

/// Emit diagnostics that the parser already collected (unclosed fenced code
/// blocks, unclosed HTML tags, unexpected close tags, table cell mismatches,
/// unused/duplicate reference definitions).
fn emit_parser_diagnostics(tree: &Tree, rel_path: &Path, out: &mut Vec<Diagnostic>) {
    let source = tree.source();
    for diag in tree.diagnostics() {
        let line = block::byte_offset_to_line(source, diag.span.start);
        let severity = match diag.level {
            block::DiagnosticLevel::Error => Severity::Error,
            block::DiagnosticLevel::Warning => Severity::Warning,
        };
        out.push(Diagnostic {
            file: rel_path.to_path_buf(),
            line,
            severity,
            message: diag.message.clone(),
            span: Some(diag.span),
        });
    }
}

// ---------------------------------------------------------------------------
// Bare path diagnostics (from tree)
// ---------------------------------------------------------------------------

/// Emit diagnostics for bare file paths detected by the tree's `bare_paths()`
/// scanner. Severity depends on the `bare_paths` policy and file existence.
fn emit_tree_bare_paths(
    tree: &Tree,
    rel_path: &Path,
    config: &Config,
    file_exists: &dyn Fn(&Path) -> bool,
    out: &mut Vec<Diagnostic>,
) {
    if config.policy.bare_paths == BarePathPolicy::Disabled {
        return;
    }

    let bare_paths = tree.bare_paths();
    for bare in &bare_paths {
        let target = resolve_relative(rel_path, &bare.path);
        let exists = file_exists(&target);

        let severity = match (exists, config.policy.bare_paths) {
            (true, BarePathPolicy::Deny) => Severity::Error,
            (true, _) => Severity::Warning,
            (false, _) => Severity::Hint,
        };

        out.push(Diagnostic {
            file: rel_path.to_path_buf(),
            line: bare.line,
            severity,
            message: format!("bare path `{}`: convert to a markdown link", bare.path),
            // `BarePath` carries only a line; fall back to a whole-line range.
            span: None,
        });
    }
}

// ---------------------------------------------------------------------------
// Heading diagnostics
// ---------------------------------------------------------------------------

/// Emit heading diagnostics: skipped levels, multiple H1, duplicate text,
/// empty headings.
fn emit_heading_diagnostics(tree: &Tree, rel_path: &Path, out: &mut Vec<Diagnostic>) {
    let source = tree.source();
    let mut prev_level: Option<u8> = None;
    let mut h1_count = 0u32;
    let mut seen_texts: HashMap<String, usize> = HashMap::new();

    for node in tree.nodes() {
        let ElementKind::Heading { level } = &node.kind else {
            continue;
        };
        let level = *level;
        let line = block::byte_offset_to_line(source, node.span.start);

        let raw = &source[node.span.start..node.span.end];
        let text = heading_display_text(raw, node.syntax);

        if text.trim().is_empty() {
            out.push(Diagnostic {
                file: rel_path.to_path_buf(),
                line,
                severity: Severity::Warning,
                message: "empty heading".to_string(),
                span: Some(node.span),
            });
            prev_level = Some(level);
            continue;
        }

        if level == 1 {
            h1_count += 1;
            if h1_count == 2 {
                out.push(Diagnostic {
                    file: rel_path.to_path_buf(),
                    line,
                    severity: Severity::Warning,
                    message: "multiple H1 headings".to_string(),
                    span: Some(node.span),
                });
            }
        }

        if let Some(prev) = prev_level
            && level > prev + 1
        {
            out.push(Diagnostic {
                file: rel_path.to_path_buf(),
                line,
                severity: Severity::Warning,
                message: format!("skipped heading level: H{prev} to H{level}"),
                span: Some(node.span),
            });
        }

        prev_level = Some(level);

        let normalized = text.trim().to_lowercase();
        if let Some(&first_line) = seen_texts.get(&normalized) {
            out.push(Diagnostic {
                file: rel_path.to_path_buf(),
                line,
                severity: Severity::Warning,
                message: format!(
                    "duplicate heading text `{}` (first at line {first_line})",
                    text.trim()
                ),
                span: Some(node.span),
            });
        } else {
            seen_texts.insert(normalized, line);
        }
    }
}

/// Extract display text from a heading node.
fn heading_display_text(raw: &str, syntax: Syntax) -> String {
    if syntax == Syntax::Html {
        return block::extract_html_heading_text(raw);
    }

    let trimmed = raw.trim_start();
    if trimmed.starts_with('#') {
        let first_line = raw.lines().next().unwrap_or("");
        let after_hashes = first_line.trim_start_matches('#');
        let content = after_hashes.trim();
        let content = content.trim_end_matches('#').trim_end();
        if let Some(brace) = content.rfind("{#")
            && content.ends_with('}')
        {
            return content[..brace].trim().to_string();
        }
        content.to_string()
    } else {
        let lines: Vec<&str> = raw.lines().collect();
        if lines.len() > 1 {
            lines[..lines.len() - 1].join(" ").trim().to_string()
        } else {
            raw.trim().to_string()
        }
    }
}

// ---------------------------------------------------------------------------
// Bare path / URL / quoted path / backticked path diagnostics
// ---------------------------------------------------------------------------

/// Resolve the severity of a prose bare-path diagnostic from the policy.
///
/// `base` is the diagnostic's default severity under `Warn`; `Deny` escalates
/// it to an error. `Disabled` is handled by an early return in the caller, so
/// it never reaches here.
const fn bare_path_severity(policy: BarePathPolicy, base: Severity) -> Severity {
    match policy {
        BarePathPolicy::Deny => Severity::Error,
        _ => base,
    }
}

/// Emit diagnostics for bare URLs, quoted paths, and backticked paths found in
/// inline-host text — paragraphs and table cells alike, matching the cells the
/// link/edge extractor already walks.
///
/// Honors the `bare_paths` policy: `Disabled` suppresses all of these, `Deny`
/// escalates them to errors (mirroring `emit_tree_bare_paths`).
fn emit_bare_path_diagnostics(
    tree: &Tree,
    rel_path: &Path,
    policy: BarePathPolicy,
    file_exists: &dyn Fn(&Path) -> bool,
    out: &mut Vec<Diagnostic>,
) {
    if policy == BarePathPolicy::Disabled {
        return;
    }

    let source = tree.source();

    // Scan the same inline hosts the inline pass populates with children
    // (`Paragraph` and `TableCell`), so dark-matter detection covers table
    // cells — the very cells the link/edge extractor already walks. Without
    // the `TableCell` arm, a backticked existing-file path in a cell forms a
    // first-class graph edge once linked yet draws no "make it a link" hint.
    for node in tree.nodes() {
        if !matches!(node.kind, ElementKind::Paragraph | ElementKind::TableCell) {
            continue;
        }

        let excluded: Vec<Span> = node
            .children
            .iter()
            .map(|&child| tree.node(child).span)
            .collect();

        let text = &source[node.span.start..node.span.end];
        let base = node.span.start;

        scan_text_for_paths(
            text,
            base,
            source,
            rel_path,
            policy,
            file_exists,
            &excluded,
            out,
        );

        // Check InlineCode children for backticked paths.
        for &child_id in &node.children {
            let child = tree.node(child_id);
            if matches!(child.kind, ElementKind::InlineCode) {
                let code_text = &source[child.span.start..child.span.end];
                // Strip backticks to get inner content.
                let inner = strip_backtick_delimiters(code_text);
                if looks_like_path(inner) {
                    let target = resolve_relative(rel_path, inner);
                    if file_exists(&target) {
                        let line = block::byte_offset_to_line(source, child.span.start);
                        out.push(Diagnostic {
                            file: rel_path.to_path_buf(),
                            line,
                            severity: bare_path_severity(policy, Severity::Hint),
                            message: format!(
                                "backticked path `{inner}` refers to an existing file: consider making it a link"
                            ),
                            span: Some(child.span),
                        });
                    }
                }
            }
        }
    }
}

/// Scan a text segment for bare URLs and quoted paths.
#[allow(
    clippy::too_many_arguments,
    reason = "scan context parameters are distinct concerns"
)]
fn scan_text_for_paths(
    text: &str,
    base: usize,
    source: &str,
    rel_path: &Path,
    policy: BarePathPolicy,
    file_exists: &dyn Fn(&Path) -> bool,
    excluded: &[Span],
    out: &mut Vec<Diagnostic>,
) {
    for (line_offset, line_text) in text.split('\n').enumerate() {
        let line_start = base
            + text
                .match_indices('\n')
                .take(line_offset)
                .last()
                .map_or(0, |(i, _)| i + 1);
        let line_num = block::byte_offset_to_line(source, line_start);

        scan_line_for_bare_urls(
            line_text, line_start, line_num, rel_path, policy, excluded, out,
        );
        scan_line_for_quoted_paths(
            line_text,
            line_start,
            line_num,
            rel_path,
            policy,
            file_exists,
            excluded,
            out,
        );
    }
}

/// Check if a byte position falls inside any excluded span.
fn is_excluded(pos: usize, excluded: &[Span]) -> bool {
    excluded.iter().any(|s| pos >= s.start && pos < s.end)
}

/// Scan a line for bare URLs (`http://` or `https://`) not inside links.
fn scan_line_for_bare_urls(
    line: &str,
    line_start: usize,
    line_num: usize,
    rel_path: &Path,
    policy: BarePathPolicy,
    excluded: &[Span],
    out: &mut Vec<Diagnostic>,
) {
    for prefix in &["https://", "http://"] {
        let mut search_start = 0;
        while let Some(idx) = line[search_start..].find(prefix) {
            let abs_pos = line_start + search_start + idx;
            search_start += idx + prefix.len();

            if is_excluded(abs_pos, excluded) {
                continue;
            }

            let rest = &line[search_start - prefix.len()..];
            let url_end = rest
                .find(|c: char| c.is_whitespace() || c == ')' || c == ']' || c == '>')
                .unwrap_or(rest.len());
            // Exclude trailing sentence punctuation, mirroring GFM autolink:
            // a trailing `.` `,` `;` `:` `!` `?` is not part of the URL.
            let url = rest[..url_end].trim_end_matches(['.', ',', ';', ':', '!', '?']);

            if url.len() <= prefix.len() {
                continue;
            }

            out.push(Diagnostic {
                file: rel_path.to_path_buf(),
                line: line_num,
                severity: bare_path_severity(policy, Severity::Warning),
                message: format!(
                    "bare URL `{url}`: wrap in angle brackets or make a markdown link"
                ),
                // `abs_pos` is the URL start; `url` is already punctuation-trimmed.
                span: Some(Span::new(abs_pos, abs_pos + url.len())),
            });
        }
    }
}

/// Scan a line for quoted paths (`"foo.md"`).
#[allow(
    clippy::too_many_arguments,
    reason = "scan context parameters are distinct concerns"
)]
fn scan_line_for_quoted_paths(
    line: &str,
    line_start: usize,
    line_num: usize,
    rel_path: &Path,
    policy: BarePathPolicy,
    file_exists: &dyn Fn(&Path) -> bool,
    excluded: &[Span],
    out: &mut Vec<Diagnostic>,
) {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'"' {
            let start = i + 1;
            if let Some(end) = line[start..].find('"') {
                let inner = &line[start..start + end];
                let abs_pos = line_start + i;

                if !is_excluded(abs_pos, excluded) && looks_like_path(inner) {
                    let target = resolve_relative(rel_path, inner);
                    if file_exists(&target) {
                        out.push(Diagnostic {
                            file: rel_path.to_path_buf(),
                            line: line_num,
                            severity: bare_path_severity(policy, Severity::Hint),
                            message: format!(
                                "quoted path `\"{inner}\"`: use backticks or make a markdown link"
                            ),
                            // Span the whole quoted token, both quotes included.
                            span: Some(Span::new(abs_pos, line_start + start + end + 1)),
                        });
                    }
                }
                i = start + end + 1;
            } else {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
}

/// Strip backtick delimiters from a code span (e.g. `` `foo` `` → `foo`).
fn strip_backtick_delimiters(s: &str) -> &str {
    let bytes = s.as_bytes();
    let tick_count = bytes.iter().take_while(|&&b| b == b'`').count();
    if tick_count == 0 || s.len() < tick_count * 2 {
        return s;
    }
    let end = s.len() - tick_count;
    &s[tick_count..end]
}

/// Check if a string looks like a file path (has an extension we recognize).
fn looks_like_path(s: &str) -> bool {
    const PATH_EXTENSIONS: &[&str] = &[
        ".md", ".png", ".jpg", ".svg", ".pdf", ".toml", ".yaml", ".yml", ".json", ".txt", ".xml",
        ".rs", ".ts", ".js",
    ];
    !s.is_empty()
        && !s.contains(' ')
        && (s.contains('/') || s.contains('.'))
        && PATH_EXTENSIONS.iter().any(|ext| s.ends_with(ext))
}

/// Resolve a relative path against a file's directory.
fn resolve_relative(file_path: &Path, target: &str) -> std::path::PathBuf {
    file_path
        .parent()
        .map_or_else(|| std::path::PathBuf::from(target), |dir| dir.join(target))
}

// ---------------------------------------------------------------------------
// HTML diagnostics
// ---------------------------------------------------------------------------

/// Emit HTML-specific diagnostics from tree structure.
fn emit_html_diagnostics(tree: &Tree, rel_path: &Path, out: &mut Vec<Diagnostic>) {
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
fn check_markdown_in_opaque_html(tree: &Tree, rel_path: &Path, out: &mut Vec<Diagnostic>) {
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

// ---------------------------------------------------------------------------
// Code block diagnostics
// ---------------------------------------------------------------------------

/// Emit code block language tag diagnostics.
fn emit_code_block_diagnostics(
    tree: &Tree,
    rel_path: &Path,
    config: &Config,
    out: &mut Vec<Diagnostic>,
) {
    let severity = match config.policy.code_block_language {
        CodeBlockLanguagePolicy::Disabled => return,
        CodeBlockLanguagePolicy::Hint => Severity::Hint,
        CodeBlockLanguagePolicy::Warn => Severity::Warning,
        CodeBlockLanguagePolicy::Deny => Severity::Error,
    };

    let source = tree.source();

    for node in tree.nodes() {
        if !matches!(node.kind, ElementKind::CodeBlock) || node.syntax == Syntax::Html {
            continue;
        }

        let raw = &source[node.span.start..node.span.end];
        let first_line = raw.lines().next().unwrap_or("");
        let trimmed = first_line.trim();

        let is_fenced = trimmed.starts_with("```") || trimmed.starts_with("~~~");
        if !is_fenced {
            continue;
        }

        let fence_end = trimmed
            .find(|c: char| c != '`' && c != '~')
            .unwrap_or(trimmed.len());
        let info = trimmed[fence_end..].trim();

        if info.is_empty() {
            let line = block::byte_offset_to_line(source, node.span.start);
            out.push(Diagnostic {
                file: rel_path.to_path_buf(),
                line,
                severity,
                message:
                    "code block without a language tag — add one (use `text` for non-code output)"
                        .to_string(),
                span: Some(node.span),
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Image diagnostics
// ---------------------------------------------------------------------------

/// Emit diagnostics for images with empty alt text.
fn emit_image_diagnostics(tree: &Tree, rel_path: &Path, out: &mut Vec<Diagnostic>) {
    let source = tree.source();

    for node in tree.nodes() {
        if !matches!(
            &node.kind,
            ElementKind::Image { .. } | ElementKind::Video { .. } | ElementKind::Audio { .. }
        ) {
            continue;
        }

        let raw = &source[node.span.start..node.span.end];
        if node.syntax == Syntax::Markdown
            && raw.starts_with("![")
            && let Some(close) = raw.find("](")
        {
            let alt = &raw[2..close];
            if alt.trim().is_empty() {
                let line = block::byte_offset_to_line(source, node.span.start);
                out.push(Diagnostic {
                    file: rel_path.to_path_buf(),
                    line,
                    severity: Severity::Warning,
                    message: "image with empty alt text".to_string(),
                    span: Some(node.span),
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Trailing whitespace diagnostics
// ---------------------------------------------------------------------------

/// Emit diagnostics for invalid trailing whitespace (1 or 3+ trailing spaces).
///
/// Two trailing spaces is a valid hard line break in `CommonMark`.
/// Lines inside fenced code blocks and HTML blocks are excluded.
fn emit_trailing_whitespace_diagnostics(
    source: &str,
    rel_path: &Path,
    tree: &Tree,
    out: &mut Vec<Diagnostic>,
) {
    let excluded: Vec<Span> = tree
        .nodes()
        .iter()
        .filter(|n| {
            matches!(
                n.kind,
                ElementKind::CodeBlock | ElementKind::HtmlBlock | ElementKind::Math
            )
        })
        .map(|n| n.span)
        .collect();

    for (line_idx, line) in source.lines().enumerate() {
        let line_num = line_idx + 1;
        let line_start = source
            .match_indices('\n')
            .take(line_idx)
            .last()
            .map_or(0, |(i, _)| i + 1);

        if excluded
            .iter()
            .any(|s| line_start >= s.start && line_start < s.end)
        {
            continue;
        }

        let trailing = line.len() - line.trim_end_matches(' ').len();
        if trailing == 1 || trailing >= 3 {
            let line_end = line_start + line.len();
            out.push(Diagnostic {
                file: rel_path.to_path_buf(),
                line: line_num,
                severity: Severity::Warning,
                message: format!(
                    "invalid trailing whitespace ({trailing} spaces): use 2 for hard break or 0"
                ),
                // Underline only the offending trailing spaces.
                span: Some(Span::new(line_end - trailing, line_end)),
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Missing blank line diagnostics
// ---------------------------------------------------------------------------

/// Emit diagnostics for missing blank lines before block elements.
fn emit_missing_blank_line_diagnostics(tree: &Tree, rel_path: &Path, out: &mut Vec<Diagnostic>) {
    let source = tree.source();

    for node in tree.nodes() {
        if !matches!(
            node.kind,
            ElementKind::Heading { .. }
                | ElementKind::CodeBlock
                | ElementKind::QuoteBlock
                | ElementKind::Rules
                | ElementKind::Table { .. }
                | ElementKind::HtmlBlock
                | ElementKind::List { .. }
                | ElementKind::Math
        ) {
            continue;
        }

        let start = node.span.start;
        if start == 0 {
            continue;
        }

        let before = &source[..start];
        // Inside a blockquote the block's source span starts *after* its line's
        // container prefix, so `before` ends mid-line with that prefix (e.g.
        // `"> "`, or `"> > "` when nested) instead of a line terminator. Strip
        // that leading-marker suffix so the prev-line scan lands on the real
        // physical line above the block rather than the block's own prefix
        // (issue 022). Only when `before` does not already end in a terminator,
        // i.e. when the block did not start on a fresh line.
        let before = if before.ends_with(['\n', '\r']) {
            before
        } else {
            before.trim_end_matches(['>', ' ', '\t'])
        };
        // Strip only the single terminator ending the line before the block
        // (\r\n, \n, or bare \r). Stripping *all* trailing newlines collapses a
        // blank separator line and misreports it as missing.
        let before = before
            .strip_suffix("\r\n")
            .or_else(|| before.strip_suffix('\n'))
            .or_else(|| before.strip_suffix('\r'))
            .unwrap_or(before);

        let prev_line = before
            .rsplit_once(['\n', '\r'])
            .map_or(before, |(_, line)| line);

        // Strip the previous line's own `>`/whitespace container markers before
        // the emptiness test, so a `>`-only blank separator inside a blockquote
        // counts as the blank line it is (issue 022). At top level this is a
        // no-op.
        let prev_content = strip_blockquote_markers(prev_line);

        if prev_content.trim().is_empty() {
            continue;
        }

        // Don't flag after a frontmatter closing delimiter.
        if prev_content.trim() == "---" && start < 100 {
            continue;
        }

        let line = block::byte_offset_to_line(source, start);
        out.push(Diagnostic {
            file: rel_path.to_path_buf(),
            line,
            severity: Severity::Hint,
            message: missing_blank_message(&node.kind, source, node.span),
            span: Some(node.span),
        });
    }
}

/// Strip the leading blockquote container markers (`>` and surrounding
/// whitespace) from a single source line, repeated for nested quotes
/// (`> > content` → `content`). Used to normalize a line's container prefix
/// before testing whether it is empty, so a `>`-only line reads as the blank
/// separator it is. A line with no quote markers is returned with only its
/// leading whitespace trimmed; the caller's `trim()` handles the rest.
fn strip_blockquote_markers(line: &str) -> &str {
    let mut rest = line;
    loop {
        let trimmed = rest.trim_start_matches([' ', '\t']);
        match trimmed.strip_prefix('>') {
            Some(after) => rest = after,
            None => return trimmed,
        }
    }
}

/// Build the "missing blank line" message.
///
/// For an HTML block, report which of the `CommonMark` start conditions
/// (types 1–7) opened it, and name the tag when there is one. An
/// inline-looking line such as `<link …>` that begins a continuation line
/// interrupts the paragraph as a type-1–6 HTML block (spec example 185);
/// spelling out "type 6 HTML block start" keeps the hint from reading as a
/// false positive when the author meant inline content (e.g. text inside a
/// code span that the block boundary split). Types 2–5 (comment, PI,
/// declaration, CDATA) are nameless, so they report the type and kind only.
fn missing_blank_message(kind: &ElementKind, source: &str, span: Span) -> String {
    const BASE: &str = "missing blank line before block element";
    if !matches!(kind, ElementKind::HtmlBlock) {
        return BASE.to_string();
    }
    let first_line = source[span.start..]
        .split(['\n', '\r'])
        .next()
        .unwrap_or("");
    let Some(html_type) = block::html_block_start(first_line) else {
        return format!("{BASE} (starts an HTML block)");
    };
    block::extract_html_tag_name(first_line.trim_start()).map_or_else(
        || {
            format!(
                "{BASE} (type {html_type} HTML block start: {})",
                html_block_kind(html_type)
            )
        },
        |tag| format!("{BASE} (`<{tag}>` is a type {html_type} HTML block start)"),
    )
}

/// Human label for the nameless HTML block start conditions (types 2–5).
const fn html_block_kind(html_type: u8) -> &'static str {
    match html_type {
        2 => "comment",
        3 => "processing instruction",
        4 => "declaration",
        5 => "CDATA",
        _ => "HTML",
    }
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
    use std::collections::HashSet;

    use super::*;
    use crate::block;
    use crate::config::Config;
    use crate::yaml;

    fn diagnose(content: &str) -> Vec<Diagnostic> {
        let fm = yaml::parse_frontmatter_block(content);
        let fm_span = fm.as_ref().map(|b| b.span);
        let tree = block::parse_tree(content, fm_span);
        let config = Config::default();
        let rel_path = std::path::Path::new("test.md");
        collect(&tree, rel_path, &config, &|_| false)
    }

    fn diagnose_with_files(content: &str, existing: &[&str]) -> Vec<Diagnostic> {
        let fm = yaml::parse_frontmatter_block(content);
        let fm_span = fm.as_ref().map(|b| b.span);
        let tree = block::parse_tree(content, fm_span);
        let config = Config::default();
        let rel_path = std::path::Path::new("test.md");
        let existing_set: HashSet<&str> = existing.iter().copied().collect();
        collect(&tree, rel_path, &config, &|p| {
            existing_set.contains(p.to_str().unwrap_or(""))
        })
    }

    fn count_matching(diags: &[Diagnostic], severity: Severity, substr: &str) -> usize {
        diags
            .iter()
            .filter(|d| d.severity == severity && d.message.contains(substr))
            .count()
    }

    fn has_matching(diags: &[Diagnostic], severity: Severity, substr: &str) -> bool {
        diags
            .iter()
            .any(|d| d.severity == severity && d.message.contains(substr))
    }

    fn has_any(diags: &[Diagnostic], substr: &str) -> bool {
        diags.iter().any(|d| d.message.contains(substr))
    }

    // -- Parser diagnostics --

    #[test]
    fn unclosed_fenced_code_block() {
        let diags = diagnose("```rust\nfn main() {}\n");
        assert_eq!(
            count_matching(&diags, Severity::Error, "unclosed fenced code block"),
            1,
            "one error for unclosed code block: {diags:?}"
        );
    }

    #[test]
    fn closed_code_block_no_error() {
        let diags = diagnose("```rust\nfn main() {}\n```\n");
        assert!(
            !has_matching(&diags, Severity::Error, "unclosed"),
            "no errors for closed code block: {diags:?}"
        );
    }

    #[test]
    fn unclosed_html_tag() {
        let diags = diagnose("<div>\n\nSome content\n");
        assert_eq!(
            count_matching(&diags, Severity::Error, "unclosed"),
            1,
            "one error for unclosed div: {diags:?}"
        );
    }

    #[test]
    fn unexpected_close_tag() {
        let diags = diagnose("</div>\n");
        assert_eq!(
            count_matching(&diags, Severity::Error, "unexpected closing tag"),
            1,
            "one error for unexpected close: {diags:?}"
        );
    }

    // -- Heading diagnostics --

    #[test]
    fn skipped_heading_level() {
        let diags = diagnose("# H1\n\n### H3\n");
        assert_eq!(
            count_matching(&diags, Severity::Warning, "skipped heading level"),
            1,
            "one warning for skipped heading: {diags:?}"
        );
        assert!(
            has_any(&diags, "H1 to H3"),
            "message mentions levels: {diags:?}"
        );
    }

    #[test]
    fn multiple_h1() {
        let diags = diagnose("# First\n\n# Second\n");
        assert_eq!(
            count_matching(&diags, Severity::Warning, "multiple H1"),
            1,
            "one warning for multiple H1: {diags:?}"
        );
    }

    #[test]
    fn duplicate_heading_text() {
        let diags = diagnose("## Overview\n\n## Overview\n");
        assert_eq!(
            count_matching(&diags, Severity::Warning, "duplicate heading text"),
            1,
            "one warning for duplicate heading: {diags:?}"
        );
    }

    #[test]
    fn empty_heading() {
        let diags = diagnose("# \n");
        assert_eq!(
            count_matching(&diags, Severity::Warning, "empty heading"),
            1,
            "one warning for empty heading: {diags:?}"
        );
    }

    #[test]
    fn sequential_headings_no_warning() {
        let diags = diagnose("# H1\n\n## H2\n\n### H3\n");
        assert!(
            !has_matching(&diags, Severity::Warning, "skipped"),
            "no warnings for sequential headings: {diags:?}"
        );
    }

    // -- Code block language --

    #[test]
    fn code_block_without_language() {
        let diags = diagnose("```\ncode\n```\n");
        assert_eq!(
            count_matching(&diags, Severity::Hint, "without a language tag"),
            1,
            "one hint for missing language: {diags:?}"
        );
        // Issue 020: the hint must name the `text` escape hatch so authors of
        // non-code blocks (output, diagrams, trees) tag them deliberately
        // instead of guessing a language.
        assert!(
            has_matching(&diags, Severity::Hint, "`text`"),
            "missing-language hint should point at the `text` escape hatch: {diags:?}"
        );
    }

    #[test]
    fn code_block_with_language_no_diagnostic() {
        let diags = diagnose("```rust\ncode\n```\n");
        assert!(
            !has_any(&diags, "language tag"),
            "no hint for code block with language: {diags:?}"
        );
    }

    // -- Image --

    #[test]
    fn image_empty_alt_text() {
        let diags = diagnose("![](image.png)\n");
        assert_eq!(
            count_matching(&diags, Severity::Warning, "empty alt text"),
            1,
            "one warning for empty alt: {diags:?}"
        );
    }

    #[test]
    fn image_with_alt_text_no_diagnostic() {
        let diags = diagnose("![a logo](image.png)\n");
        assert!(
            !has_any(&diags, "empty alt text"),
            "no warning for image with alt: {diags:?}"
        );
    }

    // -- Anchor `<a>` href requirement (issue 025) --

    #[test]
    fn anchor_with_id_no_href_no_warning() {
        // `<a id="a"></a>` is an explicit anchor target, not a link source;
        // it legitimately carries no `href` and must not be flagged.
        let diags = diagnose("<a id=\"a\"></a>\n");
        assert!(
            !has_any(&diags, "missing required attribute `href`"),
            "no missing-href warning for an `<a id>` anchor target: {diags:?}"
        );
    }

    #[test]
    fn anchor_with_name_no_href_no_warning() {
        // `<a name="a">` is the legacy anchor-target form — also exempt.
        let diags = diagnose("<a name=\"a\"></a>\n");
        assert!(
            !has_any(&diags, "missing required attribute `href`"),
            "no missing-href warning for an `<a name>` anchor target: {diags:?}"
        );
    }

    #[test]
    fn anchor_without_href_or_anchor_attr_still_warns() {
        // The relaxation must not over-suppress: an `<a>` with neither `href`
        // nor an anchor-defining attribute is still flagged.
        let diags = diagnose("<a class=\"x\"></a>\n");
        assert_eq!(
            count_matching(
                &diags,
                Severity::Warning,
                "missing required attribute `href`"
            ),
            1,
            "an `<a>` with no href and no id/name still warns: {diags:?}"
        );
    }

    #[test]
    fn anchor_with_href_no_warning() {
        // A normal linking `<a href>` is unaffected by the relaxation.
        let diags = diagnose("<a href=\"https://example.com\">x</a>\n");
        assert!(
            !has_any(&diags, "missing required attribute `href`"),
            "no missing-href warning for a normal linking `<a href>`: {diags:?}"
        );
    }

    // -- Trailing whitespace --

    #[test]
    fn single_trailing_space() {
        let diags = diagnose("hello \n");
        assert_eq!(
            count_matching(&diags, Severity::Warning, "trailing whitespace"),
            1,
            "one warning for 1 trailing space: {diags:?}"
        );
    }

    #[test]
    fn two_trailing_spaces_ok() {
        let diags = diagnose("hello  \n");
        assert!(
            !has_any(&diags, "trailing whitespace"),
            "no warning for 2 trailing spaces: {diags:?}"
        );
    }

    #[test]
    fn three_trailing_spaces() {
        let diags = diagnose("hello   \n");
        assert_eq!(
            count_matching(&diags, Severity::Warning, "trailing whitespace"),
            1,
            "one warning for 3 trailing spaces: {diags:?}"
        );
    }

    #[test]
    fn trailing_whitespace_in_code_block_excluded() {
        let diags = diagnose("```\nhello   \n```\n");
        assert!(
            !has_any(&diags, "trailing whitespace"),
            "no warning for trailing spaces inside code: {diags:?}"
        );
    }

    // -- Bare URL --

    #[test]
    fn bare_url_in_paragraph() {
        let diags = diagnose("Visit https://example.com for info.\n");
        assert_eq!(
            count_matching(&diags, Severity::Warning, "bare URL"),
            1,
            "one warning for bare URL: {diags:?}"
        );
    }

    // Regression: issue 012 — a URL written mid-sentence had its trailing
    // punctuation folded into the reported URL (`https://example.com,`). GFM
    // autolink excludes trailing `.,;:!?`, and so must the bare-URL hint.
    #[test]
    fn bare_url_trailing_punctuation_excluded() {
        let diags = diagnose("See https://example.com, then continue.\n");
        assert!(
            has_matching(&diags, Severity::Warning, "bare URL `https://example.com`"),
            "trailing comma excluded from the reported URL: {diags:?}"
        );
        assert!(
            !has_any(&diags, "https://example.com,"),
            "reported URL must not include the trailing comma: {diags:?}"
        );
    }

    // Regression: issue 006 — a bare URL past the midpoint of its line drove
    // `scan_line_for_bare_urls` to slice at `2*idx`, an out-of-bounds byte
    // index that aborted the LSP. It must warn, not panic.
    #[test]
    fn bare_url_past_line_midpoint_no_panic() {
        let diags =
            diagnose("A long line of filler text before the link, then https://example.com\n");
        assert_eq!(
            count_matching(&diags, Severity::Warning, "bare URL"),
            1,
            "one warning for bare URL past line midpoint: {diags:?}"
        );
    }

    // Issue 011: producers must carry a precise byte span, not just a line.
    #[test]
    fn bare_url_diagnostic_has_precise_span() {
        let content = "Visit https://example.com for info.\n";
        let diags = diagnose(content);
        let d = diags
            .iter()
            .find(|d| d.message.contains("bare URL"))
            .expect("a bare URL diagnostic");
        let span = d.span.expect("bare URL diagnostic carries a span");
        assert_eq!(
            &content[span.start..span.end],
            "https://example.com",
            "span underlines exactly the URL: {diags:?}"
        );
    }

    #[test]
    fn trailing_whitespace_diagnostic_spans_the_spaces() {
        // Three trailing spaces after "hello"; the span must cover only them.
        let content = "hello   \nworld\n";
        let diags = diagnose(content);
        let d = diags
            .iter()
            .find(|d| d.message.contains("trailing whitespace"))
            .expect("a trailing whitespace diagnostic");
        let span = d
            .span
            .expect("trailing whitespace diagnostic carries a span");
        assert_eq!(
            &content[span.start..span.end],
            "   ",
            "span covers exactly the three trailing spaces: {diags:?}"
        );
    }

    // -- Error recovery --

    #[test]
    fn unclosed_html_no_cascade_to_valid_content() {
        let diags = diagnose("<div>\n\n# Valid Heading\n\nSome paragraph.\n");
        let errors: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == Severity::Error)
            .collect();
        assert_eq!(errors.len(), 1, "only one error, no cascading: {diags:?}");
        assert!(
            errors[0].message.contains("unclosed"),
            "the error is about unclosed tag: {}",
            errors[0].message
        );
    }

    // -- Quoted path --

    #[test]
    fn quoted_path_with_existing_file() {
        let diags = diagnose_with_files("See \"other.md\" for details.\n", &["other.md"]);
        assert_eq!(
            count_matching(&diags, Severity::Hint, "quoted path"),
            1,
            "one hint for quoted path: {diags:?}"
        );
    }

    // -- Backticked path --

    #[test]
    fn backticked_path_with_existing_file() {
        let diags = diagnose_with_files("See `other.md` for details.\n", &["other.md"]);
        assert_eq!(
            count_matching(&diags, Severity::Hint, "backticked path"),
            1,
            "one hint for backticked path: {diags:?}"
        );
    }

    #[test]
    fn backticked_path_no_file() {
        let diags = diagnose("See `other.md` for details.\n");
        assert!(
            !has_any(&diags, "backticked path"),
            "no hint when file doesn't exist: {diags:?}"
        );
    }

    // -- Table-cell dark-matter coverage (issue 023) --

    // A backticked existing-file path inside a GFM table cell must emit the
    // same "make it a link" hint as the identical path in prose, anchored at
    // the cell's row — the link/edge extractor already walks these cells.
    #[test]
    fn backticked_path_in_table_cell_emits_hint() {
        let content = "| # | Tracker |\n|---|---------|\n| 1 | `tickets/foo/README.md` |\n";
        let diags = diagnose_with_files(content, &["tickets/foo/README.md"]);

        let hits: Vec<&Diagnostic> = diags
            .iter()
            .filter(|d| d.severity == Severity::Hint && d.message.contains("backticked path"))
            .collect();
        assert_eq!(
            hits.len(),
            1,
            "exactly one backticked-path hint for the cell: {diags:?}"
        );
        // The cell sits on the third line of the document (1-based).
        assert_eq!(
            hits[0].line, 3,
            "hint is anchored at the table cell's row (line 3): {diags:?}"
        );
    }

    // The hint must agree with prose: a path that exists only in a cell is
    // surfaced; one that does not exist is not.
    #[test]
    fn backticked_path_in_table_cell_no_file() {
        let content = "| # | Tracker |\n|---|---------|\n| 1 | `tickets/foo/README.md` |\n";
        let diags = diagnose(content);
        assert!(
            !has_any(&diags, "backticked path"),
            "no hint for a non-existent cell path: {diags:?}"
        );
    }

    // Sibling dark-matter surfaces extended for parity with the edge extractor
    // (issue 023, fix point 4): bare URL, quoted path, and tree-level bare path
    // inside a table cell must each surface just as they do in prose.
    #[test]
    fn bare_url_in_table_cell_emits_warning() {
        let content = "| Site |\n|------|\n| https://example.com/page |\n";
        let diags = diagnose(content);
        assert_eq!(
            count_matching(&diags, Severity::Warning, "bare URL"),
            1,
            "one bare-URL warning for the cell: {diags:?}"
        );
    }

    #[test]
    fn quoted_path_in_table_cell_emits_hint() {
        let content = "| Ref |\n|-----|\n| \"other.md\" |\n";
        let diags = diagnose_with_files(content, &["other.md"]);
        assert_eq!(
            count_matching(&diags, Severity::Hint, "quoted path"),
            1,
            "one quoted-path hint for the cell: {diags:?}"
        );
    }

    #[test]
    fn bare_path_in_table_cell_emits_diagnostic() {
        let content = "| Ref |\n|-----|\n| docs/page.md |\n";
        let diags = diagnose_with_files(content, &["docs/page.md"]);
        assert_eq!(
            count_matching(&diags, Severity::Warning, "convert to a markdown link"),
            1,
            "one bare-path diagnostic for the cell: {diags:?}"
        );
    }

    // -- Self-closing non-void --

    #[test]
    fn self_closing_div() {
        let diags = diagnose("<div/>\n");
        assert_eq!(
            count_matching(&diags, Severity::Warning, "self-closing non-void"),
            1,
            "one warning for self-closing div: {diags:?}"
        );
    }

    #[test]
    fn self_closing_void_ok() {
        let diags = diagnose("<br/>\n");
        assert!(
            !has_any(&diags, "self-closing non-void"),
            "no warning for self-closing void: {diags:?}"
        );
    }

    // -- Unknown element --

    #[test]
    fn unknown_element() {
        let diags = diagnose("<foo>\n</foo>\n");
        assert_eq!(
            count_matching(&diags, Severity::Info, "unknown HTML element"),
            1,
            "one info for unknown element: {diags:?}"
        );
    }

    // -- Duplicate id (inline + block, issue 026) --

    #[test]
    fn duplicate_id_across_block_and_mid_paragraph_inline() {
        // Issue 026: harvesting mid-paragraph id-bearing inline tags as
        // `InlineHtml` nodes puts them on the same `Syntax::Html` surface the
        // duplicate-id pass walks, so a block `<div id>` and a mid-paragraph
        // `<span id>` sharing the same id now collide (invalid HTML — GitHub
        // anchors only the first).
        let diags = diagnose(
            "<div id=\"shared\"></div>\n\n\
             Paragraph with an <span id=\"shared\"></span> inline target.\n",
        );
        assert_eq!(
            count_matching(&diags, Severity::Error, "duplicate `id` attribute `shared`"),
            1,
            "one error for the inline id duplicating the block id: {diags:?}"
        );
    }

    #[test]
    fn distinct_mid_paragraph_inline_id_no_duplicate() {
        // A mid-paragraph inline id distinct from every other id is not flagged.
        let diags = diagnose(
            "<div id=\"block\"></div>\n\n\
             Paragraph with an <span id=\"inline\"></span> inline target.\n",
        );
        assert!(
            !has_any(&diags, "duplicate `id`"),
            "distinct ids do not collide: {diags:?}"
        );
    }

    // -- Config: code_block_language --

    #[test]
    fn code_block_language_disabled() {
        let fm = yaml::parse_frontmatter_block("```\ncode\n```\n");
        let fm_span = fm.as_ref().map(|b| b.span);
        let tree = block::parse_tree("```\ncode\n```\n", fm_span);
        let mut config = Config::default();
        config.policy.code_block_language = CodeBlockLanguagePolicy::Disabled;
        let rel_path = std::path::Path::new("test.md");
        let diags = collect(&tree, rel_path, &config, &|_| false);
        assert!(
            !has_any(&diags, "language tag"),
            "no diagnostic when disabled: {diags:?}"
        );
    }

    #[test]
    fn code_block_language_deny_is_error() {
        let fm = yaml::parse_frontmatter_block("```\ncode\n```\n");
        let fm_span = fm.as_ref().map(|b| b.span);
        let tree = block::parse_tree("```\ncode\n```\n", fm_span);
        let mut config = Config::default();
        config.policy.code_block_language = CodeBlockLanguagePolicy::Deny;
        let rel_path = std::path::Path::new("test.md");
        let diags = collect(&tree, rel_path, &config, &|_| false);
        assert_eq!(
            count_matching(&diags, Severity::Error, "without a language tag"),
            1,
            "one error when deny: {diags:?}"
        );
    }

    // -- Config: bare_paths policy governs both emitters (issue 007) --

    fn diagnose_with_policy(
        content: &str,
        existing: &[&str],
        policy: BarePathPolicy,
    ) -> Vec<Diagnostic> {
        let fm = yaml::parse_frontmatter_block(content);
        let fm_span = fm.as_ref().map(|b| b.span);
        let tree = block::parse_tree(content, fm_span);
        let mut config = Config::default();
        config.policy.bare_paths = policy;
        let rel_path = std::path::Path::new("test.md");
        let existing_set: HashSet<&str> = existing.iter().copied().collect();
        collect(&tree, rel_path, &config, &|p| {
            existing_set.contains(p.to_str().unwrap_or(""))
        })
    }

    // One paragraph exercising every bare-path emitter: a tree-level bare path
    // (`docs/page.md`), a prose bare URL, a quoted path, and a backticked path.
    const BARE_PATH_SAMPLE: &str =
        "Visit https://example.com and see \"other.md\" or `other.md` in docs/page.md here.\n";

    const BARE_PATH_NEEDLES: [&str; 4] = [
        "convert to a markdown link",
        "bare URL",
        "quoted path",
        "backticked path",
    ];

    #[test]
    fn bare_paths_disabled_silences_both_emitters() {
        let diags = diagnose_with_policy(
            BARE_PATH_SAMPLE,
            &["other.md", "docs/page.md"],
            BarePathPolicy::Disabled,
        );
        for needle in BARE_PATH_NEEDLES {
            assert!(
                !has_any(&diags, needle),
                "disabled should silence `{needle}`: {diags:?}"
            );
        }
    }

    #[test]
    fn bare_paths_deny_escalates_both_emitters() {
        let diags = diagnose_with_policy(
            BARE_PATH_SAMPLE,
            &["other.md", "docs/page.md"],
            BarePathPolicy::Deny,
        );
        for needle in BARE_PATH_NEEDLES {
            assert!(
                has_matching(&diags, Severity::Error, needle),
                "deny should escalate `{needle}` to error: {diags:?}"
            );
        }
    }

    // -- close_block_quotes HTML scope desync --

    #[test]
    fn html_in_blockquote_closed_on_blank_line() {
        // An HTML container inside a block quote followed by a blank line
        // should produce exactly one unclosed-tag diagnostic, not desync
        // the scope stacks and cascade errors.
        let diags = diagnose("> <div>\n>\n> text\n\nparagraph\n");
        assert_eq!(
            count_matching(&diags, Severity::Error, "unclosed"),
            1,
            "one unclosed div error, no cascading: {diags:?}"
        );
    }

    // -- Malformed link --

    #[test]
    fn malformed_link_destination() {
        let diags = diagnose("[text](\n");
        assert_eq!(
            count_matching(&diags, Severity::Error, "malformed link"),
            1,
            "one error for malformed link: {diags:?}"
        );
    }

    // -- Unused/duplicate ref defs are Warning, not Error --

    #[test]
    fn unused_ref_def_is_warning() {
        let diags = diagnose("[label]: https://example.com\n\nSome text.\n");
        assert_eq!(
            count_matching(&diags, Severity::Warning, "unused reference definition"),
            1,
            "unused ref def should be warning: {diags:?}"
        );
        assert!(
            !has_matching(&diags, Severity::Error, "unused reference definition"),
            "unused ref def should not be error: {diags:?}"
        );
    }

    #[test]
    fn duplicate_ref_def_is_warning() {
        let diags = diagnose("[label]: https://a.com\n[label]: https://b.com\n\n[text][label]\n");
        assert_eq!(
            count_matching(&diags, Severity::Warning, "duplicate reference definition"),
            1,
            "duplicate ref def should be warning: {diags:?}"
        );
    }

    // -- Markdown in opaque HTML --

    #[test]
    fn markdown_in_opaque_html_warns() {
        // <center> is a type 6 block tag with no structural mapping,
        // so it falls through to HtmlBlock. Content without blank
        // lines won't be parsed as markdown.
        let diags = diagnose("<center>\n# Heading\n</center>\n");
        assert_eq!(
            count_matching(
                &diags,
                Severity::Warning,
                "markdown syntax inside HTML block"
            ),
            1,
            "one warning for markdown in opaque HTML: {diags:?}"
        );
    }

    // -- Missing blank line before block --

    // Regression: issue 010 — a block separated from prior content by a blank
    // line must NOT flag. `trim_end_matches('\n')` collapsed the blank line and
    // misreported it as missing on essentially every well-formed document.
    #[test]
    fn list_after_blank_line_no_missing_blank_hint() {
        let diags = diagnose("Intro paragraph.\n\n- item one\n- item two\n");
        assert_eq!(
            count_matching(&diags, Severity::Hint, "missing blank line"),
            0,
            "blank line is present, no hint expected: {diags:?}"
        );
    }

    #[test]
    fn block_flush_against_prior_block_flags_missing_blank() {
        let diags = diagnose("## Heading\n- item one\n");
        assert_eq!(
            count_matching(&diags, Severity::Hint, "missing blank line"),
            1,
            "list flush against heading, one hint expected: {diags:?}"
        );
    }

    #[test]
    fn block_after_blank_line_crlf_no_missing_blank_hint() {
        let diags = diagnose("Intro paragraph.\r\n\r\n## Heading\r\n");
        assert_eq!(
            count_matching(&diags, Severity::Hint, "missing blank line"),
            0,
            "CRLF blank line is present, no hint expected: {diags:?}"
        );
    }

    #[test]
    fn html_block_missing_blank_names_the_tag() {
        // `<link>` is a block-level (type-6) tag, so it interrupts the
        // paragraph and starts an HTML block. The hint must name the tag and
        // its type so it reads as an explanation, not a false positive.
        let diags = diagnose("A paragraph line\n<link rel=\"x\">\n");
        let hints: Vec<&Diagnostic> = diags
            .iter()
            .filter(|d| d.severity == Severity::Hint && d.message.contains("missing blank line"))
            .collect();
        assert_eq!(hints.len(), 1, "one missing-blank hint expected: {diags:?}");
        assert!(
            hints[0]
                .message
                .contains("`<link>` is a type 6 HTML block start"),
            "hint should name the HTML tag and type, got: {}",
            hints[0].message
        );
    }

    #[test]
    fn html_block_missing_blank_reports_nameless_opener() {
        // A comment (type 2) is a nameless HTML block start. The hint reports
        // the type and kind rather than a tag name.
        let diags = diagnose("A paragraph line\n<!-- a comment -->\n");
        let hints: Vec<&Diagnostic> = diags
            .iter()
            .filter(|d| d.severity == Severity::Hint && d.message.contains("missing blank line"))
            .collect();
        assert_eq!(hints.len(), 1, "one missing-blank hint expected: {diags:?}");
        assert!(
            hints[0]
                .message
                .contains("type 2 HTML block start: comment"),
            "nameless opener should report its type and kind, got: {}",
            hints[0].message
        );
    }

    // -- Missing blank line inside a blockquote (issue 022) --
    //
    // The prev-line guard scanned raw source, so inside a blockquote it
    // resolved `prev_line` to the block line's own `> ` container prefix
    // instead of the physical line above. Every blockquote-nested block thus
    // flagged unconditionally and the hint was unsatisfiable while the block
    // stayed quoted. A block correctly separated by a `>`-only blank line must
    // NOT flag; a block flush against quote content must flag exactly once.

    fn missing_blank_count(content: &str) -> usize {
        count_matching(&diagnose(content), Severity::Hint, "missing blank line")
    }

    #[test]
    fn blockquote_list_after_quote_blank_no_missing_blank_hint() {
        assert_eq!(
            missing_blank_count("> intro paragraph here:\n>\n> - alpha\n"),
            0,
            "list separated by a `>`-only blank line should not flag",
        );
    }

    #[test]
    fn blockquote_list_flush_against_quote_flags_missing_blank() {
        assert_eq!(
            missing_blank_count("> intro paragraph here:\n> - gamma\n"),
            1,
            "list flush against quote content should flag exactly once",
        );
    }

    #[test]
    fn blockquote_heading_after_quote_blank_no_missing_blank_hint() {
        assert_eq!(
            missing_blank_count("> intro paragraph here:\n>\n> ## Section\n"),
            0,
            "heading separated by a `>`-only blank line should not flag",
        );
    }

    #[test]
    fn blockquote_heading_flush_against_quote_flags_missing_blank() {
        assert_eq!(
            missing_blank_count("> intro paragraph here:\n> ## Section\n"),
            1,
            "heading flush against quote content should flag exactly once",
        );
    }

    #[test]
    fn blockquote_code_block_after_quote_blank_no_missing_blank_hint() {
        assert_eq!(
            missing_blank_count("> some text here:\n>\n> ```rust\n> let x = 1;\n> ```\n"),
            0,
            "code block separated by a `>`-only blank line should not flag",
        );
    }

    #[test]
    fn blockquote_code_block_flush_against_quote_flags_missing_blank() {
        assert_eq!(
            missing_blank_count("> some text here:\n> ```rust\n> let x = 1;\n> ```\n"),
            1,
            "code block flush against quote content should flag exactly once",
        );
    }

    #[test]
    fn blockquote_table_after_quote_blank_no_missing_blank_hint() {
        assert_eq!(
            missing_blank_count(
                "> intro paragraph here:\n>\n> | a | b |\n> | - | - |\n> | 1 | 2 |\n"
            ),
            0,
            "table separated by a `>`-only blank line should not flag",
        );
    }

    // The parser does not recognize a GFM table whose header row is flush
    // against a preceding paragraph (the paragraph absorbs it) — this holds
    // both at top level and inside a blockquote — so a flush table produces no
    // Table node and has no flush-flags arm to test. The separated case above
    // is the table arm of the regression matrix: the false positive the bug
    // produced inside a blockquote.

    #[test]
    fn nested_blockquote_list_after_quote_blank_no_missing_blank_hint() {
        assert_eq!(
            missing_blank_count("> > intro paragraph here:\n> >\n> > - alpha\n"),
            0,
            "nested-quote list separated by a `> >`-only blank line should not flag",
        );
    }

    #[test]
    fn nested_blockquote_list_flush_against_quote_flags_missing_blank() {
        assert_eq!(
            missing_blank_count("> > intro paragraph here:\n> > - gamma\n"),
            1,
            "nested-quote list flush against quote content should flag exactly once",
        );
    }

    #[test]
    fn blockquote_list_after_quote_blank_crlf_no_missing_blank_hint() {
        assert_eq!(
            missing_blank_count("> intro paragraph here:\r\n>\r\n> - alpha\r\n"),
            0,
            "CRLF list separated by a `>`-only blank line should not flag",
        );
    }

    #[test]
    fn blockquote_list_flush_against_quote_crlf_flags_missing_blank() {
        assert_eq!(
            missing_blank_count("> intro paragraph here:\r\n> - gamma\r\n"),
            1,
            "CRLF list flush against quote content should flag exactly once",
        );
    }
}
