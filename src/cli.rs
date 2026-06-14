// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Command-line interface for Lattice.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// A markdown predicate linter and backlink reconciler.
#[derive(Debug, Parser)]
#[command(name = "lattice", version, about)]
pub struct Cli {
    /// Subcommand to execute.
    #[command(subcommand)]
    pub command: Command,
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
    /// it documents the `[external]` alias model, per-reference `exceptions`, and
    /// every `[policy]` knob without requiring the repository.
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

    use super::{CONFIG_REFERENCE, Cli};

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
