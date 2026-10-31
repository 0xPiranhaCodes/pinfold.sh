# ADR-0005: Renderer trait, CPU drawing by default on Linux

- **Status:** Accepted
- **Date:** 2026-10-31

## Context

GPU drivers can add a large memory cost, and foot shows that CPU drawing of damaged rows
is fast enough for a terminal. Review finding B3 showed that double buffering and HiDPI
make the macOS case uncertain.

## Decision

Rendering sits behind a `Renderer` trait. The Linux default is CPU drawing with
`softbuffer` and our own glyph blitter, damaged rows only. The macOS default (CPU or a
thin Metal path) is decided by the M0 measurements. An optional OpenGL 3.3 core renderer
is a build feature. There is no GUI toolkit; the whole UI is cells.

## Consequences

Pinfold owns its text drawing and chrome. The UI boards contain sub-cell details such as
rounded corners and 1 px rules, which M1 must reconcile with a cell-only renderer.
