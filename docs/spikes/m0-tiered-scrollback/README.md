# M0 bet 3: tiered scrollback

**Result: partial pass.** Scrolling into cold history passes on every corpus, at worst
2.82 ms against the 5 ms limit, and every stored row is exact. The 1 MB memory limit passes
only for some output, so **it does not hold as an unconditional target**.

| Corpus | Tiered heap (MB) | Footprint Linux | Footprint macOS | ≤ 1 MB by footprint |
| --- | --- | --- | --- | --- |
| git-diff | 0.32 | 0.46 | 0.77 | Both |
| build-log | 0.43 | 0.60 | 0.97 | Both |
| plain-log | 0.59 | 0.78 | 1.10 | Linux only |
| dense | 0.68 | 0.89 | 1.20 | Linux only |
| cjk-links | 1.21 | 1.46 | 2.13 | Neither |
| random | 1.41 | 1.61 | 1.92 | Neither |
| truecolor | 14.66 | 19.92 | 28.26 | Neither |

A stock grid holds 29.34 MB for the same 10,000 lines, or 32.05 MB for `cjk-links`. Full
tables are in [`results.md`](results.md), and `./run.sh` reproduces them.

## How it works

[`grid-eviction.patch`](grid-eviction.patch) adds 11 lines to `alacritty_terminal` 0.26.0,
so rows leaving history are handed to their owner. The spike runs a stock `Term` with no
history and keeps everything in `src/store.rs`:

- **Hot:** the last 200 to 263 rows, 8 bytes per cell plus a shared style table, with
  trailing blanks trimmed.
- **Cold:** 64-row blocks of text and style runs, LZ4-compressed, each with its own styles.

**Exactness:** the spike feeds each corpus through both a stock `Term` and the tiered one,
then compares every history row and the screen cell by cell, including colours, flags, wide
characters, combining marks, underline colours and hyperlinks. All seven corpora had no
mismatches across 10,000 rows each.

## Findings

- **Text output fits, on Linux.** Ordinary logs and diffs use 0.46–0.89 MB of PSS.
- **macOS sits about 0.3–0.7 MB higher.** Heap is identical, so the gap is probably allocator
  overhead, though the spike did not confirm that. `mimalloc` might narrow it, but it is a C
  library and was not tested.
- **Incompressible text barely can.** `random` holds about 0.97 MB of entropy alone, which
  leaves almost nothing for hot rows. LZ4 reaches 1.41 MB, so a stronger codec could get
  closer, but not with much margin.
- **Truecolor needs a byte cap.** A different random colour per cell costs 14.66 MB of heap.
  A program can do this on purpose, so a line limit does not bound memory. The store needs a
  byte cap that drops the oldest blocks, which keeps the budget but keeps fewer lines.
- **`cjk-links` is over.** It has a hyperlink and an underline colour on every line, which is
  much denser than normal output, and CJK is 3 bytes per character.
- **Feeding gets about half as fast.** On Linux, parsing without rendering drops from 73–102
  MB/s to 41–51 MB/s on plain text, and from 134 to 23 MB/s for truecolor. The patch allocates a fresh row
  for each eviction, so a production fork can do better.

## The real cost is the fork

The patch is small only because the spike stops at storing and reading rows. Search,
selection, vi mode, display scrolling and resize reflow all expect history inside the grid:
outside `grid/`, about 62 non-test lines touch its history API. A production fork must
send them through the store, and `grid/resize.rs` must reflow rows that are compressed. **None
of that is prototyped here**, and this is the largest risk left in bet 3.

## Proposed targets

1. Keep ≤ 1 MB for 10k lines, stated as "typical text output on Linux".
2. Allow about 1.5 MB on macOS until an allocator is measured.
3. Add a hard per-pane byte cap on cold storage, because no line limit bounds hostile output.
4. Budget explicitly for fork work on search, selection, vi mode and reflow.

## Not tested

- Search, selection, vi mode, reflow and display scrolling through the store.
- Real captured output. All corpora are generated, and they may not match real sessions.
- x86_64, Intel macOS, or more than one measured run per platform.
- `mimalloc`, the spill file and encryption of cold blocks.
- Memory while scrolling. Sizes are read before scroll sampling, and the 2,000-sample latency
  uses no cache.
