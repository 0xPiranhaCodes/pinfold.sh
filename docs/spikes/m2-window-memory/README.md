# M2: why the macOS window reading scattered

**Result: the scatter of 48 to 63 MB had three causes. One is narrowed, one is the Metal driver's
timing, and one is unexplained. The budget of 52.86 MB is still missed, by 1.1 to 1.9 MB.**

`./sample.sh` prints what follows, one line per run, on a measuring window that draws the same
idle screen each time. All runs are on one Mac with a 5K display, on
2026-11-02, with other builds running beside them. Its unit for footprint is MiB, rounded, as
`footprint` prints it. `cargo xtask mem-gate` reports MB of 10^6 bytes.

## What moved

| Category | Readings | What it is | Status |
| --- | --- | --- | --- |
| `IOSurface` | 22, 34 or 45 MiB | 2, 3 or 4 drawables of 11.4 MB | Unexplained, not seen in the last 58 runs |
| `IOAccelerator (graphics)` | 1.8 to 6.6 MiB | Driver memory, including what freed per-frame buffers leave | Fixed: 4.2 MiB in 9 of 10 runs |
| Driver pool | 5.4 MiB counted or reclaimable | Metal's own, marked reclaimable on its own timing | Not ours to control |

Everything else stayed within about 1 MB, including `Malloc Small` at 12 MiB.

- **Extra drawables.** 9 of the first 24 runs held 3 or 4 drawables, all at 1512x1890, despite
  `maximumDrawableCount` of 2. The next 58 runs held 2, on builds with and without the changes
  tried here, so nothing in this PR is shown to affect them. An autorelease pool around the
  frame did not stop them: 3 of 12 runs with it still had extras. No resize differs: every logged
  run resized 4 times to the same size. A window that reads about 57 or 69 MiB is this.
- **The instance buffer.** Before: 16 runs read 48 to 56 MiB. After, reusing one buffer: 9 of 10
  read 52 and one read 46.
- **The driver pool.** The 46 is the same memory in the `Reclaimable` column: 2,384 KB of
  `IOAccelerator` and 3,072 KB of unmapped footprint. Peak footprint is the same in both, and a
  window that had drawn only 7 frames in 30 s moved between the two, so drawing does not cause it.
  Metal offers no call for a pool it owns, so the honest idle reading is the higher one.

## What fills the 12 MiB heap

`heap` with `MallocStackLogging=lite` shows 9.25 MB of the heap is 66 ColorSync tables of 140 KB,
built by AppKit for the title bar's text, its traffic lights and CoreAnimation's colours. Setting
the `NSWindow` colour space to sRGB drops that to 36 tables, but the footprint moved by about
1 MiB, because the allocator keeps those pages. It would also change how the window's colours
are matched, so it was not kept. The 12 MiB of heap is mostly not Pinfold's.

## The gate after this change

Three runs of `XDG_CONFIG_HOME=<empty dir> cargo xtask mem-gate`, limit 52.86 MB.

| Reading, MB | Run 1 | Run 2 | Run 3 |
| --- | --- | --- | --- |
| Window, 1512x1890 | 54.15 | 54.04 | 53.92 |
| Window with text | 54.69 | 54.58 | 54.72 |
| Core beside that window, limit 4.00 | 3.49 | 3.49 | 3.46 |

These are steady, but over the limit. The buffer narrows single runs, not this median: two
branches without it read 54.40 and 54.04 in one gate run each, and 54.05 and 54.49 with text.
[ADR-0030](../../adr/0030-macos-window-fixed-part.md)
proposes what to do about that.

## Not tested

- Non-Retina or Intel Macs, or any display but this 5K one.
- The cost of waiting for the GPU after each frame in `Gpu::draw`, in frame time.
- What brings the extra drawables back. If they return, a reading of about 65 MB is that, and
  the first step is `sample.sh` while it happens.
