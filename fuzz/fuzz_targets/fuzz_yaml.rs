// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Fuzz the YAML frontmatter parser. Asserts block well-formedness and scalar
//! content fidelity (the byte-as-`char` mojibake class from ticket 21).

#![no_main]

use lattice::fuzz_api::parse_yaml_frontmatter;
use lattice::invariants::{assert_block_wellformed, assert_frontmatter_scalar_fidelity};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(source) = std::str::from_utf8(data) else {
        return;
    };
    if let Some(block) = parse_yaml_frontmatter(source) {
        assert_block_wellformed(&block, source);
        assert_frontmatter_scalar_fidelity(&block, source);
    }
});
