# ADR-0002: Linux and macOS for v1

- **Status:** Accepted
- **Date:** 2026-10-31

## Context

Pinfold's value depends on PTY, window-system and security behaviour that differs by
platform. Supporting everything at once would slow every milestone.

## Decision

v1 supports Linux (X11 and Wayland) and macOS (Apple Silicon and Intel). Windows is out
of v1, but platform code stays behind `cfg` and traits so it can be added later.

## Consequences

Each feature needs testing on both display servers and both Mac architectures. Unix
assumptions must stay inside `pinfold-sys` and the PTY layer.
