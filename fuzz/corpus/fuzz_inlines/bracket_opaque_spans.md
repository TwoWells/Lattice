# Brackets against spans the scanner treats as opaque (issue 056)

Math holding a balanced pair: [bound $a[i]$ tight](./a.md "references") links.

Math holding an unbalanced opener: [bound $a[i$ loose](./b.md) — the `[` inside
the math span is live in the bracket table but skipped by the scanner.

Math holding an unbalanced closer: [bound $a]i$ loose](./c.md) here.

Raw HTML holding an unbalanced opener: [a <span>x[y</span> b](./d.md) here.

A tag attribute holding a bracket: [a <span data-x="[">z</span> b](./j.md) here.

A tag that never closes: [a <span data-x="[ b](./k.md) here.

Autolink holding a bracket: [a <https://example.com/x[y> b](./e.md) here.

An HTML comment holding brackets: [a <!-- [b] --> c](./f.md) here.

Dollar that is not math: a $5 [link](./g.md) and $ [x](./h.md) $ here.

Math span with no closer: $a[b [c](./i.md) then more text.
