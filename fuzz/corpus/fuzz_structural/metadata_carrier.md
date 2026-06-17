# Metadata channel carrier (decision 015)

A naked `yaml lattice` carrier under the H1:

```yaml lattice
backlinks:
  referenced_by:
    - ../README.md
exceptions:
  stale_references:
    "old.md": legacy
```

A `<details>`-wrapped carrier, collapsed at the foot:

<details><summary>lattice</summary>

```yaml lattice
backlinks:
  referenced_by:
    - café.md
```

</details>

A documented example nested in an outer fence stays inert:

````markdown
```yaml lattice
backlinks:
  referenced_by:
    - example.md
```
````
