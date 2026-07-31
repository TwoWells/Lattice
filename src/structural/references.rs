// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! The reference scanners — the 028 family.
//!
//! Everything here answers one question in four spellings: *this text names a
//! file; does that file exist, and should the mention have been a link?* The
//! spellings are a bare path in prose, a bare URL, a quoted path, and a
//! backticked path, and each has its own scanner because each has its own
//! delimiters and its own false-positive traps.
//!
//! Two routings sit on top of the scanners and are the reason they live
//! together. A reference that resolves outside the workspace goes through the
//! external-alias resolution (`[external]` bases, the `ExternalExistence`
//! oracle) rather than being judged as a missing local file; and a mention that
//! an alias could resolve is steered toward citation form rather than reported
//! as dark matter (issue 073). Every emission is offered to the exception
//! router in [`super::ledger`] before it becomes a diagnostic.

use std::path::Path;

use crate::block::{self, ElementKind, Tree};
use crate::config::{BarePathPolicy, Config, StaleReferencePolicy};
use crate::fm::ExceptionLint;
use crate::span::Span;
use crate::validation::{Diagnostic, Severity};

use super::ExternalExistence;
use super::ledger::ExceptionLookup;

// ---------------------------------------------------------------------------
// Bare path diagnostics (from tree)
// ---------------------------------------------------------------------------

/// Emit diagnostics for bare `.md` paths detected by the tree's `bare_paths()`
/// scanner.
///
/// A resolving bare path draws the make-it-a-link nudge (gated by `bare_paths`,
/// `Deny` escalating it to an error); a dangling one draws the stale-reference
/// diagnostic instead (gated by `stale_references`, issue 028). The two policies
/// are independent, so a missing reference is still reported when `bare_paths`
/// is `Disabled`, and vice versa.
pub fn emit_tree_bare_paths(
    tree: &Tree,
    rel_path: &Path,
    config: &Config,
    file_exists: &dyn Fn(&Path) -> bool,
    external_exists: &dyn Fn(&Path) -> ExternalExistence,
    lookup: &ExceptionLookup,
    out: &mut Vec<Diagnostic>,
) {
    let bare_paths = tree.bare_paths();
    for bare in &bare_paths {
        // An external-namespace reference (`{Name}/…`) is resolved existence-
        // only against its alias directory, never dir/root-joined (issue 030),
        // and regardless of extension — a cross-repo directory or non-`.md`
        // file is a real reference too.
        if route_external_reference(
            config,
            external_exists,
            config.policy.stale_references,
            rel_path,
            bare.line,
            None,
            &bare.path,
            lookup,
            out,
        )
        .is_some()
        {
            continue;
        }
        if resolves_under_any_base(rel_path, &bare.path, file_exists) {
            if config.policy.bare_paths == BarePathPolicy::Disabled {
                continue;
            }
            let diag = Diagnostic {
                file: rel_path.to_path_buf(),
                line: bare.line,
                severity: bare_path_severity(config.policy.bare_paths, Severity::Warning),
                message: format!(
                    "bare path `{}`: would moving the target update this mention? if so it's a reference — convert to a markdown link; if not it's an example — except it (see `lattice help config`)",
                    bare.path
                ),
                // `BarePath` carries only a line; fall back to a whole-line range.
                span: None,
            };
            lookup.route(ExceptionLint::BarePaths, &bare.path, diag, out);
        } else {
            route_stale_reference(
                config,
                external_exists,
                config.policy.stale_references,
                rel_path,
                bare.line,
                None,
                &bare.path,
                lookup,
                out,
            );
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

/// Emit the stale-reference diagnostic for a dangling `.md`-shaped reference.
///
/// Closes the missing quadrant (issue 028): a `.md` reference — backtick or
/// bare, `#fragment` already stripped — that resolves to no file is a defect,
/// the mirror of the `link target does not exist` *error*. Both forms share one
/// severity here, governed solely by [`StaleReferencePolicy`]:
/// [`Disabled`](StaleReferencePolicy::Disabled) suppresses it (the make-it-a-
/// link resolve hint, gated by [`BarePathPolicy`], still fires); `Hint`/`Warn`/
/// `Deny` set the severity. `reference` is the displayed reference text `X`.
///
/// The message frames the choice as decision 014's move test (issue 039) — a
/// dangling mention is a reference only if moving the target would force this
/// update — and names the `{repo}/…` external-namespace escape (issue 030,
/// following suggestion 001's self-documenting-message principle), so an agent
/// learns from the diagnostic that a cross-repo reference should be written and
/// aliased rather than left to dangle.
///
/// This is the message for a path with no known citation. When the path *does*
/// exist under a configured alias directory,
/// [`build_external_citation_steer`] replaces the generic lesson with the
/// concrete spelling (issue 073).
fn build_stale_reference(
    policy: StaleReferencePolicy,
    rel_path: &Path,
    line: usize,
    span: Option<Span>,
    reference: &str,
) -> Option<Diagnostic> {
    let severity = match policy {
        StaleReferencePolicy::Disabled => return None,
        StaleReferencePolicy::Hint => Severity::Hint,
        StaleReferencePolicy::Warn => Severity::Warning,
        StaleReferencePolicy::Deny => Severity::Error,
    };

    Some(Diagnostic {
        file: rel_path.to_path_buf(),
        line,
        severity,
        message: format!(
            "stale reference: `{reference}` — no such markdown file under this root; would moving the target update this mention? if so it's a reference — fix the path (or write it as `{{repo}}/…` and alias `repo` in .lattice.toml if it's in another repo); if not it's an example — except it (see `lattice help config`)"
        ),
        span,
    })
}

/// Emit the steering variant of the stale-reference diagnostic: the dangling
/// path exists under a configured `[external]` alias directory, so the message
/// names the exact citation to write instead of teaching the `{repo}/…` form
/// generically (issue 073).
///
/// Distinct from [`build_stale_reference`] only in wording and evidence: that
/// message says the external form *exists*, this one says *this path is one* and
/// spells it. Everything else is deliberately identical — the `stale reference:`
/// prefix keeps [`classify_028_lint`] routing it to the same ledger row and
/// exception namespace, `reference` is still the key the exception lookup
/// matches verbatim, and severity tracks [`StaleReferencePolicy`] with
/// `Disabled` returning `None`. Steering changes the message, never the tier.
fn build_external_citation_steer(
    policy: StaleReferencePolicy,
    rel_path: &Path,
    line: usize,
    span: Option<Span>,
    reference: &str,
    alias: &str,
    citation: &str,
) -> Option<Diagnostic> {
    let severity = match policy {
        StaleReferencePolicy::Disabled => return None,
        StaleReferencePolicy::Hint => Severity::Hint,
        StaleReferencePolicy::Warn => Severity::Warning,
        StaleReferencePolicy::Deny => Severity::Error,
    };

    Some(Diagnostic {
        file: rel_path.to_path_buf(),
        line,
        severity,
        message: format!(
            "stale reference: `{reference}` — path exists in external `{alias}` — cite it as `{citation}`"
        ),
        span,
    })
}

/// Rewrite a dark-matter reference as the workspace-relative path to probe under
/// an alias directory, or `None` when it has no external-citation form.
///
/// Collapses `.` and `..` by pure component arithmetic over the `/`-separated
/// markdown reference grammar, and drops the leading `/` of a root-relative
/// citation — which must go before the probe, because [`Path::join`] with an
/// absolute argument *discards* the alias directory and would `stat` a
/// filesystem-absolute path instead. A reference that escapes the workspace root
/// (`../sibling.md`) has no citation form under any alias and returns `None`,
/// mirroring [`candidate_exists`]'s rejection of the same shape.
///
/// The result doubles as the citation spelling's path part, so it is rebuilt
/// with `/` rather than reusing [`block::normalize_path`]'s `PathBuf`: the
/// message quotes a markdown reference, whose separator is `/` on every
/// platform.
fn external_citation_candidate(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.trim_start_matches('/').split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            normal => parts.push(normal),
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

/// Test whether a dangling reference names a path that exists under a configured
/// `[external]` alias directory, and if so return that alias and the exact
/// citation spelling to write (issue 073).
///
/// This reads the alias table in the direction nothing read it before.
/// [`route_external_reference`] consults it to validate a `{Name}/…` citation
/// someone already wrote; nothing connected a path already typed to an alias
/// already configured, so the generic message taught the escape without evidence
/// that *this* path was an instance of it — and a hand-written exception stayed
/// the cheaper local move (issue 066 accumulated nine of them, every one
/// resolvable under an alias already in `.lattice.toml`).
///
/// **Ordering is structural, not conditional.** The only caller is
/// [`route_stale_reference`], which every 028 emit site reaches solely in the
/// branch where intra-repo resolution has *already* failed. A reference that
/// resolves in this workspace can never be steered into a cross-repo citation —
/// that would be a regression, not a hint.
///
/// **Only `Present` claims a citation.** The probe reuses [`resolve_external`],
/// so the tri-state oracle degrades exactly as decision 010's tiers do: an
/// absent alias directory (partial checkout, or CI holding only this repo) and a
/// failed `stat` (issue 050) both yield no steer and fall through to the generic
/// message, rather than assert a path exists somewhere that could not be checked.
///
/// **Multiple matches: first match wins.** `config.external` is a [`BTreeMap`],
/// so the scan is alphabetical and deterministic. Every match is equally
/// correct — an external reference is existence-only and edge-free (decision
/// 010), so a path present under two alias directories genuinely has two valid
/// citations — and the message's job is to name *one* concrete spelling;
/// offering both would hand back the choice this steering exists to remove.
///
/// **Narrower than the citation machinery it borrows.** Decision 016 makes an
/// explicit `{Name}/…` reference existence-checked regardless of extension, but
/// a candidate reaching here is already `.md`-shaped by [`looks_like_path`]'s
/// intra-repo grammar, so a bare `schema.txt` sitting in an alias directory is
/// never steered — the dark-matter scan never sees it. That asymmetry *is*
/// decision 016's own rationale: the brace is the opt-in mark that removes the
/// is-this-a-reference ambiguity, and an unbraced non-`.md` path still carries
/// it.
fn steer_to_external_citation<'cfg>(
    config: &'cfg Config,
    external_exists: &dyn Fn(&Path) -> ExternalExistence,
    reference: &str,
) -> Option<(&'cfg str, String)> {
    if config.external.is_empty() {
        return None;
    }
    let (path, fragment) = split_path_fragment(reference);
    let candidate = external_citation_candidate(path)?;
    for alias in config.external.keys() {
        if resolve_external(config, external_exists, alias, &candidate) == ExternalResolution::Valid
        {
            // The `#fragment` is carried through verbatim: it is a heading
            // anchor, irrelevant to existence but part of the spelling the
            // author should write.
            let citation = fragment.map_or_else(
                || format!("{{{alias}}}/{candidate}"),
                |frag| format!("{{{alias}}}/{candidate}#{frag}"),
            );
            return Some((alias.as_str(), citation));
        }
    }
    None
}

/// Route a dangling-reference stale diagnostic through the exception lookup.
///
/// Builds the stale-reference diagnostic for `reference` and hands it to
/// [`ExceptionLookup::route`], so a literal `stale_references` exception
/// suppresses it, an active count-key buffers it, or it passes through to `out` —
/// the single seam every stale-reference emit site now shares (issue 031, issue
/// 036).
///
/// Being that single seam, it is also where the external-citation steering is
/// decided (issue 073): reaching here *is* the proof that intra-repo resolution
/// failed, so the check cannot fire on a live local reference. When the path
/// exists under a configured alias directory the message names that citation;
/// otherwise it is the generic stale message. The exception key, the ledger row,
/// and the severity are identical either way.
#[allow(
    clippy::too_many_arguments,
    reason = "routing context parameters are distinct concerns"
)]
fn route_stale_reference(
    config: &Config,
    external_exists: &dyn Fn(&Path) -> ExternalExistence,
    policy: StaleReferencePolicy,
    rel_path: &Path,
    line: usize,
    span: Option<Span>,
    reference: &str,
    lookup: &ExceptionLookup,
    out: &mut Vec<Diagnostic>,
) {
    // Both builders return `None` under `Disabled`, so this changes nothing that
    // is emitted — it keeps the steering probe's `stat`s off a path whose result
    // is discarded.
    if policy == StaleReferencePolicy::Disabled {
        return;
    }
    let diag = match steer_to_external_citation(config, external_exists, reference) {
        Some((alias, citation)) => {
            build_external_citation_steer(policy, rel_path, line, span, reference, alias, &citation)
        }
        None => build_stale_reference(policy, rel_path, line, span, reference),
    };
    if let Some(diag) = diag {
        lookup.route(ExceptionLint::StaleReferences, reference, diag, out);
    }
}

/// Build the stale-reference diagnostic for a dangling **external** `{Name}/…`
/// reference (issue 030, tier 4: a defined, present alias whose target is
/// missing under it).
///
/// Distinct from [`build_stale_reference`]: that message frames an intra-repo
/// dangle ("no such markdown file under this root") and teaches the `{repo}/…`
/// escape — both wrong here, where the reference *is* already `{repo}/…`, was
/// resolved against the alias directory rather than this root, and may name a
/// directory or non-`.md` file. This message instead names the alias and the
/// directory it resolved to, so the fix (correct the path in the aliased repo,
/// or repoint the alias) is unambiguous. Severity tracks [`StaleReferencePolicy`]
/// identically, and `Disabled` returns `None`.
fn build_external_stale_reference(
    policy: StaleReferencePolicy,
    rel_path: &Path,
    line: usize,
    span: Option<Span>,
    reference: &str,
    alias: &str,
    config: &Config,
) -> Option<Diagnostic> {
    let severity = match policy {
        StaleReferencePolicy::Disabled => return None,
        StaleReferencePolicy::Hint => Severity::Hint,
        StaleReferencePolicy::Warn => Severity::Warning,
        StaleReferencePolicy::Deny => Severity::Error,
    };
    // `Stale` is reached only for a defined alias, so the lookup is present; the
    // fallback keeps the message coherent rather than panicking if that ever
    // changes.
    let dir = config
        .external
        .get(alias)
        .map_or_else(String::new, |p| p.display().to_string());

    Some(Diagnostic {
        file: rel_path.to_path_buf(),
        line,
        severity,
        message: format!(
            "stale reference: `{reference}` — external alias `{alias}` resolves to `{dir}`, but no such file or directory exists there; fix the path in that repo (or repoint the `{alias}` alias in .lattice.toml), or except it (see `lattice help config`)"
        ),
        span,
    })
}

/// Build the cannot-verify diagnostic for an external `{Name}/…` reference
/// whose existence `stat` failed (issue 050,
/// [`ExternalResolution::Unverifiable`]).
///
/// A failed `stat` is not "absent": degrading it to the exempt tier silently
/// converted an I/O flake into exemption and misreported the reference's
/// frontmatter exception as unused. Surfacing it keeps the failure visible; a
/// `{Name}/…`-keyed `stale_references` exception still suppresses it, so an
/// excepted reference reports as suppressed, not unused. The `stale
/// reference:` prefix keeps [`classify_028_lint`] routing it to the same
/// ledger row and exception namespace as the resolution it stands in for.
/// Severity tracks [`StaleReferencePolicy`], mirroring
/// [`build_external_stale_reference`].
fn build_external_unverifiable_reference(
    policy: StaleReferencePolicy,
    rel_path: &Path,
    line: usize,
    span: Option<Span>,
    reference: &str,
    alias: &str,
    config: &Config,
) -> Option<Diagnostic> {
    let severity = match policy {
        StaleReferencePolicy::Disabled => return None,
        StaleReferencePolicy::Hint => Severity::Hint,
        StaleReferencePolicy::Warn => Severity::Warning,
        StaleReferencePolicy::Deny => Severity::Error,
    };
    // `Unverifiable` is reached only for a defined alias, so the lookup is
    // present; the fallback keeps the message coherent rather than panicking
    // if that ever changes.
    let dir = config
        .external
        .get(alias)
        .map_or_else(String::new, |p| p.display().to_string());

    Some(Diagnostic {
        file: rel_path.to_path_buf(),
        line,
        severity,
        message: format!(
            "stale reference: `{reference}` — external alias `{alias}` resolves to `{dir}`, but checking existence there failed (stat error, not \"absent\"); if this persists, check permissions on that directory"
        ),
        span,
    })
}

/// Emit diagnostics for bare URLs, quoted paths, and backticked paths found in
/// inline-host text — paragraphs and table cells alike, matching the cells the
/// link/edge extractor already walks.
///
/// The bare-URL and make-it-a-link (resolving path) nudges honor the
/// `bare_paths` policy: `Disabled` suppresses them, `Deny` escalates them to
/// errors. A dangling `.md` reference instead draws the stale-reference
/// diagnostic, governed independently by `stale_references` (issue 028), so it
/// fires even when `bare_paths` is `Disabled`.
pub fn emit_bare_path_diagnostics(
    tree: &Tree,
    rel_path: &Path,
    config: &Config,
    file_exists: &dyn Fn(&Path) -> bool,
    external_exists: &dyn Fn(&Path) -> ExternalExistence,
    lookup: &ExceptionLookup,
    out: &mut Vec<Diagnostic>,
) {
    let policy = config.policy.bare_paths;
    let stale = config.policy.stale_references;
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
            stale,
            file_exists,
            external_exists,
            config,
            lookup,
            &excluded,
            out,
        );

        // Check InlineCode children for backticked `.md` paths.
        for &child_id in &node.children {
            let child = tree.node(child_id);
            if matches!(child.kind, ElementKind::InlineCode) {
                let code_text = &source[child.span.start..child.span.end];
                // Strip backticks to get inner content.
                let inner = strip_backtick_delimiters(code_text);
                if !looks_like_path(inner) {
                    continue;
                }
                // Resolve the path part only; the `#fragment` is the heading
                // anchor and does not affect file existence.
                let path = split_path_fragment(inner).0;
                let line = block::byte_offset_to_line(source, child.span.start);
                // An external-namespace reference (`{Name}/…`) is resolved
                // existence-only against its alias directory (issue 030),
                // regardless of extension.
                if route_external_reference(
                    config,
                    external_exists,
                    stale,
                    rel_path,
                    line,
                    Some(child.span),
                    inner,
                    lookup,
                    out,
                )
                .is_some()
                {
                    continue;
                }
                if resolves_under_any_base(rel_path, path, file_exists) {
                    if policy != BarePathPolicy::Disabled {
                        let diag = Diagnostic {
                            file: rel_path.to_path_buf(),
                            line,
                            severity: bare_path_severity(policy, Severity::Hint),
                            message: format!(
                                "backticked path `{inner}` refers to an existing file: would moving it update this mention? if so it's a reference — make it a link; if not it's an example — drop the extension (a name) or except it with a reason (see `lattice help config`)"
                            ),
                            span: Some(child.span),
                        };
                        lookup.route(ExceptionLint::BarePaths, inner, diag, out);
                    }
                } else {
                    route_stale_reference(
                        config,
                        external_exists,
                        stale,
                        rel_path,
                        line,
                        Some(child.span),
                        inner,
                        lookup,
                        out,
                    );
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
    stale: StaleReferencePolicy,
    file_exists: &dyn Fn(&Path) -> bool,
    external_exists: &dyn Fn(&Path) -> ExternalExistence,
    config: &Config,
    lookup: &ExceptionLookup,
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

        // Bare URLs are governed solely by `bare_paths`; suppress them when it
        // is `Disabled`. Quoted `.md` paths still scan, because a dangling one
        // draws the stale-reference diagnostic (governed by `stale_references`).
        if policy != BarePathPolicy::Disabled {
            scan_line_for_bare_urls(
                line_text, line_start, line_num, rel_path, policy, excluded, out,
            );
        }
        scan_line_for_quoted_paths(
            line_text,
            line_start,
            line_num,
            rel_path,
            policy,
            stale,
            file_exists,
            external_exists,
            config,
            lookup,
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

/// Whether byte offset `i` sits at a left boundary for an opening quote: the
/// char immediately before it is whitespace or an opening paren `(`, or `i` is
/// the line start.
///
/// `(` is allowed so a quoted path in a parenthetical (`('docs/x.md')`,
/// `(see 'docs/x.md')`) opens; `[` is deliberately *not* allowed — it is markdown
/// link / reference syntax and would clash. `i` must be a char boundary; the
/// look-behind decodes the preceding char from the string slice (never a raw
/// byte), so it is Unicode-correct and panic-free on multi-byte input.
fn prev_is_boundary(line: &str, i: usize) -> bool {
    line[..i]
        .chars()
        .next_back()
        .is_none_or(|c| c.is_whitespace() || c == '(')
}

/// Whether the char immediately *after* byte offset `i` is alphanumeric.
///
/// `i` must be a char boundary; the look-ahead decodes the following char from
/// the string slice (never a raw byte). The end of the line counts as a
/// non-alphanumeric boundary (no following char).
fn next_is_alphanumeric(line: &str, i: usize) -> bool {
    line[i..].chars().next().is_some_and(char::is_alphanumeric)
}

/// Whether a `'` at byte offset `i` is a quote delimiter rather than an
/// apostrophe.
///
/// `'` doubles as an apostrophe, so it is a delimiter only at a boundary: an
/// *opening* `'` requires whitespace, an opening paren `(`, or the line start
/// immediately before it; a *closing* `'` requires a non-alphanumeric char (or
/// line end) immediately after it. The opening side is the stricter of the two on
/// purpose — a `'` preceded by a letter (`it's`) or most punctuation
/// (`example_'s`) is apostrophe-ish and must not open a span. `(` is the one
/// non-whitespace opener allowed, so a parenthetical path (`('docs/x.md')`) is
/// caught; `[` is excluded because it is markdown link syntax. A closing quote
/// may be followed by punctuation (`'path'.`, `'path')`). `"` is unambiguous and
/// never takes this guard.
fn is_quote_delimiter(line: &str, i: usize, quote: u8, opening: bool) -> bool {
    if quote == b'"' {
        return true;
    }
    if opening {
        prev_is_boundary(line, i)
    } else {
        // `i` is the byte offset of the `'`; the look-ahead inspects the char
        // after it (one byte past, since `'` is ASCII and one byte wide).
        !next_is_alphanumeric(line, i + 1)
    }
}

/// Find the next closing `quote` at or after byte offset `from`, honoring the
/// word-boundary guard so an apostrophe inside a word does not close the span.
///
/// Returns the byte offset of the closing quote within `line`. The search
/// iterates char indices (never raw bytes), so it is char-boundary-safe on
/// multi-byte input.
fn find_closing_quote(line: &str, from: usize, quote: u8) -> Option<usize> {
    let quote_char = char::from(quote);
    line[from..].char_indices().find_map(|(off, c)| {
        let abs = from + off;
        (c == quote_char && is_quote_delimiter(line, abs, quote, false)).then_some(abs)
    })
}

/// Scan a line for quoted paths (`"foo.md"` and `'foo.md'`).
///
/// Both quote styles are first-class and share identical downstream handling
/// (issue 032): the external-namespace resolution (issue 030),
/// make-it-a-link / stale-reference classification, and the exception-lookup
/// suppression (issue 031). `"` pairs unconditionally; `'` is treated as a
/// delimiter only at a word boundary (see [`is_quote_delimiter`]) so an
/// apostrophe is never mistaken for a quote.
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
    stale: StaleReferencePolicy,
    file_exists: &dyn Fn(&Path) -> bool,
    external_exists: &dyn Fn(&Path) -> ExternalExistence,
    config: &Config,
    lookup: &ExceptionLookup,
    excluded: &[Span],
    out: &mut Vec<Diagnostic>,
) {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let quote = bytes[i];
        // `"` and `'` are both ASCII (one byte), so byte indexing into `bytes`
        // here lands on a char boundary and the slice operations below are safe.
        if (quote == b'"' || quote == b'\'') && is_quote_delimiter(line, i, quote, true) {
            let start = i + 1;
            if let Some(end_abs) = find_closing_quote(line, start, quote) {
                let inner = &line[start..end_abs];
                let abs_pos = line_start + i;

                if !is_excluded(abs_pos, excluded) && looks_like_path(inner) {
                    // Span the whole quoted token, both quotes included.
                    let span = Span::new(abs_pos, line_start + end_abs + 1);
                    // Resolve the path part only; the `#fragment` is the
                    // heading anchor and does not affect file existence.
                    let path = split_path_fragment(inner).0;
                    // An external-namespace reference (`{Name}/…`) resolves
                    // existence-only against its alias directory (issue 030),
                    // regardless of extension.
                    if route_external_reference(
                        config,
                        external_exists,
                        stale,
                        rel_path,
                        line_num,
                        Some(span),
                        inner,
                        lookup,
                        out,
                    )
                    .is_some()
                    {
                        i = end_abs + 1;
                        continue;
                    }
                    if resolves_under_any_base(rel_path, path, file_exists) {
                        if policy != BarePathPolicy::Disabled {
                            let q = char::from(quote);
                            let diag = Diagnostic {
                                file: rel_path.to_path_buf(),
                                line: line_num,
                                severity: bare_path_severity(policy, Severity::Hint),
                                message: format!(
                                    "quoted path `{q}{inner}{q}`: would moving the target update this mention? if so it's a reference — make it a markdown link; if not it's an example — except it (see `lattice help config`)"
                                ),
                                span: Some(span),
                            };
                            lookup.route(ExceptionLint::BarePaths, inner, diag, out);
                        }
                    } else {
                        route_stale_reference(
                            config,
                            external_exists,
                            stale,
                            rel_path,
                            line_num,
                            Some(span),
                            inner,
                            lookup,
                            out,
                        );
                    }
                }
                i = end_abs + 1;
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

/// Check if a string looks like a markdown path-shaped reference.
///
/// Scoped to the markdown link-target grammar — `path[#fragment]`, ending in
/// `.md` (issue 028). `.md` is the one extension that forms a graph edge, so it
/// is the only path-shape the dark-matter scan nudges into a link; the render-
/// changing nudge on a `.rs`/`.toml`/image path fixes no graph defect (decision
/// 009). Non-`.md` *link existence* validation is separate (in `validation.rs`)
/// and unaffected.
///
/// A protocol-relative reference (`//host/path`) is a URL, not a workspace
/// path — a renderer resolves it against the current scheme and host, never
/// the repository root — so it is never path-shaped. A single leading `/` is
/// root-relative and stays path-shaped (resolved at the workspace root by
/// [`resolves_under_any_base`]).
///
/// Shapes that are not workspace paths at all are rejected outright (no
/// make-it-a-link hint, no stale-reference warning): a `~`-leading token
/// (home-relative, out of the repo, e.g. `~/Projects/Archive/AGENTS.md`); a
/// token containing `<` or `>` (a placeholder, e.g. `<name>/SKILL.md`); a token
/// containing `*` (a glob, e.g. `NN_*.md`); and a token containing an ellipsis —
/// `…` (U+2026) or `...` — which is documentation shorthand for a path shape
/// (e.g. the `{repo}/…` syntax this tool teaches), not a real file.
///
/// An external-namespace token (`{Name}/…`, issue 030) is admitted regardless
/// of extension — the `.md` scope guards against linkifying non-graph intra-repo
/// paths, but an external reference is never a local link or a graph edge
/// (decision 010), so a cross-repo directory or non-`.md` file
/// (`{Archive}/docs`) is still existence-checked against its alias directory.
/// The ellipsis exclusion still applies, so the `{Name}/…` placeholder itself
/// stays exempt while a concrete `{Name}/path` resolves.
fn looks_like_path(s: &str) -> bool {
    let path = split_path_fragment(s).0;
    !path.is_empty()
        && !path.starts_with("//")
        && !path.starts_with('~')
        && !path.contains(' ')
        && !path.contains('<')
        && !path.contains('>')
        && !path.contains('*')
        && !path.contains('…')
        && !path.contains("...")
        && (path.contains('/') || path.contains('.'))
        && (Path::new(path).extension().is_some_and(|ext| ext == "md")
            || block::external_namespace(path).is_some())
}

/// Split a path-shaped token into its path and optional `#fragment`.
///
/// Mirrors the link-target classifier (issue 028): a markdown link can target
/// `path#fragment`, so the dark-matter scan strips the fragment before the
/// `.md` check and existence resolution. The fragment is the heading anchor —
/// once the reference is linked, the existing fragment check validates it; the
/// make-it-a-link hint and the stale-reference warning need only file
/// existence on the path part.
fn split_path_fragment(s: &str) -> (&str, Option<&str>) {
    match s.split_once('#') {
        Some((path, frag)) => (path, Some(frag)),
        None => (s, None),
    }
}

/// The disposition of an external-namespace reference under its alias.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExternalResolution {
    /// The alias is undefined, or its directory is absent — exempt, unverified
    /// (the floor and the CI / partial-checkout guard). No diagnostic.
    Exempt,
    /// The alias directory is present and the referenced file exists under it.
    Valid,
    /// The alias directory is present but the referenced file is missing — a
    /// genuinely broken cross-repo reference.
    Stale,
    /// The alias is defined but a `stat` failed while resolving, so existence
    /// is unknown — the reference is neither validated nor exempted (issue
    /// 050). Surfaced as its own diagnostic rather than degraded to
    /// [`Exempt`](Self::Exempt), which masked an I/O failure as exemption and
    /// misreported the reference's frontmatter exception as unused.
    Unverifiable,
}

/// Resolve an external-namespace reference to its tiered disposition.
///
/// `alias`/`rest` come from [`block::external_namespace`]; `external_exists`
/// `stat`s an absolute filesystem path (decision 010 — existence-only, edge-free: the
/// aliased directory is touched by `stat` alone, never read, parsed, or
/// indexed). The tiers (issue 030):
///
/// 1. alias undefined → [`Exempt`](ExternalResolution::Exempt);
/// 2. alias defined, its directory absent → [`Exempt`](ExternalResolution::Exempt);
/// 3. alias defined, directory present, file present → [`Valid`](ExternalResolution::Valid);
/// 4. alias defined, directory present, file missing → [`Stale`](ExternalResolution::Stale).
///
/// A `stat` that *fails* (as opposed to answering "absent") lands in none of
/// the four tiers: either check reporting
/// [`Unknown`](ExternalExistence::Unknown) makes the disposition
/// [`Unverifiable`](ExternalResolution::Unverifiable) (issue 050).
fn resolve_external(
    config: &Config,
    external_exists: &dyn Fn(&Path) -> ExternalExistence,
    alias: &str,
    rest: &str,
) -> ExternalResolution {
    let Some(dir) = config.external.get(alias) else {
        return ExternalResolution::Exempt;
    };
    match external_exists(dir) {
        ExternalExistence::Absent => ExternalResolution::Exempt,
        ExternalExistence::Unknown => ExternalResolution::Unverifiable,
        ExternalExistence::Present => match external_exists(&dir.join(rest)) {
            ExternalExistence::Present => ExternalResolution::Valid,
            ExternalExistence::Absent => ExternalResolution::Stale,
            ExternalExistence::Unknown => ExternalResolution::Unverifiable,
        },
    }
}

/// Resolve and route an external-namespace reference, if `reference` is one.
///
/// Returns `Some(())` when `reference` is an external `{Name}/…` token — the
/// caller has nothing further to do, because an external reference is
/// existence-only and never a local link, so it skips both the make-it-a-link
/// nudge and intra-repo resolution (issue 030, decision 010). On the stale tier
/// (a defined, present alias whose target is missing) it emits the
/// external-specific stale diagnostic, and on the unverifiable disposition (a
/// `stat` failure, issue 050) the cannot-verify one — both routed through the
/// exception lookup so a `{Name}/…`-keyed `stale_references` exception still
/// suppresses them (issue 031). Returns `None` when the token is not external,
/// so the caller falls through to ordinary `.md` resolution.
///
/// `reference` is the displayed token; any `#fragment` is stripped before
/// recognition and existence resolution (the fragment is a heading anchor and
/// does not affect whether the file exists), mirroring the intra-repo arms.
#[allow(
    clippy::too_many_arguments,
    reason = "routing context parameters are distinct concerns"
)]
fn route_external_reference(
    config: &Config,
    external_exists: &dyn Fn(&Path) -> ExternalExistence,
    policy: StaleReferencePolicy,
    rel_path: &Path,
    line: usize,
    span: Option<Span>,
    reference: &str,
    lookup: &ExceptionLookup,
    out: &mut Vec<Diagnostic>,
) -> Option<()> {
    let path = split_path_fragment(reference).0;
    let (alias, rest) = block::external_namespace(path)?;
    let diag = match resolve_external(config, external_exists, alias, rest) {
        ExternalResolution::Exempt | ExternalResolution::Valid => None,
        ExternalResolution::Stale => {
            build_external_stale_reference(policy, rel_path, line, span, reference, alias, config)
        }
        ExternalResolution::Unverifiable => build_external_unverifiable_reference(
            policy, rel_path, line, span, reference, alias, config,
        ),
    };
    if let Some(diag) = diag {
        lookup.route(ExceptionLint::StaleReferences, reference, diag, out);
    }
    Some(())
}

/// Resolve a path-shaped reference against both candidate bases, normalized,
/// and report whether it exists in the workspace.
///
/// A `.md` reference written in prose can be either **dir-relative** (resolved
/// against the source file's parent, like a markdown link target) or
/// **root-relative** (a full repo-path citation, the way people cite docs in
/// prose). The dark-matter scan accepts either: the reference "resolves" if a
/// file exists under *either* base. The leading-`/` form is unambiguously
/// root-relative, so only that base is tried for it (issue 028).
///
/// Each candidate is lexically normalized (collapsing `.`/`..` by pure path-
/// component arithmetic, no filesystem access) before the existence check, so
/// a `../sibling.md` reference matches the clean workspace key. A candidate
/// that escapes the workspace root after normalization (i.e. begins with `..`)
/// is not a valid workspace path and is not checked.
///
/// This drives both branches of the same decision: the make-it-a-link hint
/// fires when it resolves under either base, and the stale-reference warning
/// fires only when it resolves under neither.
fn resolves_under_any_base(
    file_path: &Path,
    target: &str,
    file_exists: &dyn Fn(&Path) -> bool,
) -> bool {
    // A leading single `/` is unambiguously root-relative (GitHub and web
    // renderers resolve `/foo.md` against the repository root). Try only the
    // root base for it.
    if let Some(rooted) = target.strip_prefix('/') {
        return candidate_exists(Path::new(rooted), file_exists);
    }

    // Dir-relative: against the source file's parent directory.
    let dir_relative = file_path
        .parent()
        .map_or_else(|| std::path::PathBuf::from(target), |dir| dir.join(target));
    if candidate_exists(&dir_relative, file_exists) {
        return true;
    }

    // Root-relative: the target taken as a workspace-relative path.
    candidate_exists(Path::new(target), file_exists)
}

/// Lexically normalize a candidate path and check it against the workspace.
///
/// Returns `false` for a candidate that escapes the workspace root after
/// normalization (its first component is `..`): such a path is not a valid
/// workspace-relative reference, so it is never a resolution.
fn candidate_exists(candidate: &Path, file_exists: &dyn Fn(&Path) -> bool) -> bool {
    let normalized = block::normalize_path(candidate);
    if matches!(
        normalized.components().next(),
        Some(std::path::Component::ParentDir)
    ) {
        return false;
    }
    file_exists(&normalized)
}
