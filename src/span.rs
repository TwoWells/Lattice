// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Byte-offset span type shared by all parser components.
//!
//! [`Span`] represents a contiguous region of source text by its start
//! (inclusive) and end (exclusive) byte offsets. Every parsed node carries
//! a `Span` that maps it back to the original source.

use std::ops::Range;

/// A byte-offset range into source text.
///
/// Represents a contiguous region of source text by its start (inclusive)
/// and end (exclusive) byte offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Span {
    /// Inclusive start byte offset.
    pub start: usize,
    /// Exclusive end byte offset.
    pub end: usize,
}

impl Span {
    /// Create a new span from start (inclusive) and end (exclusive) byte
    /// offsets.
    #[must_use]
    pub const fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    /// The length of this span in bytes.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.end - self.start
    }

    /// Whether this span is empty (zero length).
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.start == self.end
    }
}

impl From<Range<usize>> for Span {
    fn from(range: Range<usize>) -> Self {
        Self {
            start: range.start,
            end: range.end,
        }
    }
}

impl From<Span> for Range<usize> {
    fn from(span: Span) -> Self {
        span.start..span.end
    }
}
