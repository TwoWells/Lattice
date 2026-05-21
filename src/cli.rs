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
    /// and 1 when any error-level diagnostic is present.
    Lint {
        /// Directory to lint (defaults to the current working directory).
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}
