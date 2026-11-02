# ADR-0030: The macOS window's fixed part is 33 MB

- **Status:** Accepted
- **Date:** 2026-11-02

## Context

ADR-0021 budgets the macOS window at 30 MB + 2 × 4 B × window pixels, which is 52.86 MB at
1512x1890, and asks for a re-check once M2 moved the core out. That window now reads 54.15 MB, or
54.72 MB with text, and [the M2 spike](../spikes/m2-window-memory/README.md) explains the rest of
the scatter that hid this.

Two drawables take 22.9 MB as budgeted. Of the rest, about 9 MB is ColorSync tables that AppKit
builds for the title bar, and 5.4 MB is a pool that Metal owns and marks reclaimable on its own
timing. Removing either would mean no native title bar or no Metal. An sRGB window saved about
1 MiB, at a cost in how colours are matched, so it was not kept.

## Decision

Raise the fixed part from 30 MB to 33 MB, which is 55.86 MB at this size, and hold the line there.

Reaching the current limit by luck is possible: Metal's pool drops the reading to about 47 MB on
some runs. The gate should not pass on that timing, so the budget assumes the pool is counted.

## Consequences

The margin is about 1.05 MB with text, on one Mac. The budget now rests on measurements of a real
renderer, and `docs` that quote 30 MB change with it. A window holding 3 or 4 drawables, as 9 of
the first 24 spike runs did, still fails the gate, and should: it is 11.4 MB of real memory.

Revisit this if the extra drawables come back or a Mac without Retina reads differently.
