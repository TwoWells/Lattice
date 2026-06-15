// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Configuration loading and predicate vocabulary.
//!
//! Loads `.lattice.toml` if present, merging with built-in defaults.
//! Produces a resolved [`Config`] consumed by the rest of the system.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use glob::Pattern;
use serde::Deserialize;
use thiserror::Error;

/// Errors that can occur when loading configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Failed to read the config file from disk.
    #[error("failed to read {path}: {source}")]
    Read {
        /// Path that could not be read.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },

    /// Config file is not valid TOML.
    #[error("failed to parse {path}: {source}")]
    Parse {
        /// Path that could not be parsed.
        path: PathBuf,
        /// TOML parse error.
        source: toml::de::Error,
    },

    /// Config file has valid TOML but invalid values.
    #[error("invalid config in {path}: {message}")]
    Invalid {
        /// Path containing the invalid config.
        path: PathBuf,
        /// What is wrong.
        message: String,
    },
}

/// Whether links must carry an explicit predicate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PredicatePolicy {
    /// Links without a predicate default to `references`.
    #[default]
    Optional,
    /// Every link must have an explicit predicate.
    Required,
}

/// How bare file paths in prose are handled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BarePathPolicy {
    /// Bare paths produce warnings.
    #[default]
    Warn,
    /// Bare paths produce errors.
    Deny,
    /// Bare path detection is off.
    Disabled,
}

/// Admonition syntax policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AdmonitionPolicy {
    /// Non-`<div class>` admonition syntax gets a hint suggesting portable equivalent.
    #[default]
    Portable,
    /// GitHub `> [!NOTE]` syntax accepted; other flavors flagged.
    Github,
    /// GitLab `:::` syntax accepted; other flavors flagged.
    Gitlab,
    /// No admonition diagnostics.
    Disabled,
}

/// Code block language tag policy.
///
/// Per decision 009 the default is [`Disabled`](Self::Disabled): an untagged
/// fence is valid `CommonMark` with a render-neutral non-fix, so it produces no
/// diagnostic by default. Opt in via `[policy] code_block_language = "hint"`
/// (or `"warn"` / `"deny"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CodeBlockLanguagePolicy {
    /// Code blocks without language tags get a hint.
    Hint,
    /// Warning severity.
    Warn,
    /// Error severity.
    Deny,
    /// No diagnostic (default).
    #[default]
    Disabled,
}

/// Stale path-shaped reference policy.
///
/// Controls the diagnostic for a `.md`-shaped reference (backtick or bare,
/// `#fragment` stripped) that resolves to **no file** — the missing-quadrant
/// mirror of the `link target does not exist` error (issue 028). Decoupled
/// from [`BarePathPolicy`]: "don't nudge me to linkify bare paths" and "don't
/// tell me my references dangle" are different wants.
///
/// Per issue 028 the default is [`Warn`](Self::Warn): a dangling reference is a
/// defect, and the resolving sibling already warns. Set via
/// `[policy] stale_references = "warn"` (or `"hint"` / `"deny"` / `"disabled"`).
/// [`Disabled`](Self::Disabled) suppresses only this diagnostic; the
/// make-it-a-link resolve hint (gated by [`BarePathPolicy`]) still fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StaleReferencePolicy {
    /// Dangling `.md` references get a hint.
    Hint,
    /// Warning severity (default).
    #[default]
    Warn,
    /// Error severity.
    Deny,
    /// No diagnostic.
    Disabled,
}

/// Graph connectivity (topology) check level.
///
/// An escalating ladder where each level flags a superset of the previous
/// one (`no-orphans ⊆ no-islands ⊆ reachable`), given the uniform root
/// exemption. See issue 018.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnectivityPolicy {
    /// No connectivity check (default).
    #[default]
    Off,
    /// Flag any non-root document with no intra-project edge (degree 0).
    NoOrphans,
    /// Flag any non-root document outside a root's connected component
    /// (edges treated as undirected).
    NoIslands,
    /// Flag any non-root document not forward-reachable from a root.
    Reachable,
}

/// Slug algorithm for heading-fragment validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FragmentAlgorithm {
    /// GitHub slug convention.
    Github,
    /// GitLab slug convention.
    Gitlab,
    /// VS Code slug convention.
    Vscode,
}

/// Policy settings that control diagnostic behavior.
#[allow(
    clippy::struct_excessive_bools,
    reason = "each bool is an independent on/off toggle for a distinct opt-in diagnostic, not a state machine"
)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// Whether predicates are required on links.
    pub predicates: PredicatePolicy,
    /// Whether backlink consistency is checked.
    pub backlinks: bool,
    /// How bare paths are handled.
    pub bare_paths: BarePathPolicy,
    /// How stale (dangling) `.md`-shaped references are handled.
    pub stale_references: StaleReferencePolicy,
    /// Slug algorithm for fragment validation. `None` tries all.
    pub fragments: Option<FragmentAlgorithm>,
    /// Admonition syntax policy.
    pub admonitions: AdmonitionPolicy,
    /// Code block language tag policy.
    pub code_block_language: CodeBlockLanguagePolicy,
    /// Whether to flag multiple H1 headings in one document.
    ///
    /// A convention check, not a defect: multiple H1s are valid `CommonMark`
    /// and render fine. Per decision 009 it is opt-in (default `false`); enable
    /// with `[policy] multiple_h1 = true`.
    pub multiple_h1: bool,
    /// Whether to flag a skipped heading level (e.g. H1 directly to H3).
    ///
    /// A convention check, not a defect: skipped levels are valid `CommonMark`
    /// and render fine. Per decision 009 it is opt-in (default `false`);
    /// enable with `[policy] skipped_heading_level = true`.
    pub skipped_heading_level: bool,
    /// Whether to flag images with empty alt text.
    ///
    /// A convention check, not a defect: empty alt text is valid (and the
    /// correct choice for decorative images). Per decision 009 it is opt-in
    /// (default `false`); enable with `[policy] image_empty_alt = true`.
    pub image_empty_alt: bool,
    /// Graph connectivity (topology) check level.
    pub connectivity: ConnectivityPolicy,
    /// Entry-point documents for connectivity checks.
    ///
    /// Workspace-relative paths; normalized and matched against indexed files
    /// at check time. Roots are exempt from connectivity flagging at every
    /// level and anchor the `no-islands`/`reachable` traversals. Defaults to
    /// the workspace-root `README.md`.
    pub roots: Vec<PathBuf>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            predicates: PredicatePolicy::Optional,
            backlinks: true,
            bare_paths: BarePathPolicy::Warn,
            stale_references: StaleReferencePolicy::default(),
            fragments: None,
            admonitions: AdmonitionPolicy::default(),
            code_block_language: CodeBlockLanguagePolicy::default(),
            multiple_h1: false,
            skipped_heading_level: false,
            image_empty_alt: false,
            connectivity: ConnectivityPolicy::default(),
            roots: vec![PathBuf::from("README.md")],
        }
    }
}

/// How a `[[override]]` entry sets one 028-family lint for its glob (decision
/// 012 part 2, issue 037).
///
/// A per-lint key in an override is **either** a level string **or** an inline
/// `{ expect = N }` table — two distinct mechanisms:
///
/// - [`Level`](Self::Level) is a **per-file policy override**: a file matching
///   the glob resolves that lint to this level *instead of* the repo-wide one
///   (it may lower or raise — `warn` → `deny`). [`disabled`](BarePathPolicy::Disabled)
///   is the deliberate *freeze* (lint off for those files, no reconciliation).
/// - [`Expect`](Self::Expect) is a **workspace-level aggregate tripwire**: the
///   lint stays at its base level per file, but across *all* files the override
///   matches, the live diagnostics of that lint (after frontmatter carve-outs)
///   are summed — total `== N` suppresses them all (they become ledger rows),
///   total `!= N` resurfaces them all plus one drift flag naming the override.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleReferenceOverride {
    /// A per-file level override (lowers or raises, including the `disabled`
    /// freeze).
    Level(StaleReferencePolicy),
    /// A workspace-aggregate `{ expect = N }` tripwire over the override's glob.
    Expect(usize),
}

/// How a `[[override]]` entry sets the `bare_paths` lint for its glob.
///
/// The `bare_paths` counterpart of [`StaleReferenceOverride`]: a per-file level
/// override (lower/raise/freeze) or a workspace-aggregate `{ expect = N }`
/// tripwire. See [`StaleReferenceOverride`] for the two-mechanism semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarePathOverride {
    /// A per-file level override.
    Level(BarePathPolicy),
    /// A workspace-aggregate `{ expect = N }` tripwire over the override's glob.
    Expect(usize),
}

/// A `[[override]]` entry from `.lattice.toml` (decision 012 part 2, issue 037).
///
/// Sets the 028-family policy (`stale_references` / `bare_paths`) per path-glob,
/// generalizing the repo-wide knob to per-path. A file matching one of the
/// entry's [`paths`](Self::paths) globs resolves the named lints to the entry's
/// policy instead of the repo-wide one. When two entries match the same file the
/// **last** one wins (decision 012). A **frontmatter** declaration (per-reference
/// exception or count-key) wins over any override on the same file.
#[derive(Debug, Clone)]
pub struct Override {
    /// The compiled globs, matched against workspace-relative paths.
    pub paths: Vec<Pattern>,
    /// The globs as written, for the unused-override / ledger messages.
    pub raw_paths: Vec<String>,
    /// The `stale_references` mode this entry sets, if any.
    pub stale_references: Option<StaleReferenceOverride>,
    /// The `bare_paths` mode this entry sets, if any.
    pub bare_paths: Option<BarePathOverride>,
    /// The optional hint (no required reason — decision 012; honesty is the
    /// ledger).
    pub hint: Option<String>,
}

impl Override {
    /// Whether any of this entry's globs matches the workspace-relative
    /// `rel_path`.
    #[must_use]
    pub fn matches(&self, rel_path: &Path) -> bool {
        self.paths.iter().any(|p| p.matches_path(rel_path))
    }

    /// A display label for diagnostics and the ledger — the entry's globs joined
    /// by `, ` (e.g. `archive/**, *_bak.md`).
    #[must_use]
    pub fn label(&self) -> String {
        self.raw_paths.join(", ")
    }

    /// The optional hint as a trailing ` — <hint>` suffix for ledger rows and
    /// messages, or the empty string when no hint was declared.
    ///
    /// Decision 012 makes the hint the config-grain honesty signal: it is
    /// surfaced at lint time (in the ledger and in the override flags) rather
    /// than enforced as a required reason. This is the single owner of that
    /// rendering.
    #[must_use]
    pub fn hint_suffix(&self) -> String {
        self.hint
            .as_deref()
            .map_or_else(String::new, |h| format!(" — {h}"))
    }
}

/// Resolved Lattice configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Predicate vocabulary: forward predicate → inverse predicate.
    pub predicates: BTreeMap<String, String>,
    /// Policy settings.
    pub policy: Policy,
    /// External formatter command for `textDocument/formatting`.
    pub format_command: Option<String>,
    /// External-namespace aliases: alias name → resolved directory.
    ///
    /// Populated from the `[external]` table in `.lattice.toml` (issue 030,
    /// decision 010). Each value is resolved to an absolute path at load time:
    /// a relative value (the preferred sibling-checkout form, e.g.
    /// `../Catenary`) against the config file's directory, a `~`-leading value
    /// against the home directory, and an absolute value verbatim. A
    /// `{Name}/path` citation is checked **existence-only** against the matching
    /// alias directory — never read, parsed, indexed, or treated as a graph edge.
    /// An undefined alias, or one whose directory is absent, degrades to exempt.
    pub external: BTreeMap<String, PathBuf>,
    /// Per-subtree policy overrides (decision 012 part 2, issue 037).
    ///
    /// Each `[[override]]` entry sets the 028-family policy
    /// (`stale_references` / `bare_paths`) for its path-globs, in source order;
    /// when two entries match the same file the last one wins. A frontmatter
    /// declaration on a file wins over any override matching it.
    pub overrides: Vec<Override>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            predicates: default_predicates(),
            policy: Policy::default(),
            format_command: None,
            external: BTreeMap::new(),
            overrides: Vec::new(),
        }
    }
}

impl Config {
    /// Load configuration by searching upward from `start` for `.lattice.toml`.
    ///
    /// Returns defaults when no config file is found. The search stops at the
    /// nearest git root (directory containing `.git`) or the filesystem root.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if a config file is found but cannot be read,
    /// parsed, or contains invalid values.
    pub fn load(start: &Path) -> Result<Self, ConfigError> {
        let Some(path) = find_config_file(start) else {
            return Ok(Self::default());
        };

        let contents = std::fs::read_to_string(&path).map_err(|e| ConfigError::Read {
            path: path.clone(),
            source: e,
        })?;

        let raw: RawConfig = toml::from_str(&contents).map_err(|e| ConfigError::Parse {
            path: path.clone(),
            source: e,
        })?;

        Self::from_raw(raw, path)
    }

    /// Build a resolved config from raw TOML values, merging with defaults.
    fn from_raw(raw: RawConfig, path: PathBuf) -> Result<Self, ConfigError> {
        let mut config = Self::default();

        if let Some(predicates) = raw.predicates {
            for (forward, inverse) in &predicates {
                if inverse.is_empty() {
                    return Err(ConfigError::Invalid {
                        path,
                        message: format!("predicate '{forward}' has an empty inverse"),
                    });
                }
            }
            for (forward, inverse) in predicates {
                config.predicates.insert(forward, inverse);
            }
        }

        if let Some(policy) = raw.policy {
            apply_policy(&mut config.policy, policy, &path)?;
        }

        if let Some(format) = raw.format {
            config.format_command = format.command;
        }

        if let Some(external) = raw.external {
            let base_dir = path.parent().unwrap_or_else(|| Path::new(""));
            for (alias, dir) in external {
                config
                    .external
                    .insert(alias, resolve_alias_dir(&dir, base_dir));
            }
        }

        if let Some(overrides) = raw.overrides {
            for raw_override in overrides {
                config.overrides.push(parse_override(raw_override, &path)?);
            }
        }

        Ok(config)
    }

    /// Returns `true` if `predicate` is a known forward predicate.
    pub fn is_known_forward(&self, predicate: &str) -> bool {
        self.predicates.contains_key(predicate)
    }

    /// Returns `true` if `predicate` is a known inverse predicate.
    pub fn is_known_inverse(&self, predicate: &str) -> bool {
        self.predicates.values().any(|v| v == predicate)
    }

    /// Returns the inverse for a forward predicate.
    pub fn inverse_of(&self, forward: &str) -> Option<&str> {
        self.predicates.get(forward).map(String::as_str)
    }

    /// Returns the forward predicate for an inverse predicate.
    pub fn forward_of(&self, inverse: &str) -> Option<&str> {
        self.predicates
            .iter()
            .find(|(_, v)| v.as_str() == inverse)
            .map(|(k, _)| k.as_str())
    }

    /// Returns `true` if `predicate` is a known forward *or* inverse predicate.
    ///
    /// Decision 008 lifts the direction restriction: a link or backlink may
    /// name either member of a vocabulary pair. The closed vocabulary remains
    /// the floor — a string in neither direction is still unknown.
    pub fn is_known_predicate(&self, predicate: &str) -> bool {
        self.is_known_forward(predicate) || self.is_known_inverse(predicate)
    }

    /// Returns the opposite member of `predicate`'s vocabulary pair.
    ///
    /// For a forward predicate this is its inverse; for an inverse predicate,
    /// the forward. Returns `None` when `predicate` belongs to neither
    /// direction. The opposite is the label a forward link derives on its
    /// target's backlinks, so an inverse-predicate link (`"superseded_by"`)
    /// derives the forward label (`"supersedes"`).
    ///
    /// Assumes forward keys and inverse values are disjoint (sane configs);
    /// on the unlikely overlap the inverse reading wins.
    pub fn opposite_of(&self, predicate: &str) -> Option<&str> {
        self.inverse_of(predicate)
            .or_else(|| self.forward_of(predicate))
    }

    /// Resolve the effective `Policy` for the file at workspace-relative
    /// `rel_path`, applying any matching subtree overrides (issue 037).
    ///
    /// Only the 028-family levels (`stale_references` / `bare_paths`) are
    /// adjusted, and only by an override entry that sets that lint as a **level**
    /// ([`StaleReferenceOverride::Level`] / [`BarePathOverride::Level`]). An
    /// `{ expect = N }` mode is a workspace-level aggregate, not a per-file level,
    /// so it leaves the per-file policy at its base value — the lint stays active
    /// at its repo-wide level and its live diagnostics are summed by the lint
    /// loop's expect pass instead. Last matching entry wins (decision 012).
    ///
    /// Every other policy field is copied from the repo-wide policy unchanged.
    #[must_use]
    pub fn effective_policy(&self, rel_path: &Path) -> Policy {
        let mut policy = self.policy.clone();
        // Resolve each 028-family lint independently by the LAST matching entry
        // that names it (decision 012's last-match-wins). A later `{ expect = N }`
        // entry beats an earlier level entry for the same lint: expect keeps the
        // lint at its base level (the aggregate is the lint loop's concern), so a
        // winning expect must reset the level even if an earlier entry lowered or
        // raised it. Walking forward and overwriting on each match yields exactly
        // "last winner".
        for ov in &self.overrides {
            if !ov.matches(rel_path) {
                continue;
            }
            match ov.stale_references {
                Some(StaleReferenceOverride::Level(level)) => policy.stale_references = level,
                Some(StaleReferenceOverride::Expect(_)) => {
                    policy.stale_references = self.policy.stale_references;
                }
                None => {}
            }
            match ov.bare_paths {
                Some(BarePathOverride::Level(level)) => policy.bare_paths = level,
                Some(BarePathOverride::Expect(_)) => {
                    policy.bare_paths = self.policy.bare_paths;
                }
                None => {}
            }
        }
        policy
    }
}

// --- Raw deserialization types ---

#[derive(Debug, Deserialize)]
struct RawConfig {
    predicates: Option<HashMap<String, String>>,
    policy: Option<RawPolicy>,
    format: Option<RawFormat>,
    external: Option<HashMap<String, String>>,
    #[serde(rename = "override")]
    overrides: Option<Vec<RawOverride>>,
}

#[derive(Debug, Deserialize)]
struct RawFormat {
    command: Option<String>,
}

/// A raw `[[override]]` array-of-tables entry (issue 037).
#[derive(Debug, Deserialize)]
struct RawOverride {
    paths: Vec<String>,
    stale_references: Option<RawOverrideLint>,
    bare_paths: Option<RawOverrideLint>,
    hint: Option<String>,
}

/// A per-lint override value: a level string (`"disabled"` etc.) **or** an
/// inline `{ expect = N }` table. The two are the distinct mechanisms of issue
/// 037 (per-file level vs workspace-aggregate tripwire), disambiguated by serde
/// shape: a TOML string vs an inline table.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawOverrideLint {
    /// A level string.
    Level(String),
    /// An `{ expect = N }` aggregate tripwire.
    Expect {
        /// The expected aggregate live-diagnostic count over the glob.
        expect: usize,
    },
}

#[derive(Debug, Deserialize)]
struct RawPolicy {
    predicates: Option<String>,
    backlinks: Option<bool>,
    bare_paths: Option<String>,
    stale_references: Option<String>,
    fragments: Option<String>,
    admonitions: Option<String>,
    code_block_language: Option<String>,
    multiple_h1: Option<bool>,
    skipped_heading_level: Option<bool>,
    image_empty_alt: Option<bool>,
    connectivity: Option<String>,
    roots: Option<Vec<String>>,
}

// --- Helpers ---

/// Built-in predicate vocabulary.
fn default_predicates() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("amends".into(), "amended_by".into()),
        ("blocks".into(), "blocked_by".into()),
        ("depends_on".into(), "dependency_of".into()),
        ("implements".into(), "implemented_by".into()),
        ("imports".into(), "imported_by".into()),
        ("references".into(), "referenced_by".into()),
        ("supersedes".into(), "superseded_by".into()),
    ])
}

/// Resolve an `[external]` alias directory value to an absolute path.
///
/// `dir` is the value as written in `.lattice.toml`; `base_dir` is the config
/// file's directory (the workspace root in the normal case). Resolution follows
/// the three accepted forms (issue 030):
///
/// - an **absolute** path (`/srv/Catenary`) is taken verbatim;
/// - a **`~`-leading** path (`~/Projects/Catenary`) is expanded against the
///   home directory — a machine-specific form the spec discourages but accepts;
/// - a **relative** path (`../Catenary`, the preferred sibling-checkout form) is
///   joined onto `base_dir`.
///
/// The result is not normalized or canonicalized: it is only ever `stat`-ed for
/// existence (decision 010), so component arithmetic is unnecessary and a
/// missing directory must remain a plain absent path (degrading to exempt)
/// rather than erroring. When `~` cannot be expanded (no home directory), the
/// `~` is left literal so the path simply fails to resolve — exempt, never a
/// false break.
fn resolve_alias_dir(dir: &str, base_dir: &Path) -> PathBuf {
    if let Some(rest) = dir.strip_prefix("~/") {
        return std::env::home_dir().map_or_else(|| PathBuf::from(dir), |home| home.join(rest));
    }
    if dir == "~" {
        return std::env::home_dir().unwrap_or_else(|| PathBuf::from(dir));
    }

    let path = Path::new(dir);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_dir.join(path)
    }
}

/// Walk up from `start` looking for `.lattice.toml`.
///
/// Checks each directory from `start` upward. Stops after checking a
/// directory that contains `.git`, or when the filesystem root is reached.
fn find_config_file(start: &Path) -> Option<PathBuf> {
    let dir = if start.is_file() {
        start.parent()?
    } else {
        start
    };

    let mut current = dir;
    loop {
        let candidate = current.join(".lattice.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        if current.join(".git").exists() {
            return None;
        }
        current = current.parent()?;
    }
}

/// Merge a raw `[policy]` table onto resolved defaults.
///
/// Each field overrides its default when present; an unrecognized enum value
/// produces [`ConfigError::Invalid`] naming the offending value and the
/// accepted set.
fn apply_policy(policy: &mut Policy, raw: RawPolicy, path: &Path) -> Result<(), ConfigError> {
    let invalid = |message: String| ConfigError::Invalid {
        path: path.to_path_buf(),
        message,
    };

    if let Some(ref value) = raw.predicates {
        policy.predicates = parse_predicate_policy(value).ok_or_else(|| {
            invalid(format!(
                "unknown predicates policy '{value}': expected 'optional' or 'required'"
            ))
        })?;
    }
    if let Some(backlinks) = raw.backlinks {
        policy.backlinks = backlinks;
    }
    if let Some(ref value) = raw.bare_paths {
        policy.bare_paths = parse_bare_path_policy(value).ok_or_else(|| {
            invalid(format!(
                "unknown bare_paths policy '{value}': expected 'warn', 'deny', or 'disabled'"
            ))
        })?;
    }
    if let Some(ref value) = raw.stale_references {
        policy.stale_references = parse_stale_reference_policy(value).ok_or_else(|| {
            invalid(format!(
                "unknown stale_references policy '{value}': expected 'hint', 'warn', 'deny', or 'disabled'"
            ))
        })?;
    }
    if let Some(ref value) = raw.fragments {
        policy.fragments = Some(parse_fragment_algorithm(value).ok_or_else(|| {
            invalid(format!(
                "unknown fragments algorithm '{value}': expected 'github', 'gitlab', or 'vscode'"
            ))
        })?);
    }
    if let Some(ref value) = raw.admonitions {
        policy.admonitions = parse_admonition_policy(value).ok_or_else(|| {
            invalid(format!(
                "unknown admonitions policy '{value}': expected 'portable', 'github', 'gitlab', or 'disabled'"
            ))
        })?;
    }
    if let Some(ref value) = raw.code_block_language {
        policy.code_block_language = parse_code_block_language_policy(value).ok_or_else(|| {
            invalid(format!(
                "unknown code_block_language policy '{value}': expected 'hint', 'warn', 'deny', or 'disabled'"
            ))
        })?;
    }
    if let Some(multiple_h1) = raw.multiple_h1 {
        policy.multiple_h1 = multiple_h1;
    }
    if let Some(skipped_heading_level) = raw.skipped_heading_level {
        policy.skipped_heading_level = skipped_heading_level;
    }
    if let Some(image_empty_alt) = raw.image_empty_alt {
        policy.image_empty_alt = image_empty_alt;
    }
    if let Some(ref value) = raw.connectivity {
        policy.connectivity = parse_connectivity_policy(value).ok_or_else(|| {
            invalid(format!(
                "unknown connectivity policy '{value}': expected 'off', 'no-orphans', 'no-islands', or 'reachable'"
            ))
        })?;
    }
    if let Some(roots) = raw.roots {
        policy.roots = roots.iter().map(PathBuf::from).collect();
    }

    Ok(())
}

/// Resolve a raw `[[override]]` entry into an [`Override`] (issue 037).
///
/// Compiles each glob (an invalid glob is [`ConfigError::Invalid`]), and parses
/// each per-lint value as either a level string (the same accepted set as the
/// repo-wide knob) or an `{ expect = N }` aggregate (`N >= 1`). An entry with no
/// globs, or one that names neither lint, is rejected — it can never match or do
/// anything, so it is a config mistake worth surfacing at load time.
fn parse_override(raw: RawOverride, path: &Path) -> Result<Override, ConfigError> {
    let invalid = |message: String| ConfigError::Invalid {
        path: path.to_path_buf(),
        message,
    };

    if raw.paths.is_empty() {
        return Err(invalid(
            "an [[override]] entry must list at least one path glob".to_string(),
        ));
    }

    let mut paths = Vec::with_capacity(raw.paths.len());
    for glob in &raw.paths {
        let pattern = Pattern::new(glob)
            .map_err(|e| invalid(format!("invalid override glob '{glob}': {e}")))?;
        paths.push(pattern);
    }

    let stale_references = raw
        .stale_references
        .map(|lint| parse_stale_reference_override(lint, &invalid))
        .transpose()?;
    let bare_paths = raw
        .bare_paths
        .map(|lint| parse_bare_path_override(lint, &invalid))
        .transpose()?;

    if stale_references.is_none() && bare_paths.is_none() {
        return Err(invalid(format!(
            "[[override]] entry for '{}' sets neither stale_references nor bare_paths",
            raw.paths.join(", ")
        )));
    }

    Ok(Override {
        paths,
        raw_paths: raw.paths,
        stale_references,
        bare_paths,
        hint: raw.hint,
    })
}

/// Parse a per-lint override value for `stale_references` (issue 037): a level
/// string or an `{ expect = N }` aggregate.
fn parse_stale_reference_override(
    raw: RawOverrideLint,
    invalid: &impl Fn(String) -> ConfigError,
) -> Result<StaleReferenceOverride, ConfigError> {
    match raw {
        RawOverrideLint::Level(value) => parse_stale_reference_policy(&value)
            .map(StaleReferenceOverride::Level)
            .ok_or_else(|| {
                invalid(format!(
                    "unknown override stale_references level '{value}': expected 'hint', 'warn', 'deny', 'disabled', or {{ expect = N }}"
                ))
            }),
        RawOverrideLint::Expect { expect } => {
            check_expect(expect, "stale_references", invalid)?;
            Ok(StaleReferenceOverride::Expect(expect))
        }
    }
}

/// Parse a per-lint override value for `bare_paths` (issue 037): a level string
/// or an `{ expect = N }` aggregate.
fn parse_bare_path_override(
    raw: RawOverrideLint,
    invalid: &impl Fn(String) -> ConfigError,
) -> Result<BarePathOverride, ConfigError> {
    match raw {
        RawOverrideLint::Level(value) => parse_bare_path_policy(&value)
            .map(BarePathOverride::Level)
            .ok_or_else(|| {
                invalid(format!(
                    "unknown override bare_paths level '{value}': expected 'warn', 'deny', 'disabled', or {{ expect = N }}"
                ))
            }),
        RawOverrideLint::Expect { expect } => {
            check_expect(expect, "bare_paths", invalid)?;
            Ok(BarePathOverride::Expect(expect))
        }
    }
}

/// Validate an `{ expect = N }` aggregate count: `N` must be at least 1 (a
/// zero-count tripwire is a config mistake — there is nothing to reconcile).
fn check_expect(
    expect: usize,
    lint: &str,
    invalid: &impl Fn(String) -> ConfigError,
) -> Result<(), ConfigError> {
    if expect == 0 {
        return Err(invalid(format!(
            "override {lint} {{ expect = 0 }} must be at least 1"
        )));
    }
    Ok(())
}

fn parse_predicate_policy(s: &str) -> Option<PredicatePolicy> {
    match s {
        "optional" => Some(PredicatePolicy::Optional),
        "required" => Some(PredicatePolicy::Required),
        _ => None,
    }
}

fn parse_bare_path_policy(s: &str) -> Option<BarePathPolicy> {
    match s {
        "warn" => Some(BarePathPolicy::Warn),
        "deny" => Some(BarePathPolicy::Deny),
        "disabled" => Some(BarePathPolicy::Disabled),
        _ => None,
    }
}

fn parse_stale_reference_policy(s: &str) -> Option<StaleReferencePolicy> {
    match s {
        "hint" => Some(StaleReferencePolicy::Hint),
        "warn" => Some(StaleReferencePolicy::Warn),
        "deny" => Some(StaleReferencePolicy::Deny),
        "disabled" => Some(StaleReferencePolicy::Disabled),
        _ => None,
    }
}

fn parse_fragment_algorithm(s: &str) -> Option<FragmentAlgorithm> {
    match s {
        "github" => Some(FragmentAlgorithm::Github),
        "gitlab" => Some(FragmentAlgorithm::Gitlab),
        "vscode" => Some(FragmentAlgorithm::Vscode),
        _ => None,
    }
}

fn parse_admonition_policy(s: &str) -> Option<AdmonitionPolicy> {
    match s {
        "portable" => Some(AdmonitionPolicy::Portable),
        "github" => Some(AdmonitionPolicy::Github),
        "gitlab" => Some(AdmonitionPolicy::Gitlab),
        "disabled" => Some(AdmonitionPolicy::Disabled),
        _ => None,
    }
}

fn parse_code_block_language_policy(s: &str) -> Option<CodeBlockLanguagePolicy> {
    match s {
        "hint" => Some(CodeBlockLanguagePolicy::Hint),
        "warn" => Some(CodeBlockLanguagePolicy::Warn),
        "deny" => Some(CodeBlockLanguagePolicy::Deny),
        "disabled" => Some(CodeBlockLanguagePolicy::Disabled),
        _ => None,
    }
}

fn parse_connectivity_policy(s: &str) -> Option<ConnectivityPolicy> {
    match s {
        "off" => Some(ConnectivityPolicy::Off),
        "no-orphans" => Some(ConnectivityPolicy::NoOrphans),
        "no-islands" => Some(ConnectivityPolicy::NoIslands),
        "reachable" => Some(ConnectivityPolicy::Reachable),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, reason = "tests use expect for clarity")]
mod tests {
    use std::fs;

    use super::*;

    fn temp_dir_with(toml_content: Option<&str>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("create temp dir");
        if let Some(content) = toml_content {
            fs::write(dir.path().join(".lattice.toml"), content).expect("write .lattice.toml");
        }
        dir
    }

    #[test]
    fn defaults_when_no_config() {
        let dir = temp_dir_with(None);
        fs::create_dir(dir.path().join(".git")).expect("create .git");

        let config = Config::load(dir.path()).expect("load should succeed");

        assert_eq!(
            config.predicates.len(),
            7,
            "should have 7 default predicates"
        );
        assert_eq!(
            config.inverse_of("supersedes"),
            Some("superseded_by"),
            "supersedes → superseded_by"
        );
        assert_eq!(
            config.inverse_of("references"),
            Some("referenced_by"),
            "references → referenced_by"
        );
        assert_eq!(
            config.policy.predicates,
            PredicatePolicy::Optional,
            "default predicate policy"
        );
        assert!(config.policy.backlinks, "default backlinks enabled");
        assert_eq!(
            config.policy.bare_paths,
            BarePathPolicy::Warn,
            "default bare_paths"
        );
        assert_eq!(
            config.policy.stale_references,
            StaleReferencePolicy::Warn,
            "default stale_references is warn (issue 028)"
        );
        assert!(
            config.policy.fragments.is_none(),
            "default fragments tries all"
        );
    }

    #[test]
    fn custom_predicates_merge_with_defaults() {
        let dir = temp_dir_with(Some(
            r#"
[predicates]
supersedes = "replaced_by"
tracks = "tracked_by"
"#,
        ));

        let config = Config::load(dir.path()).expect("load should succeed");

        assert_eq!(
            config.inverse_of("supersedes"),
            Some("replaced_by"),
            "supersedes overridden"
        );
        assert_eq!(
            config.inverse_of("tracks"),
            Some("tracked_by"),
            "new predicate added"
        );
        assert_eq!(
            config.inverse_of("implements"),
            Some("implemented_by"),
            "default implements preserved"
        );
        assert_eq!(
            config.inverse_of("references"),
            Some("referenced_by"),
            "default references preserved"
        );
        assert!(
            config.predicates.len() >= 8,
            "at least 8 predicates (7 defaults + 1 new)"
        );
    }

    #[test]
    fn partial_policy_override() {
        let dir = temp_dir_with(Some(
            r#"
[policy]
predicates = "required"
bare_paths = "deny"
"#,
        ));

        let config = Config::load(dir.path()).expect("load should succeed");

        assert_eq!(
            config.policy.predicates,
            PredicatePolicy::Required,
            "predicates overridden"
        );
        assert_eq!(
            config.policy.bare_paths,
            BarePathPolicy::Deny,
            "bare_paths overridden"
        );
        assert!(config.policy.backlinks, "backlinks default preserved");
        assert!(
            config.policy.fragments.is_none(),
            "fragments default preserved"
        );
    }

    #[test]
    fn full_policy_override() {
        let dir = temp_dir_with(Some(
            r#"
[policy]
predicates = "required"
backlinks = false
bare_paths = "disabled"
fragments = "gitlab"
"#,
        ));

        let config = Config::load(dir.path()).expect("load should succeed");

        assert_eq!(
            config.policy.predicates,
            PredicatePolicy::Required,
            "predicates"
        );
        assert!(!config.policy.backlinks, "backlinks disabled");
        assert_eq!(
            config.policy.bare_paths,
            BarePathPolicy::Disabled,
            "bare_paths"
        );
        assert_eq!(
            config.policy.fragments,
            Some(FragmentAlgorithm::Gitlab),
            "fragments"
        );
    }

    #[test]
    fn all_fragment_algorithms() {
        for (input, expected) in [
            ("github", FragmentAlgorithm::Github),
            ("gitlab", FragmentAlgorithm::Gitlab),
            ("vscode", FragmentAlgorithm::Vscode),
        ] {
            let dir = temp_dir_with(Some(&format!("[policy]\nfragments = \"{input}\"")));
            let config = Config::load(dir.path()).expect("load should succeed");
            assert_eq!(
                config.policy.fragments,
                Some(expected),
                "fragment algorithm for '{input}'"
            );
        }
    }

    #[test]
    fn empty_config_returns_defaults() {
        let dir = temp_dir_with(Some(""));

        let config = Config::load(dir.path()).expect("load should succeed");

        assert_eq!(config.predicates.len(), 7, "defaults preserved");
        assert_eq!(
            config.policy.predicates,
            PredicatePolicy::Optional,
            "default policy"
        );
    }

    #[test]
    fn walks_up_to_find_config() {
        let dir = temp_dir_with(Some("[policy]\npredicates = \"required\""));
        let subdir = dir.path().join("a").join("b").join("c");
        fs::create_dir_all(&subdir).expect("create subdirs");

        let config = Config::load(&subdir).expect("load should succeed");

        assert_eq!(
            config.policy.predicates,
            PredicatePolicy::Required,
            "found config from parent"
        );
    }

    #[test]
    fn stops_at_git_root() {
        let dir = tempfile::tempdir().expect("create temp dir");
        fs::write(
            dir.path().join(".lattice.toml"),
            "[policy]\npredicates = \"required\"",
        )
        .expect("write config");

        let project = dir.path().join("project");
        fs::create_dir(&project).expect("create project dir");
        fs::create_dir(project.join(".git")).expect("create .git");

        let config = Config::load(&project).expect("load should succeed");

        assert_eq!(
            config.policy.predicates,
            PredicatePolicy::Optional,
            "should use defaults"
        );
    }

    #[test]
    fn config_at_git_root_is_found() {
        let dir = tempfile::tempdir().expect("create temp dir");
        fs::create_dir(dir.path().join(".git")).expect("create .git");
        fs::write(
            dir.path().join(".lattice.toml"),
            "[policy]\npredicates = \"required\"",
        )
        .expect("write config");

        let subdir = dir.path().join("docs");
        fs::create_dir(&subdir).expect("create docs dir");

        let config = Config::load(&subdir).expect("load should succeed");

        assert_eq!(
            config.policy.predicates,
            PredicatePolicy::Required,
            "config at git root should be found"
        );
    }

    #[test]
    fn load_from_file_path() {
        let dir = temp_dir_with(Some("[policy]\nbare_paths = \"deny\""));
        let file = dir.path().join("doc.md");
        fs::write(&file, "# Hello").expect("write file");

        let config = Config::load(&file).expect("load should succeed");

        assert_eq!(
            config.policy.bare_paths,
            BarePathPolicy::Deny,
            "found config when starting from a file"
        );
    }

    #[test]
    fn invalid_predicate_policy() {
        let dir = temp_dir_with(Some("[policy]\npredicates = \"always\""));
        let err = Config::load(dir.path()).expect_err("should fail");
        let msg = err.to_string();
        assert!(msg.contains("always"), "mentions bad value: {msg}");
        assert!(
            msg.contains("optional") && msg.contains("required"),
            "lists valid options: {msg}"
        );
    }

    #[test]
    fn invalid_bare_paths_policy() {
        let dir = temp_dir_with(Some("[policy]\nbare_paths = \"error\""));
        let err = Config::load(dir.path()).expect_err("should fail");
        let msg = err.to_string();
        assert!(msg.contains("error"), "mentions bad value: {msg}");
    }

    #[test]
    fn invalid_fragments_algorithm() {
        let dir = temp_dir_with(Some("[policy]\nfragments = \"bitbucket\""));
        let err = Config::load(dir.path()).expect_err("should fail");
        let msg = err.to_string();
        assert!(msg.contains("bitbucket"), "mentions bad value: {msg}");
    }

    #[test]
    fn convention_flags_default_false() {
        let dir = temp_dir_with(None);
        fs::create_dir(dir.path().join(".git")).expect("create .git");

        let config = Config::load(dir.path()).expect("load should succeed");

        assert!(
            !config.policy.multiple_h1,
            "multiple_h1 defaults off (decision 009)"
        );
        assert!(
            !config.policy.skipped_heading_level,
            "skipped_heading_level defaults off (decision 009)"
        );
        assert!(
            !config.policy.image_empty_alt,
            "image_empty_alt defaults off (decision 009)"
        );
    }

    #[test]
    fn convention_flags_parse_true() {
        let dir = temp_dir_with(Some(
            r"
[policy]
multiple_h1 = true
skipped_heading_level = true
image_empty_alt = true
",
        ));

        let config = Config::load(dir.path()).expect("load should succeed");

        assert!(
            config.policy.multiple_h1,
            "multiple_h1 = true enables the check"
        );
        assert!(
            config.policy.skipped_heading_level,
            "skipped_heading_level = true enables the check"
        );
        assert!(
            config.policy.image_empty_alt,
            "image_empty_alt = true enables the check"
        );
    }

    #[test]
    fn code_block_language_defaults_disabled() {
        let dir = temp_dir_with(None);
        fs::create_dir(dir.path().join(".git")).expect("create .git");

        let config = Config::load(dir.path()).expect("load should succeed");

        assert_eq!(
            config.policy.code_block_language,
            CodeBlockLanguagePolicy::Disabled,
            "code_block_language defaults to disabled (decision 009)"
        );
    }

    #[test]
    fn code_block_language_parses_hint() {
        let dir = temp_dir_with(Some("[policy]\ncode_block_language = \"hint\""));
        let config = Config::load(dir.path()).expect("load should succeed");
        assert_eq!(
            config.policy.code_block_language,
            CodeBlockLanguagePolicy::Hint,
            "code_block_language = \"hint\" enables the hint"
        );
    }

    #[test]
    fn stale_references_defaults_warn() {
        let dir = temp_dir_with(None);
        fs::create_dir(dir.path().join(".git")).expect("create .git");

        let config = Config::load(dir.path()).expect("load should succeed");

        assert_eq!(
            config.policy.stale_references,
            StaleReferencePolicy::Warn,
            "stale_references defaults to warn (issue 028)"
        );
    }

    #[test]
    fn stale_references_levels_parse() {
        for (value, expected) in [
            ("hint", StaleReferencePolicy::Hint),
            ("warn", StaleReferencePolicy::Warn),
            ("deny", StaleReferencePolicy::Deny),
            ("disabled", StaleReferencePolicy::Disabled),
        ] {
            let dir = temp_dir_with(Some(&format!("[policy]\nstale_references = \"{value}\"")));
            let config = Config::load(dir.path()).expect("load should succeed");
            assert_eq!(
                config.policy.stale_references, expected,
                "stale_references = {value:?} parses"
            );
        }
    }

    #[test]
    fn invalid_stale_references_policy() {
        let dir = temp_dir_with(Some("[policy]\nstale_references = \"error\""));
        let err = Config::load(dir.path()).expect_err("should fail");
        let msg = err.to_string();
        assert!(msg.contains("error"), "mentions bad value: {msg}");
        assert!(
            msg.contains("hint") && msg.contains("disabled"),
            "lists valid options: {msg}"
        );
    }

    // -- External-namespace aliases (issue 030) --

    #[test]
    fn external_defaults_empty() {
        let dir = temp_dir_with(None);
        fs::create_dir(dir.path().join(".git")).expect("create .git");

        let config = Config::load(dir.path()).expect("load should succeed");

        assert!(
            config.external.is_empty(),
            "no [external] table means no aliases (the exempt floor)"
        );
    }

    #[test]
    fn external_relative_alias_resolves_against_config_dir() {
        // The preferred sibling-checkout form: a relative value resolves against
        // the config file's directory.
        let dir = temp_dir_with(Some("[external]\nCatenary = \"../Catenary\""));
        let config = Config::load(dir.path()).expect("load should succeed");

        assert_eq!(
            config.external.get("Catenary"),
            Some(&dir.path().join("../Catenary")),
            "relative alias resolves against the config file's directory"
        );
    }

    #[test]
    fn external_absolute_alias_parses_verbatim() {
        let dir = temp_dir_with(Some("[external]\nCatenary = \"/srv/Catenary\""));
        let config = Config::load(dir.path()).expect("load should succeed");

        assert_eq!(
            config.external.get("Catenary"),
            Some(&PathBuf::from("/srv/Catenary")),
            "an absolute alias value is taken verbatim"
        );
    }

    #[test]
    fn external_home_alias_parses() {
        // A `~`-leading value parses (machine-specific, discouraged, but
        // accepted) and expands against the home directory when one is known.
        let dir = temp_dir_with(Some("[external]\nCatenary = \"~/Projects/Catenary\""));
        let config = Config::load(dir.path()).expect("load should succeed");

        let resolved = config
            .external
            .get("Catenary")
            .expect("home-relative alias parses");
        if let Some(home) = std::env::home_dir() {
            assert_eq!(
                resolved,
                &home.join("Projects/Catenary"),
                "`~/` expands against the home directory"
            );
        } else {
            assert_eq!(
                resolved,
                &PathBuf::from("~/Projects/Catenary"),
                "with no home directory the `~` is left literal (resolves to absent → exempt)"
            );
        }
    }

    #[test]
    fn external_table_round_trips_multiple_aliases() {
        let dir = temp_dir_with(Some(
            "[external]\nCatenary = \"../Catenary\"\nHedgeMaze = \"/opt/HedgeMaze\"",
        ));
        let config = Config::load(dir.path()).expect("load should succeed");

        assert_eq!(config.external.len(), 2, "both aliases round-trip");
        assert_eq!(
            config.external.get("Catenary"),
            Some(&dir.path().join("../Catenary")),
            "relative alias preserved"
        );
        assert_eq!(
            config.external.get("HedgeMaze"),
            Some(&PathBuf::from("/opt/HedgeMaze")),
            "absolute alias preserved"
        );
    }

    #[test]
    fn connectivity_defaults() {
        let dir = temp_dir_with(None);
        fs::create_dir(dir.path().join(".git")).expect("create .git");

        let config = Config::load(dir.path()).expect("load should succeed");

        assert_eq!(
            config.policy.connectivity,
            ConnectivityPolicy::Off,
            "connectivity defaults off"
        );
        assert_eq!(
            config.policy.roots,
            vec![PathBuf::from("README.md")],
            "roots default to the workspace-root README"
        );
    }

    #[test]
    fn connectivity_levels_parse() {
        for (value, expected) in [
            ("no-orphans", ConnectivityPolicy::NoOrphans),
            ("no-islands", ConnectivityPolicy::NoIslands),
            ("reachable", ConnectivityPolicy::Reachable),
            ("off", ConnectivityPolicy::Off),
        ] {
            let dir = temp_dir_with(Some(&format!("[policy]\nconnectivity = \"{value}\"")));
            let config = Config::load(dir.path()).expect("load should succeed");
            assert_eq!(
                config.policy.connectivity, expected,
                "connectivity = {value:?} parses"
            );
        }
    }

    #[test]
    fn connectivity_custom_roots_parse() {
        let dir = temp_dir_with(Some(
            "[policy]\nconnectivity = \"reachable\"\nroots = [\"docs/index.md\", \"CONTRIBUTING.md\"]",
        ));
        let config = Config::load(dir.path()).expect("load should succeed");

        assert_eq!(
            config.policy.roots,
            vec![
                PathBuf::from("docs/index.md"),
                PathBuf::from("CONTRIBUTING.md"),
            ],
            "custom roots override the default"
        );
    }

    #[test]
    fn invalid_connectivity_policy() {
        let dir = temp_dir_with(Some("[policy]\nconnectivity = \"weak\""));
        let err = Config::load(dir.path()).expect_err("should fail");
        let msg = err.to_string();
        assert!(msg.contains("weak"), "mentions bad value: {msg}");
        assert!(
            msg.contains("no-orphans") && msg.contains("reachable"),
            "lists valid options: {msg}"
        );
    }

    #[test]
    fn empty_inverse_predicate() {
        let dir = temp_dir_with(Some("[predicates]\nfoo = \"\""));
        let err = Config::load(dir.path()).expect_err("should fail");
        let msg = err.to_string();
        assert!(msg.contains("foo"), "mentions the predicate: {msg}");
        assert!(msg.contains("empty"), "says empty: {msg}");
    }

    #[test]
    fn malformed_toml() {
        let dir = temp_dir_with(Some("this is not valid toml [[["));
        let err = Config::load(dir.path()).expect_err("should fail");
        assert!(
            matches!(err, ConfigError::Parse { .. }),
            "should be a parse error"
        );
    }

    #[test]
    fn vocabulary_lookups() {
        let config = Config::default();

        assert!(
            config.is_known_forward("supersedes"),
            "supersedes is forward"
        );
        assert!(
            !config.is_known_forward("superseded_by"),
            "superseded_by is not forward"
        );
        assert!(
            config.is_known_inverse("superseded_by"),
            "superseded_by is inverse"
        );
        assert!(
            !config.is_known_inverse("supersedes"),
            "supersedes is not inverse"
        );

        assert_eq!(
            config.inverse_of("supersedes"),
            Some("superseded_by"),
            "inverse lookup"
        );
        assert_eq!(
            config.inverse_of("unknown"),
            None,
            "unknown forward returns None"
        );
    }

    #[test]
    fn known_predicate_accepts_either_direction() {
        let config = Config::default();

        assert!(
            config.is_known_predicate("supersedes"),
            "forward member is known"
        );
        assert!(
            config.is_known_predicate("superseded_by"),
            "inverse member is known"
        );
        assert!(
            !config.is_known_predicate("invented"),
            "a string in neither direction is unknown"
        );
    }

    #[test]
    fn opposite_of_maps_both_directions() {
        let config = Config::default();

        assert_eq!(
            config.opposite_of("supersedes"),
            Some("superseded_by"),
            "forward maps to its inverse"
        );
        assert_eq!(
            config.opposite_of("superseded_by"),
            Some("supersedes"),
            "inverse maps to its forward"
        );
        assert_eq!(
            config.opposite_of("references"),
            Some("referenced_by"),
            "the default predicate still derives referenced_by"
        );
        assert_eq!(
            config.opposite_of("invented"),
            None,
            "neither direction returns None"
        );
    }

    // -- Subtree overrides (issue 037, decision 012 part 2) --

    #[test]
    fn no_override_table_means_no_overrides() {
        let dir = temp_dir_with(None);
        fs::create_dir(dir.path().join(".git")).expect("create .git");
        let config = Config::load(dir.path()).expect("load should succeed");
        assert!(
            config.overrides.is_empty(),
            "no [[override]] table means no overrides"
        );
    }

    #[test]
    fn override_level_string_parses() {
        let dir = temp_dir_with(Some(
            "[[override]]\npaths = [\"archive/**\", \"*_bak.md\"]\nstale_references = \"disabled\"\nhint = \"frozen docs\"\n",
        ));
        let config = Config::load(dir.path()).expect("load should succeed");
        assert_eq!(config.overrides.len(), 1, "one override entry parses");
        let ov = &config.overrides[0];
        assert_eq!(
            ov.stale_references,
            Some(StaleReferenceOverride::Level(
                StaleReferencePolicy::Disabled
            )),
            "a level string parses to a Level override"
        );
        assert_eq!(ov.bare_paths, None, "bare_paths unset on this entry");
        assert_eq!(
            ov.hint.as_deref(),
            Some("frozen docs"),
            "the optional hint round-trips"
        );
        assert_eq!(
            ov.raw_paths,
            vec!["archive/**".to_string(), "*_bak.md".to_string()],
            "the raw globs round-trip for the label"
        );
    }

    #[test]
    fn override_expect_table_parses() {
        let dir = temp_dir_with(Some(
            "[[override]]\npaths = [\"tickets/sweep/**\"]\nstale_references = { expect = 40 }\n",
        ));
        let config = Config::load(dir.path()).expect("load should succeed");
        assert_eq!(
            config.overrides[0].stale_references,
            Some(StaleReferenceOverride::Expect(40)),
            "an inline {{ expect = N }} table parses to an Expect override"
        );
    }

    #[test]
    fn override_raise_level_parses() {
        // bare_paths raised from the default warn to deny on a strict subtree.
        let dir = temp_dir_with(Some(
            "[[override]]\npaths = [\"strict/**\"]\nbare_paths = \"deny\"\n",
        ));
        let config = Config::load(dir.path()).expect("load should succeed");
        assert_eq!(
            config.overrides[0].bare_paths,
            Some(BarePathOverride::Level(BarePathPolicy::Deny)),
            "a raise to deny parses"
        );
    }

    #[test]
    fn override_glob_matches_workspace_relative_paths() {
        let dir = temp_dir_with(Some(
            "[[override]]\npaths = [\"archive/**\", \"*_bak.md\"]\nstale_references = \"disabled\"\n",
        ));
        let config = Config::load(dir.path()).expect("load should succeed");
        let ov = &config.overrides[0];
        assert!(
            ov.matches(Path::new("archive/old/cli.md")),
            "a nested path under archive/ matches archive/**"
        );
        assert!(
            ov.matches(Path::new("notes_bak.md")),
            "a top-level *_bak.md path matches"
        );
        assert!(
            !ov.matches(Path::new("archived/x.md")),
            "a sibling sharing a name prefix must not match archive/**"
        );
        assert!(
            !ov.matches(Path::new("docs/live.md")),
            "an unrelated path must not match"
        );
    }

    #[test]
    fn effective_policy_applies_matching_level_override() {
        let dir = temp_dir_with(Some(
            "[[override]]\npaths = [\"archive/**\"]\nstale_references = \"disabled\"\n",
        ));
        let config = Config::load(dir.path()).expect("load should succeed");
        assert_eq!(
            config
                .effective_policy(Path::new("archive/old.md"))
                .stale_references,
            StaleReferencePolicy::Disabled,
            "a matching file resolves the lint to the override level"
        );
        assert_eq!(
            config
                .effective_policy(Path::new("docs/live.md"))
                .stale_references,
            StaleReferencePolicy::Warn,
            "a non-matching file keeps the repo-wide level"
        );
    }

    #[test]
    fn effective_policy_expect_keeps_base_level() {
        let dir = temp_dir_with(Some(
            "[[override]]\npaths = [\"sweep/**\"]\nstale_references = { expect = 5 }\n",
        ));
        let config = Config::load(dir.path()).expect("load should succeed");
        assert_eq!(
            config
                .effective_policy(Path::new("sweep/audit.md"))
                .stale_references,
            StaleReferencePolicy::Warn,
            "an expect override leaves the per-file level at its base value"
        );
    }

    #[test]
    fn effective_policy_last_match_wins() {
        // Two entries match the same file: a freeze, then a raise. Last wins.
        let dir = temp_dir_with(Some(
            "[[override]]\npaths = [\"x/**\"]\nbare_paths = \"disabled\"\n\n[[override]]\npaths = [\"x/strict/**\"]\nbare_paths = \"deny\"\n",
        ));
        let config = Config::load(dir.path()).expect("load should succeed");
        assert_eq!(
            config
                .effective_policy(Path::new("x/strict/a.md"))
                .bare_paths,
            BarePathPolicy::Deny,
            "the last matching entry's level wins for an overlapping file"
        );
        assert_eq!(
            config.effective_policy(Path::new("x/other.md")).bare_paths,
            BarePathPolicy::Disabled,
            "a file matched only by the first entry keeps that entry's level"
        );
    }

    #[test]
    fn effective_policy_later_expect_resets_earlier_level() {
        // An earlier level entry then a later expect entry on the same lint/file:
        // expect wins (last-match) and resets the per-file level to base.
        let dir = temp_dir_with(Some(
            "[[override]]\npaths = [\"a/**\"]\nstale_references = \"disabled\"\n\n[[override]]\npaths = [\"a/**\"]\nstale_references = { expect = 3 }\n",
        ));
        let config = Config::load(dir.path()).expect("load should succeed");
        assert_eq!(
            config
                .effective_policy(Path::new("a/x.md"))
                .stale_references,
            StaleReferencePolicy::Warn,
            "a later expect entry resets the lint to its base level (the earlier freeze loses)"
        );
    }

    #[test]
    fn override_with_no_paths_is_invalid() {
        let dir = temp_dir_with(Some(
            "[[override]]\npaths = []\nstale_references = \"disabled\"\n",
        ));
        let err = Config::load(dir.path()).expect_err("empty paths should fail");
        assert!(
            err.to_string().contains("at least one path glob"),
            "the error names the empty-paths problem: {err}"
        );
    }

    #[test]
    fn override_naming_no_lint_is_invalid() {
        let dir = temp_dir_with(Some("[[override]]\npaths = [\"x/**\"]\nhint = \"oops\"\n"));
        let err = Config::load(dir.path()).expect_err("an override naming no lint should fail");
        assert!(
            err.to_string()
                .contains("neither stale_references nor bare_paths"),
            "the error names the no-lint problem: {err}"
        );
    }

    #[test]
    fn override_invalid_glob_is_reported() {
        let dir = temp_dir_with(Some(
            "[[override]]\npaths = [\"a/[\"]\nstale_references = \"disabled\"\n",
        ));
        let err = Config::load(dir.path()).expect_err("a malformed glob should fail");
        assert!(
            err.to_string().contains("invalid override glob"),
            "the error names the bad glob: {err}"
        );
    }

    #[test]
    fn override_invalid_level_is_reported() {
        let dir = temp_dir_with(Some(
            "[[override]]\npaths = [\"x/**\"]\nstale_references = \"loud\"\n",
        ));
        let err = Config::load(dir.path()).expect_err("a bad level should fail");
        let msg = err.to_string();
        assert!(msg.contains("loud"), "mentions the bad value: {msg}");
        assert!(
            msg.contains("expect = N"),
            "the error names the expect alternative: {msg}"
        );
    }

    #[test]
    fn override_expect_zero_is_invalid() {
        let dir = temp_dir_with(Some(
            "[[override]]\npaths = [\"x/**\"]\nbare_paths = { expect = 0 }\n",
        ));
        let err = Config::load(dir.path()).expect_err("expect = 0 should fail");
        assert!(
            err.to_string().contains("at least 1"),
            "the error names the expect>=1 rule: {err}"
        );
    }
}
