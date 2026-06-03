// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Binary entry point for Lattice. All logic lives in the library crate
//! (`lattice::run`); this shim exists so the parsers are also reachable as a
//! library by the `cargo-fuzz` targets under `fuzz/`.

use std::process::ExitCode;

fn main() -> ExitCode {
    lattice::run()
}
