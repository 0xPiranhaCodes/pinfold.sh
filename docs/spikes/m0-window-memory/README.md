# M0 bet 2: window memory

**Result: the bet fails on macOS, so Metal becomes the default there. But Metal misses 12 MB too.**
A half-screen Retina window uses 65.23 MB with CPU drawing and 47.65 MB with Metal, against a
limit of 12 MB. A bare window with nothing drawn already uses 19.37 MB, so **no renderer can
reach that limit**. Linux fits its budget, narrowly on Wayland.

| MB, median of 3 runs | Drawn | Empty window | CPU | Metal | ADR-0008 budget |
| --- | --- | --- | --- | --- | --- |
| macOS, half | 1512x1890 | 19.37 | 65.23 | **47.65** | 28.86 |
| macOS, full | 3024x1890 | 19.27 | 111.05 | **73.11** | 51.72 |
| X11, half | 960x1080 | 4.21 | 8.37 | — | 14.29 |
| X11, full | 1920x1080 | 4.17 | 12.51 | — | 22.59 |
| Wayland, half | 960x1080 | 4.14 | 12.94 | — | 14.29 |
| Wayland, full | 1920x1080 | 5.22 | 21.83 | — | 22.59 |

Linux totals include compositor growth. macOS totals do not, because WindowServer memory cannot
be attributed to one app. Full tables are in [`results.md`](results.md), and `./run.sh`
reproduces them.

## How it works

One window at a fixed physical size draws 60 full-window frames, then idles for 3 s before
being measured. The modes are:

- `base`: a window with no surface
- `cpu`: `softbuffer`
- `cpu-relief`: `softbuffer`, then asking the allocator to return freed pages
- `metal`: a `CAMetalLayer` with two drawables and instanced cell quads, with no CPU pixel
  buffer (macOS only)

## Findings

- **Memory follows whole buffers.** Fitting the two macOS sizes, CPU costs about the empty window
  plus 4 frames of pixels, and Metal about 25 MB plus 2. On Linux, X11 adds about 1 frame and
  Wayland about 2. Two sizes cannot prove a line, so treat these as estimates.
- **macOS's fixed cost breaks ADR-0008.** Its 6 MB fixed part cannot hold when an empty AppKit
  window takes about 19 MB. This M1 Pro result may differ on other Macs.
- **Metal saves about 18 MB at half size and 38 MB at full size.** The saving grows with pixels.
- **Allocator relief did nothing.** `malloc_zone_pressure_relief` and `malloc_trim` changed
  nothing measurable. The spike calls them once, straight after the last frame, so this does not
  rule out later or repeated calls. Architecture technique 4 needs evidence before relying on it.
- **Wayland has little margin.** At full size it is within 0.76 MB of budget before any font is
  loaded, and fonts plus the glyph cache are budgeted at about 1 MB.

## Not tested

- WindowServer's share on macOS, so real macOS totals are higher than shown.
- Fonts, glyph cache, a real cell renderer, or damage-only drawing and tiles.
- Real Linux desktops. Xvfb and headless Weston do no GPU compositing, so GNOME and KDE may differ.
- Intel Macs and other Apple GPUs. Metal drawables may be counted differently there.
- Sustained use. Each case draws 60 frames, then idles.
