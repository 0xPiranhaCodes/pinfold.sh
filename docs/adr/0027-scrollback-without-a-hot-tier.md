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

## Consequences

`cargo run --release -p pinfold-benches --bin scrollback` measures all seven spike corpora
smaller than the spike did, and truecolor stays under the cap by keeping fewer rows.

Reading recent rows now needs a decode, where a hot row did not. #25 may need a small cache.
Links over 2 KB and combining marks past 16 per cell are not kept in scrollback.
