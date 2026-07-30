# Unmatched and spurious brackets (issue 056)

Openers with no closer: [[[[ and [a [b [c and [text](./a.md

Closers with no opener: ]]]] and a]b]c] and ](./b.md) alone.

One closer for many openers: [[[[a] then [[[[b]]

More closers than openers: [a]]]] and [[a]]]] and a][b][c.

A closer before its opener: ]text[ and ][ and ]] [[ ]].

Shortcut with no definition [dangling] and [dangling][] and [a][missing].

An opener at end of line [
and a closer at start of line
] done.

Trailing opener at end of file [
