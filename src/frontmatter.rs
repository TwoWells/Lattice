// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! YAML frontmatter parsing for backlink extraction.
//!
//! Detects `---` delimited frontmatter at the start of a markdown file,
//! parses the `backlinks` section, and validates inverse predicates against
//! the configured vocabulary.

use std::collections::HashMap;
use std::ops::Range;

use serde::Deserialize;

use crate::config::Config;

/// Errors that can occur when parsing frontmatter.
#[derive(Debug, thiserror::Error)]
pub enum FrontmatterError {
    /// The YAML in the frontmatter block is malformed.
    #[error("invalid YAML in frontmatter (line {line}): {message}")]
    InvalidYaml {
        /// 1-based line number where the frontmatter starts.
        line: usize,
        /// Description of the parse error.
        message: String,
    },
}

/// A diagnostic about a backlink predicate issue.
#[derive(Debug, PartialEq, Eq)]
pub struct BacklinkDiagnostic {
    /// 1-based line number of the predicate key in the source file.
    pub line: usize,
    /// The unknown inverse predicate.
    pub predicate: String,
}

/// Parsed frontmatter from a markdown document.
#[derive(Debug)]
pub struct Frontmatter {
    /// Byte range of the entire frontmatter block (including `---` delimiters).
    pub byte_range: Range<usize>,
    /// 1-based line of the opening `---`.
    pub start_line: usize,
    /// 1-based line of the closing `---`.
    pub end_line: usize,
    /// Parsed backlinks: inverse predicate → list of relative file paths.
    pub backlinks: HashMap<String, Vec<String>>,
}

/// Result of frontmatter extraction.
#[derive(Debug)]
pub struct FrontmatterResult {
    /// Parsed frontmatter, if present.
    pub frontmatter: Option<Frontmatter>,
    /// Diagnostics for unknown inverse predicates.
    pub diagnostics: Vec<BacklinkDiagnostic>,
}

/// Raw YAML structure for the frontmatter block.
#[derive(Debug, Deserialize)]
struct RawFrontmatter {
    backlinks: Option<HashMap<String, Vec<String>>>,
    /// Capture remaining fields so we don't reject unknown keys.
    #[serde(flatten)]
    _rest: HashMap<String, serde_yaml_ng::Value>,
}

/// Parse frontmatter from a markdown document and validate backlinks.
///
/// Returns a [`FrontmatterResult`] containing the parsed frontmatter (if any)
/// and diagnostics for unknown inverse predicates.
///
/// # Errors
///
/// Returns [`FrontmatterError`] if the frontmatter block contains invalid YAML.
pub fn parse_frontmatter(
    source: &str,
    config: &Config,
) -> Result<FrontmatterResult, FrontmatterError> {
    let Some((yaml_content, byte_range, start_line, end_line)) = extract_raw_frontmatter(source)
    else {
        return Ok(FrontmatterResult {
            frontmatter: None,
            diagnostics: Vec::new(),
        });
    };

    if yaml_content.trim().is_empty() {
        return Ok(FrontmatterResult {
            frontmatter: Some(Frontmatter {
                byte_range,
                start_line,
                end_line,
                backlinks: HashMap::new(),
            }),
            diagnostics: Vec::new(),
        });
    }

    let raw: RawFrontmatter =
        serde_yaml_ng::from_str(yaml_content).map_err(|e| FrontmatterError::InvalidYaml {
            line: start_line,
            message: e.to_string(),
        })?;

    let backlinks = raw.backlinks.unwrap_or_default();

    let mut diagnostics = Vec::new();
    for predicate in backlinks.keys() {
        if !config.is_known_inverse(predicate) {
            let line = find_key_line(yaml_content, predicate)
                .map_or(start_line, |offset| start_line + offset);
            diagnostics.push(BacklinkDiagnostic {
                line,
                predicate: predicate.clone(),
            });
        }
    }

    Ok(FrontmatterResult {
        frontmatter: Some(Frontmatter {
            byte_range,
            start_line,
            end_line,
            backlinks,
        }),
        diagnostics,
    })
}

/// Extract the raw YAML content between `---` delimiters.
///
/// Returns `(yaml_content, byte_range, start_line, end_line)` or `None`
/// if the file does not start with frontmatter.
fn extract_raw_frontmatter(source: &str) -> Option<(&str, Range<usize>, usize, usize)> {
    // Frontmatter must start at the very beginning of the file.
    if !source.starts_with("---\n") && !source.starts_with("---\r\n") {
        return None;
    }

    let opener_len = if source.starts_with("---\r\n") { 5 } else { 4 };

    let rest = &source[opener_len..];
    let closing_pos = find_closing_delimiter(rest)?;

    let yaml_content = &rest[..closing_pos];
    let closing_line_len = if rest[closing_pos..].starts_with("---\r\n") {
        5
    } else if rest[closing_pos..].starts_with("---\n") {
        4
    } else {
        // Closing `---` at end of file without trailing newline.
        3
    };

    let byte_end = opener_len + closing_pos + closing_line_len;
    let byte_range = 0..byte_end;

    let start_line = 1;
    let newline_count = source[..byte_end].bytes().filter(|&b| b == b'\n').count();
    let end_line = newline_count + usize::from(!source[..byte_end].ends_with('\n'));

    Some((yaml_content, byte_range, start_line, end_line))
}

/// Find the 0-based line offset of a YAML key within the frontmatter content.
///
/// Looks for `key:` as a YAML mapping key (with leading whitespace only).
/// Returns the line offset from the start of the YAML content, or `None`
/// if the key is not found.
fn find_key_line(yaml_content: &str, key: &str) -> Option<usize> {
    for (line_offset, line) in yaml_content.lines().enumerate() {
        let trimmed = line.trim_start();
        if let Some(after_key) = trimmed.strip_prefix(key)
            && after_key.starts_with(':')
        {
            // +1 because the YAML content starts on the line after `---`.
            return Some(line_offset + 1);
        }
    }
    None
}

/// Find the byte offset of the closing `---` delimiter in the remaining text.
///
/// The closing delimiter must appear at the start of a line.
fn find_closing_delimiter(rest: &str) -> Option<usize> {
    let mut search_from = 0;
    loop {
        let candidate = rest[search_from..].find("---")?;
        let abs_pos = search_from + candidate;

        // Must be at start of a line (position 0 or preceded by newline).
        let at_line_start = abs_pos == 0 || rest.as_bytes().get(abs_pos - 1) == Some(&b'\n');
        if !at_line_start {
            search_from = abs_pos + 3;
            continue;
        }

        // Must be followed by newline, CRLF, or EOF.
        let after = abs_pos + 3;
        let valid_end = after >= rest.len()
            || rest.as_bytes().get(after) == Some(&b'\n')
            || rest.as_bytes().get(after) == Some(&b'\r');
        if !valid_end {
            search_from = after;
            continue;
        }

        return Some(abs_pos);
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for clarity")]
mod tests {
    use super::*;

    fn default_config() -> Config {
        Config::default()
    }

    #[test]
    fn parse_valid_backlinks() {
        let source = "---\nbacklinks:\n  superseded_by:\n    - decisions/38.md\n  amended_by:\n    - decisions/38.md\n    - tickets/14h.md\n---\n# Document\n";
        let result =
            parse_frontmatter(source, &default_config()).expect("should parse successfully");
        let fm = result.frontmatter.expect("should have frontmatter");

        assert_eq!(fm.backlinks.len(), 2, "should have two backlink predicates");
        assert_eq!(
            fm.backlinks.get("superseded_by"),
            Some(&vec!["decisions/38.md".to_string()]),
            "superseded_by should have one entry"
        );
        assert_eq!(
            fm.backlinks.get("amended_by"),
            Some(&vec![
                "decisions/38.md".to_string(),
                "tickets/14h.md".to_string()
            ]),
            "amended_by should have two entries"
        );
        assert!(
            result.diagnostics.is_empty(),
            "known predicates should produce no diagnostics"
        );
    }

    #[test]
    fn no_frontmatter() {
        let source = "# Just a heading\n\nSome text.\n";
        let result =
            parse_frontmatter(source, &default_config()).expect("should parse successfully");
        assert!(
            result.frontmatter.is_none(),
            "should return None when no frontmatter"
        );
        assert!(result.diagnostics.is_empty(), "should have no diagnostics");
    }

    #[test]
    fn empty_frontmatter() {
        let source = "---\n---\n# Heading\n";
        let result =
            parse_frontmatter(source, &default_config()).expect("should parse successfully");
        let fm = result.frontmatter.expect("should have frontmatter");
        assert!(
            fm.backlinks.is_empty(),
            "empty frontmatter should have no backlinks"
        );
    }

    #[test]
    fn frontmatter_without_backlinks_key() {
        let source = "---\ntitle: My Document\nauthor: Test\n---\n# Heading\n";
        let result =
            parse_frontmatter(source, &default_config()).expect("should parse successfully");
        let fm = result.frontmatter.expect("should have frontmatter");
        assert!(
            fm.backlinks.is_empty(),
            "frontmatter without backlinks key should have empty backlinks"
        );
    }

    #[test]
    fn invalid_yaml() {
        let source = "---\n: invalid: yaml: [[\n---\n";
        let result = parse_frontmatter(source, &default_config());
        assert!(result.is_err(), "malformed YAML should produce an error");
        let err = result.expect_err("should be an error");
        assert!(
            matches!(err, FrontmatterError::InvalidYaml { line: 1, .. }),
            "error should report line 1, got: {err:?}"
        );
    }

    #[test]
    fn unknown_inverse_predicate() {
        let source = "---\nbacklinks:\n  invented_by:\n    - foo.md\n---\n";
        let result =
            parse_frontmatter(source, &default_config()).expect("should parse successfully");
        assert_eq!(
            result.diagnostics.len(),
            1,
            "should flag one unknown predicate"
        );
        assert_eq!(
            result.diagnostics[0].predicate, "invented_by",
            "should flag the unknown predicate"
        );
        assert_eq!(
            result.diagnostics[0].line, 3,
            "should point at the predicate key line"
        );
    }

    #[test]
    fn byte_range_covers_delimiters() {
        let source = "---\ntitle: test\n---\nBody text.\n";
        let result =
            parse_frontmatter(source, &default_config()).expect("should parse successfully");
        let fm = result.frontmatter.expect("should have frontmatter");
        assert_eq!(
            &source[fm.byte_range], "---\ntitle: test\n---\n",
            "byte range should cover entire frontmatter block including delimiters"
        );
    }

    #[test]
    fn line_numbers() {
        let source = "---\ntitle: test\nbacklinks:\n  referenced_by:\n    - a.md\n---\nBody.\n";
        let result =
            parse_frontmatter(source, &default_config()).expect("should parse successfully");
        let fm = result.frontmatter.expect("should have frontmatter");
        assert_eq!(fm.start_line, 1, "frontmatter should start at line 1");
        assert_eq!(fm.end_line, 6, "closing delimiter should be on line 6");
    }

    #[test]
    fn no_frontmatter_when_dashes_not_at_start() {
        let source = "Some text\n---\ntitle: test\n---\n";
        let result =
            parse_frontmatter(source, &default_config()).expect("should parse successfully");
        assert!(
            result.frontmatter.is_none(),
            "dashes not at file start should not be treated as frontmatter"
        );
    }

    #[test]
    fn crlf_line_endings() {
        let source = "---\r\nbacklinks:\r\n  superseded_by:\r\n    - a.md\r\n---\r\nBody.\r\n";
        let result =
            parse_frontmatter(source, &default_config()).expect("should parse CRLF frontmatter");
        let fm = result.frontmatter.expect("should have frontmatter");
        assert_eq!(
            fm.backlinks.get("superseded_by"),
            Some(&vec!["a.md".to_string()]),
            "should parse backlinks with CRLF endings"
        );
    }

    #[test]
    fn multiple_unknown_predicates() {
        let source =
            "---\nbacklinks:\n  unknown_one:\n    - a.md\n  unknown_two:\n    - b.md\n---\n";
        let result =
            parse_frontmatter(source, &default_config()).expect("should parse successfully");
        assert_eq!(
            result.diagnostics.len(),
            2,
            "should flag both unknown predicates"
        );
        let flagged: Vec<&str> = result
            .diagnostics
            .iter()
            .map(|d| d.predicate.as_str())
            .collect();
        assert!(flagged.contains(&"unknown_one"), "should flag unknown_one");
        assert!(flagged.contains(&"unknown_two"), "should flag unknown_two");
    }

    #[test]
    fn mixed_known_and_unknown_predicates() {
        let source =
            "---\nbacklinks:\n  superseded_by:\n    - a.md\n  fake_pred:\n    - b.md\n---\n";
        let result =
            parse_frontmatter(source, &default_config()).expect("should parse successfully");
        let fm = result.frontmatter.expect("should have frontmatter");
        assert_eq!(
            fm.backlinks.len(),
            2,
            "should parse both known and unknown predicates"
        );
        assert_eq!(
            result.diagnostics.len(),
            1,
            "should flag only the unknown predicate"
        );
        assert_eq!(
            result.diagnostics[0].predicate, "fake_pred",
            "should flag fake_pred"
        );
        assert_eq!(
            result.diagnostics[0].line, 5,
            "should point at the fake_pred key line"
        );
    }

    #[test]
    fn frontmatter_at_eof_without_trailing_newline() {
        let source = "---\ntitle: test\n---";
        let result =
            parse_frontmatter(source, &default_config()).expect("should parse successfully");
        let fm = result
            .frontmatter
            .expect("should parse frontmatter at EOF without trailing newline");
        assert!(fm.backlinks.is_empty(), "should have empty backlinks");
    }

    #[test]
    fn empty_backlinks_list() {
        let source = "---\nbacklinks:\n  superseded_by: []\n---\n";
        let result =
            parse_frontmatter(source, &default_config()).expect("should parse successfully");
        let fm = result.frontmatter.expect("should have frontmatter");
        assert_eq!(
            fm.backlinks.get("superseded_by"),
            Some(&vec![]),
            "empty list should parse as empty vec"
        );
    }
}
