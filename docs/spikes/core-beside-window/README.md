# Why a core beside a window costs 1 MB more than an idle one (issue #408)

**Result: 0.92 MB of live heap for the one pane the window shows, and 0.15 MB more once that
pane has drawn twice. Two buffers an idle link kept at the size of the last frame made up
0.41 MB of that, and they are now freed. The budget of 4 MB (ADR-0021) stays.**

`cargo xtask mem-gate` reads "Core, 8 idle panes" at 2.86 MB and "Core beside that window, 8
idle tabs" at 3.92 MB on `ubuntu-latest`, against 4.00 MB for both. All numbers below that are
not from CI are from `ubuntu:24.04` on aarch64 in Docker on an Apple M1 Pro, on 2026-11-10,
under `xvfb-run -a -s "-screen 0 1920x1080x24"`, in MB of 10^6 bytes of PSS.

## What the extra memory is

A window of 1920x1080 gives its pane 53 rows of 236 columns, 12,508 cells. Only the pane the
active tab shows is resized and sent; the 7 hidden panes stay at 80x24 and cost the same as in
an idle core. Massif on a `release-core` build with debug info, read 40 s after the start:

```sh
CARGO_PROFILE_RELEASE_CORE_DEBUG=1 CARGO_PROFILE_RELEASE_CORE_STRIP=false cargo build --locked \
    --profile release-core --no-default-features --package pinfold-cli --bin pinfold-core
valgrind --tool=massif --time-unit=ms --detailed-freq=1 --threshold=0.5 \
    target/release-core/pinfold-core --window 8 1920x1080
```

It shows 18,638,331 bytes of heap beside a window and 17,713,011 idle. Of both,
16,777,216 are the 2 MiB that `vte` reserves per pane for synchronized updates and never
touches, so PSS does not count them. The 925,320 bytes between them are:

| Bytes | What | Where |
| --- | --- | --- |
| 508,224 | The shown pane's two grids, primary and alternate, at 24 bytes a cell: 2 × 300,192, less the 2 × 46,080 they held at 80x24 | `alacritty_terminal`'s `Grid::resize`, from `Reactor::lay_out` |
| 262,144 | The link's send buffer, kept at `KEPT_OUTPUT` after the first full frame | `Session::flush` |
| 150,096 | The last frame sent, 12 bytes a cell, which the next frame is a diff against | `Session::send_pane` |

A pane that draws a second frame adds 150,096 more: the spare frame, kept up to `KEPT_CELLS`.
`/bin/sh` draws its prompt once, before the window gives a size, so massif saw one frame. bash
draws it again at the new size, and CI's shell is bash. `sample.sh` shows that as 0.13 MB:
3.61 with `SHELL=/bin/sh` and 3.74 with `SHELL=/bin/bash`.

The mappings agree. `sample.sh` reads 1,636 kB of `[heap]` beside a window against 860 idle,
about 400 kB of other anonymous memory against 235, and the same 1,488 kB of files.

## What changed

`KEPT_OUTPUT` goes from 256 KiB to 32 KiB and `KEPT_CELLS` from 16,384 cells to 80x24. Both
still hold a full frame of an 80x24 pane, and a few changed rows of any pane, so typing in a
pane of any size allocates nothing. A full frame of a larger pane allocates its buffers and
frees them once the socket has taken it. `VtEngine::frame` now reserves the frame's cells in
one allocation. The two grids and the last frame sent are needed as they are.

`sample.sh 12`, one reading a run, lowest, median and highest:

| Shell | Before (`ac80508`) | After |
| --- | --- | --- |
| `/bin/sh` | 3.60, 3.61, 3.62 | 3.43, 3.45, 3.50 |
| `/bin/bash` | 3.73, 3.74, 3.78 | 3.41, 3.42, 3.45 |

`sample.sh 6 1 --idle 8` reads 2.63 before and after, as an idle core has no link.

The cost is in a pane larger than 80x24 that prints without pause, where each frame now
allocates. `flood.sh 4`, twice for each build in turn, gives the core's CPU time for 5,000,000
lines in a 1920x1080 window: 2,298 to 2,527 ms before, median 2,320, and 2,302 to 2,482 ms
after, median 2,388. That is 3% in the median and inside the spread.

## Whether #407 moved it

Not here: `sample.sh 12` reads 3.60, 3.61, 3.63 on `b2e1f6e`, the commit before #407's merge,
and 3.60, 3.61, 3.67 on `ac80508`, the merge. `sample.sh 6 1 --idle 8` reads 2.63 on both.

On CI the gate's row read 3.82 to 3.95 in the 47 runs before the merge, median 3.89, and 3.92,
3.92, 3.92 and 3.97 in the 4 runs that hold it since. The idle
row read 2.70 to 2.84 before, median 2.78, and 2.80 to 2.86 since. So #407 added about 0.03 MB
to both rows on x86-64, which the link does not explain as an idle core has none. The 4.00 and
4.05 that failed were runs of #406, which changes only `pinfold-config`, and their idle row
read 2.86 and 2.93 too.
