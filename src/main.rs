// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Lattice: a markdown predicate linter and backlink reconciler.

use std::io::{self, Write};
use std::process::ExitCode;

use clap::Parser;

mod cli;
mod config;
mod frontmatter;
mod lint;
mod lsp;
mod markdown;
mod server;
mod span;
mod validation;
mod workspace;
mod yaml;

fn main() -> ExitCode {
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
