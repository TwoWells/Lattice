# Title

A carrier carrying both backlinks and a reconciled `exceptions` block.

```yaml lattice
backlinks:
  referenced_by:
    - ../README.md
exceptions:
  stale_references:
    "old.md": migrated to new.md
  bare_paths:
    "./notes.md": kept intentionally
```
