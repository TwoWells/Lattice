// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Content scanners: the per-document checks that judge no reference.
//!
//! Four families that share nothing but their subject — the document as it is
//! written, rather than what it points at:
//!
//! - **Parser diagnostics.** Surfacing what the block parser already recorded
//!   (an unclosed fence, a truncated walk) as workspace diagnostics.
//! - **Headings.** Empty headings and duplicate slugs, which are genuine
//!   defects under decision 009; skipped levels and the rest are policy-gated.
//! - **Code blocks and images.** A fence with no language, an image with no
//!   alt text.
//! - **Trailing whitespace.** Underlined precisely on the offending run.

use std::collections::HashMap;
use std::path::Path;

use crate::block::{self, ElementKind, Syntax, Tree};
use crate::config::{CodeBlockLanguagePolicy, Config, FragmentAlgorithm};
use crate::span::Span;
use crate::validation::{Diagnostic, Severity};

// ---------------------------------------------------------------------------
// Parser diagnostics
// ---------------------------------------------------------------------------

/// Emit diagnostics that the parser already collected (unclosed fenced code
/// blocks, unclosed HTML tags, unexpected close tags, table cell mismatches,
/// unused/duplicate reference definitions).
pub fn emit_parser_diagnostics(tree: &Tree, rel_path: &Path, out: &mut Vec<Diagnostic>) {
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
// Heading diagnostics
// ---------------------------------------------------------------------------

/// Emit heading diagnostics: empty headings and duplicate slugs fire on by
/// default (both are genuine defects per decision 009). Skipped levels and
/// multiple H1 are convention checks, gated behind opt-in policy flags
/// (`config.policy.skipped_heading_level` / `config.policy.multiple_h1`).
pub fn emit_heading_diagnostics(
    tree: &Tree,
    rel_path: &Path,
    config: &Config,
    out: &mut Vec<Diagnostic>,
) {
    let source = tree.source();
    let mut prev_level: Option<u8> = None;
    let mut h1_count = 0u32;
    // Maps a base slug to the line of its first heading, to flag genuine slug
    // collisions (where `#slug` resolves only to the first heading).
    let mut seen_slugs: HashMap<String, usize> = HashMap::new();

    for node in tree.nodes() {
        let ElementKind::Heading { level } = &node.kind else {
            continue;
        };
        let level = *level;
        let line = block::byte_offset_to_line(source, node.span.start);

        let raw = &source[node.span.start..node.span.end];
        let text = heading_display_text(raw, node.syntax);

        if text.trim().is_empty() {
            // An empty heading produces a degenerate (empty) slug — a defect,
            // so it fires on by default.
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

        if config.policy.multiple_h1 && level == 1 {
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

        if config.policy.skipped_heading_level
            && let Some(prev) = prev_level
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

        // Collision is on the *base* slug, before `block::deduplicate` appends
        // a `-1`/`-2` suffix: two headings whose bases match means `#base`
        // resolves only to the first. When no fragment algorithm is configured
        // default to GitHub — the dominant renderer, and what the old
        // lowercase proxy approximated.
        let slug = match config.policy.fragments {
            Some(FragmentAlgorithm::Github) | None => block::github_slug(&text),
            Some(FragmentAlgorithm::Gitlab) => block::gitlab_slug(&text),
            Some(FragmentAlgorithm::Vscode) => block::vscode_slug(&text),
        };
        if let Some(&first_line) = seen_slugs.get(&slug) {
            out.push(Diagnostic {
                file: rel_path.to_path_buf(),
                line,
                severity: Severity::Warning,
                message: format!(
                    "duplicate heading slug `{slug}` (first at line {first_line}) — `#{slug}` resolves only to the first"
                ),
                span: Some(node.span),
            });
        } else {
            seen_slugs.insert(slug, line);
        }
    }
}

/// Extract display text from a heading node.
pub fn heading_display_text(raw: &str, syntax: Syntax) -> String {
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
// Code block diagnostics
// ---------------------------------------------------------------------------

/// Emit code block language tag diagnostics.
pub fn emit_code_block_diagnostics(
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
///
/// A convention check, not a defect (empty alt is the correct choice for
/// decorative images), so per decision 009 it is gated behind the opt-in
/// `config.policy.image_empty_alt` flag and off by default.
pub fn emit_image_diagnostics(
    tree: &Tree,
    rel_path: &Path,
    config: &Config,
    out: &mut Vec<Diagnostic>,
) {
    if !config.policy.image_empty_alt {
        return;
    }

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
pub fn emit_trailing_whitespace_diagnostics(
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
