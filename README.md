# Lattice

[![crates.io](https://img.shields.io/crates/v/lattice.svg)](https://crates.io/crates/lattice)
[![docs.rs](https://img.shields.io/docsrs/lattice)](https://docs.rs/lattice)
[![CI](https://github.com/TwoWells/Lattice/actions/workflows/ci.yml/badge.svg)](https://github.com/TwoWells/Lattice/actions/workflows/ci.yml)
[![license](https://img.shields.io/crates/l/lattice.svg)](#license)

A markdown predicate linter and backlink reconciler, shipped as an LSP server.

Lattice encodes link predicates in CommonMark title text and maintains
backlinks in YAML frontmatter. The graph lives in the files themselves —
no database, everything in git.

## Installation

```sh
cargo install lattice
```

Or download a prebuilt binary (Linux, macOS, Windows) from the
[latest release](https://github.com/TwoWells/Lattice/releases/latest).
Building from source needs Rust 1.95 or newer.

## Usage

```sh
# Lint all markdown files in the current project (exit 1 on errors)
lattice lint

# Run as an LSP server (used by editors)
lattice serve

# Print the full configuration reference (every option, with defaults)
lattice config

# Print the version (with git commit and dirty state)
lattice --version
```

## Neovim

Lattice is not shipped by `nvim-lspconfig`, so configure it directly.
On Neovim 0.11+:

```lua
vim.lsp.config.lattice = {
  cmd = { "lattice", "serve" },
  filetypes = { "markdown" },
  root_markers = { ".lattice.toml", ".git" },
}
vim.lsp.enable("lattice")
```

Diagnostics appear inline on open, change, and save. When the editor
supports LSP dynamic file-watch registration
(`workspace/didChangeWatchedFiles`), Lattice also registers watches for
`.lattice.toml` and project `.md` files, so config edits hot-reload (no
restart) and on-disk changes made outside the editor (e.g. a `git
checkout`) update the graph live; without that support, both apply only
on restart. Lattice is
diagnostic-first, but the server is a full markdown LSP: completion
(paths, headings, predicates, reference labels, footnotes), hover,
document and workspace symbols, references, rename, folding, document
links, formatting, semantic tokens (emphasis styling, below), and
go-to-definition/declaration/type/implementation plus call and type
hierarchy over the predicate graph.

### Emphasis styling (semantic tokens)

Lattice emits semantic tokens for emphasis runs: one token type,
`markup`, carrying `bold`, `italic`, and `strikethrough` modifiers.
Neovim maps each modifier to an `@lsp.mod.<name>` highlight group, and
neither Neovim core nor common colorschemes define these, so the tokens
paint nothing until you do — once, after your colorscheme loads (a
`ColorScheme` autocmd survives reloads):

```lua
vim.api.nvim_set_hl(0, "@lsp.mod.bold", { bold = true })
vim.api.nvim_set_hl(0, "@lsp.mod.italic", { italic = true })
vim.api.nvim_set_hl(0, "@lsp.mod.strikethrough", { strikethrough = true })
```

Tokens are served for any opened markdown document, rooted or not: a
scratch file outside every workspace root (a note in `/tmp`, say) gets
the same emphasis styling as a project file. Workspace roots gate only
the graph-linting tier — backlinks, predicates, connectivity.

Treesitter's own markdown emphasis highlighting stays active alongside,
and extmark attributes merge — so a span the grammar gets wrong stays
lit no matter what Lattice says (tree-sitter-markdown-inline
approximates GFM flanking rules and will, for example, pair a
single-tilde strikethrough across a soft line break). To hand emphasis
styling to Lattice entirely, blank the language-qualified treesitter
groups; the qualified name shadows the generic `@markup.*` for markdown
only, and every other filetype keeps its styling:

```lua
vim.api.nvim_set_hl(0, "@markup.strong.markdown_inline", {})
vim.api.nvim_set_hl(0, "@markup.italic.markdown_inline", {})
vim.api.nvim_set_hl(0, "@markup.strikethrough.markdown_inline", {})
```

Leave treesitter itself enabled for markdown: it still provides the
structural highlighting (headings, links, code spans) and the injected
highlighting inside fenced code blocks; only its emphasis captures go
quiet. Emphasis styling then updates at semantic-token cadence (on
server response) rather than per keystroke.

On Neovim older than 0.11, start it per buffer instead:

```lua
vim.api.nvim_create_autocmd("FileType", {
  pattern = "markdown",
  callback = function(args)
    vim.lsp.start({
      name = "lattice",
      cmd = { "lattice", "serve" },
      root_dir = vim.fs.root(args.buf, { ".lattice.toml", ".git" }),
    })
  end,
})
```

## Git hook

Add to `.githooks/pre-commit` or `.git/hooks/pre-commit`:

```sh
#!/bin/sh
lattice lint
```

Commits with broken links, unknown predicates, or missing backlinks
will be rejected.

## Configuration

An optional `.lattice.toml` at the project root overrides defaults:

```toml
[predicates]
supersedes = "superseded_by"
implements = "implemented_by"
depends_on = "dependency_of"
amends = "amended_by"
blocks = "blocked_by"
references = "referenced_by"

[policy]
predicates = "optional"    # or "required"
backlinks = true
bare_paths = "warn"        # or "deny", "disabled"
stale_references = "warn"  # or "hint", "deny", "disabled" — dangling `.md` references
# fragments = "github"    # or "gitlab", "vscode"; omit to try all
# connectivity = "off"    # or "no-orphans", "no-islands", "reachable"
# roots = ["README.md"]   # entry points for "reachable"; default = root README

# Opt-in convention checks (off by default — they flag valid CommonMark, not defects):
# code_block_language = "disabled"  # or "hint", "warn", "deny" — flag untagged code fences
# multiple_h1 = false               # flag more than one H1 heading
# skipped_heading_level = false     # flag a skipped heading level (e.g. H1 -> H3)
# image_empty_alt = false           # flag images with empty alt text

# Cross-repo references: a `{Name}/path` citation (backtick/quoted/bare, not a
# link) is checked existence-only against an aliased directory — never read,
# indexed, or treated as a graph edge. An undefined alias, or one whose
# directory is absent, is exempt; a present directory with a missing file is a
# stale reference. Relative (sibling-checkout) values are preferred.
# [external]
# Catenary = "../Catenary"
```

### Per-reference exceptions

A path-shaped string that is deliberately not a live reference (a worked
example, a counterexample, a knowingly-dead link) can be excepted from the
`stale_references` / `bare_paths` lints in the document's own frontmatter,
keyed by the literal reference with a required reason as the value:

```yaml
---
exceptions:
  stale_references:
    "tickets/acquire/DESIGN.md": "hypothetical path in the worked example"
    "{Catenary}/old/layout.md": "pre-refactor path, kept for the changelog note"
  bare_paths:
    "README.md": "naming the file, deliberately not a link"
---
```

Exceptions are reconciled like backlinks, not silenced: an entry that matches
no live diagnostic — its reference gone, or now resolving — is itself flagged
as an *unused exception* whose message echoes the stored reason, and an entry
with an empty reason is flagged too. Lattice flags, never auto-removes — the
reason is the surviving record of a vanished reference's intent. A `{Name}/…`
key flows through identically. An exception is never a graph edge and imposes
no backlink obligation.

## Agent instructions

Add this to your project's `AGENTS` or `CLAUDE` file:

> Markdown links follow [Lattice](https://github.com/TwoWells/Lattice)
> conventions: predicates are encoded in title text, e.g.
> `[Doc](doc.md "supersedes")`. The predicate vocabulary is: supersedes,
> implements, depends_on, amends, blocks, references. Backlinks are
> maintained in YAML frontmatter.

## License

AGPL-3.0-or-later. Commercial license available — contact [Two Wells](mailto:contact@twowells.dev).
