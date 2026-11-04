#!/usr/bin/env python3
"""Checks the website before GitHub Pages serves it.

Usage: check-site.py [DIR]  (default: site)

Fails if a link or asset in an HTML file does not resolve to a file inside DIR, if a
fragment names no id on its page, or if anything but an <a> or canonical link reaches another
origin, so a page makes no third-party request. Prints the site's total size. A missing DIR passes.
"""
import re
import sys
from html.parser import HTMLParser
from pathlib import Path
from urllib.parse import unquote, urlsplit

# Attributes that make the browser fetch or navigate to a URL.
URL_ATTRS = {"href", "src", "poster", "data", "action", "formaction"}
CSS_URL = re.compile(r"""url\(\s*['"]?([^'")]+)""")


class Page(HTMLParser):
    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.ids = set()
        self.urls = []  # (tag, url)
        self.in_style = False

    def handle_starttag(self, tag, attrs):
        self.in_style = tag == "style"
        # A canonical link names the page's address and fetches nothing, like <a>.
        if tag == "link" and "canonical" in (dict(attrs).get("rel") or "").lower().split():
            tag = "link rel=canonical"
        for name, value in attrs:
            if value is None:
                continue
            if name == "id" or (tag == "a" and name == "name"):
                self.ids.add(value)
            elif name in URL_ATTRS:
                self.urls.append((tag, value.strip()))
            elif name in ("srcset", "imagesrcset"):
                self.urls += [(tag, c.split()[0]) for c in value.split(",") if c.strip()]
            elif name == "style":
                self.urls += [(tag, u.strip()) for u in CSS_URL.findall(value)]

    def handle_endtag(self, tag):
        self.in_style = False

    def handle_data(self, data):
        if self.in_style:
            self.urls += [("style", u.strip()) for u in CSS_URL.findall(data)]


def check(root):
    root = root.resolve()
    pages, errors = {}, []
    for path in sorted(root.rglob("*.html")):
        page = Page()
        page.feed(path.read_text(encoding="utf-8"))
        pages[path] = page
    refs = [(p, tag, url) for p, page in pages.items() for tag, url in page.urls]
    for css in sorted(root.rglob("*.css")):
        refs += [(css, "css", u.strip()) for u in CSS_URL.findall(css.read_text(encoding="utf-8"))]

    for source, tag, url in refs:
        where = f"{source.relative_to(root)}: <{tag}> {url}"
        parts = urlsplit(url)
        if parts.scheme in ("mailto", "tel", "data"):
            continue
        if parts.scheme or parts.netloc:
            if parts.scheme not in ("http", "https", "") or tag not in ("a", "link rel=canonical"):
                errors.append(f"{where}: only <a> and a canonical <link> may point at another origin")
            continue
        if parts.path.startswith("/"):
            errors.append(f"{where}: root-relative paths break under /pinfold.sh/; use a relative path")
            continue
        target = (source.parent / unquote(parts.path)).resolve() if parts.path else source
        if not target.is_relative_to(root):
            errors.append(f"{where}: points outside the site")
            continue
        if target.is_dir():
            target = target / "index.html"
        if not target.is_file():
            errors.append(f"{where}: no such file")
        elif parts.fragment and target in pages and unquote(parts.fragment) not in pages[target].ids:
            errors.append(f"{where}: no id {parts.fragment!r} on that page")

    size = sum(f.stat().st_size for f in root.rglob("*") if f.is_file())
    print(f"{len(pages)} pages, {len(refs)} links and assets, {size} bytes ({size / 1024:.1f} KiB) in total")
    return errors


def main(args):
    root = Path(args[0] if args else "site")
    if not root.is_dir():
        print(f"{root}/ does not exist; nothing to check")
        return 0
    errors = check(root)
    for error in errors:
        print(f"::error::{error}")
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
