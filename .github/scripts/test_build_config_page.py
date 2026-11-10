"""Tests for build-config-page.py. Run with unittest discovery in this directory."""

import importlib.util
import unittest
from pathlib import Path

SCRIPT = Path(__file__).with_name("build-config-page.py")
SPEC = importlib.util.spec_from_file_location("build_config_page", SCRIPT)
BUILD = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(BUILD)


class BuildConfigPage(unittest.TestCase):
    def render(self, markdown):
        renderer = BUILD.Renderer()
        return renderer.render(markdown)

    def test_table_rows_get_stable_key_ids(self):
        rendered = self.render("| Key | Default |\n| --- | --- |\n| `font_size` | `13` |\n")
        self.assertIn('id="font_size" data-config-key="font_size"', rendered)
        self.assertIn('<code>font_size</code>', rendered)

    def test_fenced_code_is_escaped(self):
        rendered = self.render("```toml\nvalue = \"<script>&\"\n```\n")
        self.assertIn("&lt;script&gt;&amp;", rendered)
        self.assertNotIn("<script>", rendered)
        self.assertIn('class="language-toml"', rendered)

    def test_text_links_and_attributes_are_escaped(self):
        rendered = self.render('# A <b> & "quote"\n\n[API](api/README.md?x=\"bad\") and <img>.\n')
        self.assertIn('id="a-b-quote"', rendered)
        self.assertIn("A &lt;b&gt; &amp; &quot;quote&quot;", rendered)
        self.assertIn("&lt;img&gt;", rendered)
        self.assertIn("https://github.com/0xPiranhaCodes/pinfold.sh/blob/main/docs/api/README.md?x=&quot;bad&quot;", rendered)

    def test_headings_keep_unique_stable_ids(self):
        rendered = self.render("## Themes\n\n## Themes\n")
        self.assertIn('id="themes"', rendered)
        self.assertIn('id="themes-2"', rendered)

    def test_lists_emphasis_and_in_page_links(self):
        rendered = self.render("- **Safe** and *local* [jump](#themes)\n")
        self.assertIn('<ul><li><strong>Safe</strong> and <em>local</em> <a href="#themes">jump</a></li></ul>', rendered)


if __name__ == "__main__":
    unittest.main()
