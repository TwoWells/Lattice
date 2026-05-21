// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Configuration loading and predicate vocabulary.
//!
//! Loads `.lattice.toml` if present, merging with built-in defaults.
//! Produces a resolved [`Config`] consumed by the rest of the system.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// Whether predicates are required on links.
    pub predicates: PredicatePolicy,
    /// Whether backlink consistency is checked.
    pub backlinks: bool,
    /// How bare paths are handled.
    pub bare_paths: BarePathPolicy,
    /// Slug algorithm for fragment validation. `None` tries all.
    pub fragments: Option<FragmentAlgorithm>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            predicates: PredicatePolicy::Optional,
            backlinks: true,
            bare_paths: BarePathPolicy::Warn,
            fragments: None,
        }
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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            predicates: default_predicates(),
            policy: Policy::default(),
            format_command: None,
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
            if let Some(ref value) = policy.predicates {
                config.policy.predicates =
                    parse_predicate_policy(value).ok_or_else(|| ConfigError::Invalid {
                        path: path.clone(),
                        message: format!(
                            "unknown predicates policy '{value}': expected 'optional' or 'required'"
                        ),
                    })?;
            }
            if let Some(backlinks) = policy.backlinks {
                config.policy.backlinks = backlinks;
            }
            if let Some(ref value) = policy.bare_paths {
                config.policy.bare_paths =
                    parse_bare_path_policy(value).ok_or_else(|| ConfigError::Invalid {
                        path: path.clone(),
                        message: format!(
                            "unknown bare_paths policy '{value}': expected 'warn', 'deny', or 'disabled'"
                        ),
                    })?;
            }
            if let Some(ref value) = policy.fragments {
                config.policy.fragments =
                    Some(parse_fragment_algorithm(value).ok_or_else(|| {
                        ConfigError::Invalid {
                            path: path.clone(),
                            message: format!(
                                "unknown fragments algorithm '{value}': expected 'github', 'gitlab', or 'vscode'"
                            ),
                        }
                    })?);
            }
        }

        if let Some(format) = raw.format {
            config.format_command = format.command;
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
}

// --- Raw deserialization types ---

#[derive(Debug, Deserialize)]
struct RawConfig {
    predicates: Option<HashMap<String, String>>,
    policy: Option<RawPolicy>,
    format: Option<RawFormat>,
}

#[derive(Debug, Deserialize)]
struct RawFormat {
    command: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawPolicy {
    predicates: Option<String>,
    backlinks: Option<bool>,
    bare_paths: Option<String>,
    fragments: Option<String>,
}

// --- Helpers ---

/// Built-in predicate vocabulary.
fn default_predicates() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("amends".into(), "amended_by".into()),
        ("blocks".into(), "blocked_by".into()),
        ("depends_on".into(), "dependency_of".into()),
        ("implements".into(), "implemented_by".into()),
        ("references".into(), "referenced_by".into()),
        ("supersedes".into(), "superseded_by".into()),
    ])
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

fn parse_fragment_algorithm(s: &str) -> Option<FragmentAlgorithm> {
    match s {
        "github" => Some(FragmentAlgorithm::Github),
        "gitlab" => Some(FragmentAlgorithm::Gitlab),
        "vscode" => Some(FragmentAlgorithm::Vscode),
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
            6,
            "should have 6 default predicates"
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
            config.predicates.len() >= 7,
            "at least 7 predicates (6 defaults + 1 new)"
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

        assert_eq!(config.predicates.len(), 6, "defaults preserved");
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
}
