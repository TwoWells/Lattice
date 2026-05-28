// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! YAML frontmatter parsing for backlink extraction.
//!
//! Detects `---` delimited frontmatter at the start of a markdown file,
//! parses the `backlinks` section, and validates inverse predicates against
//! the configured vocabulary. Uses the span-aware YAML parser from
//! [`crate::yaml`] instead of `serde_yaml_ng`.

use std::collections::HashMap;
use std::ops::Range;

use crate::config::Config;
use crate::yaml;

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

/// Parse frontmatter from a markdown document and validate backlinks.
///
/// Returns a [`FrontmatterResult`] containing the parsed frontmatter (if any)
/// and diagnostics for unknown inverse predicates.
///
/// # Errors
///
/// Returns [`FrontmatterError`] if the frontmatter block contains fatal YAML
/// errors (currently the new parser recovers from all errors via diagnostics,
/// so this only fires for internal consistency issues).
pub fn parse_frontmatter(
    source: &str,
    config: &Config,
) -> Result<FrontmatterResult, FrontmatterError> {
    let Some(block) = yaml::parse_frontmatter_block(source) else {
        return Ok(FrontmatterResult {
            frontmatter: None,
            diagnostics: Vec::new(),
        });
    };

    // Check for hard parse errors that should be surfaced as FrontmatterError.
    for diag in &block.diagnostics {
        if diag.severity == yaml::YamlSeverity::Error {
            let line = byte_offset_to_line(source, diag.span.start);
            return Err(FrontmatterError::InvalidYaml {
                line,
                message: diag.message.clone(),
            });
        }
    }

    let byte_range: Range<usize> = block.span.into();
    let start_line = 1;
    let end_byte = byte_range.end;
    let newline_count = source[..end_byte.min(source.len())]
        .bytes()
        .filter(|&b| b == b'\n')
        .count();
    let end_line =
        newline_count + usize::from(!source[..end_byte.min(source.len())].ends_with('\n'));

    let backlinks = yaml::extract_backlinks(&block, source);

    let mut diagnostics = Vec::new();
    for predicate in backlinks.keys() {
        if !config.is_known_inverse(predicate) {
            let line = yaml::find_predicate_line(&block, predicate, source);
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

/// Convert a byte offset to a 1-based line number.
fn byte_offset_to_line(source: &str, offset: usize) -> usize {
    source[..offset.min(source.len())]
        .bytes()
        .filter(|&b| b == b'\n')
        .count()
        + 1
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
            matches!(err, FrontmatterError::InvalidYaml { line: 2, .. }),
            "error should report line 2 (the malformed line), got: {err:?}"
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
