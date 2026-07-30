# Brackets against multi-byte bytes (issue 056)

Multi-byte right against the brackets: 日[本](./a.md)語 and 🎉[party](./🎉.md)🎉.

Multi-byte inside the text: [café résumé 日本語 🎉](./café.md "references").

Zero-width joiner inside the label: [a​b](./b.md) and [c‍d][r] here.

Zero-width space around brackets: ​[a]​(./c.md)​ and ​[b](./d.md).

A backslash escaping a multi-byte character: \é[a](./e.md) then \🎉[b](./f.md).

An escape whose target is the first byte of a multi-byte char: [a \日 b](./g.md).

Combining marks: [école](./h.md) and [é́](./i.md) here.

Right-to-left text: [العربية](./j.md) and [עברית](./k.md "implements").

Code span with multi-byte and brackets: `日[本]語` then [a](./l.md).

[r]: ./m.md "references"
