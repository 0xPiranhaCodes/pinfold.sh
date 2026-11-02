# ADR-0029: History reflows at read time, and row numbers belong to a width

- **Status:** Accepted
- **Date:** 2026-11-02

## Context

History kept the width it was written at, while the grid reflows on resize. A window drag sends
many resizes to one reactor, and the core's budget is 4 MB (ADR-0021).

Rewriting blocks on a resize fails on more than speed. A store going from 120 columns to 2 holds
60 times the rows, so the 10,000-row cap would evict most of history, and growing back could not
restore it. Each new row also adds a style run, which spends the byte cap the same way.

The window assumes a frame's rows are numbered `top + line`, in display columns. Selection
overlays, vi motions and the wire format in #73 all rest on that, so numbers that named stored
rows would need a map from stored to shown rows in every frame.

`Flags::WRAPLINE` was already stored, in each run's flags, so the block format does not change.

## Decision

Stored rows never change. Reading lays them out at the pane's width, the same way
`grid/resize.rs` does. A row number names a shown row at the current width.

- **Layout:** each block keeps how many shown rows start in it and the layout as it starts, the
  column reached and whether a line is open. A read starts at one block with no look back,
  however long the line, so it costs the blocks holding the rows it wants.
- **A new width** costs a field write. The first thing to read history at that width lays out
  every block once, and a live view reads nothing, so dragging at the live screen stays O(1).
- **Numbers** count back from the same end. A new width first moves numbers up by what it could
  need, so the later layout changes nothing and rows on the screen keep their numbers.
  `view_rows` is an estimate until that read, because it runs on every frame.
- **`INFLATED_CAP`** holds a store to 64 MiB inflated, which bounds that one read. The byte cap
  does not, as rows of repeated links compress far past it. The test
  `what_a_new_width_inflates_is_capped` shows this cap evicting with no byte cap at all.

There is no cache. A block's queue entry grows from 16 to 40 bytes, counted in the byte cap as
before, which is 3,744 bytes at 156 full blocks.

### What each user of a row number does across a change of width

| User | Across a new width |
| --- | --- |
| `AlacrittyEngine::view` | Moves to the stored row it was on, read back at the new width |
| `rows_back`, `frame_history`, `scroll` | Count shown rows, so the wheel moves one row of the new width |
| `Position`, `Selection` | Name other text afterwards. The window already drops a selection when `columns` changes |
| Selection drag, vi visual | End with that selection, as `vi_key` sees it gone |
| Vi cursor | Keeps its number and `settle` clamps it, so it stays near the bottom but not on its text |
| `Search`, `Copying` in flight | Clamp to the new `held_rows`. A step can skip or repeat rows, and neither can panic |
| Search bar | Restarts from the newest row, as it has no selection left |
| `held_rows`, `Request::Reveal` | Same numbers. A first `gg` can stop short, and the next one reaches the top |
| `Frame.top`, `held`, `Position` in #73 | Unchanged on the wire. A new `columns` means the numbers restarted |
| `Scrollback::rows`, `read_rows`, `end`, `index`, `len` | Now shown rows. As stored until `set_width` is called |

Only `view` needed to survive, so only it has a stored anchor.

## Consequences

Measured on macOS by `cargo run --release -p pinfold-benches --bin scrollback -- --resizes
<corpus>`, at 120 columns on a 50-row screen, with 1024 columns for huge links. Linux is not
measured yet.

| Corpus | Rows held | Live step p50 ms | p99 ms | First read ms | View step p50 ms | p99 ms | Notch p50 ms | p99 ms |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| build-log | 9967 | 0.027 | 0.061 | 2.190 | 1.500 | 2.197 | 0.033 | 0.037 |
| plain-log | 9967 | 0.019 | 0.039 | 0.916 | 0.869 | 0.937 | 0.024 | 0.030 |
| git-diff | 9967 | 0.019 | 0.029 | 0.699 | 0.625 | 0.701 | 0.021 | 0.023 |
| dense | 9967 | 0.022 | 0.048 | 1.148 | 0.961 | 1.181 | 0.032 | 0.037 |
| random | 7535 | 0.022 | 0.062 | 0.783 | 0.659 | 0.884 | 0.034 | 0.045 |
| truecolor | 911 | 0.018 | 0.088 | 2.180 | 2.378 | 2.503 | 0.120 | 0.206 |
| cjk-links | 8687 | 0.018 | 0.032 | 14.181 | 13.814 | 14.187 | 0.179 | 0.233 |
| one line of 8,950 rows | 8950 | 0.024 | 0.048 | 0.839 | 0.773 | 0.871 | 0.042 | 0.047 |
| huge links | 470 | 0.229 | 0.443 | 16.401 | 16.576 | 18.045 | 1.713 | 2.317 |

A view step is a resize with the view in history, which pays the first read each time. A drag
over history therefore stalls the reactor by that read per step, up to 2.5 ms for typical text and
18 ms at the worst measured. Eight panes all scrolled back would pay it eight times. If that shows
up, keep row lengths beside each block, so that a layout need not inflate.

A line crossing from history into the grid stays cut at that seam, short of the width, because the
grid cannot take cells from the store. Copying and search still join it. A wrap flag on a row
followed by a blank row reads as one line unless that line fills the width, which is what brings
the blank row back at the width it was written.

History that a width pads differently, such as a background filled by a line feed, stays as
written. Tab stops are not reconsidered, so copied text can differ in the blanks after a tab.

Revisit this if the window ever needs positions that outlive a width.
