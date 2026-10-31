# ADR-0023: Keep two processes despite the macOS miss

- **Status:** Accepted
- **Date:** 2026-10-31

## Context

M0 bet 1 set a limit of 2 MB over one process. Linux passed at +0.99 to +1.42 MB, and macOS missed
at +2.33 to +3.51 MB.

## Decision

Keep ADR-0003's two-process design.

## Consequences

Crash isolation, the small trusted core and live upgrade all stay. macOS pays a few MB beside a
window of about 48 MB. The likely cause of the miss, that `phys_footprint` does not divide shared
pages, is inferred and not tested.
