// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Build script: stamp the binary with the git short-commit and a dirty flag.
//!
//! At compile time this shells out to `git` (no build dependency — the crate's
//! tree is deliberately lean) to capture the short commit and whether the
//! working tree had uncommitted changes, then exposes both to the crate via
//! `cargo:rustc-env`. `src/cli.rs` reads them with `option_env!` and composes
//! the `--version` string (see `cli::compose_version`).
//!
//! Graceful fallback: if `git` is not on `PATH`, or this is not a git checkout
//! (a crates.io / tarball build), no git env vars are emitted and `--version`
//! falls back to the bare crate version. This script never fails the build on a
//! git absence or error.
//!
//! `println!` here is the cargo-mandated mechanism for build-script directives
//! (`cargo:rustc-env=…`, `cargo:rerun-if-changed=…`), emitted over stdout — not
//! application logging — so the crate's `print_stdout` denial does not apply to
//! this file (build scripts are a separate compilation unit).

use std::process::Command;

/// Run a `git` subcommand and return its trimmed stdout when it exits cleanly.
///
/// Returns `None` if `git` is missing, the command fails, or the output is
/// empty — every failure mode degrades to "no git info" rather than erroring.
fn git_stdout(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

fn main() {
    // Best-effort rebuild freshness: a commit moves `.git/HEAD` (on a checkout)
    // or the ref it points at; a staged change rewrites `.git/index`. Watching
    // all three re-stamps the version on a commit or a staged change.
    //
    // Caveat: this is not a perfect rebuild trigger. An UNSTAGED-only edit
    // (which flips the dirty flag but touches none of these files) may not
    // re-trigger the build script, so a stale `dirty`/clean marker can survive
    // until the next rebuild for another reason. Documented rather than
    // over-engineered (issue 040).
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
    if let Some(head_ref) = git_stdout(&["symbolic-ref", "HEAD"])
        && let Some(ref_path) = git_stdout(&["rev-parse", "--git-path", &head_ref])
    {
        println!("cargo:rerun-if-changed={ref_path}");
    }

    // Short commit. Absent (no git / not a repo) -> emit nothing; `--version`
    // falls back to the bare crate version.
    if let Some(hash) = git_stdout(&["rev-parse", "--short", "HEAD"]) {
        println!("cargo:rustc-env=LATTICE_GIT_HASH={hash}");

        // Dirty when `git status --porcelain` prints any line. `git_stdout`
        // returns `None` for empty output, so a clean tree maps to "0".
        let dirty = git_stdout(&["status", "--porcelain"]).is_some();
        let flag = if dirty { "1" } else { "0" };
        println!("cargo:rustc-env=LATTICE_GIT_DIRTY={flag}");
    }
}
