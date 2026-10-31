# M1 spike: font stack

**Result: `swash`.** It adds 0.18–2.27 MB across these runs, while `fontdue` adds 10–480 MB,
and quality looked about the same. See [ADR-0025](../../adr/0025-font-stack.md).

| Load + 415 glyphs at 14 px, MB | Font file | `fontdue` | `swash` |
| --- | --- | --- | --- |
| Menlo, macOS | 2.1 | 10.68 | 0.18 |
| STHeiti Light, macOS | 55.8 | 480.38 | 0.21 |
| DejaVu Sans Mono, Linux | 0.3 | 10.31 | 0.96 |
| Noto Sans CJK, Linux | 19.5 | 349.07 | 2.27 |

Full tables are in [`results.md`](results.md), and `./run.sh` reproduces them.

## Findings

- **`fontdue` parses every glyph at load.** That is why its cost follows the font's glyph
  count. A single CJK fallback font would break the window budget by itself.
- **`swash` reads a mapped font lazily.** Load cost is at most 0.14 MB. Its Linux numbers are
  higher because PSS counts the font file's touched pages, and `phys_footprint` does not.
- **Mask size is the same.** It is about 36 KB at 14 px and 130 KB at 28 px for these glyphs.
- **Speed does not separate them.** At 1–3 µs per glyph, 415 glyphs take about 1 ms, and each
  glyph is cached after first use.
- **Quality looked about the same.** I compared Menlo at 13 and 26 px by eye, and `swash` with
  hinting was slightly crisper at 13 px. That is a judgement, not a measurement.

## Not tested

- **`crossfont`, FreeType and CoreText.** FreeType is a C library, so it was left out, and there
  is no comparison with native text on either platform.
- JetBrains Mono, the font the UI design names. It was not installed here.
- Shaping and combining marks. The spike draws single characters.
- Low-DPI Linux quality, subpixel rendering and gamma. Only macOS output was viewed.
- A real glyph cache or font fallback.
