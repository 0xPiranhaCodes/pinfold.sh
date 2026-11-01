# ADR-0027: Scrollback without a separate hot tier

- **Status:** Accepted
- **Date:** 2026-11-01

## Context

The architecture review (B3) asks for compact hot rows of 8–12 bytes per cell, and the M0 spike
built them, with a shared style table beside compressed blocks. That table is a second place
for hostile output to spend memory: truecolor reached 14.66 MB of heap in the spike, against
ADR-0021's hard cap of 2 MB.

## Decision

Rows go straight into one open block of inline style runs, which is compressed at 64 rows or
64 KB. There is no hot tier and no style table, so the cap covers every byte the store holds.
Scrollback also keeps at most 10,000 rows, so a quiet pane stays well under the cap.

Compressed blocks live in one ring of 64 KB chunks, and the working buffers and the last emptied
chunk are reused, so a full store does not touch the allocator. The cap is a limit on real
footprint, so the store counts only part of the 2 MB: 1.6 MB on Linux and 1 MB on macOS. The
rest is for the heap around it. `cargo xtask mem-gate` holds a pane to the 2 MB.

Blocks allocated one by one failed that: truecolor held 1.97 MB and cost 3.15 MB of footprint on
macOS, because blocks of about 30 KB, freed oldest first, left holes. macOS needs the larger
share because identical allocations there cost up to 0.9 MB more from run to run.

## Consequences

`cargo run --release -p pinfold-benches --bin scrollback` measures the store with its working
buffers. On Linux, against the spike's heap, it is smaller for plain-log, dense, random,
truecolor and cjk-links, level for build-log, and larger for git-diff (0.36 MB against 0.32 MB).

The worst cases give up history to stay under the cap. Of 12,000 lines fed, Linux keeps 1,647
truecolor rows, and macOS keeps 911, with 7,535 of random and 8,687 of cjk-links. Typical text
keeps 9,967 on both.

Reading recent rows now needs a decode, where a hot row did not. #25 may need a small cache.
Links over 2 KB and combining marks past 16 per cell are not kept in scrollback.
