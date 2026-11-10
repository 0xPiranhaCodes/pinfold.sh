#!/usr/bin/env python3
"""Build site/configuration.html from docs/configuration.md using only Python."""

from __future__ import annotations

import argparse
import html
import re
from pathlib import Path, PurePosixPath
from urllib.parse import urlsplit

REPO = "https://github.com/0xPiranhaCodes/pinfold.sh"
HEADING = re.compile(r"^(#{1,6})\s+(.+?)\s*$")
LIST_ITEM = re.compile(r"^([-+*]|\d+\.)\s+(.+)$")
TABLE_RULE = re.compile(r"^:?-{3,}:?$")
RAW_CODE = re.compile(r"<code>(.*?)</code>", re.DOTALL)


def plain(text: str) -> str:
    """Return text without the supported inline Markdown markers."""
    text = RAW_CODE.sub(lambda match: html.unescape(match.group(1)), text)
    text = re.sub(r"`([^`]*)`", r"\1", text)
    text = re.sub(r"!?\[([^]]+)\]\([^)]+\)", r"\1", text)
    return text.replace("*", "").strip()


def slug(text: str) -> str:
    value = plain(text).lower().replace(".", "_").replace("<", "").replace(">", "")
    value = re.sub(r"[^a-z0-9_-]+", "-", value).strip("-")
    return value or "section"


class Renderer:
    def __init__(self, source_path: PurePosixPath = PurePosixPath("docs/configuration.md")):
        self.source_path = source_path
        self.used_ids: dict[str, int] = {}
        self.headings: list[tuple[int, str, str]] = []
        self.first_table = True

    def unique_id(self, candidate: str) -> str:
        count = self.used_ids.get(candidate, 0)
        self.used_ids[candidate] = count + 1
        return candidate if count == 0 else f"{candidate}-{count + 1}"

    def link(self, target: str) -> str:
        target = html.unescape(target.strip())
        parts = urlsplit(target)
        if target.startswith("#"):
            return target
        if parts.scheme in ("http", "https", "mailto"):
            return target
        if parts.scheme or parts.netloc or target.startswith("/"):
            return "#"
        path, marker, fragment = target.partition("#")
        repo_path = (self.source_path.parent / path).as_posix()
        while repo_path.startswith("../"):
            repo_path = repo_path[3:]
        url = f"{REPO}/blob/main/{repo_path}"
        return f"{url}{marker}{fragment}" if marker else url

    def inline(self, text: str) -> str:
        out: list[str] = []
        i = 0
        while i < len(text):
            if text.startswith("<code>", i):
                end = text.find("</code>", i + 6)
                if end >= 0:
                    out.append(f"<code>{html.escape(html.unescape(text[i + 6:end]))}</code>")
                    i = end + 7
                    continue
            if text[i] == "`":
                end = text.find("`", i + 1)
                if end >= 0:
                    out.append(f"<code>{html.escape(text[i + 1:end])}</code>")
                    i = end + 1
                    continue
            if text[i] == "[":
                label_end = text.find("](", i + 1)
                target_end = text.find(")", label_end + 2) if label_end >= 0 else -1
                if label_end >= 0 and target_end >= 0:
                    label = self.inline(text[i + 1:label_end])
                    target = html.escape(self.link(text[label_end + 2:target_end]), quote=True)
                    out.append(f'<a href="{target}">{label}</a>')
                    i = target_end + 1
                    continue
            marker = "**" if text.startswith("**", i) else text[i] if text[i] in "*_" else ""
            if marker:
                end = text.find(marker, i + len(marker))
                if end > i + len(marker):
                    tag = "strong" if marker == "**" else "em"
                    out.append(f"<{tag}>{self.inline(text[i + len(marker):end])}</{tag}>")
                    i = end + len(marker)
                    continue
            out.append(html.escape(text[i]))
            i += 1
        return "".join(out)

    @staticmethod
    def cells(line: str) -> list[str]:
        return [cell.strip() for cell in line.strip().strip("|").split("|")]

    def table(self, lines: list[str], start: int) -> tuple[str, int]:
        rows: list[list[str]] = []
        i = start
        while i < len(lines) and lines[i].lstrip().startswith("|"):
            rows.append(self.cells(lines[i]))
            i += 1
        if len(rows) < 2 or not all(TABLE_RULE.fullmatch(cell) for cell in rows[1]):
            return f"<p>{self.inline(lines[start])}</p>", start + 1
        is_keys = self.first_table
        self.first_table = False
        head = "".join(f"<th scope=\"col\">{self.inline(cell)}</th>" for cell in rows[0])
        body = []
        for row in rows[2:]:
            attrs = ""
            if row:
                row_id = self.unique_id(slug(row[0]))
                attrs = f' id="{html.escape(row_id, quote=True)}"'
                if is_keys:
                    key = plain(row[0])
                    attrs += f' data-config-key="{html.escape(key.lower(), quote=True)}"'
            body.append(f"<tr{attrs}>" + "".join(f"<td>{self.inline(cell)}</td>" for cell in row) + "</tr>")
        label = "Configuration keys" if is_keys else plain(rows[0][0])
        table_class = ' id="configuration-keys" class="config-keys"' if is_keys else ""
        return (
            f'<div class="table-wrap"><table{table_class}><caption class="sr-only">'
            f"{html.escape(label)}</caption><thead><tr>{head}</tr></thead><tbody>"
            + "".join(body)
            + "</tbody></table></div>",
            i,
        )

    def render(self, markdown: str) -> str:
        lines = markdown.splitlines()
        blocks: list[str] = []
        i = 0
        while i < len(lines):
            line = lines[i]
            if not line.strip():
                i += 1
                continue
            match = HEADING.match(line)
            if match:
                level, text = len(match.group(1)), match.group(2)
                heading_id = self.unique_id(slug(text))
                self.headings.append((level, plain(text), heading_id))
                blocks.append(f'<h{level} id="{heading_id}">{self.inline(text)}</h{level}>')
                i += 1
                continue
            if line.startswith("```"):
                language = line[3:].strip()
                code: list[str] = []
                i += 1
                while i < len(lines) and not lines[i].startswith("```"):
                    code.append(lines[i])
                    i += 1
                i += i < len(lines)
                lang = f' class="language-{html.escape(language, quote=True)}"' if language else ""
                blocks.append(
                    '<div class="code-block"><button type="button" class="code-copy" '
                    'aria-label="Copy code block" hidden data-js>copy</button>'
                    f"<pre><code{lang}>{html.escape(chr(10).join(code))}</code></pre></div>"
                )
                continue
            if line.lstrip().startswith("|"):
                block, i = self.table(lines, i)
                blocks.append(block)
                continue
            item = LIST_ITEM.match(line)
            if item:
                ordered = item.group(1)[0].isdigit()
                tag = "ol" if ordered else "ul"
                items: list[str] = []
                while i < len(lines):
                    found = LIST_ITEM.match(lines[i])
                    if not found or found.group(1)[0].isdigit() != ordered:
                        break
                    content = [found.group(2)]
                    i += 1
                    while i < len(lines) and lines[i].strip() and not LIST_ITEM.match(lines[i]):
                        content.append(lines[i].strip())
                        i += 1
                    items.append(f"<li>{self.inline(' '.join(content))}</li>")
                    while i < len(lines) and not lines[i].strip():
                        i += 1
                blocks.append(f"<{tag}>" + "".join(items) + f"</{tag}>")
                continue
            paragraph = [line.strip()]
            i += 1
            while i < len(lines) and lines[i].strip():
                if HEADING.match(lines[i]) or lines[i].startswith("```") or lines[i].lstrip().startswith("|") or LIST_ITEM.match(lines[i]):
                    break
                paragraph.append(lines[i].strip())
                i += 1
            blocks.append(f"<p>{self.inline(' '.join(paragraph))}</p>")
        return "\n".join(blocks)


def toc(headings: list[tuple[int, str, str]]) -> str:
    return "\n".join(
        f'<li class="toc-level-{level}"><a href="#{html.escape(heading_id, quote=True)}">{html.escape(text)}</a></li>'
        for level, text, heading_id in headings
        if level > 1
    )


def page(markdown: str) -> str:
    renderer = Renderer()
    article = renderer.render(markdown)
    return f"""<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="theme-color" content="#0B0D11">
<meta name="description" content="The complete PinFold configuration reference: every key, default, theme, shortcut and path.">
<link rel="canonical" href="https://0xpiranhacodes.github.io/pinfold.sh/configuration.html">
<link rel="icon" href="favicon.ico" sizes="any">
<link rel="stylesheet" href="style.css">
<title>Configuration — PinFold</title>
<script src="configuration.js" defer></script>
</head>
<body class="config-page">
<a class="skip" href="#configuration">Skip to configuration</a>
<div class="config-wrap">
  <header class="window config-window">
    <nav class="titlebar" aria-label="Site">
      <span class="lights" aria-hidden="true"><i></i><i></i><i></i></span>
      <ul class="tabs"><li><a href="index.html">~/pinfold.sh</a></li><li><a href="#configuration" aria-current="page">configuration.md</a></li></ul>
      <span class="title">PinFold · config</span>
    </nav>
    <div class="config-hero">
      <p class="eyebrow"><span class="dot"></span> full reference · generated from docs/configuration.md</p>
      <h1>Configure <em>PinFold</em>.</h1>
      <p class="lede">One TOML file, every key optional, applied on save. Themes are on <code>main</code> and arrive <strong>in the next beta</strong>.</p>
      <div class="config-path" aria-label="Configuration directories"><span>~/.config/pinfold/</span><b>pinfold.toml</b><span class="path-branch">└─ themes/</span></div>
    </div>
  </header>
  <div class="config-layout">
    <aside class="config-sidebar" aria-label="Configuration navigation">
      <div class="filter-box">
        <label for="key-filter">filter configuration keys</label>
        <input id="key-filter" type="search" placeholder="font, api, theme…" autocomplete="off" aria-controls="configuration-keys" disabled>
        <p id="filter-status" aria-live="polite">JavaScript optional; the full reference is shown.</p>
      </div>
      <nav aria-label="On this page"><p class="toc-label">on this page</p><ol>{toc(renderer.headings)}</ol></nav>
    </aside>
    <main id="configuration" class="config-content">{article}</main>
  </div>
  <footer><span>PinFold configuration · generated from the repository's single source of truth</span><span><a href="index.html">home</a> · <a href="{REPO}">source</a></span></footer>
</div>
</body>
</html>
"""


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, default=Path("docs/configuration.md"))
    parser.add_argument("--output", type=Path, default=Path("site/configuration.html"))
    args = parser.parse_args()
    args.output.write_text(page(args.source.read_text(encoding="utf-8")), encoding="utf-8")
    print(f"built {args.output} from {args.source}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
