# ADR-0026: Bundle JetBrains Mono

- **Status:** Accepted
- **Date:** 2026-10-31

## Context

The UI design names JetBrains Mono, and few machines have it. ADR-0025 rules out Fontconfig, so
finding a good system font is unreliable. The maintainer approved bundling it.

## Decision

The binary embeds JetBrains Mono Regular, unmodified, as the default font. A configured font can
replace it once #21 adds that.

## Consequences

Pinfold looks the same everywhere, works with no fonts installed, and tests need no system font.
The file adds 0.27 MB to the binary. Its bytes live in the binary's file-backed pages, so only
those read should count as memory. The font is under the SIL OFL 1.1, so releases must ship
`OFL.txt`. Bold, italic and fallback faces are not included, and CJK text needs a fallback font.
