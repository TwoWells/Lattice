// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Fragment resolution — the one answer to "what does `#x` name in this
//! document?" (issue 072).
//!
//! Three surfaces ask that question, and an answer that differs between them is
//! a defect in whichever one disagrees:
//!
//! - the **diagnostic** ([`crate::validation`]'s fragment check) — does `#x`
//!   resolve at all, or is it drift?
//! - the **rename engine** ([`crate::mv`], issue 057) — *which* heading does
//!   `#x` name, so a rename retargets exactly its own referrers and no others?
//! - **`find_references`** ([`crate::server`]) — which referrers name *this*
//!   heading?
//!
//! All three route through [`resolve`] here, so a future slug policy or anchor
//! form cannot land in two of them and miss the third.
//!
//! # The resolution order
//!
//! 1. The HTML top-of-document idioms — an empty fragment (`#`) and `#top`
//!    (ASCII case-insensitive) — scroll to the top regardless of headings, so
//!    they resolve to [`Target::Top`] and never to a heading.
//! 2. A heading whose anchor under an *eligible* [`SlugForm`] equals the
//!    fragment; first match in document order wins, as a renderer resolves
//!    duplicate ids. Which forms are eligible is what `[policy] fragments`
//!    pins — a repo that names one slug algorithm has declared which spellings
//!    are real, and the others are not coordinates at all.
//! 3. An explicit raw-HTML anchor (`<a id="x">`, `<a name="x">`, any element
//!    bearing `id="x"` — issue 025) defined anywhere in the document.

use crate::block::{self, Anchor, Heading, HeadingId};
use crate::config::FragmentAlgorithm;

/// Which anchor form a fragment matched a heading through — the coordinate's
/// "spelling style" on the fragment axis, the analogue of a path's style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlugForm {
    /// The heading's explicit `{#id}` attribute.
    Explicit,
    /// The computed GitHub slug.
    Github,
    /// The computed GitLab slug.
    Gitlab,
    /// The computed VS Code slug.
    Vscode,
}

impl SlugForm {
    /// The forms a fragment may resolve through under `algorithm`, in
    /// resolution order.
    ///
    /// An explicit `{#id}` is always eligible — it is not a computed slug, so
    /// no algorithm gates it. The computed forms are gated by the
    /// `[policy] fragments` pin: exactly one when pinned, all three when not
    /// (the default validates against any convention).
    #[must_use]
    pub fn eligible(algorithm: Option<FragmentAlgorithm>) -> &'static [Self] {
        match algorithm {
            Some(FragmentAlgorithm::Github) => &[Self::Explicit, Self::Github],
            Some(FragmentAlgorithm::Gitlab) => &[Self::Explicit, Self::Gitlab],
            Some(FragmentAlgorithm::Vscode) => &[Self::Explicit, Self::Vscode],
            None => &[Self::Explicit, Self::Github, Self::Gitlab, Self::Vscode],
        }
    }

    /// A heading's anchor under this form, or `None` when the heading has no
    /// such form (a computed heading has no explicit id, and an explicitly
    /// pinned one computes no slug).
    #[must_use]
    pub fn coordinate(self, heading: &Heading) -> Option<&str> {
        match (&heading.id, self) {
            (HeadingId::Explicit(id), Self::Explicit) => Some(id),
            (
                HeadingId::Computed {
                    github,
                    gitlab,
                    vscode,
                },
                form,
            ) => match form {
                Self::Github => Some(github),
                Self::Gitlab => Some(gitlab),
                Self::Vscode => Some(vscode),
                Self::Explicit => None,
            },
            (HeadingId::Explicit(_), _) => None,
        }
    }
}

/// What a fragment names in a document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// The top of the document — the `#` / `#top` idioms, which are always
    /// valid and are never a heading coordinate.
    Top,
    /// The heading at `index` in the document's heading list, matched through
    /// `form`.
    Heading {
        /// Position in the heading list (document order).
        index: usize,
        /// The eligible form the fragment matched through.
        form: SlugForm,
    },
    /// The explicit raw-HTML anchor at `index` in the document's anchor list.
    Anchor {
        /// Position in the anchor list (document order).
        index: usize,
    },
}

/// Whether `fragment` is one of the HTML top-of-document idioms.
fn is_top(fragment: &str) -> bool {
    fragment.is_empty() || fragment.eq_ignore_ascii_case("top")
}

/// Resolve `fragment` against one document's headings and explicit anchors.
///
/// `headings` and `anchors` are the target document's cached extractions, in
/// document order; `algorithm` is the effective `[policy] fragments` pin.
/// Returns `None` exactly when the fragment resolves to nothing — the
/// diagnostic's "not found".
#[must_use]
pub fn resolve(
    headings: &[Heading],
    anchors: &[Anchor],
    algorithm: Option<FragmentAlgorithm>,
    fragment: &str,
) -> Option<Target> {
    if is_top(fragment) {
        return Some(Target::Top);
    }
    if let Some((index, form)) = resolve_heading(headings, algorithm, fragment) {
        return Some(Target::Heading { index, form });
    }
    anchors
        .iter()
        .position(|anchor| anchor.id == fragment)
        .map(|index| Target::Anchor { index })
}

/// Whether `fragment` resolves to anything in the document — the fragment
/// diagnostic's predicate.
#[must_use]
pub fn resolves(
    headings: &[Heading],
    anchors: &[Anchor],
    algorithm: Option<FragmentAlgorithm>,
    fragment: &str,
) -> bool {
    resolve(headings, anchors, algorithm, fragment).is_some()
}

/// The index of the first heading `fragment` resolves to, with the form it
/// matched through.
///
/// Headings alone — the top-of-document idioms and explicit anchors are
/// [`resolve`]'s business. The rename engine needs this arm on its own: it
/// resolves against a document's pre- and post-rename heading lists, where an
/// anchor match means "not a heading coordinate, so not this rename's to move".
#[must_use]
pub fn resolve_heading(
    headings: &[Heading],
    algorithm: Option<FragmentAlgorithm>,
    fragment: &str,
) -> Option<(usize, SlugForm)> {
    headings.iter().enumerate().find_map(|(index, heading)| {
        SlugForm::eligible(algorithm)
            .iter()
            .find(|form| form.coordinate(heading) == Some(fragment))
            .map(|form| (index, *form))
    })
}

/// Whether `fragment` names the heading at `heading_index` — the reference
/// query's predicate.
///
/// This is [`resolve`] plus heading identity: a fragment that resolves to some
/// *other* heading is not a reference to this one, and a fragment that resolves
/// to nothing is drift, not an edge. An explicit anchor counts when it pins
/// this heading (see [`pinned_heading`]).
#[must_use]
pub fn names_heading(
    headings: &[Heading],
    anchors: &[Anchor],
    source: &str,
    algorithm: Option<FragmentAlgorithm>,
    fragment: &str,
    heading_index: usize,
) -> bool {
    match resolve(headings, anchors, algorithm, fragment) {
        Some(Target::Heading { index, .. }) => index == heading_index,
        Some(Target::Anchor { index }) => anchors.get(index).is_some_and(|anchor| {
            pinned_heading(headings, source, anchor.line) == Some(heading_index)
        }),
        // The top-of-document idioms address the document, not a heading — so
        // they are never a reference *to* one, even where a `# Top` heading
        // happens to exist. Both other authorities agree: the diagnostic waves
        // them through without consulting a heading, and the rename engine
        // never retargets them.
        Some(Target::Top) | None => false,
    }
}

/// The heading an explicit anchor on 1-based `anchor_line` pins, if any.
///
/// Issue 025's construct: an `<a id="x"></a>` placed immediately above a
/// heading gives it a short, stable fragment in place of a long, brittle slug.
/// An anchor pins the heading when it stands on the heading's own line, or on a
/// line above it separated only by blank lines. An anchor anywhere else — in
/// the middle of a section, or after the last heading — is a free-standing
/// in-page coordinate that belongs to no heading, so a reference to it is a
/// reference to that spot, not to a heading.
#[must_use]
pub fn pinned_heading(headings: &[Heading], source: &str, anchor_line: usize) -> Option<usize> {
    let (index, heading) = headings
        .iter()
        .enumerate()
        .find(|(_, heading)| heading.line >= anchor_line)?;
    if heading.line == anchor_line {
        return Some(index);
    }
    // `content_lines` is 0-based and matches the parser's own line counting, so
    // the lines strictly between the two 1-based numbers start at `anchor_line`.
    let between = heading.line - anchor_line - 1;
    block::content_lines(source)
        .skip(anchor_line)
        .take(between)
        .all(|line| line.trim().is_empty())
        .then_some(index)
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    reason = "tests use expect for clarity (AGENTS.md)"
)]
mod tests {
    use super::{SlugForm, Target, names_heading, pinned_heading, resolve, resolves};
    use crate::block::{Anchor, Heading, HeadingId, Syntax};
    use crate::config::FragmentAlgorithm;
    use crate::span::Span;

    fn computed(line: usize, github: &str, gitlab: &str, vscode: &str) -> Heading {
        Heading {
            line,
            level: 2,
            text: github.to_string(),
            id: HeadingId::Computed {
                github: github.to_string(),
                gitlab: gitlab.to_string(),
                vscode: vscode.to_string(),
            },
            text_span: Span::new(0, 0),
            syntax: Syntax::Markdown,
        }
    }

    fn explicit(line: usize, id: &str) -> Heading {
        Heading {
            line,
            level: 2,
            text: id.to_string(),
            id: HeadingId::Explicit(id.to_string()),
            text_span: Span::new(0, 0),
            syntax: Syntax::Markdown,
        }
    }

    #[test]
    fn pinned_algorithm_gates_the_computed_forms_but_not_explicit_ids() {
        assert_eq!(
            SlugForm::eligible(Some(FragmentAlgorithm::Gitlab)),
            &[SlugForm::Explicit, SlugForm::Gitlab],
            "a pin admits its own slug and the ungated explicit id, nothing else"
        );
        assert_eq!(
            SlugForm::eligible(None).len(),
            4,
            "unpinned admits the explicit id and all three conventions"
        );
    }

    #[test]
    fn resolution_honors_the_pin() {
        let headings = [computed(3, "héllo", "hllo", "héllo")];
        let anchors: [Anchor; 0] = [];

        assert!(
            resolves(&headings, &anchors, None, "hllo"),
            "unpinned, the gitlab spelling resolves"
        );
        assert!(
            !resolves(&headings, &anchors, Some(FragmentAlgorithm::Github), "hllo"),
            "pinned to github, the gitlab-only spelling is not a coordinate"
        );
        assert!(
            resolves(
                &headings,
                &anchors,
                Some(FragmentAlgorithm::Github),
                "héllo"
            ),
            "pinned to github, the github spelling resolves"
        );
    }

    #[test]
    fn heading_identity_is_first_match_in_document_order() {
        let headings = [
            computed(1, "dup", "dup", "dup"),
            computed(5, "dup-1", "dup-1", "dup-1"),
        ];
        let anchors: [Anchor; 0] = [];

        assert_eq!(
            resolve(&headings, &anchors, None, "dup"),
            Some(Target::Heading {
                index: 0,
                form: SlugForm::Github,
            }),
            "the first heading answering to the spelling wins"
        );
        assert!(
            !names_heading(&headings, &anchors, "", None, "dup", 1),
            "the sibling that does not answer to it is not named by it"
        );
    }

    #[test]
    fn explicit_id_shadows_no_computed_slug() {
        let headings = [explicit(1, "pinned")];
        let anchors: [Anchor; 0] = [];
        assert!(
            resolves(
                &headings,
                &anchors,
                Some(FragmentAlgorithm::Vscode),
                "pinned"
            ),
            "an explicit `{{#id}}` resolves under any pin"
        );
    }

    #[test]
    fn top_idioms_resolve_to_the_document_not_a_heading() {
        let headings = [computed(1, "top", "top", "top")];
        let anchors: [Anchor; 0] = [];
        assert_eq!(
            resolve(&headings, &anchors, None, "top"),
            Some(Target::Top),
            "`#top` addresses the top of the document"
        );
        assert!(
            !names_heading(&headings, &anchors, "", None, "top", 0),
            "even a literal `Top` heading is not what `#top` names"
        );
        assert_eq!(
            resolve(&headings, &anchors, None, ""),
            Some(Target::Top),
            "the empty fragment likewise"
        );
    }

    #[test]
    fn an_anchor_pins_the_heading_it_stands_above() {
        //  1: # Doc
        //  2:
        //  3: <a id="pinned"></a>
        //  4:
        //  5: ## Section
        let source = "# Doc\n\n<a id=\"pinned\"></a>\n\n## Section\n";
        let headings = [
            computed(1, "doc", "doc", "doc"),
            computed(5, "section", "section", "section"),
        ];
        assert_eq!(
            pinned_heading(&headings, source, 3),
            Some(1),
            "only blank lines separate the anchor from the heading below it"
        );
        let anchors = [Anchor {
            line: 3,
            id: "pinned".to_string(),
        }];
        assert!(
            names_heading(&headings, &anchors, source, None, "pinned", 1),
            "a reference to the pinned id is a reference to the heading"
        );
        assert!(
            !names_heading(&headings, &anchors, source, None, "pinned", 0),
            "it is not a reference to the heading above the anchor"
        );
    }

    #[test]
    fn a_mid_section_anchor_pins_nothing() {
        //  1: # Doc
        //  2:
        //  3: <a id="loose"></a>
        //  4:
        //  5: Body text.
        //  6:
        //  7: ## Section
        let source = "# Doc\n\n<a id=\"loose\"></a>\n\nBody text.\n\n## Section\n";
        let headings = [
            computed(1, "doc", "doc", "doc"),
            computed(7, "section", "section", "section"),
        ];
        assert_eq!(
            pinned_heading(&headings, source, 3),
            None,
            "prose between the anchor and the next heading breaks the pin"
        );
        let anchors = [Anchor {
            line: 3,
            id: "loose".to_string(),
        }];
        assert!(
            !names_heading(&headings, &anchors, source, None, "loose", 1),
            "a free-standing anchor names a spot, not a heading"
        );
        assert!(
            resolves(&headings, &anchors, None, "loose"),
            "it is still a valid fragment target — it just belongs to no heading"
        );
    }

    #[test]
    fn an_anchor_on_the_heading_line_pins_it() {
        let source = "## <a id=\"pinned\"></a> Section\n";
        let headings = [computed(1, "section", "section", "section")];
        assert_eq!(
            pinned_heading(&headings, source, 1),
            Some(0),
            "an anchor inside the heading's own line pins that heading"
        );
    }

    #[test]
    fn an_anchor_after_the_last_heading_pins_nothing() {
        let source = "## Section\n\n<a id=\"trailing\"></a>\n";
        let headings = [computed(1, "section", "section", "section")];
        assert_eq!(
            pinned_heading(&headings, source, 3),
            None,
            "no heading follows the anchor, so it pins none"
        );
    }
}
