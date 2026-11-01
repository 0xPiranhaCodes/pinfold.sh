# ADR-0027: Scrollback without a separate hot tier

- **Status:** Proposed
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

Compressed blocks live in one ring of 64 KB chunks, and the working buffers live as long as the
store. The cap is a limit on real footprint, so the store counts 1.6 MB of its 2 MB and leaves
0.4 MB for the heap around it. `cargo xtask mem-gate` holds a pane to the 2 MB.

Blocks allocated one by one failed that: truecolor held 1.97 MB and cost 3.15 MB of footprint on
macOS, because blocks of about 30 KB, freed oldest first, left holes.

## Consequences

`cargo run --release -p pinfold-benches --bin scrollback` measures the store with its working
buffers. Against the spike's heap, it is smaller for plain-log, dense, random and truecolor,
level for build-log, and larger for git-diff (0.35 MB against 0.32 MB) and cjk-links (1.43 MB
against 1.21 MB). Truecolor stays under the cap by keeping 1,739 rows, where typical
text keeps 9,967.

Reading recent rows now needs a decode, where a hot row did not. #25 may need a small cache.
Links over 2 KB and combining marks past 16 per cell are not kept in scrollback.
