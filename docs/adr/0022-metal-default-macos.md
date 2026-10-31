# ADR-0022: Metal is the macOS default renderer

- **Status:** Accepted
- **Date:** 2026-10-31

## Context

ADR-0005 left the macOS default to M0, and the bet's rule made Metal the default if CPU drawing
missed 12 MB at half-screen Retina. CPU drawing used 65.23 MB and a thin Metal path 47.65 MB.
Neither can reach 12 MB, but Metal saves about 18 MB at half size and 38 MB at full size.

## Decision

macOS draws with Metal by default, through the `Renderer` trait. Linux keeps CPU drawing.

## Consequences

M1 builds and tests two renderers, and all Metal calls go through `pinfold-sys` or a safe binding
crate. The spike drew quads, not glyphs, so a real atlas adds memory that is not measured yet.
