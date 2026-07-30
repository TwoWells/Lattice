# Image brackets versus link brackets (issue 056)

An image ![alt](./pic.png "t") and a link [text](./a.md "references") together.

Image with nested brackets in its alt: ![a [b] c](./pic.png) here.

Image inside link text: [see ![alt](./pic.png) now](./b.md "implements").

Bang without a bracket: a! [link](./c.md) and !not-an-image.

Escaped bang before a bracket: \![alt](./pic.png) is a link, not an image.

Bang before an escaped bracket: !\[alt](./pic.png) is literal.

Reference image forms: ![full][r] and ![collapsed][] and ![shortcut].

Doubled bangs: !![alt](./pic.png) and !!![alt](./pic.png).

[r]: ./pic.png "references"
[collapsed]: ./pic2.png
[shortcut]: ./pic3.png "implements"
