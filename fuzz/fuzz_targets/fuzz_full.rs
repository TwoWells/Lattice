// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Fuzz the full document pipeline the workspace loader uses: frontmatter
//! detection (YAML → TOML → JSON) → block tree → backlink extraction. Asserts
//! tree and frontmatter well-formedness, both content-fidelity invariants, and
//! the LSP byte ↔ position round-trip the server relies on for diagnostics.
//!
//! Also drives the `yaml lattice` metadata-carrier channel (decision 015): when
//! a document sources its metadata from a top-level carrier rather than a leading
//! block, [`assert_carrier_fidelity`] checks the carrier-sourced frontmatter is
//! faithful and agrees with the same YAML as a leading `---` block (ticket 25).

#![no_main]

use std::path::Path;

use lattice::fuzz_api::{Config, parse_content};
use lattice::invariants::{
    assert_block_wellformed, assert_carrier_fidelity, assert_frontmatter_scalar_fidelity,
    assert_inline_resource_fidelity, assert_line_index_agrees, assert_position_round_trip,
    assert_tree_wellformed, detect_frontmatter,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(source) = std::str::from_utf8(data) else {
        return;
    };
    let file = parse_content(source, Path::new("fuzz.md"), &Config::default());
    assert_tree_wellformed(&file.tree);
    assert_inline_resource_fidelity(&file.tree);
    assert_position_round_trip(source);
    // The cached index must be a byte-for-byte drop-in for the scalar conversion.
    assert_line_index_agrees(source, &file.line_index);

    // The pipeline detected and consumed frontmatter internally; re-detect it
    // so the scalar-fidelity invariant can inspect the parsed block.
    if let (Some(block), _) = detect_frontmatter(source) {
        assert_block_wellformed(&block, source);
        assert_frontmatter_scalar_fidelity(&block, source);
    }

    // When metadata is sourced from a `yaml lattice` carrier instead of a leading
    // block, assert the carrier-sourced frontmatter is faithful and that it agrees
    // with the same YAML presented as a leading `---` block (ticket 25). The
    // leading-block fidelity check above re-detects only `detect_frontmatter`'s
    // `---`/`+++`/`{` block and skips the carrier entirely, so this closes the
    // content-fidelity blind spot for the carrier channel.
    assert_carrier_fidelity(source);
});
