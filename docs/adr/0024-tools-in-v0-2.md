# ADR-0024: Built-in tools move to v0.2

- **Status:** Accepted
- **Date:** 2026-10-31

## Context

The review estimated 40–50 solo weeks to v0.1, or moving the built-in tools to v0.2 (finding H2).
M0 added work: a second renderer, the scrollback fork and identity hardening.

## Decision

v0.1 ships the terminal, multiplexer, agent API, sessions and packaging. The markdown reader, file
manager and diff viewer (M4) ship in v0.2. Milestone names stay the same, and the order becomes
M1, M2, M3, M5, then M4.

## Consequences

v0.1 ships without three features in the product pitch, so the docs must say they come later. Tool
fuzz targets and the controls for threat T6 also move. Threat T5, workspace trust, stays in M5.
