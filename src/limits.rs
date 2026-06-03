// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Two Wells <contact@twowells.dev>

//! Parser resource limits guarding against adversarial input.
//!
//! Hand-rolled parsers must degrade gracefully on pathological input —
//! deeply nested structures, enormous frontmatter blocks, and degenerate
//! patterns can otherwise cause stack overflows, quadratic runtime, or
//! unbounded memory growth. These constants cap nesting depth, document
//! size, and tree growth so the parser cannot blow up on any input.
//!
//! When a limit is reached the parser emits a diagnostic at the point of
//! truncation and continues with reduced fidelity. The document is still
//! indexed — limits cause degradation, not failure. The values are
//! deliberately generous: no reasonable document approaches them.
//!
//! Limits are constants, not configurable.

/// Maximum block quote nesting depth. Deeper `>` markers are treated as
/// text rather than opening further quote scopes.
pub const MAX_QUOTE_NESTING: usize = 100;

/// Maximum list nesting depth. Deeper list markers are treated as text
/// rather than opening further list scopes.
pub const MAX_LIST_NESTING: usize = 100;

/// Maximum HTML container nesting depth. Once reached, new container tags
/// stop opening scopes and are recorded as flat leaves.
pub const MAX_HTML_NESTING: usize = 100;

/// Maximum frontmatter mapping / table / object nesting depth, shared by
/// the YAML, TOML, and JSON parsers. Deeper structure is flattened.
pub const MAX_FRONTMATTER_NESTING: usize = 64;

/// Hard limit on the scope stack depth across all container types. This is
/// the backstop that bounds total nesting when several container kinds are
/// interleaved.
pub const MAX_SCOPE_DEPTH: usize = 256;

/// Maximum number of nodes in a single parse tree. Once reached, no new
/// nodes are created and the remaining structure is left unindexed.
pub const MAX_NODES: usize = 100_000;

/// Maximum size in bytes of frontmatter content (the text between the
/// delimiters). Larger blocks are treated as opaque and skipped.
pub const MAX_FRONTMATTER_BYTES: usize = 1024 * 1024;

/// Maximum length in bytes of a single line scanned for inline elements.
/// Bytes past this point on a line are treated as plain text, so a
/// degenerate line cannot drive quadratic inline scanning. Block structure
/// (headings, list markers, fences) is recognized at line start regardless.
pub const MAX_INLINE_LINE_BYTES: usize = 10_000;
