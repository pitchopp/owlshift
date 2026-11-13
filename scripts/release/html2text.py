"""Plain text of an HTML file, for the Rust standard library notices (OWL-551).

    python3 -I html2text.py FILE

Writes the text to stdout. Script, style and title blocks are dropped, a line break
falls at each block tag, entities are decoded, whitespace runs collapse
outside <pre> and are kept as they are inside it. Exits 1 with a message when
the file cannot be read or holds no text. Standard library only.
"""

import sys
from html.parser import HTMLParser

BLOCKS = {
    "p", "div", "br", "li", "ul", "ol", "tr", "table", "pre", "hr",
    "h1", "h2", "h3", "h4", "h5", "h6", "details", "summary", "section",
}


class Text(HTMLParser):
    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.out = []
        self.skip = 0
        self.pre = 0

    def handle_starttag(self, tag, attrs):
        if tag in ("script", "style", "title"):
            self.skip += 1
        if tag == "pre":
            self.pre += 1
        if tag in BLOCKS:
            self.out.append("\n")

    def handle_endtag(self, tag):
        if tag in ("script", "style", "title") and self.skip:
            self.skip -= 1
        if tag == "pre" and self.pre:
            self.pre -= 1
        if tag in BLOCKS:
            self.out.append("\n")

    def handle_data(self, data):
        if self.skip:
            return
        if self.pre:
            self.out.append(data)
            return
        text = " ".join(data.split())
        if data[:1].isspace():
            text = " " + text
        if data[-1:].isspace():
            text += " "
        if not self.out or self.out[-1].endswith("\n"):
            text = text.lstrip()
        self.out.append(text)


def main():
    if len(sys.argv) != 2:
        print("usage: html2text.py FILE", file=sys.stderr)
        return 2
    try:
        with open(sys.argv[1], encoding="utf-8") as f:
            html = f.read()
    except OSError as e:
        print(f"error: cannot read {sys.argv[1]}: {e.strerror}", file=sys.stderr)
        return 1
    parser = Text()
    parser.feed(html)
    parser.close()
    text = "\n".join(line.rstrip() for line in "".join(parser.out).split("\n"))
    while "\n\n\n" in text:
        text = text.replace("\n\n\n", "\n\n")
    text = text.strip()
    if not text:
        print(f"error: {sys.argv[1]} holds no text", file=sys.stderr)
        return 1
    print(text)
    return 0


sys.exit(main())
