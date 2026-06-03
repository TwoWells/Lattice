// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Lattice: a markdown predicate linter and backlink reconciler.
//!
//! The crate ships as a single binary (`lattice`) that is both the CLI
//! (`lattice lint`) and the LSP server (`lattice serve`). The library target
//! exists so internal parsers can be exercised by out-of-crate test harnesses
//! — notably the `cargo-fuzz` targets under `fuzz/`, which link this crate
//! with the `fuzzing` feature and call into [`fuzz_api`].

use std::io::{self, Write};
use std::process::ExitCode;

use clap::Parser;

mod block;
mod cli;
mod config;
mod fm;
mod html;
mod inline;
mod json;
mod limits;
mod lint;
mod lsp;
mod server;
mod span;
mod structural;
mod toml;
mod validation;
mod workspace;
mod yaml;

#[cfg(test)]
mod encoding_tests;
#[cfg(test)]
mod property_tests;

/// Shared parse invariants, asserted by both the property suite and the fuzz
/// targets so the two cannot drift. Compiled for tests and for fuzzing only.
#[cfg(any(test, feature = "fuzzing"))]
pub mod invariants;

/// Stable facade over the internal parser entry points for the `fuzz/` crate.
///
/// Compiled only with the `fuzzing` feature, so the normal build keeps its
/// public surface to [`run`] alone.
#[cfg(feature = "fuzzing")]
pub mod fuzz_api;

/// Run the Lattice CLI: parse arguments and dispatch to `lint` or `serve`.
///
/// Returns the process exit code — `0` on success, `1` when linting finds
/// errors or a subcommand fails.
#[must_use]
pub fn run() -> ExitCode {
    let args = cli::Cli::parse();

    match args.command {
        cli::Command::Lint { path } => {
            let mut stderr = io::stderr().lock();
            match lint::run(&path, &mut stderr) {
                Ok(has_errors) => {
                    if has_errors {
                        ExitCode::from(1)
                    } else {
                        ExitCode::from(0)
                    }
                }
                Err(e) => {
                    let _ = writeln!(stderr, "error: {e:#}");
                    ExitCode::from(1)
                }
            }
        }
        cli::Command::Serve => match server::run() {
            Ok(()) => ExitCode::from(0),
            Err(e) => {
                let _ = writeln!(io::stderr().lock(), "error: {e:#}");
                ExitCode::from(1)
            }
        },
    }
}
