# Escaped brackets (issue 056)

An escaped opener \[not a link](./a.md) and an escaped closer [text\](./b.md).

Both escaped: \[text\](./c.md) stays literal text.

A doubled backslash before an opener \\[still a link](./d.md "references").

Three backslashes \\\[escape the bracket](./e.md) again.

Escaped bracket inside link text: [a \] b](./f.md) and [a \[ b](./g.md).

A lone trailing backslash escapes nothing: [tail](./h.md) \
