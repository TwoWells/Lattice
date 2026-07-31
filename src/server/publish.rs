// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! When diagnostics are sent, to whom, and under whose perspective.
//!
//! The publication path is a diff, not a broadcast: a desired set is computed
//! per root, filtered through the held `[[override]]` verdict, materialized only
//! for the files whose rows actually moved, and compared against the per-URI
//! record of what the client was last told. Only the delta goes on the wire.
//!
//! [`merge_perspectives`] is decision 024 clause 9's headline in one function:
//! a document's rows are computed from the saved world with *its own* buffer
//! overlaid and nobody else's, so an open buffer never rewrites its neighbours'
//! diagnostics. [`PublishScope`] records which column of clause 2 the
//! triggering event sits in — a buffer event republishes one document, a
//! commitment runs the full pass.
//!
//! What a row *is* belongs to the sibling [`super::diagnostics`] module.

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Result;
use lsp_server::{Connection, Message, Notification};

use crate::config::ConfigError;
use crate::line_index::LineIndex;
use crate::lsp;
use crate::overrides::{self, OverrideVerdicts, VerdictKind};
use crate::uri::{path_to_uri, uri_to_path};
use crate::validation::{Diagnostic, Severity};
use crate::workspace::WorkspaceView;

use super::diagnostics::{collect_all_diagnostics, file_desired, to_lsp_diagnostic};
use super::workspaces::{PublishedDiagnostics, RootMeta, Workspaces};

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

/// Build a one-element force-rematerialize set for the single-document callers
/// of [`publish_all_diagnostics`] / [`diff_diagnostics`] (a `didOpen` /
/// `didSave` / `didChange` / `didClose` names exactly the document it touched).
pub fn one_uri(uri: &str) -> HashSet<String> {
    let mut set = HashSet::with_capacity(1);
    set.insert(uri.to_string());
    set
}

/// Republish a document's current diagnostics unconditionally.
///
/// Two notification boundaries force a publish even when nothing changed:
///
/// - `didOpen` is a client-state boundary (decision 022): the server cannot
///   know what a client remembers about a document it just opened, so the
///   per-URI last-published record is invalidated before the sync's publish
///   pass — closing the false-clean gap a client that drops its per-URI
///   record on reopen would otherwise hit.
/// - `didSave` is a client-expectation boundary (issue 062): on a push-only
///   server (decision 022 removed the pull surface) a client that hears
///   nothing for a document it just saved cannot distinguish "no findings"
///   from "no answer" except by timeout. The delta diff reads an unchanged
///   set as nothing-to-send, so the save's answer is forced the same way.
///
/// A clean indexed file needs one extra step: its desired set is empty and it
/// holds no cache entry, so the diff suppresses it (an unchanged empty is not a
/// change). Push-only owes it an *explicit* empty publish there, not a skip — so
/// when the pass leaves this document with no cache entry (i.e. it is clean), an
/// empty `publishDiagnostics` is sent for it. A rootless or unindexed document
/// (issue 051) resolves to no workspace and publishes nothing, as before.
///
/// `scope` declares which column of decision 024 clause 2 the triggering event
/// sits in: a `didOpen` is a **buffer event** and republishes this document
/// alone; a `didSave` is a **commitment** and runs the full pass.
///
/// No diagnostics are recomputed beyond the ordinary publish pass.
pub fn force_republish(
    connection: &Connection,
    workspaces: &mut Workspaces,
    uri: &str,
    scope: &PublishScope,
) -> Result<()> {
    // The publish/cache key for this document when it is indexed under a root
    // (the same base `diff_diagnostics` keys the cache by). `None` for a
    // rootless or unindexed open, which publishes nothing.
    let canonical = workspaces
        .resolve(uri)
        .map(|(workspace, rel_path)| path_to_uri(&workspace.root().join(rel_path)));

    // Invalidate the last-published record so the diff re-sends the current set
    // even when the content is unchanged.
    if let Some(canonical) = &canonical {
        workspaces.published.remove(canonical);
    }

    let sets = diff_diagnostics_with(workspaces, &one_uri(uri), Adjudication::Commit, scope);
    send_publishes(connection, sets)?;

    // After the pass, a document that carries diagnostics has its cache entry
    // repopulated; a clean one has none (only non-empty entries are cached). The
    // clean file was suppressed by the diff, so send it the explicit empty
    // publish push-only owes it.
    if let Some(canonical) = canonical
        && !workspaces.published.contains_key(&canonical)
    {
        let params = lsp::PublishDiagnosticsParams {
            uri: canonical,
            diagnostics: Vec::new(),
        };
        let notif = Notification::new(lsp::method::PUBLISH_DIAGNOSTICS.to_string(), params);
        connection.sender.send(Message::Notification(notif))?;
    }

    Ok(())
}

/// Force-republish one root's `.lattice.toml` channel (decision 023
/// addendum), given any URI spelling of the marker.
///
/// The config is unsynced by default, so no `didOpen` boundary ever resets
/// its publish record client-side — the record is invalidated here so the
/// publish pass re-sends the current set even when unchanged, and a clean
/// channel (no cache entry after the pass) is still answered with the
/// explicit empty push-only owes it. Serves both accepted-sync couriers
/// (`didOpen` as the client-state boundary, `didSave` as the commitment);
/// the watched-marker batch path performs the same invalidation inline so
/// one batch pays one publish pass.
pub fn force_republish_config(
    connection: &Connection,
    workspaces: &mut Workspaces,
    marker_uri: &str,
) -> Result<()> {
    let marker_path = uri_to_path(marker_uri);
    let Some(root) = marker_path
        .parent()
        .and_then(|dir| workspaces.registered_root_at(dir))
    else {
        // A config outside every registered scope publishes nothing.
        return Ok(());
    };
    let config_uri = path_to_uri(&root.join(".lattice.toml"));
    workspaces.published.remove(&config_uri);
    publish_all_diagnostics(connection, workspaces, &HashSet::new())?;
    if !workspaces.published.contains_key(&config_uri) {
        let params = lsp::PublishDiagnosticsParams {
            uri: config_uri,
            diagnostics: Vec::new(),
        };
        let notif = Notification::new(lsp::method::PUBLISH_DIAGNOSTICS.to_string(), params);
        connection.sender.send(Message::Notification(notif))?;
    }
    Ok(())
}

/// Whether a publish pass is a save-point commitment or a mid-edit draft
/// (decision 023): a commitment re-adjudicates each root's `[[override]]`
/// expect verdicts from the freshly computed live set and holds them; a draft
/// filters through the verdicts held from the last commitment — counts move
/// mid-edit, the suppression decision does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Adjudication {
    /// Recompute each root's verdicts from this pass's live set and hold them
    /// (`didOpen` / `didSave` / watched-files batches and every other
    /// disk-anchored publish).
    Commit,
    /// Filter through the held verdicts without re-adjudicating (the
    /// `didChange` graph-tier arm — the draft).
    Held,
}

/// Publish diagnostics for the files whose diagnostics changed, at a
/// **commitment point**: the pass re-adjudicates every root's override
/// verdicts before filtering (decision 023).
///
/// The cheap whole-graph recompute still happens internally (see
/// [`diff_diagnostics`]), but the expensive per-diagnostic materialization and
/// the `publishDiagnostics` notifications are both restricted to the documents
/// an edit actually moved — collapsing the per-keystroke cost from `O(files)`
/// down to the handful that changed (issue 013 — publication diffing, then
/// ticket perf 02's materialization cache).
///
/// `changed_uris` names the documents whose source text just changed, if any,
/// so each one's materialization is refreshed unconditionally; see
/// [`diff_diagnostics`] for why an edited file cannot trust its cached LSP form,
/// and why a whole batch of changed files is forced together in one pass.
pub fn publish_all_diagnostics(
    connection: &Connection,
    workspaces: &mut Workspaces,
    changed_uris: &HashSet<String>,
) -> Result<()> {
    let sets = diff_diagnostics(workspaces, changed_uris);
    send_publishes(connection, sets)
}

/// Publish diagnostics for the files whose diagnostics changed, **between**
/// commitments (the `didChange` graph-tier arm): live diagnostics are
/// recomputed as always, but the published sets are filtered through each
/// root's held verdicts — no re-adjudication, so a mid-edit count crossing
/// never flashes a resurface (decision 023).
///
/// Scoped to `focus` alone (decision 024 clause 2): a buffer event may change
/// only that document's rows, so no other file's cache entry is read or
/// written.
pub fn publish_draft_diagnostics(
    connection: &Connection,
    workspaces: &mut Workspaces,
    changed_uris: &HashSet<String>,
    focus: &Path,
) -> Result<()> {
    let sets = diff_diagnostics_with(
        workspaces,
        changed_uris,
        Adjudication::Held,
        &PublishScope::Only(focus.to_path_buf()),
    );
    send_publishes(connection, sets)
}

/// Send one `publishDiagnostics` notification per diffed `(uri, set)` pair.
pub fn send_publishes(
    connection: &Connection,
    sets: Vec<(String, Vec<lsp::Diagnostic>)>,
) -> Result<()> {
    for (uri, diagnostics) in sets {
        let params = lsp::PublishDiagnosticsParams { uri, diagnostics };
        let notif = Notification::new(lsp::method::PUBLISH_DIAGNOSTICS.to_string(), params);
        connection.sender.send(Message::Notification(notif))?;
    }

    Ok(())
}

/// Publish the diagnostic delta for a single file (issue 013 — stage 2.5).
///
/// Recomputes the desired diagnostics for just `uri` and sends a
/// `publishDiagnostics` only if its vector changed. This avoids the
/// `O(workspace)` materialize/diff that [`publish_all_diagnostics`] pays every
/// sync. It is correct only when the triggering edit cannot affect any other
/// file's diagnostics — i.e. a content edit (no membership change) in the
/// structural tier — so the caller must gate on that.
pub fn publish_file_diagnostics(
    connection: &Connection,
    workspaces: &mut Workspaces,
    uri: &str,
) -> Result<()> {
    if let Some((uri, diagnostics)) = diff_file_diagnostics(workspaces, uri) {
        let params = lsp::PublishDiagnosticsParams { uri, diagnostics };
        let notif = Notification::new(lsp::method::PUBLISH_DIAGNOSTICS.to_string(), params);
        connection.sender.send(Message::Notification(notif))?;
    }

    Ok(())
}

/// Diff one file's freshly computed diagnostics against the last published set,
/// updating the cache and returning the `(uri, diagnostics)` to send when it
/// changed (including the transition to empty, which clears the file). Returns
/// `None` when nothing changed or the URI resolves to no workspace.
///
/// The single-file counterpart to [`diff_diagnostics`]; it touches only this
/// file's cache entry, leaving every other file's last-published set intact —
/// which is correct precisely under the structural-tier, no-membership-change
/// precondition its caller enforces.
pub fn diff_file_diagnostics(
    workspaces: &mut Workspaces,
    uri: &str,
) -> Option<(String, Vec<lsp::Diagnostic>)> {
    let (canonical, lattice, lsp) = {
        let (workspace, rel_path) = workspaces.resolve(uri)?;
        let canonical = path_to_uri(&workspace.root().join(&rel_path));
        let (lattice, lsp) = file_desired(&workspace, &rel_path);
        (canonical, lattice, lsp)
    };

    let unchanged = workspaces
        .published
        .get(&canonical)
        .map_or(lsp.is_empty(), |prev| prev.lsp == lsp);
    if unchanged {
        return None;
    }

    // Keep the cache invariant: only non-empty entries are stored, so an absent
    // entry means "the client currently holds none". Caching the Lattice vector
    // alongside the LSP form keeps this entry coherent with the full path's
    // change-detector (ticket perf 02).
    if lsp.is_empty() {
        workspaces.published.remove(&canonical);
    } else {
        workspaces.published.insert(
            canonical.clone(),
            PublishedDiagnostics {
                lattice,
                lsp: lsp.clone(),
            },
        );
    }

    Some((canonical, lsp))
}

/// Compute the full desired diagnostic set across all workspaces, keyed by
/// document URI, materializing every file from scratch.
///
/// Every indexed file gets an entry — an empty vector when it has no
/// diagnostics — so a caller can tell a file that just became clean apart from
/// one that left the workspace. This is the unconditional from-scratch
/// recompute that the differential tests use as their oracle; production goes
/// through [`diff_diagnostics`], which materializes only the files an edit
/// moved.
#[cfg(test)]
pub fn desired_diagnostics(workspaces: &Workspaces) -> BTreeMap<String, Vec<lsp::Diagnostic>> {
    let mut desired: BTreeMap<String, Vec<lsp::Diagnostic>> = BTreeMap::new();
    let empty = LineIndex::default();

    // Ascending root order: a document under overlapping folders is inserted by
    // the shallow root first and overwritten by the deepest, so the deepest
    // workspace's set wins the shared URI (matching `diff_diagnostics`).
    for (root, meta) in &workspaces.roots {
        // The desired set is post-adjudication: filtered through the root's
        // held override verdicts, exactly as production publishes (decision
        // 023). The oracle never re-adjudicates — it asks what the client
        // should currently hold, and that is the held verdict's answer
        // ([`Adjudication::Held`]) — and it carries the perspective merge, the
        // config channel, and the fabricated-config guard through the same
        // shared collection.
        let (mut by_file, _) = root_desired_rows(
            workspaces,
            root,
            meta,
            Adjudication::Held,
            &PublishScope::Full,
        );

        let config_rel = PathBuf::from(".lattice.toml");
        let config_channel =
            (meta.has_config || meta.config_error.is_some()).then_some(config_rel.clone());
        let rels: Vec<PathBuf> = workspaces
            .store
            .current_files(root)
            .into_keys()
            .chain(config_channel)
            .collect();
        for rel_path in rels {
            let uri = path_to_uri(&root.join(&rel_path));
            let lattice = by_file.remove(&rel_path).unwrap_or_default();
            // Materialize against the document's own current text: an overlay
            // document's rows are anchored in its buffer.
            let fd = workspaces
                .store
                .current(&root.join(&rel_path))
                .map(|doc| &doc.data);
            let source = fd.map_or("", |fd| fd.tree.source());
            let index = fd.map_or(&empty, |fd| &fd.line_index);
            let diagnostics = lattice
                .iter()
                .map(|d| to_lsp_diagnostic(d, source, index))
                .collect();
            desired.insert(uri, diagnostics);
        }
    }

    desired
}

/// Diff the freshly computed diagnostics against the last-published set,
/// returning only the `(uri, diagnostics)` pairs that must be sent and updating
/// the cache to match.
///
/// Runs the cheap whole-graph recompute ([`collect_all_diagnostics`]) and then,
/// per file, compares the new Lattice diagnostic vector against the cached one.
/// A file whose Lattice vector is unchanged keeps its cached materialization
/// untouched — the expensive UTF-16 [`to_lsp_diagnostic`] pass runs only for the
/// files the recompute shows actually moved — so a graph-tier (`.lattice.toml`)
/// sync no longer re-materializes every file on every keystroke. Detection,
/// not prediction: the whole-graph recompute already reflects cross-file edges
/// (a missing backlink reported on the *source*), so a dependent file an edit
/// touched only indirectly is caught the same way (issue 013 — ticket perf 02).
///
/// `changed_uris` names the documents whose source text just changed, if any.
/// Each is force-re-materialized unconditionally: a length-preserving edit can
/// leave the Lattice vector byte-identical yet shift a span's UTF-16 column (an
/// astral-plane swap upstream of the span on its line), so the cached LSP form
/// cannot be trusted for an edited file even when its Lattice vector matches.
/// Every *other* file's source is unchanged, so Lattice-vector equality there
/// does guarantee an identical materialization. Passing a set (rather than a
/// single URI) lets one pass force-re-materialize a whole batch of changed
/// files — a bulk on-disk change reconciles all of them in one O(workspace)
/// recompute instead of one per file (ticket perf 07). Pass an empty set when
/// no single file's text changed (e.g. a workspace-folder add/remove — newly
/// scanned files are cache misses and re-materialize regardless).
///
/// A pair is sent when its materialized vector differs from what the client last
/// received, including the transition to empty — a file that became clean, or
/// one that left the workspace (deleted, or its folder removed) — so stale
/// diagnostics are cleared. Only non-empty entries are cached, so an absent
/// entry means "the client currently holds none". The result is sorted by URI
/// for deterministic output.
///
/// The commitment-mode, full-scope entry point ([`Adjudication::Commit`],
/// [`PublishScope::Full`]); the `didChange` draft goes through
/// [`diff_diagnostics_with`] directly.
pub fn diff_diagnostics(
    workspaces: &mut Workspaces,
    changed_uris: &HashSet<String>,
) -> Vec<(String, Vec<lsp::Diagnostic>)> {
    diff_diagnostics_with(
        workspaces,
        changed_uris,
        Adjudication::Commit,
        &PublishScope::Full,
    )
}

/// Which documents a publish pass computes and diffs (decision 024 clause 2's
/// enforceable pair).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishScope {
    /// Every document under every root — a **commitment event** (`didSave`, a
    /// watched-files batch, `didClose`, a folder or marker change). Anyone's
    /// rows may have moved.
    Full,
    /// One document's own rows — a **buffer event** (`didOpen`, `didChange`).
    /// The saved world did not move, so no other file's rows can have moved,
    /// and no other file's cache entry is read or written.
    Only(PathBuf),
}

/// The buffer-locality perspective merge (decision 024 clause 8) — the seam the
/// [`crate::invariants::assert_buffer_locality`] differential pins.
///
/// `saved_live` is the saved world's diagnostic vector: every document judged
/// against everyone's last-committed state. `perspectives` supplies, for each
/// document that has a diverged buffer, the view of the saved world with *that*
/// document's buffer swapped in. Its rows replace that document's saved rows —
/// **and only that document's**: every other row the perspective pass computed
/// describes a document judged from somewhere other than its own seat, and is
/// discarded unread.
///
/// That discard is the whole invariant. Merging any other row from a
/// perspective pass would let one document's buffer reach another document's
/// verdict, which is exactly what decision 024 forbids.
pub fn merge_perspectives(
    saved_live: Vec<Diagnostic>,
    perspectives: &[(PathBuf, WorkspaceView<'_>)],
) -> BTreeMap<PathBuf, Vec<Diagnostic>> {
    let mut by_file: BTreeMap<PathBuf, Vec<Diagnostic>> = BTreeMap::new();
    for diag in saved_live {
        by_file.entry(diag.file.clone()).or_default().push(diag);
    }
    for (rel, view) in perspectives {
        let rows = collect_all_diagnostics(view)
            .into_iter()
            .filter(|diag| &diag.file == rel)
            .collect();
        by_file.insert(rel.clone(), rows);
    }
    by_file
}

/// One root's desired per-file rows: the perspective merge, filtered through
/// the root's `[[override]]` verdicts (issue 064, decision 023).
///
/// At a commitment the verdicts are re-adjudicated and returned (for the caller
/// to hold on the [`RootMeta`] once its borrow ends); between commitments the
/// held verdicts bind and `None` is returned. Either way the matched members'
/// findings are filtered out here — exactly once, at the seam where desired
/// sets materialize, and nowhere else: `compute_structural` is shared with
/// `lint`, which runs its own aggregate pass, so filtering there would
/// double-apply.
///
/// **Adjudication reads the saved world only.** A count is an aggregate over
/// committed state; letting a diverged buffer move a verdict would suppress or
/// resurface *other* files' rows from one document's unsaved draft — a buffer
/// locality violation, and a conformance break against `lattice lint`, which
/// reads disk.
pub fn root_desired_rows(
    workspaces: &Workspaces,
    root: &Path,
    meta: &RootMeta,
    adjudication: Adjudication,
    scope: &PublishScope,
) -> (BTreeMap<PathBuf, Vec<Diagnostic>>, Option<OverrideVerdicts>) {
    // The focus of a scoped pass, as a root-relative key — `None` when the
    // scoped document does not belong to this root, in which case the pass has
    // nothing to say about it.
    let focus_rel = match scope {
        PublishScope::Full => None,
        PublishScope::Only(abs) => match abs.strip_prefix(root) {
            Ok(rel) if workspaces.store.primary_root(abs).as_deref() == Some(root) => {
                Some(rel.to_path_buf())
            }
            _ => return (BTreeMap::new(), None),
        },
    };

    // A root holding a fabricated config — broken at scope registration with
    // no last-good (decision 023 addendum, issue 065) — publishes nothing
    // computed under it: defaults are the semantics of an absent config, not
    // a broken one. Only the load error reaches the wire, on the config
    // channel; the next valid commitment restores the full surface.
    if !meta.config_committed {
        let mut by_file: BTreeMap<PathBuf, Vec<Diagnostic>> = BTreeMap::new();
        let channel = config_channel_diagnostics(meta, &meta.verdicts);
        if !channel.is_empty() {
            by_file.insert(PathBuf::from(".lattice.toml"), channel);
        }
        let committed = match adjudication {
            Adjudication::Commit => Some(OverrideVerdicts::default()),
            Adjudication::Held => None,
        };
        return (by_file, committed);
    }

    // A scoped draft needs no saved-world collect at all: it neither
    // adjudicates nor reports on any other document.
    let skip_saved_collect = focus_rel.is_some() && adjudication == Adjudication::Held;
    let saved_view = workspaces.saved_view(root);
    let saved_live = if skip_saved_collect {
        Vec::new()
    } else {
        collect_all_diagnostics(&saved_view)
    };
    let committed = match adjudication {
        Adjudication::Commit => Some(overrides::adjudicate(
            &meta.config.overrides,
            saved_view.files().keys().map(PathBuf::as_path),
            &saved_live,
        )),
        Adjudication::Held => None,
    };

    // The perspectives to merge: every diverged buffer this pass reports on.
    let overlay_keys: Vec<PathBuf> = focus_rel.as_ref().map_or_else(
        || workspaces.store.overlay_keys_of_root(root),
        |rel| vec![root.join(rel)],
    );
    let perspectives: Vec<(PathBuf, WorkspaceView<'_>)> = overlay_keys
        .iter()
        .filter(|abs| workspaces.store.has_overlay(abs))
        .filter_map(|abs| {
            abs.strip_prefix(root)
                .ok()
                .map(|rel| (rel.to_path_buf(), workspaces.perspective_view(root, abs)))
        })
        .collect();

    let mut by_file = merge_perspectives(saved_live, &perspectives);
    // A scoped pass reports on exactly one document; drop every other row the
    // saved-world collect produced (it was computed only to feed adjudication).
    if let Some(rel) = &focus_rel {
        let rows = by_file.remove(rel).unwrap_or_default();
        by_file = BTreeMap::new();
        by_file.insert(rel.clone(), rows);
    }

    let verdicts = committed.as_ref().unwrap_or(&meta.verdicts);
    for rows in by_file.values_mut() {
        overrides::suppress_matched(verdicts, rows);
    }

    // The config channel (decision 023 clause 4) rides the same per-root map
    // under the marker's pseudo-path, so the publish diff treats the config
    // URI exactly like a document's: sent when it changes, cleared when it
    // empties. A scoped **draft** leaves it alone — nothing it computes can
    // move a workspace-health flag — but a scoped commitment (`didOpen`) must
    // still answer it, because it re-adjudicated.
    if focus_rel.is_none() || adjudication == Adjudication::Commit {
        let channel = config_channel_diagnostics(meta, verdicts);
        if !channel.is_empty() {
            by_file.insert(PathBuf::from(".lattice.toml"), channel);
        }
    }
    (by_file, committed)
}

/// The `.lattice.toml` channel's desired diagnostic set for one root
/// (decision 023 clause 4): the config load error (severity error) and the
/// expect-drift / unused-override flags (severity warning, the CLI's exact
/// wording via the shared [`overrides`] renderers, hint suffix included).
///
/// All entries are file-level — no span, line 1 — and the config is never
/// indexed, so materialization takes the unindexed-file fallback (empty
/// source) and anchors every range at position 0,0. Stanza spans via
/// `toml::Spanned` are a possible refinement, not scope.
///
/// The flags describe the config that governs adjudication, so they render
/// from the same verdicts the publish pass filters through; a fabricated
/// default (broken at startup, no last-good) governs nothing and contributes
/// only its load error.
/// Render a [`ConfigError`] and its source chain as a single diagnostic
/// message. The error variants deliberately omit `{source}` from their Display
/// (it is a `.source()`), so the detail — the toml parse location, the io
/// cause — lives only in the chain; this walks it, mirroring what anyhow's
/// `{:#}` gives the CLI so both surfaces read identically.
pub fn config_error_message(error: &ConfigError) -> String {
    let mut message = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        let _ = write!(message, ": {cause}");
        source = cause.source();
    }
    message
}

pub fn config_channel_diagnostics(meta: &RootMeta, verdicts: &OverrideVerdicts) -> Vec<Diagnostic> {
    let file = PathBuf::from(".lattice.toml");
    let mut diagnostics = Vec::new();
    if let Some(error) = &meta.config_error {
        diagnostics.push(Diagnostic {
            file: file.clone(),
            line: 1,
            severity: Severity::Error,
            message: config_error_message(error),
            span: None,
        });
    }
    if !meta.config_committed {
        return diagnostics;
    }
    for &idx in &verdicts.unused {
        diagnostics.push(Diagnostic {
            file: file.clone(),
            line: 1,
            severity: Severity::Warning,
            message: overrides::unused_message(&meta.config.overrides[idx]),
            span: None,
        });
    }
    for verdict in &verdicts.entries {
        if let VerdictKind::Drifted { expect, found } = verdict.kind {
            diagnostics.push(Diagnostic {
                file: file.clone(),
                line: 1,
                severity: Severity::Warning,
                message: overrides::drift_message(
                    &meta.config.overrides[verdict.entry],
                    verdict.lint,
                    expect,
                    found,
                ),
                span: None,
            });
        }
    }
    diagnostics
}

/// Canonicalize each changed URI to the form the publish cache is keyed by, so
/// the force-re-materialize check lines up with the per-file URIs. A document's
/// canonical form joins its primary root's canonical scan path (which differs
/// from the client-supplied folder key only under a symlink — issue 047).
pub fn canonicalize_changed_uris(
    workspaces: &Workspaces,
    changed_uris: &HashSet<String>,
) -> HashSet<String> {
    changed_uris
        .iter()
        .filter_map(|uri| {
            let abs = uri_to_path(uri);
            let root = workspaces.store.primary_root(&abs)?;
            let meta = workspaces.roots.get(&root)?;
            let rel = abs.strip_prefix(&root).ok()?;
            Some(path_to_uri(&meta.canonical_root.join(rel)))
        })
        .collect()
}

/// A file the publish detector decided to (re-)materialize: its fresh Lattice
/// and LSP vectors, plus whether the LSP form differs from what the client
/// holds.
pub struct Materialized {
    /// The publish-cache key (the client-spelling URI).
    uri: String,
    /// The freshly computed Lattice vector — the cheap change-detection key.
    lattice: Vec<Diagnostic>,
    /// Its UTF-16 materialization — what would go on the wire.
    lsp: Vec<lsp::Diagnostic>,
    /// Whether that materialization differs from the client's copy.
    send: bool,
}

/// Materialize one root's share of a publish pass: decide, per file, whether
/// its vector moved, and materialize only those that did.
///
/// Split out of [`diff_diagnostics_with`]'s phase 1 so the pass reads as its
/// three phases (detect / apply / clear) rather than one long loop.
#[allow(
    clippy::too_many_arguments,
    reason = "the per-root detection step genuinely needs the store, the root and its metadata, the scope, its computed rows, the force set, and both accumulators; bundling them into a struct would only rename the same parameters"
)]
pub fn materialize_root(
    workspaces: &Workspaces,
    root: &Path,
    meta: &RootMeta,
    scope: &PublishScope,
    mut by_file: BTreeMap<PathBuf, Vec<Diagnostic>>,
    changed_canonical: &HashSet<String>,
    present: &mut HashSet<String>,
    materialized: &mut Vec<Materialized>,
) {
    // Fallback index for the defensive unindexed-file path (the config
    // channel); real files use their own cached `line_index`.
    let empty = LineIndex::default();

    // The publish/cache URI is keyed by the client-supplied folder path
    // (`root`); the force-re-materialize check lines up with
    // `changed_canonical`, derived from the root's canonical scan path. The two
    // bases coincide unless the client opened the folder through a symlink;
    // when they differ, the comparison is run on the canonical root so a moved
    // diagnostic is not skipped (issue 047).
    let root_is_canonical = root == meta.canonical_root.as_path();

    // The config channel: one per-root URI outside the indexed markdown set
    // (decision 023 clause 4), admitted to the publish-diff cache only while
    // the root has a config state to report on — a marker on disk, or a
    // recorded load error. On a config delete or root deregistration it drops
    // out of `present`, so the phase-3 clear empties the client's copy.
    let config_channel =
        (meta.has_config || meta.config_error.is_some()).then(|| PathBuf::from(".lattice.toml"));

    // A scoped pass visits exactly the keys `root_desired_rows` produced (its
    // focus, plus the config channel when it committed); a full pass visits
    // every current document of the root — the saved membership plus any
    // buffer-only document, which is a member of its own perspective alone.
    let rels: Vec<PathBuf> = match scope {
        PublishScope::Full => workspaces
            .store
            .current_files(root)
            .into_keys()
            .chain(config_channel)
            .collect(),
        PublishScope::Only(_) => by_file.keys().cloned().collect(),
    };

    for rel_path in rels {
        let uri = path_to_uri(&root.join(&rel_path));
        if !present.insert(uri.clone()) {
            // Already claimed by a deeper root in this pass.
            continue;
        }

        let lattice = by_file.remove(&rel_path).unwrap_or_default();
        let cached = workspaces.published.get(&uri);
        let force = if root_is_canonical {
            changed_canonical.contains(&uri)
        } else {
            changed_canonical.contains(&path_to_uri(&meta.canonical_root.join(&rel_path)))
        };

        // Reuse the cached materialization when this file's source is unchanged
        // (it is not the edited file) and its Lattice vector still matches what
        // produced the cached LSP form.
        if !force {
            match cached {
                Some(prev) if prev.lattice == lattice => continue,
                None if lattice.is_empty() => continue,
                _ => {}
            }
        }

        // Materialize against the document's **current** text: an overlay
        // document's rows are anchored in its buffer, which is the text the
        // client is displaying.
        let fd = workspaces
            .store
            .current(&root.join(&rel_path))
            .map(|doc| &doc.data);
        let source = fd.map_or("", |fd| fd.tree.source());
        let index = fd.map_or(&empty, |fd| &fd.line_index);
        let lsp: Vec<lsp::Diagnostic> = lattice
            .iter()
            .map(|d| to_lsp_diagnostic(d, source, index))
            .collect();
        let send = cached.map_or(!lsp.is_empty(), |prev| prev.lsp != lsp);
        materialized.push(Materialized {
            uri,
            lattice,
            lsp,
            send,
        });
    }
}

/// [`diff_diagnostics`] with an explicit adjudication mode and publish scope.
///
/// Adjudication (decision 023): under [`Adjudication::Commit`] each root's
/// verdicts are re-adjudicated from the **saved world** and held; under
/// [`Adjudication::Held`] the last commitment's verdicts bind unchanged.
///
/// Scope (decision 024 clause 2): under [`PublishScope::Full`] every document
/// is recomputed and diffed; under [`PublishScope::Only`] exactly one
/// document's rows are, and every other cache entry is left untouched — a
/// buffer event cannot move another file's rows, so re-reading them would be
/// work with no possible result. The phase-3 "cleared files" sweep is likewise
/// skipped for a scoped pass: nothing left the workspace.
///
/// The per-file computation itself lives in [`root_desired_rows`].
pub fn diff_diagnostics_with(
    workspaces: &mut Workspaces,
    changed_uris: &HashSet<String>,
    adjudication: Adjudication,
    scope: &PublishScope,
) -> Vec<(String, Vec<lsp::Diagnostic>)> {
    // Count this whole-workspace recompute pass so tests can assert that a
    // batched watched-file notification collapses to one pass, not one per
    // changed file (ticket perf 07). Compiled out of release builds.
    #[cfg(test)]
    RECOMPUTE_COUNT.with(|count| count.set(count.get() + 1));

    let changed_canonical = canonicalize_changed_uris(workspaces, changed_uris);

    let mut materialized: Vec<Materialized> = Vec::new();
    let mut present: HashSet<String> = HashSet::new();
    // The verdicts a commitment pass re-adjudicated, applied to each root's
    // `RootMeta` after the immutable phase-1 borrow ends (decision 023).
    let mut new_verdicts: Vec<(PathBuf, OverrideVerdicts)> = Vec::new();

    // Phase 1 — detection. With an immutable view of the store and the published
    // cache, recompute each file's Lattice vector, decide whether it changed,
    // and materialize only the changed files. Collect owned results so the cache
    // can be mutated afterward.
    //
    // Deepest root first (reverse key order), and each absolute URI is claimed
    // by the first (deepest) root that indexes it: nested roots range-scan the
    // same absolute file, and letting both compute the same publish-cache key
    // makes successive passes alternate between the two roots' diagnostic sets
    // — the deeper one's vector one pass, the shallower's the next (issue 050's
    // flip-flop shape). The deepest root owning the URI matches how `resolve`
    // routes document events and how the test oracle `desired_diagnostics`
    // settles the same URI.
    for (root, meta) in workspaces.roots.iter().rev() {
        let (by_file, committed) = root_desired_rows(workspaces, root, meta, adjudication, scope);
        if let Some(verdicts) = committed {
            new_verdicts.push((root.clone(), verdicts));
        }
        materialize_root(
            workspaces,
            root,
            meta,
            scope,
            by_file,
            &changed_canonical,
            &mut present,
            &mut materialized,
        );
    }

    // Hold the commitment's fresh verdicts (decision 023): every publish until
    // the next commitment — the `didChange` drafts — filters through these.
    for (root, verdicts) in new_verdicts {
        if let Some(meta) = workspaces.roots.get_mut(&root) {
            meta.verdicts = verdicts;
        }
    }

    // Keyed by URI so the result is deterministically ordered.
    let mut to_send: BTreeMap<String, Vec<lsp::Diagnostic>> = BTreeMap::new();

    // Phase 2 — apply. Update only the changed entries in place; untouched files
    // keep their cache entries, so this stays O(changed), not O(workspace).
    for entry in materialized {
        if entry.send {
            to_send.insert(entry.uri.clone(), entry.lsp.clone());
        }
        if entry.lsp.is_empty() {
            workspaces.published.remove(&entry.uri);
        } else {
            workspaces.published.insert(
                entry.uri,
                PublishedDiagnostics {
                    lattice: entry.lattice,
                    lsp: entry.lsp,
                },
            );
        }
    }

    // Phase 3 — clear files that left the workspace (cached but no longer
    // present): send an empty vector and drop the entry. Only a full pass can
    // conclude a file is absent — a scoped pass never enumerated the others.
    if *scope == PublishScope::Full {
        let absent: Vec<String> = workspaces
            .published
            .keys()
            .filter(|uri| !present.contains(uri.as_str()))
            .cloned()
            .collect();
        for uri in absent {
            workspaces.published.remove(&uri);
            to_send.insert(uri, Vec::new());
        }
    }

    to_send.into_iter().collect()
}

// Counts `diff_diagnostics` invocations — one per whole-workspace recompute /
// publish pass — so tests can assert that a batched watched-file notification
// collapses N changed files into a single pass, not N (ticket perf 07).
// Compiled out of release builds.
#[cfg(test)]
thread_local! {
    pub static RECOMPUTE_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
