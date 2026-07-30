# Brackets across code spans (issue 056, backtick runs of issue 017)

A bracket inside a code span is not a bracket: `[not a link](x)` stays code.

Unbalanced opener inside a span: [text `a[b` more](./a.md "references") links.

Unbalanced closer inside a span: [text `a]b` more](./b.md) links too.

A double-backtick span holds a single backtick: ``a ` [b] c`` then [d](./c.md).

A run longer than its closer never closes: ```[a](./d.md) and `[b](./e.md).

Runs of different lengths: ``[a`` `[b` ```[c``` [d](./f.md).

An escaped backtick does not open a span: \`[a](./g.md) is a link.

An escaped backtick inside a span: `a \` b` [c](./h.md) after.

Span closed on the next line: `[a
b` [c](./i.md)

Unclosed span swallows the rest of the paragraph `[a](./j.md) [b](./k.md)
