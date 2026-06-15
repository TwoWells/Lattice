// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Format-neutral frontmatter types and helpers.
//!
//! Defines the shared types consumed by tree construction, backlink
//! extraction, and predicate validation. The YAML, TOML, and JSON
//! frontmatter parsers all produce these types.

use crate::span::Span;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Parsed frontmatter block with span information.
#[derive(Debug)]
pub struct FrontmatterBlock {
    /// Full range including delimiters (`---`, `+++`, or `{`...`}`).
    pub span: Span,
    /// Content between delimiters.
    #[allow(dead_code, reason = "used by tree construction ticket 06a")]
    pub content_span: Span,
    /// Top-level entries.
    pub entries: Vec<FmNode>,
    /// Parse diagnostics (errors and warnings).
    pub diagnostics: Vec<FmDiagnostic>,
}

/// A node in the frontmatter tree.
#[derive(Debug)]
pub enum FmNode {
    /// A key-value mapping entry.
    Mapping {
        /// The mapping key.
        key: ScalarSpan,
        /// The mapping value.
        value: FmValue,
        /// Span covering the full key-value pair.
        span: Span,
    },
    /// A sequence item (`- value` in YAML, array element in TOML).
    SequenceItem {
        /// The item value.
        value: FmValue,
        /// Span covering the item.
        #[allow(dead_code, reason = "used by tree construction ticket 06a")]
        span: Span,
    },
}

/// A frontmatter value.
#[derive(Debug)]
pub enum FmValue {
    /// A scalar value (plain, quoted, or null).
    Scalar(ScalarSpan),
    /// A block sequence (list of `FmNode::SequenceItem`).
    Sequence(Vec<FmNode>),
    /// A block mapping (list of `FmNode::Mapping`).
    Mapping(Vec<FmNode>),
    /// An inline flow sequence (`[a, b, c]`).
    FlowSequence {
        /// Span of the entire flow sequence including brackets.
        #[allow(dead_code, reason = "used by tree construction ticket 06a")]
        span: Span,
        /// Scalar items.
        items: Vec<ScalarSpan>,
    },
    /// An inline flow mapping (`{a: b, c: d}`).
    FlowMapping {
        /// Span of the entire flow mapping including braces.
        #[allow(dead_code, reason = "used by tree construction ticket 06a")]
        span: Span,
        /// Key-value pairs.
        #[allow(dead_code, reason = "used by tree construction ticket 06a")]
        entries: Vec<(ScalarSpan, ScalarSpan)>,
    },
    /// A block scalar (literal `|` or folded `>`).
    BlockScalar {
        /// Span of the entire block scalar content.
        #[allow(dead_code, reason = "used by tree construction ticket 06a")]
        span: Span,
    },
}

/// A scalar with its source span and resolved text.
#[derive(Debug)]
pub struct ScalarSpan {
    /// Byte range in the original source.
    pub span: Span,
    /// Resolved text content.
    pub text: String,
}

/// Severity of a frontmatter diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FmSeverity {
    /// A hard parse error.
    Error,
    /// A warning (e.g. unsupported feature that was skipped).
    #[allow(dead_code, reason = "used by structural diagnostics ticket 07")]
    Warning,
}

/// A diagnostic emitted during frontmatter parsing.
#[derive(Debug)]
pub struct FmDiagnostic {
    /// Location in the source.
    pub span: Span,
    /// Severity level.
    pub severity: FmSeverity,
    /// Human-readable message.
    pub message: String,
}

// ---------------------------------------------------------------------------
// BOM stripping
// ---------------------------------------------------------------------------

/// UTF-8 byte order mark.
pub const BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

/// Strip a UTF-8 BOM at byte 0, returning the remainder and the byte offset.
pub fn strip_bom(source: &str) -> (&str, usize) {
    if source.as_bytes().starts_with(BOM) {
        (&source[3..], 3)
    } else {
        (source, 0)
    }
}

// ---------------------------------------------------------------------------
// UTF-8 decoding for byte-oriented scanners
// ---------------------------------------------------------------------------

/// Number of bytes in the UTF-8 sequence introduced by lead byte `lead`.
///
/// Returns 1 for ASCII bytes (and, defensively, for stray continuation
/// bytes that cannot legally appear in valid UTF-8 input).
const fn utf8_seq_len(lead: u8) -> usize {
    match lead {
        0xF0..=0xF7 => 4,
        0xE0..=0xEF => 3,
        0xC0..=0xDF => 2,
        _ => 1,
    }
}

/// Append the whole UTF-8 character beginning at `bytes[start]` to `text` and
/// return the index just past it.
///
/// The byte-at-a-time frontmatter scanners would otherwise push each byte as
/// its own `char`, turning a multi-byte character (e.g. a CJK key) into Latin-1
/// mojibake. Callers pass `bytes` from a `&str`, so the sequence is always
/// valid and complete; an unexpected truncation degrades to the replacement
/// character rather than panicking.
pub fn push_utf8_char(text: &mut String, bytes: &[u8], start: usize) -> usize {
    let lead = bytes[start];
    if lead.is_ascii() {
        text.push(char::from(lead));
        return start + 1;
    }
    let end = (start + utf8_seq_len(lead)).min(bytes.len());
    match std::str::from_utf8(&bytes[start..end]) {
        Ok(s) => text.push_str(s),
        Err(_) => text.push(char::REPLACEMENT_CHARACTER),
    }
    end
}

// ---------------------------------------------------------------------------
// Backlink extraction helper
// ---------------------------------------------------------------------------

/// Extract backlinks from a parsed frontmatter block.
///
/// Walks the tree looking for a top-level `backlinks` key whose value
/// is a mapping of predicate → list of paths. Returns the backlinks map
/// and any entries that don't match the expected shape.
pub fn extract_backlinks(
    block: &FrontmatterBlock,
    source: &str,
) -> std::collections::HashMap<String, Vec<String>> {
    let mut backlinks = std::collections::HashMap::new();

    for entry in &block.entries {
        if let FmNode::Mapping { key, value, .. } = entry {
            if key.text != "backlinks" {
                continue;
            }

            let FmValue::Mapping(predicates) = value else {
                break;
            };

            for pred_entry in predicates {
                let FmNode::Mapping {
                    key: pred_key,
                    value: pred_value,
                    ..
                } = pred_entry
                else {
                    continue;
                };

                let mut paths = Vec::new();

                match pred_value {
                    FmValue::Sequence(items) => {
                        for item in items {
                            if let FmNode::SequenceItem {
                                value: FmValue::Scalar(s),
                                ..
                            } = item
                            {
                                paths.push(s.text.clone());
                            }
                        }
                    }
                    FmValue::FlowSequence { items, .. } => {
                        for item in items {
                            paths.push(item.text.clone());
                        }
                    }
                    _ => {}
                }

                backlinks.insert(pred_key.text.clone(), paths);
            }

            break;
        }
    }

    let _ = source; // reserved for future span-based extraction
    backlinks
}

// ---------------------------------------------------------------------------
// Exception extraction helper (issue 031, decision 011)
// ---------------------------------------------------------------------------

/// A path-shaped lint that an `exceptions` block may namespace over.
///
/// Exceptions apply only to the path-shaped lints — the 028 family
/// (issue 031, decision 011); they are never a graph edge and impose no
/// backlink obligation. The two variants are the two namespaces accepted under
/// the `exceptions` frontmatter key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExceptionLint {
    /// Excepts the `stale_references` (dangling `.md`) diagnostic on a
    /// reference — including a leading `{Name}/…` external alias.
    StaleReferences,
    /// Excepts the `bare_paths` (resolving make-it-a-link / quoted / bare)
    /// nudge on a reference.
    BarePaths,
}

impl ExceptionLint {
    /// The frontmatter namespace key (`exceptions.<lint>`) for this lint.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::StaleReferences => "stale_references",
            Self::BarePaths => "bare_paths",
        }
    }

    /// The plural human-readable noun for this lint, used in the count-key drift
    /// message (`expected N <noun> here, found M` — issue 036).
    #[must_use]
    pub const fn noun(self) -> &'static str {
        match self {
            Self::StaleReferences => "stale references",
            Self::BarePaths => "bare paths",
        }
    }
}

/// A single `exceptions.<lint>` entry: a reference, its declared reason, and the
/// source position of the key (for reconciliation diagnostics).
///
/// The reason is an epitaph — the only surviving record of a vanished
/// reference's intent — so the entry retains the key's span and line to anchor
/// the unused-exception and empty-reason diagnostics at the offending key
/// (decision 011).
#[derive(Debug, Clone)]
pub struct ExceptionEntry {
    /// The literal reference string keyed in the frontmatter — matched
    /// verbatim against a diagnostic's reference, including any leading
    /// `{Name}/…` external alias (issue 031).
    pub reference: String,
    /// The declared reason. Empty when the value was missing, blank, or not a
    /// scalar — which is itself a diagnostic (required-reason).
    pub reason: String,
    /// Byte span of the key token in the source.
    pub key_span: Span,
    /// 1-based line of the key token in the source.
    pub line: usize,
}

/// A per-document **count-key** under an `exceptions.<lint>` namespace (issue
/// 036, decision 012).
///
/// An all-digits key (shape `^[0-9]+$`, e.g. `31` or a quoted `"31"`) is a
/// *count sentinel*, not a literal reference. It claims the lint's **residual**
/// — the live diagnostics of that lint in the document minus those already
/// suppressed by literal-path keys. When the residual count equals
/// [`expected`](Self::expected) the whole residual is suppressed under the
/// single shared [`reason`](Self::reason); when it drifts the sentinel goes
/// inert and a drift warning is anchored at [`key_span`](Self::key_span).
///
/// No real reference is named `31`, so the shape alone disambiguates: a
/// path-shaped key (with a name, slash, or `#`) is always a literal reference.
#[derive(Debug, Clone)]
pub struct CountKey {
    /// The expected residual count `N` (`N >= 1`; parsed from the all-digits
    /// key). A key whose digits overflow `usize` is clamped to [`usize::MAX`],
    /// which no real residual will reach, so it reads as a permanent drift.
    pub expected: usize,
    /// The declared shared reason — the document-level epitaph. Empty when the
    /// value was missing, blank, or not a scalar, which is itself diagnosed
    /// (required-reason), exactly like a literal exception.
    pub reason: String,
    /// Byte span of the all-digits key token in the source (anchors the
    /// empty-reason and drift diagnostics).
    pub key_span: Span,
    /// 1-based line of the key token in the source.
    pub line: usize,
    /// The key text exactly as written (e.g. `31`), for the drift / ledger
    /// messages.
    pub raw: String,
}

/// Whether `key` is a count-key by shape — one or more ASCII digits and nothing
/// else (`^[0-9]+$`, issue 036).
///
/// A literal reference is always path-shaped (it has a name, a slash, or a
/// fragment), so an all-digits key is unambiguously the count sentinel; `31` and
/// a quoted `"31"` both match, while `31.md` and `a/31` do not.
#[must_use]
pub fn is_count_key(key: &str) -> bool {
    !key.is_empty() && key.bytes().all(|b| b.is_ascii_digit())
}

/// Parsed `exceptions` frontmatter block (issue 031, decision 011; issue 036,
/// decision 012).
///
/// Sibling to `backlinks`, lint-namespaced. A path-shaped key is a literal
/// reference paired with its reason ([`ExceptionEntry`]); an all-digits key is
/// the per-document count sentinel ([`CountKey`]). Entries preserve source order
/// and retain per-key positions so reconciliation can anchor diagnostics at the
/// offending key.
#[derive(Debug, Default)]
pub struct Exceptions {
    /// Literal-reference entries under `exceptions.stale_references`.
    pub stale_references: Vec<ExceptionEntry>,
    /// Literal-reference entries under `exceptions.bare_paths`.
    pub bare_paths: Vec<ExceptionEntry>,
    /// The count-key under `exceptions.stale_references`, if one was declared.
    pub stale_references_count: Option<CountKey>,
    /// The count-key under `exceptions.bare_paths`, if one was declared.
    pub bare_paths_count: Option<CountKey>,
}

impl Exceptions {
    /// The literal-reference entries declared for `lint`.
    #[must_use]
    pub fn entries(&self, lint: ExceptionLint) -> &[ExceptionEntry] {
        match lint {
            ExceptionLint::StaleReferences => &self.stale_references,
            ExceptionLint::BarePaths => &self.bare_paths,
        }
    }

    /// The count-key declared for `lint`, if any.
    #[must_use]
    pub fn count_key(&self, lint: ExceptionLint) -> Option<&CountKey> {
        match lint {
            ExceptionLint::StaleReferences => self.stale_references_count.as_ref(),
            ExceptionLint::BarePaths => self.bare_paths_count.as_ref(),
        }
    }
}

/// Extract the `exceptions` block from a parsed frontmatter block.
///
/// Walks for a top-level `exceptions` key whose value is a mapping of lint name
/// (`stale_references` / `bare_paths`) → mapping of literal reference → reason.
/// Reuses the same machinery as [`extract_backlinks`] (decision 011: `exceptions`
/// is a sibling block in the same frontmatter), retaining each key's span and
/// line so reconciliation can point a diagnostic at the offending entry.
///
/// A namespace whose value is not a mapping is skipped; an entry whose value is
/// not a scalar yields an empty reason (the required-reason diagnostic fires on
/// it downstream). Lint namespaces other than the two recognized ones are
/// ignored — they cannot name a path-shaped lint, so they carry no obligation.
///
/// An all-digits key (`^[0-9]+$`, issue 036) is the per-document count sentinel
/// rather than a literal reference: it is parsed into the namespace's
/// [`CountKey`] slot (the first one wins; at most one sentinel per namespace),
/// not the literal-entry bucket. Every other key remains a literal reference.
#[must_use]
pub fn extract_exceptions(block: &FrontmatterBlock, source: &str) -> Exceptions {
    let mut exceptions = Exceptions::default();

    for entry in &block.entries {
        let FmNode::Mapping { key, value, .. } = entry else {
            continue;
        };
        if key.text != "exceptions" {
            continue;
        }
        let FmValue::Mapping(namespaces) = value else {
            break;
        };

        for ns_entry in namespaces {
            let FmNode::Mapping {
                key: ns_key,
                value: ns_value,
                ..
            } = ns_entry
            else {
                continue;
            };
            let lint = match ns_key.text.as_str() {
                "stale_references" => ExceptionLint::StaleReferences,
                "bare_paths" => ExceptionLint::BarePaths,
                _ => continue,
            };
            let FmValue::Mapping(refs) = ns_value else {
                continue;
            };

            for ref_entry in refs {
                let FmNode::Mapping {
                    key: ref_key,
                    value: ref_value,
                    ..
                } = ref_entry
                else {
                    continue;
                };
                let reason = match ref_value {
                    FmValue::Scalar(s) => s.text.clone(),
                    _ => String::new(),
                };
                let key_line = byte_offset_to_line(source, ref_key.span.start);

                // Discriminate by shape (issue 036): an all-digits key is the
                // count sentinel; everything else is a literal reference.
                if is_count_key(&ref_key.text) {
                    let count_slot = match lint {
                        ExceptionLint::StaleReferences => &mut exceptions.stale_references_count,
                        ExceptionLint::BarePaths => &mut exceptions.bare_paths_count,
                    };
                    // At most one sentinel per namespace — the first one wins.
                    if count_slot.is_none() {
                        *count_slot = Some(CountKey {
                            expected: ref_key.text.parse().unwrap_or(usize::MAX),
                            reason,
                            key_span: ref_key.span,
                            line: key_line,
                            raw: ref_key.text.clone(),
                        });
                    }
                    continue;
                }

                let bucket = match lint {
                    ExceptionLint::StaleReferences => &mut exceptions.stale_references,
                    ExceptionLint::BarePaths => &mut exceptions.bare_paths,
                };
                bucket.push(ExceptionEntry {
                    reference: ref_key.text.clone(),
                    reason,
                    key_span: ref_key.span,
                    line: key_line,
                });
            }
        }

        break;
    }

    exceptions
}

/// Find the 1-based line number for a top-level key in the frontmatter.
///
/// Searches for the `backlinks` → predicate key and returns its line
/// number in the original source.
pub fn find_predicate_line(block: &FrontmatterBlock, predicate: &str, source: &str) -> usize {
    for entry in &block.entries {
        if let FmNode::Mapping { key, value, .. } = entry {
            if key.text != "backlinks" {
                continue;
            }

            let FmValue::Mapping(predicates) = value else {
                break;
            };

            for pred_entry in predicates {
                if let FmNode::Mapping { key: pred_key, .. } = pred_entry
                    && pred_key.text == predicate
                {
                    return byte_offset_to_line(source, pred_key.span.start);
                }
            }
        }
    }

    // Fallback: line 1 (the opening delimiter).
    1
}

// ---------------------------------------------------------------------------
// Line counting (shared by every line-number computation in the crate)
// ---------------------------------------------------------------------------

/// Count line breaks in `bytes`, treating `\n`, `\r\n`, and bare `\r` each as
/// a single break.
///
/// Counting the two bytes of a `\r\n` pair separately would double every
/// Windows line ending; ignoring bare `\r` would miss legacy-Mac breaks. This
/// is the single source of truth so diagnostics, LSP positions, and folding
/// ranges all agree regardless of ending style.
pub fn count_line_breaks(bytes: &[u8]) -> usize {
    let mut count = 0;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                count += 1;
                i += 1;
            }
            b'\r' => {
                count += 1;
                i += if bytes.get(i + 1) == Some(&b'\n') {
                    2
                } else {
                    1
                };
            }
            _ => i += 1,
        }
    }
    count
}

/// Number of lines in `source`, recognizing `\n`, `\r\n`, and bare `\r`.
///
/// Matches `str::lines().count()` semantics — a trailing line break does not
/// add an empty final line — while also splitting on bare `\r`.
pub fn line_count(source: &str) -> usize {
    let bytes = source.as_bytes();
    let breaks = count_line_breaks(bytes);
    if bytes.is_empty() || matches!(bytes.last(), Some(b'\n' | b'\r')) {
        breaks
    } else {
        breaks + 1
    }
}

/// Convert a byte offset to a 1-based line number, recognizing `\n`, `\r\n`,
/// and bare `\r`.
pub fn byte_offset_to_line(source: &str, offset: usize) -> usize {
    count_line_breaks(&source.as_bytes()[..offset.min(source.len())]) + 1
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for clarity")]
mod tests {
    use super::{ExceptionLint, extract_exceptions, is_count_key};
    use crate::yaml::parse_frontmatter_block;

    #[test]
    fn extract_exceptions_both_namespaces() {
        let source = "---\nexceptions:\n  stale_references:\n    \"a.md\": \"reason a\"\n  bare_paths:\n    \"b.md\": \"reason b\"\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        let ex = extract_exceptions(&block, source);
        assert_eq!(
            ex.stale_references.len(),
            1,
            "one stale_references entry: {ex:?}"
        );
        assert_eq!(ex.bare_paths.len(), 1, "one bare_paths entry: {ex:?}");
        assert_eq!(
            ex.stale_references[0].reference, "a.md",
            "stale key is the reference: {ex:?}"
        );
        assert_eq!(
            ex.stale_references[0].reason, "reason a",
            "stale reason is the value: {ex:?}"
        );
        assert_eq!(
            ex.entries(ExceptionLint::BarePaths)[0].reference,
            "b.md",
            "entries() returns the bare_paths bucket: {ex:?}"
        );
    }

    #[test]
    fn extract_exceptions_empty_reason_retained() {
        // An empty/missing reason is retained as an empty string; the
        // required-reason diagnostic fires on it downstream.
        let source = "---\nexceptions:\n  stale_references:\n    \"a.md\": \"\"\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        let ex = extract_exceptions(&block, source);
        assert_eq!(ex.stale_references.len(), 1, "one entry parsed: {ex:?}");
        assert!(
            ex.stale_references[0].reason.is_empty(),
            "the empty reason is retained: {ex:?}"
        );
    }

    #[test]
    fn extract_exceptions_unknown_namespace_ignored() {
        // A lint namespace that names no path-shaped lint is ignored — it
        // carries no obligation.
        let source = "---\nexceptions:\n  not_a_lint:\n    \"a.md\": \"r\"\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        let ex = extract_exceptions(&block, source);
        assert!(
            ex.stale_references.is_empty() && ex.bare_paths.is_empty(),
            "an unknown lint namespace yields no entries: {ex:?}"
        );
    }

    #[test]
    fn extract_exceptions_records_key_line() {
        // The key's 1-based line is retained for anchoring reconciliation
        // diagnostics. `a.md` sits on line 4 (after `---`, `exceptions:`,
        // `stale_references:`).
        let source = "---\nexceptions:\n  stale_references:\n    \"a.md\": \"r\"\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        let ex = extract_exceptions(&block, source);
        assert_eq!(
            ex.stale_references[0].line, 4,
            "the key's line is recorded: {ex:?}"
        );
    }

    #[test]
    fn is_count_key_discriminates_by_shape() {
        // An all-digits key is the sentinel; any path-shaped key (name, slash,
        // or fragment) is a literal reference (issue 036).
        assert!(is_count_key("31"), "all-digits is a count key");
        assert!(is_count_key("0"), "a single digit is all-digits");
        assert!(!is_count_key("31.md"), "a `.md` name is a literal ref");
        assert!(!is_count_key("a/31"), "a slashed path is a literal ref");
        assert!(!is_count_key("3a"), "a trailing letter is a literal ref");
        assert!(!is_count_key(""), "the empty string is not a count key");
        assert!(
            !is_count_key("#31"),
            "a fragment-shaped key is a literal ref"
        );
    }

    #[test]
    fn extract_exceptions_count_key_parsed_into_sentinel_slot() {
        // An all-digits key lands in the count-key slot, not the literal
        // bucket, carrying its parsed N, reason, and span (issue 036).
        let source =
            "---\nexceptions:\n  stale_references:\n    \"31\": \"migration table\"\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        let ex = extract_exceptions(&block, source);
        assert!(
            ex.stale_references.is_empty(),
            "the count key is not a literal entry: {ex:?}"
        );
        let count = ex
            .count_key(ExceptionLint::StaleReferences)
            .expect("count key present");
        assert_eq!(count.expected, 31, "N is parsed from the key: {ex:?}");
        assert_eq!(
            count.reason, "migration table",
            "reason is the value: {ex:?}"
        );
        assert_eq!(count.raw, "31", "raw key text is retained: {ex:?}");
    }

    #[test]
    fn extract_exceptions_count_key_and_literal_compose() {
        // A literal key and a count key coexist in one namespace: the literal
        // lands in the bucket, the all-digits key in the sentinel slot.
        let source = "---\nexceptions:\n  stale_references:\n    \"a.md\": \"literal\"\n    \"31\": \"count\"\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        let ex = extract_exceptions(&block, source);
        assert_eq!(
            ex.stale_references.len(),
            1,
            "only the literal key is an entry: {ex:?}"
        );
        assert_eq!(
            ex.stale_references[0].reference, "a.md",
            "the literal key is the path: {ex:?}"
        );
        assert!(
            ex.count_key(ExceptionLint::StaleReferences).is_some(),
            "the all-digits key is the sentinel: {ex:?}"
        );
    }

    #[test]
    fn extract_exceptions_count_key_first_wins() {
        // At most one sentinel per namespace — the first all-digits key wins.
        let source =
            "---\nexceptions:\n  bare_paths:\n    \"3\": \"first\"\n    \"7\": \"second\"\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        let ex = extract_exceptions(&block, source);
        let count = ex
            .count_key(ExceptionLint::BarePaths)
            .expect("count key present");
        assert_eq!(count.expected, 3, "the first sentinel wins: {ex:?}");
    }

    #[test]
    fn extract_exceptions_absent_block_is_empty() {
        let source = "---\ntitle: test\n---\n";
        let block = parse_frontmatter_block(source).expect("should parse");
        let ex = extract_exceptions(&block, source);
        assert!(
            ex.stale_references.is_empty() && ex.bare_paths.is_empty(),
            "no exceptions block yields empty: {ex:?}"
        );
    }
}
