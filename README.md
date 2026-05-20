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
lattice
```

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
```

## License

AGPL-3.0-or-later. Commercial license available — contact [Two Wells](mailto:contact@twowells.dev).
