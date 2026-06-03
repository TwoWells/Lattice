// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Fuzz the block-structure parser entry point: scope stack, block recognition,
//! and the inline pass (which `parse_tree` runs). Asserts the universal tree
//! invariants and inline-resource content fidelity — not merely "no panic".

#![no_main]

use lattice::fuzz_api::parse_tree;
use lattice::invariants::{assert_inline_resource_fidelity, assert_tree_wellformed};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(source) = std::str::from_utf8(data) else {
        return;
    };
    let tree = parse_tree(source, None);
    assert_tree_wellformed(&tree);
    assert_inline_resource_fidelity(&tree);
});
