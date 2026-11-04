# The PinFold website

A static page: HTML, CSS and a little JavaScript the page works without. There is no build
step and no dependency. Every URL is relative, so it works at a domain root and under the
project path on GitHub Pages. `CNAME` names the custom domain.

## Preview

```sh
python3 -m http.server -d site
```

Then open <http://localhost:8000/>.

## Rules

- Every claim and number comes from committed code or docs, with a link to where.
- No requests to anyone but this site: no CDNs, web fonts from elsewhere, analytics or cookies.
- Keep the page, without screenshots, under about 300 KB.

## Assets

- `img/session.webp` and `img/trust.webp` are captures of a real PinFold window, run from a
  temporary `HOME`, `XDG_RUNTIME_DIR`, `XDG_STATE_HOME` and `XDG_CONFIG_HOME`, scaled to
  1600 pixels wide with the window's rounded corners kept.
- `img/icon-32.png`, `img/apple-touch-icon.png`, `favicon.ico` and `img/og.png` come from
  `packaging/macos/AppIcon.icns` (`iconutil -c iconset`).
- `fonts/jetbrains-mono-subset.woff2` is the bundled
  `crates/pinfold-render/fonts/JetBrainsMono-Regular.ttf`, cut to the characters the page
  uses. It stays under the SIL Open Font License 1.1 in `fonts/OFL.txt`, which declares no
  reserved font name. Regenerate it with [fontTools](https://github.com/fonttools/fonttools):

```sh
pyftsubset crates/pinfold-render/fonts/JetBrainsMono-Regular.ttf \
  --unicodes="U+0020-007E,U+00A0-00FF,U+2010-2027,U+2030-203A,U+2190-2199,U+21E7,U+2212,U+2264-2265,U+2303,U+2318,U+2325,U+23CE,U+2500-259F,U+25A0-25FF,U+2713,U+2715,U+276F" \
  --layout-features='' --no-hinting --desubroutinize --flavor=woff2 \
  --output-file=site/fonts/jetbrains-mono-subset.woff2
```
