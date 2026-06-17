// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Fuzz the inline pass (links, images, code spans, math, footnote refs,
//! emphasis / strong / strikethrough runs).
//! `parse_tree` already runs the inline pass, so re-running `parse_inlines`
//! must be a no-op: idempotence is the invariant. Also asserts the tree stays
//! well-formed, inline resource fields remain faithful to the source, and every
//! emphasis-run span is delimited correctly.

#![no_main]

use lattice::fuzz_api::{parse_inlines, parse_tree};
use lattice::invariants::{
    assert_emphasis_span_fidelity, assert_inline_resource_fidelity, assert_tree_wellformed,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(source) = std::str::from_utf8(data) else {
        return;
    };
    let mut tree = parse_tree(source, None);
    let nodes_before = tree.len();
    let diags_before = tree.diagnostics().len();

    parse_inlines(&mut tree);

    assert_eq!(
        tree.len(),
        nodes_before,
        "re-running the inline pass added nodes"
    );
    assert_eq!(
        tree.diagnostics().len(),
        diags_before,
        "re-running the inline pass added diagnostics"
    );
    assert_tree_wellformed(&tree);
    assert_inline_resource_fidelity(&tree);
    assert_emphasis_span_fidelity(&tree);
});
