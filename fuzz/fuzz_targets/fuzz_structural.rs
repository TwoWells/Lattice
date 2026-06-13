// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Fuzz the structural diagnostic layer the way the workspace loader drives it
//! (issue 033): parse frontmatter and the block tree, then run
//! `structural::collect` — dark-matter path detection, headings, raw HTML,
//! code-block language, and the quoted / bare / backtick reference scanners —
//! with a deterministic existence oracle. Mirroring `recompute_structural` also
//! exercises the 030 external-resolution and 031 exception-reconciliation paths.
//!
//! Asserts the pass never panics and that every emitted diagnostic span is a
//! valid, char-boundary byte range that round-trips through the LSP position
//! mapping — the invariant that catches the byte-index class of bug the issue
//! 032 single-quote guard is exposed to.

#![no_main]

use lattice::invariants::assert_structural_invariants;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(source) = std::str::from_utf8(data) else {
        return;
    };
    assert_structural_invariants(source);
});
