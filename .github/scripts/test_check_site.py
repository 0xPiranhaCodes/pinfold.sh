"""Tests for check-site.py. Run: python3 -m unittest discover -s .github/scripts"""
import importlib.util
import tempfile
import unittest
from contextlib import redirect_stdout
from io import StringIO
from pathlib import Path

spec = importlib.util.spec_from_file_location("check_site", Path(__file__).with_name("check-site.py"))
check_site = importlib.util.module_from_spec(spec)
spec.loader.exec_module(check_site)


def errors_for(files):
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp, "site")
        for name, text in files.items():
            (root / name).parent.mkdir(parents=True, exist_ok=True)
            (root / name).write_text(text, encoding="utf-8")
        with redirect_stdout(StringIO()):
            return check_site.check(root)


class CheckSite(unittest.TestCase):
    def test_a_resolving_site_passes(self):
        self.assertEqual(errors_for({
            "index.html": '<link rel="stylesheet" href="css/site.css"><img src="logo.svg" srcset="logo.svg 2x">'
                          '<a href="docs/">docs</a> <a href="docs/index.html#top">top</a> <a href="#me" id="me"></a>'
                          '<a href="https://github.com/0xPiranhaCodes/pinfold.sh">code</a> <a href="mailto:a@b.c">mail</a>'
                          '<link rel="canonical" href="https://pinfold.sh/">',
            "css/site.css": "@font-face { src: url('../font.woff2') }",
            "font.woff2": "",
            "logo.svg": "<svg/>",
            "docs/index.html": '<h1 id="top">Docs</h1><a href="../index.html">home</a>',
        }), [])

    def test_broken_links_and_assets_fail(self):
        errors = errors_for({
            "index.html": '<a href="gone.html">x</a><img src="nope.png"><a href="#missing">y</a>'
                          '<a href="../outside.html">z</a><a href="/root.html">r</a>'
                          '<div style="background: url(bg.png)"></div><style>p { background: url("p.png") }</style>',
            "site.css": "a { background: url(none.png) }",
        })
        self.assertEqual(len(errors), 8, errors)

    def test_only_the_404_page_may_use_root_relative_paths(self):
        errors = errors_for({
            "index.html": '<a href="/">home</a>',
            "404.html": '<a href="/">home</a><a href="/index.html">i</a><a href="/gone.html">g</a>',
            "docs/404.html": '<a href="/">home</a>',
        })
        self.assertEqual(len(errors), 3, errors)
        self.assertTrue(any("404.html: <a> /gone.html: no such file" in e for e in errors), errors)

    def test_only_links_reach_another_origin(self):
        errors = errors_for({"index.html": '<script src="https://cdn.example/x.js"></script>'
                                           '<link rel="stylesheet canonical-ish" href="//fonts.example/f.css">'
                                           '<img src="http://example.com/i.png"><iframe src="https://e.example"></iframe>'
                                           '<a href="javascript:alert(1)">j</a>'})
        self.assertEqual(len(errors), 5, errors)
        self.assertTrue(all("another origin" in e for e in errors))

    def test_a_missing_site_passes(self):
        with tempfile.TemporaryDirectory() as tmp, redirect_stdout(StringIO()):
            self.assertEqual(check_site.main([str(Path(tmp, "site"))]), 0)


if __name__ == "__main__":
    unittest.main()
