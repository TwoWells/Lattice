// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Stable facade over the internal parser entry points, for the out-of-crate
//! `cargo-fuzz` targets under `fuzz/`.
//!
//! The crate's modules are private (the only normal public symbol is
//! [`crate::run`]). This module — compiled solely under the `fuzzing` feature —
//! re-exports exactly the entry points and types the fuzz targets name, so the
//! normal build and `make check` never widen the public API or take on the
//! documentation burden of the parser internals.
//!
//! Pair these entry points with the assertions in [`crate::invariants`]: call
//! a parser here, then assert the relevant invariant there.

pub use crate::block::{Tree, parse_tree, parse_tree_with_entries};
pub use crate::config::Config;
pub use crate::fm::FrontmatterBlock;
pub use crate::html::{HtmlTag, tokenize_tag};
pub use crate::inline::parse_inlines;
pub use crate::json::parse_frontmatter_block as parse_json_frontmatter;
pub use crate::line_index::LineIndex;
pub use crate::span::Span;
pub use crate::toml::parse_frontmatter_block as parse_toml_frontmatter;
pub use crate::workspace::{FileData, parse_content};
pub use crate::yaml::parse_frontmatter_block as parse_yaml_frontmatter;
