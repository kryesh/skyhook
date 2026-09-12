#!/usr/bin/env python3
"""Check local links in the built mdBook and repository entry-point READMEs.

Uses only Python 3.11+'s standard library. Run after `mdbook build docs` (or use
`just docs-check`). External URLs are deliberately not fetched.
"""

from html.parser import HTMLParser
from pathlib import Path
import re
import sys
import tomllib
from urllib.parse import unquote, urlsplit


ROOT = Path(__file__).resolve().parent.parent
BOOK = ROOT / "docs" / "book"


class Page(HTMLParser):
    def __init__(self, text):
        super().__init__(convert_charrefs=True)
        self.anchors = set()
        self.links = []
        self.feed(text)

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if attrs.get("id"):
            self.anchors.add(attrs["id"])
        if tag == "a" and attrs.get("name"):
            self.anchors.add(attrs["name"])
        for name in ("href", "src"):
            if attrs.get(name):
                self.links.append(attrs[name])


def check_book(book, site_root="/"):
    site_root = urlsplit(site_root).path.rstrip("/") + "/"
    pages = {
        path.resolve(): Page(path.read_text(encoding="utf-8"))
        for path in book.rglob("*.html")
    }
    if not pages:
        return ["No built HTML found; run `just docs` first."]

    errors = []
    for source, page in sorted(pages.items()):
        for link in page.links:
            url = urlsplit(link)
            if url.scheme or url.netloc:
                continue
            # mdBook's generated 404 page uses the configured absolute site root.
            # Resolve it within the output, but reject links outside that prefix.
            if url.path.startswith("/"):
                if not url.path.startswith(site_root):
                    errors.append(f"{source.relative_to(book)}: link outside site root {link!r}")
                    continue
                target = (book / unquote(url.path[len(site_root):])).resolve()
            else:
                target = (source.parent / unquote(url.path)).resolve() if url.path else source
            if target.is_dir():
                target /= "index.html"
            if not target.is_relative_to(book):
                errors.append(f"{source.relative_to(book)}: link escapes the book: {link!r}")
            elif not target.is_file():
                errors.append(f"{source.relative_to(book)}: missing target {link!r}")
            elif url.fragment and target in pages:
                if unquote(url.fragment) not in pages[target].anchors:
                    errors.append(f"{source.relative_to(book)}: missing anchor {link!r}")
    return sorted(set(errors))


def check_readmes():
    errors = []
    for relative in ("README.md", "crates/skyhook-core/README.md"):
        source = ROOT / relative
        # The entry-point READMEs use inline Markdown links without spaces in URLs.
        for link in re.findall(r"\]\(([^\s)]+)(?:\s+[^)]*)?\)", source.read_text(encoding="utf-8")):
            url = urlsplit(link)
            if url.scheme or url.netloc or not url.path:
                continue
            if not (source.parent / unquote(url.path)).exists():
                errors.append(f"{relative}: missing target {link!r}")
    return errors


def main():
    with (ROOT / "docs" / "book.toml").open("rb") as source:
        config = tomllib.load(source)
    site_root = config.get("output", {}).get("html", {}).get("site-url", "/")
    errors = check_book(BOOK.resolve(), site_root) + check_readmes()
    if errors:
        print("Documentation link errors:", file=sys.stderr)
        for error in errors:
            print(f"  {error}", file=sys.stderr)
        return 1
    print("Documentation links and HTML anchors checked (external URLs not fetched).")
    return 0


if __name__ == "__main__":
    sys.exit(main())
