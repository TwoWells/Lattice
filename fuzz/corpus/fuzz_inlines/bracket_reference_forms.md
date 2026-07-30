# Reference-style bracket forms (issue 056)

Full [text][r], collapsed [collapsed][], and shortcut [shortcut] references.

Three brackets in a row: [a][b][c] and [a][][b] and [][r].

Nested label brackets: [text][a[b]c] and [a[b]c][r] here.

A footnote ref[^note] next to a link [text][r] on one line.

Escaped bracket in a label: [text][a\]b] and [text\][r].

A reference whose label spans a line break [text][
r] here.

Definition-looking lines inside a paragraph:
[not a def][r] because the paragraph continues.

[r]: ./a.md "references"
[collapsed]: ./b.md
[shortcut]: ./c.md "implements"
[a[b]c]: ./d.md
[^note]: the footnote body.
