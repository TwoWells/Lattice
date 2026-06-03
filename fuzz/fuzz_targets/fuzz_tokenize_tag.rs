// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Fuzz the HTML tag tokenizer: open/close/comment recognition and attribute
//! extraction. Asserts every reported span and consumed length stays within
//! the input text.

#![no_main]

use lattice::fuzz_api::tokenize_tag;
use lattice::invariants::assert_html_tag_in_bounds;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if let Some(tag) = tokenize_tag(text, 0) {
        assert_html_tag_in_bounds(&tag, text);
    }
});
