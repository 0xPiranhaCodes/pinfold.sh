# Eight-hour Linux soak (issue #256)

The first nightly soak (#254, 60 min on ubuntu-latest) saw the core's PSS rise about 0.03 MB a
period, from 4.28 to 4.55 MB, with no sign of levelling off. This records whether it does.

[`tests/window/soak.sh`](../tests/window/soak.sh) runs `cargo xtask soak` in the window image
under Xvfb, as uid 1000, as nightly does on a runner. A hosted runner stops a job at six hours, so
eight hours runs here. `HEAP=1` also attaches heaptrack to the core and prints its live heap,
lowest and highest, for each tenth of the run.

Both runs: commit `3de24ba710877ffd6ad1702e8f184682a8451bb5`, Docker (OrbStack, Linux 7.2.12,
aarch64) on an Apple M1 Pro, 2026-11-06. Since then `908331147fc5f6419ac012b6e5391a4378d60150`
made `pane.read` with `lines` copy only the rows it returns, so its transient allocations
differ; the weekly soak keeps checking.

## 480 minutes

`OUT=<directory> tests/window/soak.sh 480`. This run used the script's first version, which
differed only in setup: it ran in the window image as built before, had no `useradd` and set
`CI=1`, which the soak does not read.

| Period | Rounds | Reconnects | Round trip p50 ms | Round trip p99 ms | Core MB |
| --- | --- | --- | --- | --- | --- |
| 1 | 2870 | 574 | 7.89 | 28.55 | 4.31 |
| 2 | 2869 | 573 | 7.92 | 25.62 | 4.33 |
| 3 | 2869 | 574 | 7.52 | 22.04 | 4.53 |
| 4 | 2864 | 573 | 8.38 | 26.09 | 4.54 |
| 5 | 2863 | 573 | 8.69 | 26.95 | 4.60 |
| 6 | 2861 | 572 | 8.73 | 27.78 | 4.60 |
| 7 | 2861 | 572 | 8.68 | 27.68 | 4.62 |
| 8 | 2860 | 572 | 8.82 | 26.26 | 4.62 |
| 9 | 2860 | 572 | 8.82 | 26.09 | 4.64 |
| 10 | 2863 | 573 | 8.72 | 26.77 | 4.64 |

Growth after warm-up: 0.33 MB, limit 1.00 MB. Passed.

Growth levels off: 0.29 MB over the first half after warm-up (periods 1 to 5) and 0.04 MB over
the last half (periods 6 to 10). Round trips are slower than in an hour's run because each
period's echo pane lives 48 minutes, and `pane.read` reads its whole history.

## Where the growth goes

`HEAP=1 OUT=<directory> tests/window/soak.sh 40`, with the core built with line tables:

| Tenth | Live heap low MB | Live heap high MB |
| --- | --- | --- |
| 1 | 0.00 | 7.23 |
| 2 | 0.63 | 9.51 |
| 3 | 0.55 | 9.51 |
| 4 | 0.55 | 9.51 |
| 5 | 2.72 | 9.51 |
| 6 | 2.72 | 9.51 |
| 7 | 2.72 | 9.51 |
| 8 | 2.72 | 9.51 |
| 9 | 2.72 | 9.51 |
| 10 | 2.72 | 9.51 |

The core's live heap does not grow: it peaks at 9.51 MB in every tenth after the first, which
starts as heaptrack attaches. The PSS creep is the allocator keeping pages that those transient
peaks touched (two panes printing `seq 1 50000` each second, and `pane.read` replies), so it
stops below the peak. What was still allocated when the soak killed the core, 803 KB by
`heaptrack_print -f core.gz --print-leaks 1`, is bounded: the audit log's ring (426 KB), the open
panes' grids and the window link's buffers. No fix is needed.
