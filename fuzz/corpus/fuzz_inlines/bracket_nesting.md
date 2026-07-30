# Bracket nesting (issue 056)

A [link [with [deeply [nested] brackets] in] its text](./a.md "references") here.

Balanced siblings: [one] [two] [three [four] five] six.

Adjacent closers: [a [b] [c]](./b.md) and [[d]](./c.md).

Openers first: [[[[deep]]]] and [[[a]b]c]d.

An empty pair [] and a nested empty pair [[]] and [[][]].

[a [b](./inner.md) c](./outer.md "implements")
