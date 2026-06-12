// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Differential parse/diagnostic oracle (perf ticket 03), and the gate for
//! tickets perf 04 / 05.
//!
//! Decodes a fuzzer input into a base markdown document and a sequence of
//! `{range, text}` edits, then drives them through
//! [`assert_edit_sequence_stable`]: every intermediate document is re-parsed
//! from scratch and checked against the full set of parse invariants, and every
//! edit maps its range through the `LineIndex` inverse the incremental
//! text-sync path will use. Today this is a parser-stability net over random
//! edit sequences; once an incremental parse/graph path lands, the same entry
//! point gains the `incremental(edits) ≡ full(final_text)` arm.
//!
//! ## Wire format
//!
//! The input is read as UTF-8 (non-UTF-8 is rejected, like the sibling
//! targets), then split on NUL (`\0`): the first field is the base document,
//! each remaining field is one edit. An edit field is `coords \x01 replacement`,
//! where `coords` is up to four decimal integers — `start_line`, `start_char`,
//! `end_line`, `end_char` — separated by any non-digit run, and `replacement`
//! (everything after the first `\x01`, possibly empty or absent) is the text
//! spliced in. Missing coordinates default to 0 / the start, an unparsable or
//! overflowing integer becomes `u32::MAX`, and out-of-range positions are
//! clamped by `LineIndex::offset` — so every decoded edit applies. The framing
//! keeps seeds human-readable: the base segment is literal markdown.

#![no_main]

use lattice::invariants::{Edit, assert_edit_sequence_stable};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    // The first NUL-delimited field is the base; the rest are edits. `split`
    // always yields at least one field, so `base` is always present.
    let mut fields = text.split('\u{0}');
    let base = fields.next().unwrap_or("");
    let edits: Vec<Edit> = fields
        .map(|field| {
            let (coords, replacement) = field.split_once('\u{1}').unwrap_or((field, ""));
            let mut nums = coords
                .split(|c: char| !c.is_ascii_digit())
                .filter(|s| !s.is_empty())
                .map(|s| s.parse::<u32>().unwrap_or(u32::MAX));
            let start_line = nums.next().unwrap_or(0);
            let start_char = nums.next().unwrap_or(0);
            // A missing end defaults to the start: a zero-width insertion point.
            let end_line = nums.next().unwrap_or(start_line);
            let end_char = nums.next().unwrap_or(start_char);
            Edit {
                start_line,
                start_char,
                end_line,
                end_char,
                text: replacement.to_string(),
            }
        })
        .collect();
    assert_edit_sequence_stable(base, &edits);
});
