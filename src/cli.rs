// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Command-line interface for Lattice.

use std::path::PathBuf;
use std::sync::LazyLock;

use clap::{Parser, Subcommand};

/// A markdown predicate linter and backlink reconciler.
#[derive(Debug, Parser)]
#[command(name = "lattice", version = VERSION.as_str(), about)]
pub struct Cli {
    /// Subcommand to execute.
    #[command(subcommand)]
    pub command: Command,
}

/// The composed `--version` string, evaluated once.
///
/// clap's `version` wants a `'static` string; the composition is a runtime
/// `String` (it stitches the crate version with the build-time git suffix), so
/// it is memoized here and borrowed for `'static`. The composition logic itself
/// lives in the pure, unit-tested [`compose_version`].
static VERSION: LazyLock<String> = LazyLock::new(version_string);

/// Compose the `--version` string from its parts.
///
/// Pure and unit-tested so the format is verifiable without a build script.
/// `git_hash` is `None` for a build with no git information (a crates.io /
/// tarball build), in which case the bare crate version is returned. When a
/// hash is present it is shown in parentheses, with a `dirty` marker appended
/// whenever the working tree had uncommitted changes at build time:
///
/// - clean:  `lattice 0.1.0 (79f739a)`
/// - dirty:  `lattice 0.1.0 (79f739a-dirty)`
/// - no git: `lattice 0.1.0`
fn compose_version(crate_version: &str, git_hash: Option<&str>, dirty: bool) -> String {
    match git_hash {
        Some(hash) if dirty => format!("{crate_version} ({hash}-dirty)"),
        Some(hash) => format!("{crate_version} ({hash})"),
        None => crate_version.to_owned(),
    }
}

/// The composed `--version` string, fed to clap's `version` attribute.
///
/// Reads the git metadata stamped by `build.rs` (`LATTICE_GIT_HASH` /
/// `LATTICE_GIT_DIRTY`) via `option_env!`; both are absent for a build with no
/// git information, in which case [`compose_version`] yields the bare crate
/// version.
fn version_string() -> String {
    let git_hash = option_env!("LATTICE_GIT_HASH");
    let dirty = matches!(option_env!("LATTICE_GIT_DIRTY"), Some("1"));
    compose_version(env!("CARGO_PKG_VERSION"), git_hash, dirty)
}

/// Available subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Validate all markdown files in the workspace.
    ///
    /// Discovers the workspace root, loads configuration, scans all markdown
    /// files, and runs every validation check. Diagnostics are printed to
    /// stderr in `path:line: severity: message` format.
    ///
    /// Exit code is 0 when no errors are found (warnings are allowed),
    /// and 1 when any error-level diagnostic is present. Pass `--strict` to
    /// also fail on warnings.
    Lint {
        /// Directory to lint (defaults to the current working directory).
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Treat warnings as errors: exit non-zero if any warnings are found.
        ///
        /// By default only error-level diagnostics affect the exit code. With
        /// `--strict`, warning-level diagnostics (e.g. missing/stale
        /// backlinks, bare paths) also cause a non-zero exit — suitable for
        /// gating graph drift in CI and pre-commit hooks. Info/hint
        /// diagnostics never affect the exit code.
        #[arg(long, alias = "deny-warnings")]
        strict: bool,
        /// Suppress the trailing suppression ledger summary.
        ///
        /// By default `lattice lint` prints, after the diagnostics, a ledger of
        /// what was suppressed — by source (frontmatter exceptions, count-keys)
        /// and severity — so a turned-off blanket is never silent (decision
        /// 012). `--quiet` drops the ledger, leaving only the diagnostics, for
        /// machine-readable CI output.
        #[arg(long)]
        quiet: bool,
    },
    /// Start the LSP server on stdio.
    ///
    /// Publishes diagnostics on file open, save, and change.
    /// Diagnostic-only — no completions, hover, or other interactive features.
    Serve,
    /// Print the configuration reference.
    ///
    /// Running `lattice config` prints the full `.lattice.toml` and
    /// frontmatter-exceptions reference to stdout; `lattice config --help` and
    /// `lattice help config` surface the identical text (the same
    /// [`CONFIG_REFERENCE`] string backs both). The reference is self-contained:
    /// it states the reference-vs-example move test (decision 014), and documents
    /// the `[external]` alias model, per-reference `exceptions`, and every
    /// `[policy]` knob without requiring the repository.
    #[command(long_about = CONFIG_REFERENCE)]
    Config,
}

/// The self-contained configuration reference.
///
/// Backs both the `lattice config` handler (printed to stdout) and the
/// `config` subcommand's clap `long_about`, so `lattice config`,
/// `lattice config --help`, and `lattice help config` all surface the same
/// text from a single source of truth (issue 035). It must stay reconciled
/// with the real parser in [`crate::config`] — values and defaults here are
/// the ones that module enforces, not a paraphrase.
pub const CONFIG_REFERENCE: &str = "\
Lattice configuration reference
===============================

Lattice reads an optional `.lattice.toml` at the project root (discovered by
walking up from the linted path, stopping at the git root). Per-document
`exceptions` live in each file's YAML frontmatter. Everything below is the
defaults Lattice ships with — a config file only overrides what it names.


references vs. examples — the move test
---------------------------------------

Lattice flags path-shaped strings (bare, backticked, or quoted `.md` mentions).
Each has two honest dispositions, and one test decides which:

    A path-shaped mention is a REFERENCE if moving the target file would force
    you to update the mention. Otherwise it is an EXAMPLE — exempt it.

\"Would a move ripple here?\" is the same question as \"is this a graph edge?\" —
an edge is the maintenance obligation that breaks when its target moves. Apply
it per MENTION, not per file: the same file can be a link in a living index and
an example in a closed record. The judgment it localizes is one question — is
this document a maintained reference, or a frozen record? A historical note
(\"the drift was in `parser/README.md`\") is an example even though the file is
real, because you do not maintain a closed record against current paths.

The two outcomes map onto the mechanisms documented below:

  - YES, a move would force an update -> a REFERENCE:
      same repo     -> a markdown link (full reconciliation; the move is caught).
      another repo  -> `{Name}/path`  (the [external] alias model, existence-
                       checked across the graph boundary).
  - NO, a move would change nothing here -> an EXAMPLE / dead / historical
    mention -> exempt:
      a recurring known external-artifact filename -> the [graph] artifacts
                       glossary.
      a one-off, dead, or historical mention -> a frontmatter `exceptions`
                       entry, or a count-key for a document that path-quotes by
                       nature.

The dirty-lint preamble leads with this rule whenever a path-shaped diagnostic
fires, and each path-shaped message restates it tersely; the full statement
lives here.


[external] — cross-repo alias model
-----------------------------------

A `{Name}/path` citation (in backticks, quotes, or bare prose — never a
markdown link) is a cross-repo reference. It is checked existence-only against
an aliased directory: never read, parsed, indexed, or treated as a graph edge.

Table shape — `Name = \"dir\"`, one entry per alias:

    [external]
    Catenary = \"../Catenary\"      # relative: preferred sibling-checkout form
    HedgeMaze = \"/opt/HedgeMaze\"  # absolute: taken verbatim
    Archive = \"~/Projects/Archive\" # ~ expands against the home directory

A relative value resolves against the config file's directory; an absolute
value is taken verbatim; a `~`-leading value expands against the home
directory.

Four-state resolution of a `{Name}/path` reference:

  1. alias `Name` undefined                       -> exempt (no diagnostic)
  2. alias defined, its directory absent          -> exempt (no diagnostic)
  3. alias defined, directory present, file present -> valid (no diagnostic)
  4. alias defined, directory present, file missing -> stale reference

Existence-only and edge-free: tiers 1 and 2 degrade to exempt so a missing
sibling checkout never produces a false break; only a present alias directory
with a genuinely missing target (tier 4) is flagged.


[graph] artifacts — known external-filename glossary
----------------------------------------------------

The bare-filename sibling of the [external] alias model: a repo-level glossary
of known external host/plugin filenames (per-host agent-instruction and skill
files like `AGENTS.md`, `CLAUDE.md`) whose bare/backticked/quoted mentions name
an artifact in the installed HOST layout, not a document in this graph.

    [graph]
    artifacts = [\"AGENTS.md\", \"CLAUDE.md\", \"GEMINI.md\", \"SKILL.md\"]

  - Exact-match, repo-wide. A reference whose literal string is a glossary
    member is treated as outside the graph everywhere: never resolved,
    linkified, flagged, or edged — decision 010's exempt tier reached by bare
    filename instead of by `{Name}/…` alias, and not even existence-checked.
    `AGENTS.md` exempts the bare `AGENTS.md`; a path-qualified `dir/AGENTS.md`
    is a DIFFERENT reference and still draws its normal diagnostic.
  - Dark-matter only. Affects only the bare/backticked/quoted path scanners; an
    actual markdown link `[x](AGENTS.md)` is UNAFFECTED — it resolves and edges
    normally.
  - A vocabulary, not a reconciled suppression. Unlike exceptions and overrides
    the glossary is NOT reconciled — there is no \"unused artifact\" flag; you
    list host artifact names regardless of which appear in the current tree.
  - Ledger-visible. Artifact suppressions appear in the suppression ledger as
    their own source (one row per artifact name, aggregated repo-wide), so a
    swallowed make-it-a-link hint is never silent.


frontmatter `exceptions` — per-reference, reconciled
----------------------------------------------------

A path-shaped string that is deliberately not a live reference (a worked
example, a counterexample, a knowingly-dead link) can be excepted in the
document's own frontmatter — a block sibling to `backlinks`:

    ---
    exceptions:
      stale_references:
        \"tickets/acquire/DESIGN.md\": \"hypothetical path in the worked example\"
        \"{Catenary}/old/layout.md\": \"pre-refactor path, kept for the changelog\"
      bare_paths:
        \"README.md\": \"naming the file, deliberately not a link\"
    ---

  - Lint-namespaced. The two namespaces are `stale_references` and
    `bare_paths` — the path-shaped lints. A typo'd namespace is ignored.
  - Keyed by the literal reference string -> a REQUIRED reason. An empty
    reason is itself diagnosed: the reason is the surviving record of the
    excepted reference's intent.
  - `expect` semantics. Exceptions are reconciled, not silenced: an exception
    that matches nothing live (its reference gone, or now resolving) is flagged
    as an *unused exception* and its stored reason is echoed as an epitaph.
  - Per-reference only. No globs or wildcards; each key matches one reference.
  - `{Name}/…` keys flow through unchanged — an external-alias reference is
    excepted by its literal `{Name}/…` key.
  - A `Disabled` lint makes its exceptions inert: not consulted, not flagged.
  - Never auto-removed. Lattice flags; you decide whether to drop the
    exception or restore the reference.


frontmatter count-key — per-document residual, reconciled
---------------------------------------------------------

For a document that is path-quoting by nature (a migration table, a sweep
audit, a frozen archive) where enumerating one literal key per non-reference
is noise, an all-digits key under a lint namespace is a count SENTINEL, not a
reference:

    ---
    exceptions:
      stale_references:
        \"{Catenary}/old/layout.md\": \"a kept literal, carved out first\"
        31: \"consolidation migration table — every path is a record, not a live reference\"
    ---

  - Shape, not value. A key matching `^[0-9]+$` (`31`, or quoted `\"31\"`) is the
    sentinel; any path-shaped key (a name, slash, or `#`) is a literal. No real
    reference is named `31`, so `31.md` and `a/31` stay literal references.
  - Residual semantics. The sentinel `N` claims the lint's RESIDUAL — its live
    diagnostics minus those already carved out by literal keys (literals win,
    subtracted first). Let the residual be `M`.
  - Tripwire, not silence. `M == N` suppresses the whole residual under the one
    shared reason. `M != N` makes the sentinel inert: every residual diagnostic
    resurfaces, plus one warning on the key — `expected N here, found M`. Adding
    a genuinely-broken reference bumps the count and shows you the new one.
  - A REQUIRED reason, `N >= 1`, at most one sentinel per namespace; a `Disabled`
    lint makes the sentinel inert (no suppression, no flag).


[[override]] — per-subtree policy
---------------------------------

A `[[override]]` array-of-tables sets the 028-family policy
(`stale_references` / `bare_paths`) per path-glob, generalizing the repo-wide
[policy] knob to a subtree. Globs are matched against workspace-relative paths.

    [[override]]
    paths = [\"archive/**\", \"*_bak.md\"]
    stale_references = \"disabled\"          # freeze
    hint = \"frozen/superseded docs; refs rotted after the live docs moved on\"

    [[override]]
    paths = [\"tickets/sweep/**\"]
    stale_references = { expect = 40 }     # tripwire
    hint = \"sweep-audit tickets that quote dead paths as their subject\"

  - A per-lint key is EITHER a level string (`hint` / `warn` / `deny` /
    `disabled`) OR an inline `{ expect = N }` table — two distinct mechanisms.
  - Level string = a PER-FILE policy override. A matching file resolves that
    lint to this level instead of the repo-wide one; the level may LOWER or RAISE
    (e.g. `warn` -> `deny` on a strict subtree). `disabled` is the deliberate
    FREEZE (lint off for those files, no reconciliation).
  - `{ expect = N }` = a WORKSPACE-AGGREGATE tripwire. The lint stays at its base
    level per file, but across all files the glob matches, the live diagnostics
    of that lint are summed: total `== N` suppresses them all (one ledger row);
    total `!= N` resurfaces them all plus one drift flag naming the override.
  - `hint` is optional (no required reason — at config grain the honesty comes
    from the ledger, not a forced justification).
  - Last match wins when two entries match the same file. A frontmatter
    declaration (a per-reference exception or count-key) wins over any override
    on the same file.
  - Unused-override. A glob matching zero files is flagged — the config analogue
    of the unused-exception, catching a stale override after a tree was renamed.


suppression ledger (lattice lint)
---------------------------------

After the diagnostics, `lattice lint` prints a ledger of what was suppressed —
by source (frontmatter exceptions, count-keys, subtree overrides) and severity —
so a turned-off blanket is never silent. Each override row is labelled by glob
and `override (freeze)` or `override (expect=N)`. `--quiet` drops the ledger for
CI.


[policy] — lint knobs
---------------------

Each line below is `key  accepted-values  (default)`:

    predicates           optional | required                  (optional)
    backlinks            true | false                         (true)
    bare_paths           warn | deny | disabled               (warn)
    stale_references     hint | warn | deny | disabled        (warn)
    fragments            github | gitlab | vscode  (omitted: try all algorithms)
    admonitions          portable | github | gitlab | disabled (portable)
    code_block_language  hint | warn | deny | disabled        (disabled)
    connectivity         off | no-orphans | no-islands | reachable (off)
    roots                list of paths, e.g. [\"README.md\"]    ([\"README.md\"])

Opt-in convention checks (off by default — they flag valid CommonMark, not
defects, per decision 009):

    multiple_h1            true | false                       (false)
    skipped_heading_level  true | false                       (false)
    image_empty_alt        true | false                       (false)

Example `[policy]` block:

    [policy]
    predicates = \"required\"
    bare_paths = \"deny\"
    stale_references = \"warn\"
    connectivity = \"reachable\"
    roots = [\"README.md\"]
";

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for clarity")]
mod tests {
    use clap::CommandFactory;

    use super::{CONFIG_REFERENCE, Cli, compose_version};

    #[test]
    fn compose_version_clean_shows_hash_without_marker() {
        assert_eq!(
            compose_version("0.1.0", Some("79f739a"), false),
            "0.1.0 (79f739a)",
            "a clean build shows the short hash in parentheses, no dirty marker"
        );
    }

    #[test]
    fn compose_version_dirty_shows_dirty_marker() {
        let composed = compose_version("0.1.0", Some("79f739a"), true);
        assert_eq!(
            composed, "0.1.0 (79f739a-dirty)",
            "a dirty build shows the short hash with a dirty marker"
        );
        assert!(
            composed.contains("dirty"),
            "the dirty marker must be present for an uncommitted tree: {composed}"
        );
    }

    #[test]
    fn compose_version_no_git_falls_back_to_bare_version() {
        // A crates.io / tarball build has no git info; the dirty flag is
        // irrelevant and must not leak into the bare crate version.
        assert_eq!(
            compose_version("0.1.0", None, false),
            "0.1.0",
            "no git info falls back to the bare crate version"
        );
        assert_eq!(
            compose_version("0.1.0", None, true),
            "0.1.0",
            "the dirty flag is ignored when there is no git hash"
        );
    }

    #[test]
    fn version_contains_crate_version() {
        // The clap-rendered `--version` always carries the crate version,
        // whatever git suffix the build stamped on (or didn't).
        let version = Cli::command().render_version();
        assert!(
            version.contains(env!("CARGO_PKG_VERSION")),
            "--version output contains the crate version: {version}"
        );
    }

    /// The top-level `--help` / `help` output, as clap renders it.
    fn top_level_help() -> String {
        Cli::command().render_help().to_string()
    }

    /// The `config` subcommand's long help (`config --help` / `help config`).
    fn config_long_help() -> String {
        Cli::command()
            .find_subcommand_mut("config")
            .expect("config subcommand is registered")
            .render_long_help()
            .to_string()
    }

    #[test]
    fn help_lists_config_under_commands() {
        let help = top_level_help();
        assert!(
            help.contains("Commands:"),
            "top-level help has a Commands block: {help}"
        );
        assert!(
            help.contains("config"),
            "config is listed in the top-level Commands block (discoverability is the point): {help}"
        );
    }

    #[test]
    fn config_is_not_hidden() {
        // A hidden subcommand would not render in the Commands block; assert it
        // appears alongside the always-visible siblings so it is discoverable.
        let help = top_level_help();
        assert!(
            help.contains("lint") && help.contains("serve") && help.contains("config"),
            "config sits among the visible subcommands, not hidden: {help}"
        );
    }

    #[test]
    fn config_long_help_covers_the_three_sections() {
        // `config --help` / `help config` route through clap's long_about,
        // which is CONFIG_REFERENCE.
        let help = config_long_help();
        assert!(
            help.contains("[external]"),
            "config --help documents the [external] alias model: {help}"
        );
        assert!(
            help.contains("exceptions"),
            "config --help documents the per-reference exceptions block: {help}"
        );
        assert!(
            help.contains("[policy]"),
            "config --help documents the [policy] knobs: {help}"
        );
    }

    #[test]
    fn reference_documents_the_move_test_and_disposition_map() {
        // Issue 039 / decision 014: the reference states the move test and the
        // disposition map the preamble and per-message text point at.
        assert!(
            CONFIG_REFERENCE.contains("references vs. examples")
                && CONFIG_REFERENCE.contains("moving the target file would force"),
            "the reference states the move test as a named section"
        );
        // The disposition map: link (same repo) / {Name}/path (cross-repo) /
        // artifact glossary (external filename) / exception·count-key (one-off).
        assert!(
            CONFIG_REFERENCE.contains("markdown link")
                && CONFIG_REFERENCE.contains("{Name}/path")
                && CONFIG_REFERENCE.contains("artifacts")
                && CONFIG_REFERENCE.contains("count-key"),
            "the reference enumerates the four dispositions the move test selects among"
        );
        assert!(
            CONFIG_REFERENCE.contains("per MENTION, not per file"),
            "the reference notes the rule applies per mention, not per file"
        );
    }

    #[test]
    fn reference_documents_external_alias_model() {
        assert!(
            CONFIG_REFERENCE.contains("Name = \"dir\"") && CONFIG_REFERENCE.contains("{Name}/path"),
            "the reference shows the table shape and the {{Name}}/path syntax"
        );
        // The four-state resolution: exempt (undefined), exempt (dir absent),
        // valid (present), stale (missing).
        assert!(
            CONFIG_REFERENCE.contains("exempt")
                && CONFIG_REFERENCE.contains("valid")
                && CONFIG_REFERENCE.contains("stale reference"),
            "the reference enumerates the four-state resolution"
        );
        assert!(
            CONFIG_REFERENCE.contains("Existence-only") && CONFIG_REFERENCE.contains("edge-free"),
            "the reference notes the existence-only, edge-free contract"
        );
    }

    #[test]
    fn reference_documents_artifact_glossary() {
        // Issue 038: the [graph] artifacts glossary — the bare-filename external
        // model. Exact-match / repo-wide, dark-matter-only, ledger-visible.
        assert!(
            CONFIG_REFERENCE.contains("[graph] artifacts")
                && CONFIG_REFERENCE.contains("artifacts = ["),
            "the reference shows the [graph] artifacts table"
        );
        assert!(
            CONFIG_REFERENCE.contains("Exact-match, repo-wide")
                && CONFIG_REFERENCE.contains("dir/AGENTS.md"),
            "the reference states exact-match / repo-wide and the path-qualified counterexample"
        );
        assert!(
            CONFIG_REFERENCE.contains("Dark-matter only")
                && CONFIG_REFERENCE.contains("UNAFFECTED"),
            "the reference notes dark-matter-only and that markdown links are unaffected"
        );
        assert!(
            CONFIG_REFERENCE.contains("Ledger-visible"),
            "the reference notes the ledger visibility"
        );
    }

    #[test]
    fn reference_documents_exceptions_required_reason_and_expect() {
        assert!(
            CONFIG_REFERENCE.contains("REQUIRED reason")
                && CONFIG_REFERENCE.contains("reason is itself diagnosed"),
            "the reference names the required reason and the empty-reason diagnostic"
        );
        assert!(
            CONFIG_REFERENCE.contains("expect")
                && CONFIG_REFERENCE.contains("unused exception")
                && CONFIG_REFERENCE.contains("epitaph"),
            "the reference describes the expect/epitaph behaviour"
        );
        assert!(
            CONFIG_REFERENCE.contains("Lint-namespaced")
                && CONFIG_REFERENCE.contains("stale_references")
                && CONFIG_REFERENCE.contains("bare_paths"),
            "the reference describes the lint namespacing"
        );
        assert!(
            CONFIG_REFERENCE.contains("Disabled")
                && CONFIG_REFERENCE.contains("Never auto-removed"),
            "the reference notes the disabled-lint and never-auto-removed rules"
        );
    }

    #[test]
    fn reference_documents_count_key_and_ledger() {
        // Issue 036: the count-key sentinel and the suppression ledger, which
        // the new diagnostics point at via `lattice help config`.
        assert!(
            CONFIG_REFERENCE.contains("count SENTINEL")
                && CONFIG_REFERENCE.contains("^[0-9]+$")
                && CONFIG_REFERENCE.contains("RESIDUAL"),
            "the reference documents the all-digits count-key sentinel and residual"
        );
        assert!(
            CONFIG_REFERENCE.contains("expected N here, found M"),
            "the reference describes the count-key drift tripwire"
        );
        assert!(
            CONFIG_REFERENCE.contains("suppression ledger") && CONFIG_REFERENCE.contains("--quiet"),
            "the reference documents the suppression ledger and --quiet"
        );
    }

    #[test]
    fn reference_documents_subtree_override() {
        // Issue 037: the [[override]] grammar — both mechanisms (level / expect),
        // last-match-wins, frontmatter precedence, and the unused-override flag.
        assert!(
            CONFIG_REFERENCE.contains("[[override]]") && CONFIG_REFERENCE.contains("expect = N"),
            "the reference shows the [[override]] table and the expect form"
        );
        assert!(
            CONFIG_REFERENCE.contains("PER-FILE policy override")
                && CONFIG_REFERENCE.contains("WORKSPACE-AGGREGATE tripwire"),
            "the reference distinguishes the level and expect mechanisms"
        );
        assert!(
            CONFIG_REFERENCE.contains("Last match wins")
                && CONFIG_REFERENCE.contains("wins over any override"),
            "the reference documents last-match-wins and frontmatter precedence"
        );
        assert!(
            CONFIG_REFERENCE.contains("Unused-override"),
            "the reference documents the unused-override flag"
        );
        assert!(
            CONFIG_REFERENCE.contains("override (freeze)")
                && CONFIG_REFERENCE.contains("override (expect=N)"),
            "the ledger section documents the override row labels"
        );
    }

    #[test]
    fn reference_documents_every_policy_knob_with_defaults() {
        // Each knob, its accepted values, and its default — reconciled against
        // crate::config, not memory.
        for (knob, values, default) in [
            ("predicates", "optional | required", "(optional)"),
            ("backlinks", "true | false", "(true)"),
            ("bare_paths", "warn | deny | disabled", "(warn)"),
            (
                "stale_references",
                "hint | warn | deny | disabled",
                "(warn)",
            ),
            ("fragments", "github | gitlab | vscode", "try all"),
            (
                "admonitions",
                "portable | github | gitlab | disabled",
                "(portable)",
            ),
            (
                "code_block_language",
                "hint | warn | deny | disabled",
                "(disabled)",
            ),
            (
                "connectivity",
                "off | no-orphans | no-islands | reachable",
                "(off)",
            ),
            ("multiple_h1", "true | false", "(false)"),
            ("skipped_heading_level", "true | false", "(false)"),
            ("image_empty_alt", "true | false", "(false)"),
        ] {
            assert!(
                CONFIG_REFERENCE.contains(knob),
                "the reference names the `{knob}` knob"
            );
            assert!(
                CONFIG_REFERENCE.contains(values),
                "the reference lists `{knob}` values: {values}"
            );
            assert!(
                CONFIG_REFERENCE.contains(default),
                "the reference gives the `{knob}` default: {default}"
            );
        }
        assert!(
            CONFIG_REFERENCE.contains("roots"),
            "the reference names the `roots` knob"
        );
    }
}
