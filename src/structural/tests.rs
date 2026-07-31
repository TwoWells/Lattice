// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Tests for the structural diagnostic layer.

use std::collections::HashSet;

use super::ledger::*;
use super::*;
use crate::block;
use crate::config::{CodeBlockLanguagePolicy, Config};
use crate::fm::{self, ExceptionLint};
use crate::validation::Severity;
use crate::yaml;

fn diagnose(content: &str) -> Vec<Diagnostic> {
    let config = Config::default();
    diagnose_with_config(content, &config)
}

/// Parse `content`'s frontmatter and extract its `exceptions` block (issue
/// 031). Returns the empty default when there is no frontmatter.
fn exceptions_of(content: &str) -> Exceptions {
    yaml::parse_frontmatter_block(content)
        .map(|block| fm::extract_exceptions(&block, content))
        .unwrap_or_default()
}

fn diagnose_with_config(content: &str, config: &Config) -> Vec<Diagnostic> {
    let fm = yaml::parse_frontmatter_block(content);
    let fm_span = fm.as_ref().map(|b| b.span);
    let tree = block::parse_tree(content, fm_span);
    let rel_path = std::path::Path::new("test.md");
    let exceptions = exceptions_of(content);
    collect(
        &tree,
        rel_path,
        config,
        &|_| false,
        &|_| ExternalExistence::Absent,
        &exceptions,
    )
}

/// Like [`diagnose_with_config`], but with an explicit external-existence
/// oracle: `external_present` lists the absolute filesystem paths
/// (alias directories and their joined files) that `stat` finds, backing the
/// three-tier `{Name}/…` resolution (issue 030). Every other path is
/// definitively absent; [`diagnose_with_external_unknown`] covers the
/// stat-failure verdict (issue 050).
fn diagnose_with_external(
    content: &str,
    config: &Config,
    external_present: &[&str],
) -> Vec<Diagnostic> {
    let fm = yaml::parse_frontmatter_block(content);
    let fm_span = fm.as_ref().map(|b| b.span);
    let tree = block::parse_tree(content, fm_span);
    let rel_path = std::path::Path::new("test.md");
    let present: HashSet<&str> = external_present.iter().copied().collect();
    let exceptions = exceptions_of(content);
    collect(
        &tree,
        rel_path,
        config,
        &|_| false,
        &|p| {
            if present.contains(p.to_str().unwrap_or("")) {
                ExternalExistence::Present
            } else {
                ExternalExistence::Absent
            }
        },
        &exceptions,
    )
}

/// Like [`diagnose_with_external`], but paths in `unknown` answer the
/// stat-failure verdict [`ExternalExistence::Unknown`] instead of a
/// definitive presence answer (issue 050).
fn diagnose_with_external_unknown(
    content: &str,
    config: &Config,
    external_present: &[&str],
    unknown: &[&str],
) -> Vec<Diagnostic> {
    let fm = yaml::parse_frontmatter_block(content);
    let fm_span = fm.as_ref().map(|b| b.span);
    let tree = block::parse_tree(content, fm_span);
    let rel_path = std::path::Path::new("test.md");
    let present: HashSet<&str> = external_present.iter().copied().collect();
    let unknown: HashSet<&str> = unknown.iter().copied().collect();
    let exceptions = exceptions_of(content);
    collect(
        &tree,
        rel_path,
        config,
        &|_| false,
        &|p| {
            let key = p.to_str().unwrap_or("");
            if unknown.contains(key) {
                ExternalExistence::Unknown
            } else if present.contains(key) {
                ExternalExistence::Present
            } else {
                ExternalExistence::Absent
            }
        },
        &exceptions,
    )
}

/// Like [`diagnose_with_external`], but `existing` also lists the
/// workspace-relative paths that resolve *intra-repo*, so a test can drive
/// both oracles at once — the combination the external-citation steering
/// turns on (issue 073), where a path may exist locally, externally, or
/// both.
fn diagnose_with_files_and_external(
    content: &str,
    config: &Config,
    existing: &[&str],
    external_present: &[&str],
) -> Vec<Diagnostic> {
    let fm = yaml::parse_frontmatter_block(content);
    let fm_span = fm.as_ref().map(|b| b.span);
    let tree = block::parse_tree(content, fm_span);
    let rel_path = std::path::Path::new("test.md");
    let existing_set: HashSet<&str> = existing.iter().copied().collect();
    let present: HashSet<&str> = external_present.iter().copied().collect();
    let exceptions = exceptions_of(content);
    collect(
        &tree,
        rel_path,
        config,
        &|p| existing_set.contains(p.to_str().unwrap_or("")),
        &|p| {
            if present.contains(p.to_str().unwrap_or("")) {
                ExternalExistence::Present
            } else {
                ExternalExistence::Absent
            }
        },
        &exceptions,
    )
}

fn diagnose_with_files(content: &str, existing: &[&str]) -> Vec<Diagnostic> {
    diagnose_at_path_with_files("test.md", content, existing)
}

/// Like `diagnose_with_files`, but treats the document as living at
/// `rel_path` (a workspace-relative path), so path-shaped references
/// resolve relative to that location — and root-relative `/` references
/// resolve at the workspace root regardless of `rel_path`'s depth.
/// `existing` lists workspace-relative paths that exist.
fn diagnose_at_path_with_files(
    rel_path: &str,
    content: &str,
    existing: &[&str],
) -> Vec<Diagnostic> {
    let fm = yaml::parse_frontmatter_block(content);
    let fm_span = fm.as_ref().map(|b| b.span);
    let tree = block::parse_tree(content, fm_span);
    let config = Config::default();
    let rel_path = std::path::Path::new(rel_path);
    let existing_set: HashSet<&str> = existing.iter().copied().collect();
    let exceptions = exceptions_of(content);
    collect(
        &tree,
        rel_path,
        &config,
        &|p| existing_set.contains(p.to_str().unwrap_or("")),
        &|_| ExternalExistence::Absent,
        &exceptions,
    )
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
fn skipped_heading_level_silent_by_default() {
    // Decision 009: a skipped level is a convention check, not a defect, so
    // it does not fire by default.
    let diags = diagnose("# H1\n\n### H3\n");
    assert!(
        !has_any(&diags, "skipped heading level"),
        "no skipped-level warning by default: {diags:?}"
    );
}

#[test]
fn skipped_heading_level_fires_when_enabled() {
    let mut config = Config::default();
    config.policy.skipped_heading_level = true;
    let diags = diagnose_with_config("# H1\n\n### H3\n", &config);
    assert_eq!(
        count_matching(&diags, Severity::Warning, "skipped heading level"),
        1,
        "one warning for skipped heading when enabled: {diags:?}"
    );
    assert!(
        has_any(&diags, "H1 to H3"),
        "message mentions levels: {diags:?}"
    );
}

#[test]
fn multiple_h1_silent_by_default() {
    // Decision 009: multiple H1 is a convention check, not a defect.
    let diags = diagnose("# First\n\n# Second\n");
    assert!(
        !has_any(&diags, "multiple H1"),
        "no multiple-H1 warning by default: {diags:?}"
    );
}

#[test]
fn multiple_h1_fires_when_enabled() {
    let mut config = Config::default();
    config.policy.multiple_h1 = true;
    let diags = diagnose_with_config("# First\n\n# Second\n", &config);
    assert_eq!(
        count_matching(&diags, Severity::Warning, "multiple H1"),
        1,
        "one warning for multiple H1 when enabled: {diags:?}"
    );
}

#[test]
fn duplicate_heading_exact() {
    // An exact-duplicate heading slugs identically — a real collision that
    // fires on by default.
    let diags = diagnose("## Overview\n\n## Overview\n");
    assert_eq!(
        count_matching(
            &diags,
            Severity::Warning,
            "duplicate heading slug `overview`"
        ),
        1,
        "one warning for exact duplicate heading: {diags:?}"
    );
}

#[test]
fn duplicate_heading_punctuation_collision() {
    // `Hello, World` and `Hello World` both slug to `hello-world`, so
    // `#hello-world` resolves only to the first — a genuine collision the
    // old lowercase proxy missed.
    let diags = diagnose("# Hello, World\n\n# Hello World\n");
    assert_eq!(
        count_matching(
            &diags,
            Severity::Warning,
            "duplicate heading slug `hello-world`"
        ),
        1,
        "one warning for punctuation/spacing slug collision: {diags:?}"
    );
}

#[test]
fn distinct_heading_slugs_no_duplicate() {
    // Two headings with distinct slugs do not collide.
    let diags = diagnose("## Overview\n\n## Details\n");
    assert!(
        !has_any(&diags, "duplicate heading slug"),
        "no duplicate warning for distinct slugs: {diags:?}"
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
    // Even with the opt-in skipped-level check on, sequential headings
    // (H1→H2→H3) draw no warning.
    let mut config = Config::default();
    config.policy.skipped_heading_level = true;
    let diags = diagnose_with_config("# H1\n\n## H2\n\n### H3\n", &config);
    assert!(
        !has_matching(&diags, Severity::Warning, "skipped"),
        "no warnings for sequential headings: {diags:?}"
    );
}

// -- Code block language --

#[test]
fn code_block_without_language_silent_by_default() {
    // Decision 009: an untagged fence is valid CommonMark with a
    // render-neutral non-fix, so `code_block_language` defaults to
    // Disabled and produces no diagnostic by default.
    let diags = diagnose("```\ncode\n```\n");
    assert!(
        !has_any(&diags, "language tag"),
        "no missing-language diagnostic by default: {diags:?}"
    );
}

#[test]
fn code_block_without_language_fires_when_enabled() {
    // When opted in to `hint`, the untagged fence draws a hint that names
    // the `text` escape hatch (issue 020). `warn`/`deny` are covered by
    // their own tests below.
    for (policy, severity) in [
        (CodeBlockLanguagePolicy::Hint, Severity::Hint),
        (CodeBlockLanguagePolicy::Warn, Severity::Warning),
        (CodeBlockLanguagePolicy::Deny, Severity::Error),
    ] {
        let mut config = Config::default();
        config.policy.code_block_language = policy;
        let diags = diagnose_with_config("```\ncode\n```\n", &config);
        assert_eq!(
            count_matching(&diags, severity, "without a language tag"),
            1,
            "one {policy:?} diagnostic for missing language: {diags:?}"
        );
    }

    // The hint variant must name the `text` escape hatch so authors of
    // non-code blocks (output, diagrams, trees) tag them deliberately
    // instead of guessing a language.
    let mut config = Config::default();
    config.policy.code_block_language = CodeBlockLanguagePolicy::Hint;
    let diags = diagnose_with_config("```\ncode\n```\n", &config);
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
fn image_empty_alt_text_silent_by_default() {
    // Decision 009: empty alt text is a convention check, not a defect (it
    // is the correct choice for decorative images), so it is off by
    // default.
    let diags = diagnose("![](image.png)\n");
    assert!(
        !has_any(&diags, "empty alt text"),
        "no empty-alt warning by default: {diags:?}"
    );
}

#[test]
fn image_empty_alt_text_fires_when_enabled() {
    let mut config = Config::default();
    config.policy.image_empty_alt = true;
    let diags = diagnose_with_config("![](image.png)\n", &config);
    assert_eq!(
        count_matching(&diags, Severity::Warning, "empty alt text"),
        1,
        "one warning for empty alt when enabled: {diags:?}"
    );
}

#[test]
fn image_with_alt_text_no_diagnostic() {
    // Even with the opt-in flag on, a non-empty alt draws no warning.
    let mut config = Config::default();
    config.policy.image_empty_alt = true;
    let diags = diagnose_with_config("![a logo](image.png)\n", &config);
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
    let diags = diagnose("A long line of filler text before the link, then https://example.com\n");
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
    // The hint teaches both honest resolutions (suggestion 001): make it a
    // link if it's a reference, or drop the extension if it's only a name.
    assert!(
        has_matching(&diags, Severity::Hint, "make it a link"),
        "the hint offers the make-it-a-link resolution: {diags:?}"
    );
    assert!(
        has_matching(&diags, Severity::Hint, "drop the extension"),
        "the hint offers the drop-the-extension resolution for a name: {diags:?}"
    );
}

#[test]
fn backticked_path_no_file() {
    // A dangling backtick `.md` draws no make-it-a-link hint, but does
    // draw the stale-reference warning (issue 028, default `warn`).
    let diags = diagnose("See `other.md` for details.\n");
    assert!(
        !has_any(&diags, "backticked path"),
        "no make-it-a-link hint when file doesn't exist: {diags:?}"
    );
    assert_eq!(
        count_matching(&diags, Severity::Warning, "stale reference"),
        1,
        "a dangling backtick `.md` draws the stale-reference warning: {diags:?}"
    );
}

// -- Path-shaped reference detection: `.md`-scope, fragments, missing
//    quadrant (issue 028) --

#[test]
fn quoted_path_no_file_is_stale_reference() {
    // The quoted form mirrors the backtick form: a dangling `.md` draws
    // the stale-reference warning, not the make-it-a-link hint.
    let diags = diagnose("See \"other.md\" for details.\n");
    assert!(
        !has_any(&diags, "quoted path"),
        "no make-it-a-link hint for a dangling quoted path: {diags:?}"
    );
    assert_eq!(
        count_matching(&diags, Severity::Warning, "stale reference"),
        1,
        "a dangling quoted `.md` draws the stale-reference warning: {diags:?}"
    );
}

// -- Quoted dir-bearing path: single owner (issue 032) --

#[test]
fn quoted_dir_path_dangling_emits_one_stale() {
    // A quoted token carrying a directory component is seen by the quoted
    // scanner and — before issue 032 — also by the bare-path scanner, which
    // trimmed the surrounding quotes. The bare scanner now leaves quoted
    // content to its single owner, so a dangling `"docs/gone.md"` draws
    // exactly one stale-reference diagnostic.
    let diags = diagnose("See \"docs/gone.md\" for details.\n");
    assert_eq!(
        count_matching(&diags, Severity::Warning, "stale reference"),
        1,
        "a dangling quoted dir-bearing `.md` is stale exactly once: {diags:?}"
    );
}

#[test]
fn quoted_external_dir_path_dangling_emits_one_stale() {
    // The `{Name}/…` quoted form (present alias dir, missing file) is the
    // external-namespace variant of the same shape: exactly one stale
    // diagnostic, not two.
    let config = config_with_archive_alias();
    let diags = diagnose_with_external(
        "See \"{Archive}/gone.md\" for details.\n",
        &config,
        &["/ext/Archive"],
    );
    assert_eq!(
        count_matching(&diags, Severity::Warning, "stale reference"),
        1,
        "a quoted external dir-bearing `.md` is stale exactly once: {diags:?}"
    );
}

#[test]
fn quoted_dir_path_resolving_emits_one_make_it_a_link() {
    // The other double-emit variant: a quoted dir-bearing token that
    // *resolves* drew both the quoted scanner's make-it-a-link hint and the
    // bare scanner's "convert to a markdown link" nudge. With quoted spans
    // single-owned, only the quoted-path hint fires, and no bare-path nudge.
    let diags = diagnose_with_files("See \"docs/other.md\" for details.\n", &["docs/other.md"]);
    assert_eq!(
        count_matching(&diags, Severity::Hint, "quoted path"),
        1,
        "a resolving quoted dir-bearing path draws one make-it-a-link hint: {diags:?}"
    );
    assert!(
        !has_any(&diags, "convert to a markdown link"),
        "the bare-path nudge does not also fire on quoted content: {diags:?}"
    );
}

#[test]
fn two_distinct_quoted_dir_paths_each_emit_once() {
    // Single-ownership must not over-suppress: two *different* quoted
    // dir-bearing dangling paths on one line still each emit once.
    let diags = diagnose("See \"docs/a.md\" and \"docs/b.md\" for details.\n");
    assert_eq!(
        count_matching(&diags, Severity::Warning, "stale reference"),
        2,
        "two distinct quoted dir-bearing paths each emit one stale: {diags:?}"
    );
}

// -- Single-quoted paths: first-class, identical to double quotes
//    (issue 032, Option C) --

#[test]
fn single_quoted_dangling_path_emits_one_stale() {
    // A single-quoted dangling `.md` is a first-class quoted path: exactly
    // one stale-reference diagnostic, mirroring the double-quote form.
    let diags = diagnose("See 'docs/gone.md' for details.\n");
    assert_eq!(
        count_matching(&diags, Severity::Warning, "stale reference"),
        1,
        "a dangling single-quoted `.md` is stale exactly once: {diags:?}"
    );
}

#[test]
fn single_quoted_resolving_path_emits_one_make_it_a_link() {
    // A single-quoted resolving path draws exactly one make-it-a-link hint,
    // with the message reflecting the actual quote character.
    let diags = diagnose_with_files("See 'docs/other.md' for details.\n", &["docs/other.md"]);
    assert_eq!(
        count_matching(&diags, Severity::Hint, "quoted path"),
        1,
        "a resolving single-quoted path draws one make-it-a-link hint: {diags:?}"
    );
    assert!(
        has_any(&diags, "`'docs/other.md'`"),
        "the hint reflects the single-quote character: {diags:?}"
    );
}

#[test]
fn double_quoted_dangling_path_still_one_stale_no_regression() {
    // The double-quote form is unchanged by adding single-quote support.
    let diags = diagnose("See \"docs/gone.md\" for details.\n");
    assert_eq!(
        count_matching(&diags, Severity::Warning, "stale reference"),
        1,
        "a dangling double-quoted `.md` is still stale exactly once: {diags:?}"
    );
}

#[test]
fn apostrophe_not_treated_as_quote() {
    // A `'` flanked by alphanumerics is an apostrophe, never a quote
    // delimiter: contractions, possessives, and `'n'` draw no path
    // diagnostic. (There is no `.md`-shaped path here, but the guard must
    // also not pair the apostrophes into a span at all.)
    for content in ["it's a test\n", "the dogs' bowls\n", "rock 'n' roll\n"] {
        let diags = diagnose(content);
        assert!(
            !has_any(&diags, "quoted path") && !has_any(&diags, "stale reference"),
            "an apostrophe is not a quote delimiter in {content:?}: {diags:?}"
        );
    }
}

#[test]
fn opening_single_quote_requires_whitespace_before() {
    // The opening `'` must have whitespace (or line start, or `(`) before
    // it, not merely a non-alphanumeric char — `_`/`-` are non-alphanumeric
    // but not boundaries. In `set value_'docs/gone.md' now`, the `_`-preceded
    // `'` is apostrophe-ish (cf. `example_'s`) and must not open a span, even
    // though the bytes after it look like a path.
    let glued = diagnose("set value_'docs/gone.md' now\n");
    assert!(
        !has_any(&glued, "stale reference") && !has_any(&glued, "quoted path"),
        "a non-whitespace-preceded `'` must not open a quoted span: {glued:?}"
    );
    // The user's pathological prose: underscores and possessives make
    // several apostrophe-`'`s; none open.
    let prose = diagnose("the function example_'s parameters' types are typed\n");
    assert!(
        !has_any(&prose, "stale reference") && !has_any(&prose, "quoted path"),
        "apostrophe-heavy prose opens no quoted span: {prose:?}"
    );
}

#[test]
fn paren_opens_single_quote_but_bracket_does_not() {
    // `(` is allowed before an opening `'` so a quoted path in a
    // parenthetical is caught; `[` is not, because it is markdown link
    // syntax and would clash.
    let paren = diagnose("see the example ('docs/gone.md') here\n");
    assert_eq!(
        count_matching(&paren, Severity::Warning, "stale reference"),
        1,
        "a `(`-preceded `'` opens a quoted path: {paren:?}"
    );
    let bracket = diagnose("see the example ['docs/gone.md'] here\n");
    assert!(
        !has_any(&bracket, "stale reference") && !has_any(&bracket, "quoted path"),
        "a `[`-preceded `'` does not open (markdown link clash): {bracket:?}"
    );
}

#[test]
fn contraction_before_single_quoted_path_is_caught() {
    // The whole reason the closing-search must also skip apostrophe
    // candidates: in `it's in 'docs/gone.md' today`, the apostrophe of
    // `it's` must not be consumed as an opening quote, and the real
    // single-quoted path is still found exactly once.
    let diags = diagnose("it's in 'docs/gone.md' today\n");
    assert_eq!(
        count_matching(&diags, Severity::Warning, "stale reference"),
        1,
        "a contraction before a single-quoted path does not hide it: {diags:?}"
    );
}

#[test]
fn multibyte_before_single_quote_is_caught_no_panic() {
    // A multi-byte char immediately before the opening `'` must not panic
    // (the look-behind decodes a char, never a raw byte) and the path is
    // still caught: `é` is alphanumeric, but a space separates it from the
    // quote, so the quote is at a word boundary.
    let diags = diagnose("café 'docs/gone.md'\n");
    assert_eq!(
        count_matching(&diags, Severity::Warning, "stale reference"),
        1,
        "a multibyte char before a single-quoted path: caught, no panic: {diags:?}"
    );
}

#[test]
fn single_quoted_external_dir_path_dangling_emits_one_stale() {
    // The `{Name}/…` external form in single quotes (defined alias, missing
    // file) is stale exactly once.
    let config = config_with_archive_alias();
    let diags = diagnose_with_external(
        "See '{Archive}/gone.md' for details.\n",
        &config,
        &["/ext/Archive"],
    );
    assert_eq!(
        count_matching(&diags, Severity::Warning, "stale reference"),
        1,
        "a single-quoted external dir-bearing `.md` is stale exactly once: {diags:?}"
    );
}

#[test]
fn two_distinct_single_quoted_paths_each_emit_once() {
    // Single-quote support must not over-suppress: two *different*
    // single-quoted dangling paths on one line still each emit once.
    let diags = diagnose("See 'docs/a.md' and 'docs/b.md' for details.\n");
    assert_eq!(
        count_matching(&diags, Severity::Warning, "stale reference"),
        2,
        "two distinct single-quoted dir-bearing paths each emit one stale: {diags:?}"
    );
}

#[test]
fn mixed_quote_styles_with_multibyte_each_emit_once() {
    // A double- and a single-quoted path on one line, with multibyte
    // content, each emit exactly one stale — no double-emit, no panic.
    let diags = diagnose("See \"docs/other.md\" and 'docs/外部.md' for café details.\n");
    assert_eq!(
        count_matching(&diags, Severity::Warning, "stale reference"),
        2,
        "one double- and one single-quoted path each emit one stale: {diags:?}"
    );
}

#[test]
fn bare_path_no_file_is_stale_reference() {
    // The bare (unbackticked, unquoted) form, with a directory component,
    // draws the stale-reference warning when its target is missing.
    let diags = diagnose("See docs/other.md for details.\n");
    assert!(
        !has_any(&diags, "convert to a markdown link"),
        "no make-it-a-link nudge for a dangling bare path: {diags:?}"
    );
    assert_eq!(
        count_matching(&diags, Severity::Warning, "stale reference"),
        1,
        "a dangling bare `.md` draws the stale-reference warning: {diags:?}"
    );
}

#[test]
fn bare_path_existing_file_is_make_it_a_link() {
    // A resolving bare path keeps the make-it-a-link nudge and draws no
    // stale-reference warning.
    let diags = diagnose_with_files("See docs/other.md for details.\n", &["docs/other.md"]);
    assert_eq!(
        count_matching(&diags, Severity::Warning, "convert to a markdown link"),
        1,
        "a resolving bare path keeps the make-it-a-link nudge: {diags:?}"
    );
    assert!(
        !has_any(&diags, "stale reference"),
        "a resolving bare path draws no stale-reference warning: {diags:?}"
    );
}

#[test]
fn backticked_fragment_existing_file_make_it_a_link() {
    // `` `foo.md#section` `` with `foo.md` present: the fragment is
    // stripped and the make-it-a-link hint fires on the file.
    let diags = diagnose_with_files("See `other.md#intro` for details.\n", &["other.md"]);
    assert_eq!(
        count_matching(&diags, Severity::Hint, "backticked path"),
        1,
        "an anchored backtick path resolves the file (fragment stripped): {diags:?}"
    );
}

#[test]
fn backticked_fragment_missing_file_is_stale_reference() {
    // `` `foo.md#section` `` with `foo.md` absent draws the stale-reference
    // warning (fragment stripped, path part resolved).
    let diags = diagnose("See `other.md#intro` for details.\n");
    assert_eq!(
        count_matching(&diags, Severity::Warning, "stale reference"),
        1,
        "an anchored backtick to a missing file is stale: {diags:?}"
    );
}

#[test]
fn quoted_fragment_existing_file_make_it_a_link() {
    let diags = diagnose_with_files("See \"other.md#intro\" for details.\n", &["other.md"]);
    assert_eq!(
        count_matching(&diags, Severity::Hint, "quoted path"),
        1,
        "an anchored quoted path resolves the file (fragment stripped): {diags:?}"
    );
}

#[test]
fn non_md_extensions_draw_no_dark_matter() {
    // `.rs`/`.toml`/image paths are not `.md`, so they form no graph edge:
    // neither a resolving nor a dangling one draws any dark-matter
    // diagnostic (decision 009). Link-existence validation is separate and
    // untouched (see `validation.rs`).
    for path in ["src/main.rs", "Cargo.toml", "docs/logo.png"] {
        let backtick = format!("See `{path}` for details.\n");
        let resolving = diagnose_with_files(&backtick, &[path]);
        let dangling = diagnose(&backtick);
        for diags in [&resolving, &dangling] {
            assert!(
                !has_any(diags, "backticked path")
                    && !has_any(diags, "stale reference")
                    && !has_any(diags, "convert to a markdown link"),
                "non-`.md` path `{path}` draws no dark-matter diagnostic: {diags:?}"
            );
        }
    }
}

#[test]
fn stem_without_extension_is_silent() {
    // A stem (`README`, `docs/README`) has no recognized extension, so it
    // is plain prose — out of the graph, no diagnostic either way.
    for stem in ["README", "docs/README"] {
        let diags = diagnose_with_files(&format!("See `{stem}` for details.\n"), &[stem]);
        assert!(
            !has_any(&diags, "backticked path")
                && !has_any(&diags, "stale reference")
                && !has_any(&diags, "convert to a markdown link"),
            "a bare stem `{stem}` is silent: {diags:?}"
        );
    }
}

#[test]
fn file_line_syntax_is_silent() {
    // `foo.md:102` is editor `file:line` syntax, not a markdown reference
    // form — it is never recognized.
    let diags = diagnose("See docs/foo.md:102 for details.\n");
    assert!(
        !has_any(&diags, "stale reference")
            && !has_any(&diags, "convert to a markdown link")
            && !has_any(&diags, "backticked path"),
        "`file:line` syntax is not a reference form: {diags:?}"
    );
}

#[test]
fn root_relative_existing_file_make_it_a_link() {
    // `/README.md` from a nested file with `<root>/README.md` present draws
    // the make-it-a-link hint (resolved at the workspace root).
    let diags = diagnose_at_path_with_files("a/b/c.md", "See `/README.md`.\n", &["README.md"]);
    assert_eq!(
        count_matching(&diags, Severity::Hint, "backticked path"),
        1,
        "root-relative `.md` resolves at the workspace root: {diags:?}"
    );
    assert!(
        !has_any(&diags, "stale reference"),
        "a resolving root-relative path draws no stale-reference: {diags:?}"
    );
}

// -- stale_references policy (issue 028) --

fn diagnose_with_stale_policy(
    content: &str,
    existing: &[&str],
    stale: StaleReferencePolicy,
) -> Vec<Diagnostic> {
    let fm = yaml::parse_frontmatter_block(content);
    let fm_span = fm.as_ref().map(|b| b.span);
    let tree = block::parse_tree(content, fm_span);
    let mut config = Config::default();
    config.policy.stale_references = stale;
    let rel_path = std::path::Path::new("test.md");
    let existing_set: HashSet<&str> = existing.iter().copied().collect();
    let exceptions = exceptions_of(content);
    collect(
        &tree,
        rel_path,
        &config,
        &|p| existing_set.contains(p.to_str().unwrap_or("")),
        &|_| ExternalExistence::Absent,
        &exceptions,
    )
}

#[test]
fn stale_references_disabled_silences_only_the_stale_warning() {
    // `disabled` silences the stale-reference warning but leaves the
    // make-it-a-link hint intact for resolving references.
    let dangling =
        diagnose_with_stale_policy("See `gone.md`.\n", &[], StaleReferencePolicy::Disabled);
    assert!(
        !has_any(&dangling, "stale reference"),
        "disabled silences the stale-reference warning: {dangling:?}"
    );

    let resolving = diagnose_with_stale_policy(
        "See `other.md`.\n",
        &["other.md"],
        StaleReferencePolicy::Disabled,
    );
    assert_eq!(
        count_matching(&resolving, Severity::Hint, "backticked path"),
        1,
        "disabling stale_references leaves the make-it-a-link hint intact: {resolving:?}"
    );
}

#[test]
fn stale_references_deny_is_error() {
    let diags = diagnose_with_stale_policy("See `gone.md`.\n", &[], StaleReferencePolicy::Deny);
    assert_eq!(
        count_matching(&diags, Severity::Error, "stale reference"),
        1,
        "deny escalates the stale-reference to an error: {diags:?}"
    );
}

#[test]
fn stale_references_hint_is_hint() {
    let diags = diagnose_with_stale_policy("See `gone.md`.\n", &[], StaleReferencePolicy::Hint);
    assert_eq!(
        count_matching(&diags, Severity::Hint, "stale reference"),
        1,
        "hint downgrades the stale-reference to a hint: {diags:?}"
    );
}

#[test]
fn stale_reference_fires_even_when_bare_paths_disabled() {
    // The two policies are decoupled: disabling `bare_paths` (the
    // make-it-a-link nudge) must not silence the stale-reference warning.
    let fm = yaml::parse_frontmatter_block("See `gone.md`.\n");
    let fm_span = fm.as_ref().map(|b| b.span);
    let tree = block::parse_tree("See `gone.md`.\n", fm_span);
    let mut config = Config::default();
    config.policy.bare_paths = BarePathPolicy::Disabled;
    let rel_path = std::path::Path::new("test.md");
    let diags = collect(
        &tree,
        rel_path,
        &config,
        &|_| false,
        &|_| ExternalExistence::Absent,
        &Exceptions::default(),
    );
    assert_eq!(
        count_matching(&diags, Severity::Warning, "stale reference"),
        1,
        "stale_references is independent of bare_paths: {diags:?}"
    );
}

// -- Root-relative `/` dark-matter resolution (issue 028) --

#[test]
fn backticked_root_relative_path_resolves_at_workspace_root() {
    // From a nested file, `` `/README.md` `` resolves at the workspace
    // root, so an existing `<root>/README.md` draws the make-it-a-link
    // hint — not silence (the path was previously read as filesystem
    // absolute and missed).
    let diags = diagnose_at_path_with_files(
        "a/b/c.md",
        "See `/README.md` for details.\n",
        &["README.md"],
    );
    assert_eq!(
        count_matching(&diags, Severity::Hint, "backticked path"),
        1,
        "root-relative backticked path resolves at the workspace root: {diags:?}"
    );
}

#[test]
fn backticked_root_relative_resolution_independent_of_depth() {
    // The same `/README.md` reference resolves identically from the root
    // and from a deep subdirectory.
    let root = diagnose_at_path_with_files("root.md", "See `/README.md`.\n", &["README.md"]);
    let deep =
        diagnose_at_path_with_files("a/b/c/d/deep.md", "See `/README.md`.\n", &["README.md"]);
    assert_eq!(
        count_matching(&root, Severity::Hint, "backticked path"),
        count_matching(&deep, Severity::Hint, "backticked path"),
        "root-relative resolution is depth-independent: root={root:?} deep={deep:?}"
    );
    assert_eq!(
        count_matching(&deep, Severity::Hint, "backticked path"),
        1,
        "the deep reference still resolves at the workspace root: {deep:?}"
    );
}

#[test]
fn backticked_root_relative_missing_file_no_hint() {
    // A root-relative reference whose target does not exist draws no
    // make-it-a-link hint, but does draw the stale-reference warning
    // (issue 028, the missing-quadrant default).
    let diags =
        diagnose_at_path_with_files("a/b/c.md", "See `/nope.md` for details.\n", &["README.md"]);
    assert!(
        !has_any(&diags, "backticked path"),
        "no make-it-a-link hint for a missing root-relative target: {diags:?}"
    );
    assert_eq!(
        count_matching(&diags, Severity::Warning, "stale reference"),
        1,
        "a missing root-relative `.md` draws the stale-reference warning: {diags:?}"
    );
}

#[test]
fn protocol_relative_backticked_path_not_treated_as_workspace_path() {
    // `//host/lib.md` is a protocol-relative URL, not a workspace path:
    // even if a same-named file existed it must not draw a path hint.
    let diags = diagnose_at_path_with_files(
        "a/b/c.md",
        "See `//cdn.example.com/lib.md` for details.\n",
        &["cdn.example.com/lib.md", "lib.md"],
    );
    assert!(
        !has_any(&diags, "backticked path"),
        "protocol-relative `//host` is external, not a workspace path: {diags:?}"
    );
}

// -- Both-bases resolution + `..` normalization + shape exclusions
//    (issue 028 false-positive flood) --

#[test]
fn dir_relative_dotdot_is_normalized_no_stale() {
    // Bug 2 repro: a backtick `../claude_code/PostToolUse.md` in
    // `architecture/hooks/Hook.md` joins to
    // `architecture/hooks/../claude_code/PostToolUse.md`, which must
    // normalize (collapse `..`) to the clean workspace key
    // `architecture/claude_code/PostToolUse.md` — so the reference resolves
    // and draws the make-it-a-link hint, not a stale-reference warning.
    let diags = diagnose_at_path_with_files(
        "architecture/hooks/Hook.md",
        "See `../claude_code/PostToolUse.md` for details.\n",
        &["architecture/claude_code/PostToolUse.md"],
    );
    assert!(
        !has_any(&diags, "stale reference"),
        "a `..`-relative reference that resolves after normalization is not stale: {diags:?}"
    );
    assert_eq!(
        count_matching(&diags, Severity::Hint, "backticked path"),
        1,
        "the normalized dir-relative reference draws the make-it-a-link hint: {diags:?}"
    );
}

#[test]
fn repo_root_relative_citation_resolves_at_root_no_stale() {
    // Bug 1 repro: a full repo-path citation `tickets/acquire/DESIGN.md`
    // inside `tickets/acquire/v2_01_cleanup.md` must resolve at the
    // workspace root (where the file lives), not at the source file's
    // parent (which would yield `tickets/acquire/tickets/acquire/...`).
    let diags = diagnose_at_path_with_files(
        "tickets/acquire/v2_01_cleanup.md",
        "See `tickets/acquire/DESIGN.md` for details.\n",
        &["tickets/acquire/DESIGN.md"],
    );
    assert!(
        !has_any(&diags, "stale reference"),
        "a repo-root-relative citation that exists at root is not stale: {diags:?}"
    );
    assert_eq!(
        count_matching(&diags, Severity::Hint, "backticked path"),
        1,
        "the root-resolved citation draws the make-it-a-link hint: {diags:?}"
    );
}

#[test]
fn genuine_dangling_under_neither_base_is_stale() {
    // A reference that exists under neither the dir base nor the root base
    // is a genuine dangling reference and still draws the stale warning.
    let diags = diagnose_at_path_with_files(
        "tickets/x/note.md",
        "See `tickets/correlation/missing.md` for details.\n",
        &["tickets/acquire/DESIGN.md"],
    );
    assert!(
        !has_any(&diags, "backticked path"),
        "a reference resolving under no base draws no make-it-a-link hint: {diags:?}"
    );
    assert_eq!(
        count_matching(&diags, Severity::Warning, "stale reference"),
        1,
        "a reference resolving under neither base is a genuine stale reference: {diags:?}"
    );
}

#[test]
fn excluded_path_shapes_draw_no_diagnostic() {
    // `~`-leading (home/out-of-repo), `<>`-bearing (placeholder), and
    // `*`-bearing (glob) tokens are not workspace paths at all: no
    // make-it-a-link hint and no stale-reference warning, whether or not a
    // same-named file exists.
    for token in ["~/Projects/Archive/AGENTS.md", "<name>/SKILL.md", "NN_*.md"] {
        let backtick = format!("See `{token}` for details.\n");
        // Once with nothing present, once with the literal token present.
        let dangling = diagnose(&backtick);
        let with_file = diagnose_with_files(&backtick, &[token]);
        for diags in [&dangling, &with_file] {
            assert!(
                !has_any(diags, "backticked path")
                    && !has_any(diags, "stale reference")
                    && !has_any(diags, "convert to a markdown link"),
                "excluded shape `{token}` draws no dark-matter diagnostic: {diags:?}"
            );
        }
    }
}

#[test]
fn excluded_glob_bare_path_draws_no_diagnostic() {
    // A bare (unbackticked) glob path with a directory component must also
    // be excluded by the tree-level scanner (`is_bare_path`).
    let diags = diagnose("See docs/NN_*.md for details.\n");
    assert!(
        !has_any(&diags, "stale reference") && !has_any(&diags, "convert to a markdown link"),
        "a bare glob path draws no dark-matter diagnostic: {diags:?}"
    );
}

#[test]
fn plain_in_dir_dangling_still_warns() {
    // Regression: a plain in-dir `.md` that exists under neither base is
    // still a genuine stale reference (the both-bases change must not
    // suppress real dangles).
    let diags = diagnose_at_path_with_files("docs/note.md", "See `gone.md`.\n", &[]);
    assert_eq!(
        count_matching(&diags, Severity::Warning, "stale reference"),
        1,
        "a plain in-dir dangling `.md` still warns: {diags:?}"
    );
}

#[test]
fn root_file_still_resolves_via_root_base() {
    // Regression: `/README.md` with `<root>/README.md` present still
    // resolves at the root (no stale warning, make-it-a-link hint fires).
    let diags = diagnose_at_path_with_files("a/b/c.md", "See `/README.md`.\n", &["README.md"]);
    assert_eq!(
        count_matching(&diags, Severity::Hint, "backticked path"),
        1,
        "a root-relative `/README.md` with the root file present still resolves: {diags:?}"
    );
    assert!(
        !has_any(&diags, "stale reference"),
        "the resolving root file draws no stale warning: {diags:?}"
    );
}

#[test]
fn dotdot_escaping_root_is_not_a_resolution() {
    // A `..` chain that escapes the workspace root after normalization is
    // not a valid workspace candidate, so an existing same-stem key must
    // not falsely resolve it; from a top-level file it is a genuine dangle.
    let diags = diagnose_at_path_with_files(
        "note.md",
        "See `../outside.md` for details.\n",
        &["outside.md"],
    );
    assert!(
        !has_any(&diags, "backticked path"),
        "an escaping `..` reference draws no make-it-a-link hint: {diags:?}"
    );
    assert_eq!(
        count_matching(&diags, Severity::Warning, "stale reference"),
        1,
        "an escaping `..` reference is a genuine stale reference: {diags:?}"
    );
}

// -- External-namespace `{Name}/…` references (issue 030, decision 010) --

/// A config with `Archive` aliased to a fixed (test-only) directory.
fn config_with_archive_alias() -> Config {
    let mut config = Config::default();
    config.external.insert(
        "Archive".to_string(),
        std::path::PathBuf::from("/ext/Archive"),
    );
    config
}

#[test]
fn external_namespace_recognizer() {
    assert_eq!(
        block::external_namespace("{Archive}/docs/x.md"),
        Some(("Archive", "docs/x.md")),
        "a leading `{{ident}}/` is recognized, splitting alias from the remainder"
    );
    assert_eq!(
        block::external_namespace("{my_repo-2}/x.md"),
        Some(("my_repo-2", "x.md")),
        "alphanumerics, `_` and `-` are valid identifier characters"
    );
    // No extension required: a directory or non-`.md` remainder is still an
    // external reference (issue 030 — existence-only, edge-free, so the
    // `.md` graph-edge scope does not apply).
    assert_eq!(
        block::external_namespace("{Archive}/docs"),
        Some(("Archive", "docs")),
        "an extension-less directory remainder is recognized"
    );
    assert_eq!(
        block::external_namespace("{Archive}/schema.txt"),
        Some(("Archive", "schema.txt")),
        "a non-`.md` file remainder is recognized"
    );
    // Not external references — these fall through to ordinary handling.
    for token in [
        "{Archive}",         // no trailing `/`
        "{Archive}/",        // empty remainder
        "{}/x.md",           // empty identifier
        "{a b}/x.md",        // space (not an identifier)
        "docs/{Archive}.md", // brace not at the start
        "Archive/x.md",      // no braces
    ] {
        assert_eq!(
            block::external_namespace(token),
            None,
            "`{token}` is not an external-namespace reference"
        );
    }
}

#[test]
fn external_undefined_alias_is_exempt() {
    // Tier 1 (the exempt floor): with no `[external]` table, a `{Name}/…`
    // citation is external and unverified — no diagnostic, no config needed.
    let diags = diagnose("See `{Archive}/docs/configuration.md` for details.\n");
    assert!(
        !has_any(&diags, "stale reference")
            && !has_any(&diags, "backticked path")
            && !has_any(&diags, "convert to a markdown link"),
        "an undefined `{{Name}}/…` alias draws no diagnostic (exempt floor): {diags:?}"
    );
}

#[test]
fn external_alias_dir_absent_is_exempt() {
    // Tier 2 (the CI / partial-checkout guard): the alias is defined but its
    // directory is not present on disk — exempt, never a false break.
    let config = config_with_archive_alias();
    let diags = diagnose_with_external(
        "See `{Archive}/docs/configuration.md` for details.\n",
        &config,
        // Nothing present: not even the alias directory.
        &[],
    );
    assert!(
        !has_any(&diags, "stale reference"),
        "a defined alias whose directory is absent is exempt: {diags:?}"
    );
}

#[test]
fn external_alias_dir_present_file_present_is_valid() {
    // Tier 3: directory present and the referenced file exists under it —
    // valid, no diagnostic. Notably no make-it-a-link nudge either: an
    // external reference is never a local link.
    let config = config_with_archive_alias();
    let diags = diagnose_with_external(
        "See `{Archive}/docs/configuration.md` for details.\n",
        &config,
        &["/ext/Archive", "/ext/Archive/docs/configuration.md"],
    );
    assert!(
        !has_any(&diags, "stale reference")
            && !has_any(&diags, "backticked path")
            && !has_any(&diags, "convert to a markdown link"),
        "a present external file is valid and draws no diagnostic: {diags:?}"
    );
}

#[test]
fn external_alias_dir_present_file_missing_is_stale() {
    // Tier 4: directory present but the referenced file is missing — a
    // genuinely broken cross-repo reference draws the stale-reference
    // warning.
    let config = config_with_archive_alias();
    let diags = diagnose_with_external(
        "See `{Archive}/docs/configuration.md` for details.\n",
        &config,
        // The alias directory exists; the file under it does not.
        &["/ext/Archive"],
    );
    assert_eq!(
        count_matching(&diags, Severity::Warning, "stale reference"),
        1,
        "a missing file under a present alias directory is stale: {diags:?}"
    );
}

#[test]
fn external_stat_failure_is_surfaced_not_exempted() {
    // Issue 050: a failed `stat` is not "absent". Degrading it to the
    // exempt tier silently converted an I/O flake into exemption; it must
    // surface as its own diagnostic instead, on either check.
    let config = config_with_archive_alias();
    // Alias directory stat fails.
    let dir_unknown = diagnose_with_external_unknown(
        "See `{Archive}/docs/configuration.md` for details.\n",
        &config,
        &[],
        &["/ext/Archive"],
    );
    assert_eq!(
        count_matching(
            &dir_unknown,
            Severity::Warning,
            "checking existence there failed"
        ),
        1,
        "a dir-level stat failure surfaces the cannot-verify diagnostic: {dir_unknown:?}"
    );
    // Directory present, file stat fails.
    let file_unknown = diagnose_with_external_unknown(
        "See `{Archive}/docs/configuration.md` for details.\n",
        &config,
        &["/ext/Archive"],
        &["/ext/Archive/docs/configuration.md"],
    );
    assert_eq!(
        count_matching(
            &file_unknown,
            Severity::Warning,
            "checking existence there failed"
        ),
        1,
        "a file-level stat failure surfaces the cannot-verify diagnostic: {file_unknown:?}"
    );
}

#[test]
fn external_stat_failure_suppressed_by_exception_without_unused_misreport() {
    // Issue 050's partial signature: under the old exempt degradation, a
    // stat flake made the reference's exception misreport as "unused …
    // no longer in the document". The cannot-verify diagnostic routes
    // through the same exception lookup, so a `{Name}/…`-keyed exception
    // suppresses it and stays used.
    let config = config_with_archive_alias();
    let content = "---\n\
             exceptions:\n  \
               stale_references:\n    \
                 \"{Archive}/docs/configuration.md\": \"deliberately absent in that repo\"\n\
             ---\n\
             See `{Archive}/docs/configuration.md` for details.\n";
    let diags = diagnose_with_external_unknown(content, &config, &[], &["/ext/Archive"]);
    assert!(
        !has_any(&diags, "checking existence there failed"),
        "the exception suppresses the cannot-verify diagnostic: {diags:?}"
    );
    assert!(
        !has_any(&diags, "unused exception"),
        "a stat failure must not misreport the exception as unused: {diags:?}"
    );
}

#[test]
fn external_reference_quoted_and_bare_forms() {
    // The `{Name}/…` shape is recognized on every citation surface 028
    // covers: quoted and bare-with-dir, not only backtick. The quoted
    // dir-bearing token now yields exactly one stale diagnostic — issue 032
    // gave quoted spans a single owner (the structural quoted scanner), so
    // the bare-path surface no longer claims the inner string. Both surfaces
    // therefore assert "exactly one."
    let config = config_with_archive_alias();
    for content in [
        "See \"{Archive}/docs/configuration.md\" for details.\n",
        "See {Archive}/docs/configuration.md for details.\n",
    ] {
        // Present dir, missing file → stale (tier 4).
        let stale = diagnose_with_external(content, &config, &["/ext/Archive"]);
        assert_eq!(
            count_matching(&stale, Severity::Warning, "stale reference"),
            1,
            "missing external file is stale exactly once on this surface: {stale:?}"
        );
        // Undefined alias → exempt (tier 1) on the same surface.
        let exempt = diagnose(content);
        assert!(
            !has_any(&exempt, "stale reference"),
            "undefined alias is exempt on this surface: {exempt:?}"
        );
    }
}

#[test]
fn external_reference_message_teaches_the_escape() {
    // The stale-reference message names the `{repo}/…` escape (suggestion
    // 001's self-documenting-message principle).
    let diags = diagnose("See `gone/missing.md` for details.\n");
    assert!(
        has_matching(&diags, Severity::Warning, "{repo}/") && has_any(&diags, ".lattice.toml"),
        "the stale message teaches the `{{repo}}/…` external escape: {diags:?}"
    );
}

#[test]
fn external_directory_and_non_md_references_are_checked() {
    // An external `{Name}/…` reference is existence-checked regardless of
    // extension: a cross-repo *directory* (`{Archive}/docs`) or non-`.md`
    // file (`{Archive}/schema.txt`) is a real reference. It is edge-free and
    // never a local link (decision 010), so the `.md` graph-edge scope that
    // gates intra-repo dark matter does not apply. Covered on every citation
    // surface: bare, backtick, quoted.
    let config = config_with_archive_alias();
    for reference in ["{Archive}/docs", "{Archive}/schema.txt"] {
        for content in [
            format!("See {reference} for details.\n"),     // bare
            format!("See `{reference}` for details.\n"),   // backtick
            format!("See \"{reference}\" for details.\n"), // quoted
        ] {
            // Tier 4: alias dir present, target missing under it → stale.
            let stale = diagnose_with_external(&content, &config, &["/ext/Archive"]);
            assert_eq!(
                count_matching(&stale, Severity::Warning, "stale reference"),
                1,
                "a missing external `{reference}` is stale exactly once: {stale:?}"
            );
            // Tier 2: alias dir absent → exempt, no diagnostic.
            let exempt_absent = diagnose_with_external(&content, &config, &[]);
            assert!(
                !has_any(&exempt_absent, "stale reference"),
                "an absent alias directory is exempt for `{reference}`: {exempt_absent:?}"
            );
            // Tier 1: undefined alias → exempt.
            let exempt_undef = diagnose(&content);
            assert!(
                !has_any(&exempt_undef, "stale reference"),
                "an undefined alias is exempt for `{reference}`: {exempt_undef:?}"
            );
        }
    }
}

#[test]
fn external_directory_reference_present_is_valid() {
    // Tier 3 for a directory: the alias dir is present and the referenced
    // directory exists under it → valid, and (unlike a resolving intra-repo
    // path) no make-it-a-link nudge, because an external reference is never
    // a local link.
    let config = config_with_archive_alias();
    let diags = diagnose_with_external(
        "See `{Archive}/docs` for details.\n",
        &config,
        &["/ext/Archive", "/ext/Archive/docs"],
    );
    assert!(
        !has_any(&diags, "stale reference") && !has_any(&diags, "backticked path"),
        "a present external directory is valid and draws no diagnostic: {diags:?}"
    );
}

#[test]
fn external_stale_message_names_alias_not_intra_repo_escape() {
    // The external stale message must not reuse the intra-repo framing: the
    // reference resolves against the alias directory (not "this root"), is
    // already `{repo}/…` (so teaching that escape is noise), and may name a
    // directory rather than a "markdown file". It names the alias and the
    // directory it resolved to instead.
    let config = config_with_archive_alias();
    let diags = diagnose_with_external(
        "See `{Archive}/docs` for details.\n",
        &config,
        &["/ext/Archive"],
    );
    assert!(
        has_matching(&diags, Severity::Warning, "external alias `Archive`"),
        "the external stale message names the alias: {diags:?}"
    );
    assert!(
        !has_any(&diags, "under this root") && !has_any(&diags, "markdown file"),
        "it drops the intra-repo 'markdown file under this root' framing: {diags:?}"
    );
    assert!(
        !has_any(&diags, "{repo}/"),
        "it does not teach the `{{repo}}/…` escape for a reference already using it: {diags:?}"
    );
}

#[test]
fn external_namespace_ellipsis_placeholder_is_exempt() {
    // The `{repo}/…` syntax this tool teaches is documentation shorthand for
    // a path *shape*, not a real reference — so a `{Name}/…` (or `{Name}/...`)
    // placeholder is exempt even when the alias is defined and present, on
    // every surface (decision 014's move test: there is no target a move
    // could break). Without this, the syntax description would flag itself —
    // the regression dogfooding caught when external refs stopped being
    // `.md`-scoped.
    let config = config_with_archive_alias();
    for placeholder in ["{Archive}/…", "{Archive}/..."] {
        for content in [
            format!("Write it as {placeholder} for a cross-repo ref.\n"),
            format!("Write it as `{placeholder}` for a cross-repo ref.\n"),
            format!("Write it as \"{placeholder}\" for a cross-repo ref.\n"),
        ] {
            // Alias dir present (tier-4 territory for a concrete path), yet
            // the ellipsis placeholder draws nothing.
            let diags = diagnose_with_external(&content, &config, &["/ext/Archive"]);
            assert!(
                !has_any(&diags, "stale reference") && !has_any(&diags, "external alias"),
                "the `{placeholder}` placeholder is exempt, not a stale reference: {diags:?}"
            );
        }
    }
}

#[test]
fn external_reference_is_never_a_graph_edge() {
    // Decision 010: a `{Name}/…` citation imposes no graph obligation. It is
    // a backtick/quoted/bare citation, not a markdown link, so it never
    // appears in link extraction — assert nothing comes out of `links()`.
    let tree = block::parse_tree(
        "See `{Archive}/docs/configuration.md` and {Archive}/x.md.\n",
        None,
    );
    let links = tree.links(std::path::Path::new("test.md"));
    assert!(
        links.is_empty(),
        "an external `{{Name}}/…` citation forms no graph edge: {links:?}"
    );
}

// -- External-citation steering (issue 073) --

/// The three dark-matter spellings of the same reference, in the order
/// bare / backtick / quoted — the 028 family's whole surface.
fn dark_matter_spellings(reference: &str) -> [String; 3] {
    [
        format!("See {reference} for details.\n"),
        format!("See `{reference}` for details.\n"),
        format!("See \"{reference}\" for details.\n"),
    ]
}

#[test]
fn external_steering_names_the_citation_on_every_dark_matter_surface() {
    // Issue 073: a dangling reference whose path *does* exist under a
    // configured alias directory gets the concrete spelling to write, not
    // the generic "write it as `{repo}/…`" lesson. The alias table was only
    // ever read to validate a citation already written; this reads it to
    // notice that a typed path is citable — the connection whose absence
    // produced nine hand-written exceptions in issue 066.
    let config = config_with_archive_alias();
    for content in dark_matter_spellings("docs/configuration.md") {
        let diags = diagnose_with_external(
            &content,
            &config,
            &["/ext/Archive", "/ext/Archive/docs/configuration.md"],
        );
        assert_eq!(
            count_matching(
                &diags,
                Severity::Warning,
                "path exists in external `Archive`"
            ),
            1,
            "the steering variant fires exactly once for {content:?}: {diags:?}"
        );
        assert!(
            has_any(&diags, "cite it as `{Archive}/docs/configuration.md`"),
            "it names the exact `{{Alias}}/rest` spelling for {content:?}: {diags:?}"
        );
        assert!(
            !has_any(&diags, "no such markdown file under this root"),
            "it replaces the generic stale message for {content:?}: {diags:?}"
        );
    }
}

#[test]
fn external_steering_never_overrides_a_live_intra_repo_reference() {
    // The ordering rule (issue 073 point 1): the steering check runs only
    // after intra-repo resolution *fails*. A same-named file in a sibling
    // repo must never displace a live local reference — that would be a
    // regression, not a hint. Enforced structurally: `route_stale_reference`
    // is reached only from the dangling branch of every emit site.
    let config = config_with_archive_alias();
    for content in dark_matter_spellings("docs/configuration.md") {
        let diags = diagnose_with_files_and_external(
            &content,
            &config,
            // The path resolves in *this* workspace …
            &["docs/configuration.md"],
            // … and also exists under the alias directory.
            &["/ext/Archive", "/ext/Archive/docs/configuration.md"],
        );
        assert!(
            !has_any(&diags, "path exists in external") && !has_any(&diags, "stale reference"),
            "a resolving local reference is never steered externally for {content:?}: {diags:?}"
        );
    }
}

#[test]
fn external_steering_requires_a_present_verdict() {
    // Issue 073 point 2: only `Present` may claim a citation. The tri-state
    // oracle degrades exactly as decision 010's tiers do — an absent alias
    // directory (partial checkout / CI with only this repo) and a failed
    // `stat` (issue 050) both fall back to the generic stale message rather
    // than assert a path exists somewhere unchecked.
    let config = config_with_archive_alias();
    let content = "See `docs/configuration.md` for details.\n";
    let cases = [
        // Alias directory absent: nothing to claim against.
        (
            "absent alias directory",
            diagnose_with_external(content, &config, &[]),
        ),
        // Alias directory present, the path is not under it.
        (
            "present directory, path missing under it",
            diagnose_with_external(content, &config, &["/ext/Archive"]),
        ),
        // The alias directory's own `stat` failed.
        (
            "unknown alias directory",
            diagnose_with_external_unknown(content, &config, &[], &["/ext/Archive"]),
        ),
        // Directory present, but the path's `stat` failed.
        (
            "unknown path under a present directory",
            diagnose_with_external_unknown(
                content,
                &config,
                &["/ext/Archive"],
                &["/ext/Archive/docs/configuration.md"],
            ),
        ),
    ];
    for (label, diags) in cases {
        assert!(
            !has_any(&diags, "path exists in external"),
            "{label} must not claim a citation: {diags:?}"
        );
        assert_eq!(
            count_matching(
                &diags,
                Severity::Warning,
                "no such markdown file under this root"
            ),
            1,
            "{label} degrades to the plain stale-reference hint: {diags:?}"
        );
    }
}

#[test]
fn external_steering_severity_tracks_the_stale_policy() {
    // Steering changes the message, never the tier: severity still follows
    // `stale_references` exactly, and `Disabled` still emits nothing.
    let present = ["/ext/Archive", "/ext/Archive/docs/configuration.md"];
    let content = "See `docs/configuration.md` for details.\n";
    for (policy, severity) in [
        (StaleReferencePolicy::Hint, Severity::Hint),
        (StaleReferencePolicy::Warn, Severity::Warning),
        (StaleReferencePolicy::Deny, Severity::Error),
    ] {
        let mut config = config_with_archive_alias();
        config.policy.stale_references = policy;
        let diags = diagnose_with_external(content, &config, &present);
        assert_eq!(
            count_matching(&diags, severity, "path exists in external `Archive`"),
            1,
            "the steering message tracks the {policy:?} policy: {diags:?}"
        );
    }

    let mut config = config_with_archive_alias();
    config.policy.stale_references = StaleReferencePolicy::Disabled;
    let diags = diagnose_with_external(content, &config, &present);
    assert!(
        !has_any(&diags, "path exists in external"),
        "a disabled policy emits no steering message: {diags:?}"
    );
}

#[test]
fn external_steering_picks_the_first_alias_deterministically() {
    // Issue 073 point 3, decided: first match wins. `config.external` is a
    // `BTreeMap`, so the scan is alphabetical — independent of the order the
    // aliases were declared in. Both citations are equally correct (an
    // external reference is existence-only and edge-free), and the message's
    // job is to name *one* spelling.
    let mut config = Config::default();
    // Insert out of alphabetical order: `Zed` first, `Archive` second.
    config
        .external
        .insert("Zed".to_string(), std::path::PathBuf::from("/ext/Zed"));
    config.external.insert(
        "Archive".to_string(),
        std::path::PathBuf::from("/ext/Archive"),
    );
    let diags = diagnose_with_external(
        "See `docs/configuration.md` for details.\n",
        &config,
        &[
            "/ext/Zed",
            "/ext/Zed/docs/configuration.md",
            "/ext/Archive",
            "/ext/Archive/docs/configuration.md",
        ],
    );
    assert!(
        has_any(&diags, "cite it as `{Archive}/docs/configuration.md`"),
        "the alphabetically-first alias is named, not the first declared: {diags:?}"
    );
    assert!(
        !has_any(&diags, "{Zed}/"),
        "exactly one citation is offered, not both: {diags:?}"
    );
}

#[test]
fn external_steering_citation_normalizes_the_path_it_spells() {
    // The spelling must be one that resolves when written back: a `.`
    // segment and a root-relative leading `/` are collapsed, and a
    // reference that escapes the workspace root has no citation form at all
    // (a `..` would also make the alias-directory probe `stat` outside it).
    let config = config_with_archive_alias();
    let present = ["/ext/Archive", "/ext/Archive/docs/configuration.md"];
    for reference in ["./docs/configuration.md", "/docs/configuration.md"] {
        let content = format!("See `{reference}` for details.\n");
        let diags = diagnose_with_external(&content, &config, &present);
        assert!(
            has_any(&diags, "cite it as `{Archive}/docs/configuration.md`"),
            "`{reference}` cites as the normalized path: {diags:?}"
        );
    }

    let escaping = diagnose_with_external(
        "See `../docs/configuration.md` for details.\n",
        &config,
        &present,
    );
    assert!(
        !has_any(&escaping, "path exists in external"),
        "a root-escaping reference has no citation form: {escaping:?}"
    );
}

#[test]
fn external_steering_is_suppressed_by_a_path_keyed_exception() {
    // The steering message routes through the same exception lookup and
    // keeps the same `stale reference:` prefix, so it lands in the same
    // ledger row and namespace: an existing `stale_references` exception
    // keyed on the bare path still suppresses it, and is not misreported as
    // unused. This is the migration path for issue 066's nine suppressions —
    // the exception keeps working while the message teaches the conversion.
    let config = config_with_archive_alias();
    let content = "---\n\
             exceptions:\n  \
               stale_references:\n    \
                 \"docs/configuration.md\": \"lives in the Archive repo\"\n\
             ---\n\
             See `docs/configuration.md` for details.\n";
    let diags = diagnose_with_external(
        content,
        &config,
        &["/ext/Archive", "/ext/Archive/docs/configuration.md"],
    );
    assert!(
        !has_any(&diags, "stale reference"),
        "the exception suppresses the steering message: {diags:?}"
    );
    assert!(
        !has_any(&diags, "unused exception"),
        "and is not misreported as unused: {diags:?}"
    );
}

#[test]
fn external_steering_classifies_as_a_stale_reference_lint() {
    // The ledger and the subtree-override aggregate key on the message
    // prefix, so the steering variant must classify identically to the
    // generic stale message it replaces.
    assert_eq!(
        classify_028_lint(
            "stale reference: `docs/x.md` — path exists in external `Archive` — cite it as `{Archive}/docs/x.md`"
        ),
        Some(ExceptionLint::StaleReferences),
        "the steering variant is a stale_references diagnostic"
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
    let diags = collect(
        &tree,
        rel_path,
        &config,
        &|_| false,
        &|_| ExternalExistence::Absent,
        &Exceptions::default(),
    );
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
    let diags = collect(
        &tree,
        rel_path,
        &config,
        &|_| false,
        &|_| ExternalExistence::Absent,
        &Exceptions::default(),
    );
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
    let exceptions = exceptions_of(content);
    collect(
        &tree,
        rel_path,
        &config,
        &|p| existing_set.contains(p.to_str().unwrap_or("")),
        &|_| ExternalExistence::Absent,
        &exceptions,
    )
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

// -- Frontmatter `exceptions` (issue 031, decision 011) --

#[test]
fn exception_suppresses_unresolved_stale_reference() {
    // An exception keyed by the still-unresolved reference suppresses its
    // stale-reference diagnostic — and, having matched, is not flagged as
    // unused.
    let content = "---\n\
            exceptions:\n  \
              stale_references:\n    \
                \"gone.md\": \"hypothetical path in the worked example\"\n\
            ---\n\
            See `gone.md` for details.\n";
    let diags = diagnose_with_files(content, &[]);
    assert!(
        !has_any(&diags, "stale reference"),
        "the exception suppresses the stale-reference diagnostic: {diags:?}"
    );
    assert!(
        !has_any(&diags, "unused exception"),
        "a matched exception is not flagged as unused: {diags:?}"
    );
}

#[test]
fn exception_with_no_live_diagnostic_is_unused_and_echoes_reason() {
    // The reference is gone from the body, so the exception matches nothing
    // — flagged as unused, echoing the stored reason (the epitaph).
    let content = "---\n\
            exceptions:\n  \
              stale_references:\n    \
                \"gone.md\": \"hypothetical path in the worked example\"\n\
            ---\n\
            Nothing references it now.\n";
    let diags = diagnose_with_files(content, &[]);
    assert_eq!(
        count_matching(&diags, Severity::Warning, "unused exception: `gone.md`"),
        1,
        "an exception matching no live diagnostic is flagged as unused: {diags:?}"
    );
    assert!(
        has_any(&diags, "hypothetical path in the worked example"),
        "the unused-exception message echoes the stored reason: {diags:?}"
    );
}

#[test]
fn exception_with_empty_reason_is_a_diagnostic() {
    // A required reason: an empty reason is itself a defect, anchored at the
    // key — even though the suppression still applies.
    let content = "---\n\
            exceptions:\n  \
              stale_references:\n    \
                \"gone.md\": \"\"\n\
            ---\n\
            See `gone.md` here.\n";
    let diags = diagnose_with_files(content, &[]);
    assert_eq!(
        count_matching(&diags, Severity::Warning, "has no reason"),
        1,
        "an empty-reason exception is a diagnostic: {diags:?}"
    );
    // An empty-reason entry that matched a live diagnostic does not *also*
    // get flagged as unused — exactly one reconciliation diagnostic.
    assert!(
        !has_any(&diags, "unused exception"),
        "a matched empty-reason entry is not also flagged unused: {diags:?}"
    );
}

#[test]
fn external_alias_keyed_exception_suppresses_present_missing_stale() {
    // A `{Name}/…`-keyed exception flows through identically: a defined,
    // present alias whose target file is missing is a stale reference, and
    // the literal `{Name}/…` key suppresses it (decision 011).
    let config = config_with_archive_alias();
    let content = "---\n\
            exceptions:\n  \
              stale_references:\n    \
                \"{Archive}/old/layout.md\": \"pre-refactor path, kept for the changelog note\"\n\
            ---\n\
            See `{Archive}/old/layout.md` for the old shape.\n";
    // Alias directory present, file under it missing → tier 4 (stale).
    let diags = diagnose_with_external(content, &config, &["/ext/Archive"]);
    assert!(
        !has_any(&diags, "stale reference"),
        "a `{{Name}}/…`-keyed exception suppresses the present-missing stale: {diags:?}"
    );
    assert!(
        !has_any(&diags, "unused exception"),
        "the matched alias-keyed exception is not flagged unused: {diags:?}"
    );
}

#[test]
fn exception_scope_is_per_reference() {
    // An exception for one reference does not suppress a *different*
    // unresolved one.
    let content = "---\n\
            exceptions:\n  \
              stale_references:\n    \
                \"excepted.md\": \"deliberately not a live reference\"\n\
            ---\n\
            See `excepted.md` and also `other.md`.\n";
    let diags = diagnose_with_files(content, &[]);
    assert_eq!(
        count_matching(&diags, Severity::Warning, "stale reference: `other.md`"),
        1,
        "the unexcepted reference still fires: {diags:?}"
    );
    assert!(
        !has_any(&diags, "stale reference: `excepted.md`"),
        "the excepted reference is suppressed: {diags:?}"
    );
}

#[test]
fn exception_is_never_a_graph_edge_or_backlink_obligation() {
    // An `exceptions` block is a path-shaped-lint lever only: it must never
    // appear in link/graph extraction (decision 011 — no edge, no backlink).
    let content = "---\n\
            exceptions:\n  \
              stale_references:\n    \
                \"gone.md\": \"deliberately dead\"\n\
            ---\n\
            Body text.\n";
    let fm = yaml::parse_frontmatter_block(content);
    let fm_span = fm.as_ref().map(|b| b.span);
    let tree = block::parse_tree(content, fm_span);
    let links = tree.links(std::path::Path::new("test.md"));
    assert!(
        links.is_empty(),
        "an exception forms no graph edge: {links:?}"
    );
}

#[test]
fn rename_flags_old_key_unused_while_new_name_fires() {
    // On a rename the old exception key matches nothing (unused) while the
    // renamed reference, lacking an exception, fires fresh — both present.
    let content = "---\n\
            exceptions:\n  \
              stale_references:\n    \
                \"old-name.md\": \"the design doc, since renamed\"\n\
            ---\n\
            See `new-name.md` for the design.\n";
    let diags = diagnose_with_files(content, &[]);
    assert_eq!(
        count_matching(&diags, Severity::Warning, "unused exception: `old-name.md`"),
        1,
        "the renamed-away old key is flagged unused: {diags:?}"
    );
    assert_eq!(
        count_matching(&diags, Severity::Warning, "stale reference: `new-name.md`"),
        1,
        "the new name fires a fresh stale reference: {diags:?}"
    );
}

#[test]
fn bare_paths_exception_suppresses_resolve_hint() {
    // The `bare_paths` namespace suppresses the make-it-a-link nudge on a
    // *resolving* path (the lint fires on resolution).
    let content = "---\n\
            exceptions:\n  \
              bare_paths:\n    \
                \"README.md\": \"naming the file, deliberately not a link\"\n\
            ---\n\
            See `README.md` for the overview.\n";
    let diags = diagnose_with_files(content, &["README.md"]);
    assert!(
        !has_any(&diags, "backticked path"),
        "the bare_paths exception suppresses the resolve hint: {diags:?}"
    );
    assert!(
        !has_any(&diags, "unused exception"),
        "the matched bare_paths exception is not flagged unused: {diags:?}"
    );
}

#[test]
fn exception_round_trips_both_namespaces_and_alias_keys() {
    // A frontmatter exercising both namespaces, the map form, and a
    // `{Name}/…` key parses into the two buckets with reasons retained.
    let content = "---\n\
            exceptions:\n  \
              stale_references:\n    \
                \"tickets/acquire/DESIGN.md\": \"hypothetical path in the worked example\"\n    \
                \"{Archive}/old/layout.md\": \"pre-refactor path\"\n  \
              bare_paths:\n    \
                \"README\": \"naming the file, deliberately not a link\"\n\
            ---\n\
            Body.\n";
    let exceptions = exceptions_of(content);
    assert_eq!(
        exceptions.stale_references.len(),
        2,
        "two stale_references exceptions parsed: {exceptions:?}"
    );
    assert_eq!(
        exceptions.bare_paths.len(),
        1,
        "one bare_paths exception parsed: {exceptions:?}"
    );
    assert_eq!(
        exceptions.stale_references[0].reference, "tickets/acquire/DESIGN.md",
        "the first stale key is the literal reference: {exceptions:?}"
    );
    assert_eq!(
        exceptions.stale_references[1].reference, "{Archive}/old/layout.md",
        "the `{{Name}}/…` key is retained verbatim: {exceptions:?}"
    );
    assert_eq!(
        exceptions.bare_paths[0].reference, "README",
        "the bare_paths key is the literal reference: {exceptions:?}"
    );
    assert_eq!(
        exceptions.stale_references[0].reason, "hypothetical path in the worked example",
        "the reason is the map value: {exceptions:?}"
    );
}

#[test]
fn exception_for_disabled_lint_is_not_flagged_unused() {
    // When `stale_references` is `Disabled`, its exceptions are inert: no
    // suppression is needed and no unused-exception flood is produced.
    let content = "---\n\
            exceptions:\n  \
              stale_references:\n    \
                \"gone.md\": \"deliberately dead\"\n\
            ---\n\
            Nothing references it.\n";
    let diags = diagnose_with_stale_policy(content, &[], StaleReferencePolicy::Disabled);
    assert!(
        !has_any(&diags, "unused exception"),
        "a disabled lint's exceptions are not flagged unused: {diags:?}"
    );
}

// -- In-tool config pointer: messages close the loop in-context (issue 035) --

#[test]
fn stale_reference_message_points_at_config_help() {
    // The stale-reference message routes the agent back to the config
    // grammar from the diagnostic itself (issue 035).
    let diags = diagnose("See `gone/missing.md` for details.\n");
    assert!(
        has_matching(&diags, Severity::Warning, "lattice help config"),
        "the stale-reference message names `lattice help config`: {diags:?}"
    );
}

#[test]
fn make_it_a_link_message_names_both_escapes_and_config_help() {
    // FU2 (issue 031, folded into 035; reframed by 039): the make-it-a-link
    // hint names BOTH example escapes — drop the extension, OR except it with
    // a reason — under the move-test framing, pointing to `lattice help
    // config` (the literal `exceptions.bare_paths` namespace lives in the
    // config reference now, not in the per-occurrence message).
    let diags = diagnose_with_files("See `other.md` for details.\n", &["other.md"]);
    assert!(
        has_matching(&diags, Severity::Hint, "drop the extension"),
        "the hint still offers drop-the-extension: {diags:?}"
    );
    assert!(
        has_matching(&diags, Severity::Hint, "except it with a reason"),
        "the hint names the frontmatter exception escape with its required reason (FU2): {diags:?}"
    );
    assert!(
        has_matching(&diags, Severity::Hint, "lattice help config"),
        "the hint points at `lattice help config`: {diags:?}"
    );
}

#[test]
fn stale_reference_message_frames_the_move_test() {
    // Issue 039 / decision 014: the stale-reference message states the choice
    // as the move test ("would a move update this?"), not just a flat list of
    // knobs, while keeping the `lattice help config` pointer.
    let diags = diagnose("See `gone.md` here.\n");
    assert!(
        has_matching(&diags, Severity::Warning, "stale reference: `gone.md`"),
        "the stale-reference message still fires: {diags:?}"
    );
    assert!(
        has_matching(
            &diags,
            Severity::Warning,
            "would moving the target update this"
        ),
        "the stale-reference message frames the choice as the move test: {diags:?}"
    );
    assert!(
        has_matching(&diags, Severity::Warning, "lattice help config"),
        "the stale-reference message keeps the config pointer: {diags:?}"
    );
}

#[test]
fn make_it_a_link_message_frames_the_move_test() {
    // The resolving backticked-path (make-it-a-link) message frames link-vs-
    // example as the move test, keeps the make-it-a-link resolution, and the
    // config pointer.
    let diags = diagnose_with_files("See `other.md` here.\n", &["other.md"]);
    assert!(
        has_matching(&diags, Severity::Hint, "backticked path `other.md`"),
        "the make-it-a-link hint still fires: {diags:?}"
    );
    assert!(
        has_matching(&diags, Severity::Hint, "would moving it update this"),
        "the make-it-a-link hint frames the choice as the move test: {diags:?}"
    );
    assert!(
        has_matching(&diags, Severity::Hint, "make it a link"),
        "the make-it-a-link hint keeps the link resolution: {diags:?}"
    );
    assert!(
        has_matching(&diags, Severity::Hint, "lattice help config"),
        "the make-it-a-link hint keeps the config pointer: {diags:?}"
    );
}

#[test]
fn bare_path_make_it_a_link_message_points_at_config_help() {
    // Every bare_paths-gated nudge routes to the config grammar (issue 035):
    // the unbacticked resolving-path "convert to a markdown link" warning
    // carries the `lattice help config` pointer too.
    let diags = diagnose_with_files("See docs/other.md for details.\n", &["docs/other.md"]);
    assert!(
        has_matching(&diags, Severity::Warning, "convert to a markdown link"),
        "the bare-path nudge still fires: {diags:?}"
    );
    assert!(
        has_matching(&diags, Severity::Warning, "lattice help config"),
        "the bare-path nudge points at `lattice help config`: {diags:?}"
    );
}

#[test]
fn quoted_path_message_points_at_config_help() {
    // The quoted-path resolving hint is the third bare_paths-gated nudge; it
    // carries the `lattice help config` pointer too (issue 035).
    let diags = diagnose_with_files("See \"docs/other.md\" for details.\n", &["docs/other.md"]);
    assert!(
        has_matching(&diags, Severity::Hint, "quoted path"),
        "the quoted-path hint still fires: {diags:?}"
    );
    assert!(
        has_matching(&diags, Severity::Hint, "lattice help config"),
        "the quoted-path hint points at `lattice help config`: {diags:?}"
    );
}

#[test]
fn unused_exception_message_points_at_config_help() {
    let content = "---\n\
            exceptions:\n  \
              stale_references:\n    \
                \"gone.md\": \"hypothetical path in the worked example\"\n\
            ---\n\
            Nothing references it now.\n";
    let diags = diagnose_with_files(content, &[]);
    assert!(
        has_matching(&diags, Severity::Warning, "unused exception: `gone.md`",),
        "the unused-exception message still fires: {diags:?}"
    );
    assert!(
        has_matching(&diags, Severity::Warning, "lattice help config"),
        "the unused-exception message points at `lattice help config`: {diags:?}"
    );
}

#[test]
fn empty_reason_message_points_at_config_help() {
    let content = "---\n\
            exceptions:\n  \
              stale_references:\n    \
                \"gone.md\": \"\"\n\
            ---\n\
            See `gone.md` here.\n";
    let diags = diagnose_with_files(content, &[]);
    assert!(
        has_matching(&diags, Severity::Warning, "has no reason"),
        "the empty-reason message still fires: {diags:?}"
    );
    assert!(
        has_matching(&diags, Severity::Warning, "lattice help config"),
        "the empty-reason message points at `lattice help config`: {diags:?}"
    );
}

// -- Count-key + suppression ledger (issue 036, decision 012) --

/// Like [`diagnose_with_files`], but returns both the diagnostics and the
/// [`FileSuppressions`] ledger entry, with an explicit config so the
/// count-key tests can flip a lint to `Disabled`.
fn diagnose_full(
    content: &str,
    config: &Config,
    existing: &[&str],
) -> (Vec<Diagnostic>, FileSuppressions) {
    let fm = yaml::parse_frontmatter_block(content);
    let fm_span = fm.as_ref().map(|b| b.span);
    let tree = block::parse_tree(content, fm_span);
    let rel_path = std::path::Path::new("test.md");
    let existing_set: HashSet<&str> = existing.iter().copied().collect();
    let exceptions = exceptions_of(content);
    collect_with_suppressions(
        &tree,
        rel_path,
        config,
        &|p| existing_set.contains(p.to_str().unwrap_or("")),
        &|_| ExternalExistence::Absent,
        &exceptions,
    )
}

/// A document with three dangling stale references in the body, under a
/// `stale_references` count-key of `count`, with a non-empty shared reason.
fn three_stale_with_count(count: &str) -> String {
    format!(
        "---\n\
             exceptions:\n  \
               stale_references:\n    \
                 \"{count}\": \"migration table — every path is a record, not a live reference\"\n\
             ---\n\
             See `a.md`, `b.md`, and `c.md`.\n"
    )
}

#[test]
fn count_key_suppresses_iff_residual_equals_n() {
    // Three dangling references, N = 3: the whole residual is suppressed
    // under the single shared reason, nothing resurfaces.
    let config = Config::default();
    let (diags, sup) = diagnose_full(&three_stale_with_count("3"), &config, &[]);
    assert!(
        !has_any(&diags, "stale reference"),
        "a count-key of N == M suppresses the whole residual: {diags:?}"
    );
    assert!(
        !has_any(&diags, "expected"),
        "no drift warning when the count matches: {diags:?}"
    );
    // The ledger records the count-key suppression by severity (the default
    // stale_references policy is `warn`).
    let count_key = &sup.count_keys;
    assert_eq!(
        count_key.len(),
        1,
        "the matched count-key produces one ledger row: {sup:?}"
    );
    assert_eq!(
        count_key[0].counts.warnings, 3,
        "the ledger tallies the three suppressed warnings: {sup:?}"
    );
    assert_eq!(
        count_key[0].raw, "3",
        "the row carries the raw key: {sup:?}"
    );
}

#[test]
fn count_key_one_too_many_resurfaces_and_flags() {
    // Three dangling references, N = 2 (one too few expected → drift): the
    // sentinel is inert, every residual resurfaces, and a drift warning is
    // anchored on the key with the `expected N, found M` message.
    let config = Config::default();
    let (diags, sup) = diagnose_full(&three_stale_with_count("2"), &config, &[]);
    assert_eq!(
        count_matching(&diags, Severity::Warning, "stale reference: `"),
        3,
        "every residual diagnostic resurfaces on drift: {diags:?}"
    );
    assert!(
        has_matching(
            &diags,
            Severity::Warning,
            "expected 2 stale references here, found 3"
        ),
        "the drift warning names N and M: {diags:?}"
    );
    assert!(
        sup.count_keys.is_empty(),
        "a drifted count-key suppresses nothing, so no ledger row: {sup:?}"
    );
}

#[test]
fn count_key_one_too_few_resurfaces_and_flags() {
    // Three dangling references, N = 4 (one too many expected → drift).
    let config = Config::default();
    let (diags, sup) = diagnose_full(&three_stale_with_count("4"), &config, &[]);
    assert_eq!(
        count_matching(&diags, Severity::Warning, "stale reference: `"),
        3,
        "every residual diagnostic resurfaces on drift: {diags:?}"
    );
    assert!(
        has_matching(
            &diags,
            Severity::Warning,
            "expected 4 stale references here, found 3"
        ),
        "the drift warning names N and M: {diags:?}"
    );
    assert!(
        sup.count_keys.is_empty(),
        "a drifted count-key suppresses nothing: {sup:?}"
    );
}

#[test]
fn count_key_and_literal_compose() {
    // A literal key carves its own diagnostic out of the residual first; the
    // count-key then claims the remaining two. N = 2 over the residual.
    let content = "---\n\
            exceptions:\n  \
              stale_references:\n    \
                \"a.md\": \"the worked example path\"\n    \
                \"2\": \"the rest of the migration table\"\n\
            ---\n\
            See `a.md`, `b.md`, and `c.md`.\n";
    let config = Config::default();
    let (diags, sup) = diagnose_full(content, &config, &[]);
    assert!(
        !has_any(&diags, "stale reference"),
        "the literal carves one out and the count covers the rest: {diags:?}"
    );
    assert!(
        !has_any(&diags, "expected"),
        "no drift: the residual after the literal is exactly N: {diags:?}"
    );
    let ex = sup
        .exceptions
        .as_ref()
        .expect("the literal exception suppressed one");
    assert_eq!(
        ex.counts.warnings, 1,
        "the literal row tallies its one suppression: {sup:?}"
    );
    assert_eq!(ex.matched_entries, 1, "one literal entry matched: {sup:?}");
    assert_eq!(
        sup.count_keys.first().map(|c| c.counts.warnings),
        Some(2),
        "the count-key row tallies the residual of two: {sup:?}"
    );
}

#[test]
fn count_key_with_empty_reason_is_diagnosed() {
    // An empty reason is a defect (the shared epitaph is required), anchored
    // at the key; the residual resurfaces (the sentinel cannot suppress).
    let content = "---\n\
            exceptions:\n  \
              stale_references:\n    \
                \"3\": \"\"\n\
            ---\n\
            See `a.md`, `b.md`, and `c.md`.\n";
    let config = Config::default();
    let (diags, sup) = diagnose_full(content, &config, &[]);
    assert!(
        has_matching(&diags, Severity::Warning, "count-key `3`")
            && has_matching(&diags, Severity::Warning, "has no reason"),
        "an empty-reason count-key is diagnosed at the key: {diags:?}"
    );
    assert_eq!(
        count_matching(&diags, Severity::Warning, "stale reference: `"),
        3,
        "the residual resurfaces under an empty-reason count-key: {diags:?}"
    );
    assert!(
        sup.count_keys.is_empty(),
        "an empty-reason count-key suppresses nothing: {sup:?}"
    );
}

#[test]
fn count_key_of_zero_is_diagnosed() {
    // `N >= 1`: a `0` count-key is invalid — diagnosed at the key, residual
    // resurfaces.
    let content = "---\n\
            exceptions:\n  \
              stale_references:\n    \
                \"0\": \"a reason\"\n\
            ---\n\
            See `a.md`.\n";
    let config = Config::default();
    let (diags, _sup) = diagnose_full(content, &config, &[]);
    assert!(
        has_matching(&diags, Severity::Warning, "must be at least 1"),
        "a zero count-key is diagnosed: {diags:?}"
    );
    assert_eq!(
        count_matching(&diags, Severity::Warning, "stale reference: `"),
        1,
        "the residual resurfaces under a zero count-key: {diags:?}"
    );
}

#[test]
fn count_key_under_disabled_lint_is_inert() {
    // A `Disabled` stale_references lint makes the count-key inert: no
    // suppression, no drift flag, no empty-reason flag — and no residual to
    // resurface (the lint emits nothing).
    let mut config = Config::default();
    config.policy.stale_references = StaleReferencePolicy::Disabled;
    // N deliberately mismatches the body, which would drift if active.
    let (diags, sup) = diagnose_full(&three_stale_with_count("99"), &config, &[]);
    assert!(
        !has_any(&diags, "stale reference"),
        "a disabled lint emits no stale references: {diags:?}"
    );
    assert!(
        !has_any(&diags, "expected"),
        "a disabled lint's count-key raises no drift flag: {diags:?}"
    );
    assert!(
        sup.is_empty(),
        "a disabled-lint count-key suppresses nothing: {sup:?}"
    );
}

#[test]
fn count_key_shape_discrimination() {
    // `31` is a sentinel (claims the residual); `31.md` and `a/31.md` are
    // literal references (each suppresses only its own diagnostic). Here the
    // two literal keys carve their own out and the `31` sentinel claims the
    // single remaining dangling reference, so N = 1 suppresses cleanly.
    let content = "---\n\
            exceptions:\n  \
              stale_references:\n    \
                \"31.md\": \"a literal path-shaped key\"\n    \
                \"a/31.md\": \"another literal path-shaped key\"\n    \
                \"1\": \"the residual count sentinel\"\n\
            ---\n\
            See `31.md`, `a/31.md`, and `loose.md`.\n";
    let config = Config::default();
    let (diags, sup) = diagnose_full(content, &config, &[]);
    assert!(
        !has_any(&diags, "stale reference"),
        "the two literals carve out, the sentinel claims the rest: {diags:?}"
    );
    let ex = sup
        .exceptions
        .as_ref()
        .expect("the two path-shaped literals suppressed");
    assert_eq!(
        ex.matched_entries, 2,
        "`31.md` and `a/31.md` are literal entries, both matched: {sup:?}"
    );
    assert_eq!(
        sup.count_keys.first().map(|c| c.counts.warnings),
        Some(1),
        "the `1` sentinel claims the single residual: {sup:?}"
    );
}

// -- 028-family lint classifier (issue 037) --

#[test]
fn classify_028_lint_maps_each_message_family() {
    // The exact production message prefixes the emitters above produce.
    assert_eq!(
        classify_028_lint("stale reference: `gone.md` — no such markdown file"),
        Some(ExceptionLint::StaleReferences),
        "the stale-reference message maps to StaleReferences"
    );
    for bare in [
        "bare path `docs/x.md`: convert to a markdown link",
        "bare URL `https://x` : wrap in angle brackets",
        "quoted path `\"x.md\"`: use backticks",
        "backticked path `x.md` refers to an existing file",
    ] {
        assert_eq!(
            classify_028_lint(bare),
            Some(ExceptionLint::BarePaths),
            "a bare_paths-family message maps to BarePaths: {bare}"
        );
    }
    assert_eq!(
        classify_028_lint("empty heading"),
        None,
        "a non-028 message maps to neither lint"
    );
    assert_eq!(
        classify_028_lint("duplicate heading slug `x`"),
        None,
        "another non-028 message maps to neither lint"
    );
}

// -- Artifact glossary (issue 038, decision 013) --

/// A [`Config`] whose `[graph] artifacts` glossary lists `names`.
fn config_with_artifacts(names: &[&str]) -> Config {
    Config {
        artifacts: names.iter().map(|s| (*s).to_string()).collect(),
        ..Config::default()
    }
}

#[test]
fn artifact_name_resolving_draws_no_make_it_a_link_hint() {
    // The bare artifact name coincides with this repo's own root file, so it
    // would normally draw the make-it-a-link hint — the glossary swallows it.
    let config = config_with_artifacts(&["AGENTS.md"]);
    let (diags, sup) = diagnose_full("See `AGENTS.md` for the hooks.\n", &config, &["AGENTS.md"]);
    assert!(
        !has_any(&diags, "make it a link"),
        "a glossary artifact draws no make-it-a-link hint even when it resolves: {diags:?}"
    );
    assert!(
        !has_any(&diags, "AGENTS.md"),
        "no diagnostic mentions the artifact at all: {diags:?}"
    );
    assert_eq!(
        sup.artifacts.get("AGENTS.md").map(|c| c.hints),
        Some(1),
        "the swallowed hint is recorded in the ledger tally: {sup:?}"
    );
}

#[test]
fn artifact_name_dangling_draws_no_stale_reference() {
    // The bare artifact name resolves to no file in this repo, so it would
    // normally draw a stale_references warning — the glossary swallows it.
    let config = config_with_artifacts(&["GEMINI.md"]);
    let (diags, sup) = diagnose_full("Put hooks in `GEMINI.md`.\n", &config, &[]);
    assert!(
        !has_any(&diags, "stale reference"),
        "a glossary artifact draws no stale-reference diagnostic when it dangles: {diags:?}"
    );
    assert_eq!(
        sup.artifacts.get("GEMINI.md").map(|c| c.warnings),
        Some(1),
        "the swallowed stale-reference warning is recorded in the ledger tally: {sup:?}"
    );
}

#[test]
fn artifact_exact_match_only_path_qualified_still_flags() {
    // `AGENTS.md` is a glossary member; `dir/AGENTS.md` is a DIFFERENT
    // reference and is not matched — it still draws its normal diagnostic.
    let config = config_with_artifacts(&["AGENTS.md"]);
    let (diags, sup) = diagnose_full("See `dir/AGENTS.md`.\n", &config, &[]);
    assert!(
        has_matching(
            &diags,
            Severity::Warning,
            "stale reference: `dir/AGENTS.md`"
        ),
        "a path-qualified reference is not the bare artifact and still flags: {diags:?}"
    );
    assert!(
        sup.artifacts.is_empty(),
        "the path-qualified reference produced no artifact suppression: {sup:?}"
    );
}

#[test]
fn artifact_quoted_and_backticked_both_filtered() {
    // Both dark-matter shapes — a quoted path and a backticked path — are
    // filtered by the glossary.
    let config = config_with_artifacts(&["CLAUDE.md"]);
    let (diags, sup) = diagnose_full("Edit \"CLAUDE.md\" and also `CLAUDE.md`.\n", &config, &[]);
    assert!(
        !has_any(&diags, "CLAUDE.md"),
        "neither the quoted nor the backticked artifact mention is flagged: {diags:?}"
    );
    assert_eq!(
        sup.artifacts.get("CLAUDE.md").map(|c| c.warnings),
        Some(2),
        "both dark-matter mentions are tallied: {sup:?}"
    );
}

#[test]
fn artifact_filtered_before_count_key_residual() {
    // The artifact is removed before the count-key sees it: a count-key of
    // N = 2 over the two genuine dangling references suppresses cleanly, with
    // no drift — the artifact never entered the residual.
    let content = "---\n\
            exceptions:\n  \
              stale_references:\n    \
                \"2\": \"the two genuine dangling references\"\n\
            ---\n\
            See `AGENTS.md`, `a.md`, and `b.md`.\n";
    let config = config_with_artifacts(&["AGENTS.md"]);
    let (diags, sup) = diagnose_full(content, &config, &[]);
    assert!(
        !has_any(&diags, "stale reference"),
        "the count-key of 2 covers the two genuine refs; the artifact was filtered first: {diags:?}"
    );
    assert!(
        !has_any(&diags, "expected"),
        "no drift — the artifact never entered the residual, so the residual is exactly 2: {diags:?}"
    );
    assert_eq!(
        sup.count_keys.first().map(|c| c.counts.warnings),
        Some(2),
        "the count-key residual is the two genuine refs, not three: {sup:?}"
    );
    assert_eq!(
        sup.artifacts.get("AGENTS.md").map(|c| c.warnings),
        Some(1),
        "the artifact is tallied as its own source, not folded into the count-key: {sup:?}"
    );
}

#[test]
fn artifact_is_not_exceptable() {
    // An artifact filters before the exception machinery, so a frontmatter
    // `stale_references` exception keyed on the artifact name matches
    // nothing live and is flagged as unused — it is not the lever.
    let content = "---\n\
            exceptions:\n  \
              stale_references:\n    \
                \"SKILL.md\": \"trying (wrongly) to except the artifact here\"\n\
            ---\n\
            See `SKILL.md`.\n";
    let config = config_with_artifacts(&["SKILL.md"]);
    let (diags, sup) = diagnose_full(content, &config, &[]);
    assert!(
        has_matching(&diags, Severity::Warning, "unused exception: `SKILL.md`"),
        "the exception keyed on the artifact matches nothing live (the glossary filtered it first): {diags:?}"
    );
    assert!(
        sup.exceptions.is_none(),
        "the artifact was not suppressed by the exception: {sup:?}"
    );
    assert_eq!(
        sup.artifacts.get("SKILL.md").map(|c| c.warnings),
        Some(1),
        "the artifact suppression is recorded under the artifact source: {sup:?}"
    );
}

#[test]
fn no_glossary_keeps_current_behaviour() {
    // With an empty glossary the artifact name flags exactly as before.
    let config = Config::default();
    let (diags, sup) = diagnose_full("See `AGENTS.md`.\n", &config, &[]);
    assert!(
        has_matching(&diags, Severity::Warning, "stale reference: `AGENTS.md`"),
        "an empty glossary leaves the name to flag normally: {diags:?}"
    );
    assert!(
        sup.artifacts.is_empty(),
        "an empty glossary records no artifact suppression: {sup:?}"
    );
}
