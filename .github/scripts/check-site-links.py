#!/usr/bin/env python3
"""Every internal link of the built site resolves: its page or file exists, and its #anchor is an id on that page.

Reads the HTML under the build directory, so it covers the pages, the docs and the landing page alike. Links to other
sites are not checked.

Usage: check-site-links.py <build directory> <base path>, such as site/dist /runnerd.
"""
import sys
from html.parser import HTMLParser
from pathlib import Path
from urllib.parse import unquote, urldefrag, urljoin, urlsplit


class Page(HTMLParser):
    """The ids a page defines and the links it makes."""

    def __init__(self):
        super().__init__()
        self.ids = set()
        self.links = []

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if attrs.get("id"):
            self.ids.add(attrs["id"])
        if tag == "a" and attrs.get("name"):
            self.ids.add(attrs["name"])
        for name in ("href", "src"):
            if attrs.get(name):
                self.links.append(attrs[name])


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    root = Path(sys.argv[1]).resolve()
    base = sys.argv[2].rstrip("/")
    pages = {}
    for path in sorted(root.rglob("*.html")):
        page = Page()
        page.feed(path.read_text(encoding="utf-8"))
        pages[path] = page

    def target(path):
        """The file a path under the base serves, or None."""
        local = root / unquote(path).removeprefix(base).lstrip("/")
        if local.is_dir():
            local = local / "index.html"
        return local if local.is_file() else None

    problems = []
    checked = 0
    for path, page in pages.items():
        here = f"{base}/{path.relative_to(root).as_posix()}".removesuffix("index.html")
        for link in page.links:
            url = urlsplit(urljoin(f"https://site{here}", link))
            if url.netloc != "site":
                continue
            checked += 1
            file = target(url.path)
            where = f"{path.relative_to(root)}: {link}"
            if not url.path.startswith(f"{base}/") and url.path != base:
                problems.append(f"{where}: outside the site's base path {base}")
            elif file is None:
                problems.append(f"{where}: no such page or file")
            elif url.fragment and file.suffix == ".html":
                ids = pages[file].ids if file in pages else set()
                if unquote(url.fragment) not in ids:
                    problems.append(f"{where}: no #{url.fragment} on {file.relative_to(root)}")
    for problem in problems:
        print(problem, file=sys.stderr)
    if problems:
        sys.exit(f"{len(problems)} broken internal links")
    print(f"{checked} internal links in {len(pages)} pages resolve")


main()
