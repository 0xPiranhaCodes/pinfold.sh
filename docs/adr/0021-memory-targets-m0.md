# ADR-0021: Memory targets after M0

- **Status:** Accepted
- **Date:** 2026-10-31
- **Supersedes:** ADR-0008

## Context

The [M0 report](../spikes/m0-report.md) measured ADR-0008's targets. The core and per-pane
targets held. The window target cannot hold on macOS, where an empty window takes about 19 MB.
Wayland at full HD has 0.76 MB of margin before fonts. Scrollback fits for typical text on
Linux, but hostile output can use about 20 MB, so a line limit does not bound memory.

## Decision

| Metric | Target |
| --- | --- |
| Core, 8 idle panes, no scrollback | ≤ 4 MB |
| Each idle pane, no scrollback | ≤ 250 KB |
| 10k lines at 120 columns, typical text | ≤ 1 MB on Linux, ≤ 1.5 MB on macOS |
| Cold scrollback per pane | Hard cap of 2 MB by default; the oldest blocks are dropped |
| Window process on Linux | ≤ 8 MB + 2 × 4 B × window pixels |
| Window process on macOS | ≤ 30 MB + 2 × 4 B × window pixels, drawing with Metal |

We measure PSS on Linux and `phys_footprint` on macOS. CI fails the build on any regression
over budget.

The macOS fixed part was 26 MB until the Metal renderer measured 51.48 MB at 1512x1890, against
48.10 MB for M0's bare Metal path. The maintainer raised it to 30 MB. That window process still
holds the core, so re-check this budget after M2 moves the core out.

## Consequences

Pinfold's memory claim differs by platform, and the docs must say so. The 2 MB cap and the
window fixed parts come from measurements but have not been tested as limits. Fonts were not
measured, so M1 may need to revisit these. macOS numbers exclude WindowServer. Hostile output
can reduce a pane's history below 10k lines, by design.
