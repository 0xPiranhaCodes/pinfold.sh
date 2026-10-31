# ADR-0008: Memory targets separated from window pixels

- **Status:** Superseded by ADR-0021
- **Date:** 2026-10-31

## Context

Review finding B3 showed the original 15 MB figure could not hold: buffers are doubled,
HiDPI is normal on macOS, and the budget table's own sum exceeded it.

## Decision

Targets are:

- core ≤ 4 MB PSS
- window process ≤ 6 MB plus about 2 × 4 bytes × window pixels
- ≤ 250 KB per idle pane plus scrollback
- 10k lines of scrollback ≤ 1 MB

We measure PSS on Linux and `phys_footprint` on macOS. CI fails the build on any
regression over budget.

## Consequences

These are targets, not measurements, until M0 re-baselines them. The 250 KB pane budget
depends on compact hot rows of 8–12 bytes per cell.
