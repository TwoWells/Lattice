// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Completion context detection (decision 007, ticket integration 14).
//!
//! Completion fires mid-edit, when the construct under the cursor is usually
//! *incomplete* — `[text](./dir` has no closing `)`, so the parse tree carries
//! no `Link` node there. Detection therefore works on the line text up to the
//! cursor (the *prefix*), not the tree: it recognizes the open construct the
//! cursor sits inside and reports which completion surface applies plus the
//! partial token typed so far.
//!
//! The tree still gates the result — the server suppresses completion when the
//! cursor is inside a code span, code block, or math node — but the surface and
//! the partial are decided here, by pure string analysis. That keeps this logic
//! independently testable without constructing a workspace.

/// The completion surface detected at the cursor, with the partial token typed
/// so far (the text from the construct's open to the cursor).
#[derive(Debug, PartialEq, Eq)]
pub enum Context<'a> {
    /// Inside a link destination (`[x](./…`). `partial` is the whole
    /// destination text typed so far, directory components included.
    Path {
        /// Destination text between `](` and the cursor.
        partial: &'a str,
    },
    /// After `#` in a destination (`target.md#…`) or an in-doc link (`(#…`).
    Fragment {
        /// The destination path before the `#` (empty for an in-doc `#`).
        target: &'a str,
        /// Fragment text after the `#`.
        partial: &'a str,
    },
    /// Inside the title-text string of a link (`[x](url "…`), where predicates
    /// live. `target` is the destination URL, so the server can suppress
    /// predicates on links that don't take one (external, non-markdown).
    Predicate {
        /// The destination URL before the title quote.
        target: &'a str,
        /// Title text after the opening quote.
        partial: &'a str,
    },
    /// Inside a reference link's label — full (`[text][…`) or shortcut (`[…`).
    ReferenceLabel {
        /// Label text after the opening `[`.
        partial: &'a str,
    },
    /// After `[^…`, a footnote reference.
    Footnote {
        /// Footnote label after the `^`.
        partial: &'a str,
    },
}

/// Detect the completion context from the line text up to the cursor.
///
/// Returns `None` in prose and anywhere outside a recognized construct, so the
/// server emits no completions there. An open link destination is matched
/// first (it can nest a `[` inside a reference label), then the innermost
/// unclosed `[`.
#[must_use]
pub fn detect(prefix: &str) -> Option<Context<'_>> {
    detect_destination(prefix).or_else(|| detect_bracket(prefix))
}

/// Detect a path, fragment, or predicate context inside an open link
/// destination — the last `](` with no `)` after it.
fn detect_destination(prefix: &str) -> Option<Context<'_>> {
    let open = prefix.rfind("](")? + 2;
    let dest = &prefix[open..];

    // A `)` closes the destination: the cursor is past the link, not in it.
    if dest.contains(')') {
        return None;
    }

    // A title string opens with `"`; predicates live inside it. An already
    // closed title (a second `"`) means the cursor sits past the title — no
    // completion.
    if let Some(quote) = dest.find('"') {
        let after_quote = &dest[quote + 1..];
        if after_quote.contains('"') {
            return None;
        }
        return Some(Context::Predicate {
            target: dest[..quote].trim(),
            partial: after_quote,
        });
    }

    // Without a quote, whitespace means the cursor has left the URL for the
    // (unquoted, so un-completable) title area.
    if dest.contains(char::is_whitespace) {
        return None;
    }

    // A `#` splits the destination into a path target and a fragment.
    if let Some(hash) = dest.find('#') {
        return Some(Context::Fragment {
            target: &dest[..hash],
            partial: &dest[hash + 1..],
        });
    }

    Some(Context::Path { partial: dest })
}

/// Detect a reference-label or footnote context from the innermost unclosed
/// `[` in the prefix.
fn detect_bracket(prefix: &str) -> Option<Context<'_>> {
    let open = innermost_unclosed_bracket(prefix)?;
    let after = &prefix[open + 1..];

    // `[^…` is a footnote; full reference (`[text][label`) and shortcut
    // (`[label`) both complete the document's defined reference labels.
    Some(
        after
            .strip_prefix('^')
            .map_or(Context::ReferenceLabel { partial: after }, |label| {
                Context::Footnote { partial: label }
            }),
    )
}

/// Byte index of the innermost (rightmost) `[` left unclosed by a later `]`.
fn innermost_unclosed_bracket(prefix: &str) -> Option<usize> {
    let mut stack: Vec<usize> = Vec::new();
    for (i, b) in prefix.bytes().enumerate() {
        match b {
            b'[' => stack.push(i),
            b']' => {
                stack.pop();
            }
            _ => {}
        }
    }
    stack.pop()
}

#[cfg(test)]
mod tests {
    use super::{Context, detect};

    #[test]
    fn prose_has_no_context() {
        assert_eq!(detect(""), None, "empty prefix is not a completion site");
        assert_eq!(
            detect("just some prose here"),
            None,
            "plain prose is not a completion site"
        );
    }

    #[test]
    fn empty_destination_is_path() {
        assert_eq!(
            detect("[link]("),
            Some(Context::Path { partial: "" }),
            "an open destination with no text completes paths"
        );
    }

    #[test]
    fn partial_destination_is_path() {
        assert_eq!(
            detect("[link](./docs/gui"),
            Some(Context::Path {
                partial: "./docs/gui"
            }),
            "destination text is reported whole, directories included"
        );
    }

    #[test]
    fn image_destination_is_path() {
        assert_eq!(
            detect("![alt](img/lo"),
            Some(Context::Path { partial: "img/lo" }),
            "image destinations complete paths like link destinations"
        );
    }

    #[test]
    fn closed_destination_is_not_a_context() {
        assert_eq!(
            detect("[link](done.md)"),
            None,
            "a closed `)` ends the destination context"
        );
        assert_eq!(
            detect("[link](done.md) and more prose"),
            None,
            "text after a closed link is prose"
        );
    }

    #[test]
    fn hash_in_destination_is_fragment() {
        assert_eq!(
            detect("[link](other.md#"),
            Some(Context::Fragment {
                target: "other.md",
                partial: ""
            }),
            "`#` after a path opens a fragment against that target"
        );
        assert_eq!(
            detect("[link](other.md#sec"),
            Some(Context::Fragment {
                target: "other.md",
                partial: "sec"
            }),
            "fragment partial is the text after `#`"
        );
    }

    #[test]
    fn in_doc_hash_is_current_document_fragment() {
        assert_eq!(
            detect("[link](#"),
            Some(Context::Fragment {
                target: "",
                partial: ""
            }),
            "`(#` targets the current document (empty target)"
        );
        assert_eq!(
            detect("[link](#cont"),
            Some(Context::Fragment {
                target: "",
                partial: "cont"
            }),
            "in-doc fragment carries its partial"
        );
    }

    #[test]
    fn open_title_is_predicate() {
        assert_eq!(
            detect("[link](target.md \""),
            Some(Context::Predicate {
                target: "target.md",
                partial: ""
            }),
            "an open title quote opens predicate completion"
        );
        assert_eq!(
            detect("[link](target.md \"sup"),
            Some(Context::Predicate {
                target: "target.md",
                partial: "sup"
            }),
            "predicate partial is the text after the opening quote, target carried"
        );
    }

    #[test]
    fn closed_title_is_not_a_context() {
        assert_eq!(
            detect("[link](target.md \"supersedes\""),
            None,
            "a closed title quote ends the predicate context"
        );
    }

    #[test]
    fn unquoted_whitespace_after_url_is_not_a_context() {
        assert_eq!(
            detect("[link](target.md "),
            None,
            "whitespace with no opening quote is the un-completable title area"
        );
    }

    #[test]
    fn full_reference_label() {
        assert_eq!(
            detect("[text]["),
            Some(Context::ReferenceLabel { partial: "" }),
            "the second bracket of a full reference completes labels"
        );
        assert_eq!(
            detect("[text][la"),
            Some(Context::ReferenceLabel { partial: "la" }),
            "reference label partial is the text after `][`"
        );
    }

    #[test]
    fn shortcut_reference_label() {
        assert_eq!(
            detect("[la"),
            Some(Context::ReferenceLabel { partial: "la" }),
            "a lone open bracket completes shortcut reference labels"
        );
    }

    #[test]
    fn footnote_label() {
        assert_eq!(
            detect("[^"),
            Some(Context::Footnote { partial: "" }),
            "`[^` opens footnote completion"
        );
        assert_eq!(
            detect("text[^no"),
            Some(Context::Footnote { partial: "no" }),
            "footnote partial is the text after `^`"
        );
    }

    #[test]
    fn destination_wins_over_inner_bracket() {
        // The cursor is in the inner link's destination; the outer `[` is
        // irrelevant.
        assert_eq!(
            detect("[outer [inner]("),
            Some(Context::Path { partial: "" }),
            "an open destination is matched before any unclosed bracket"
        );
    }

    #[test]
    fn reference_after_closed_link() {
        assert_eq!(
            detect("[a](b.md) then [re"),
            Some(Context::ReferenceLabel { partial: "re" }),
            "a fresh open bracket after a closed link completes labels"
        );
    }
}
