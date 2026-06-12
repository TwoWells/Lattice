# Lattice

A markdown predicate linter and backlink reconciler, shipped as an LSP server.

Lattice encodes link predicates in CommonMark title text and maintains
backlinks in YAML frontmatter. The graph lives in the files themselves —
no database, everything in git.

## Usage

```sh
# Lint all markdown files in the current project
lattice lint

# Run as an LSP server (used by editors)
lattice serve
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

Diagnostics appear inline on open, change, and save. Lattice is
diagnostic-first, but the server also answers document/workspace
symbols, references, rename, hover, folding, and document links over
the predicate graph.

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
# fragments = "github"    # or "gitlab", "vscode"; omit to try all
# connectivity = "off"    # or "no-orphans", "no-islands", "reachable"
# roots = ["README.md"]   # entry points for "reachable"; default = root README

# Opt-in convention checks (off by default — they flag valid CommonMark, not defects):
# code_block_language = "disabled"  # or "hint", "warn", "deny" — flag untagged code fences
# multiple_h1 = false               # flag more than one H1 heading
# skipped_heading_level = false     # flag a skipped heading level (e.g. H1 -> H3)
# image_empty_alt = false           # flag images with empty alt text
```

## Agent instructions

Add this to your project's `AGENTS.md` or `CLAUDE.md`:

> Markdown links follow [Lattice](https://github.com/TwoWells/Lattice)
> conventions: predicates are encoded in title text, e.g.
> `[Doc](doc.md "supersedes")`. The predicate vocabulary is: supersedes,
> implements, depends_on, amends, blocks, references. Backlinks are
> maintained in YAML frontmatter.

## License

AGPL-3.0-or-later. Commercial license available — contact [Two Wells](mailto:contact@twowells.dev).
