# ADR-0025: Pure-Rust fonts with swash

- **Status:** Accepted
- **Date:** 2026-10-31

## Context

`docs/design/architecture.md` names `crossfont`, which uses FreeType on Linux and CoreText on
macOS. FreeType is a C library, and fonts are the part of the window budget nothing has measured
yet. The [font spike](../spikes/m1-font-stack/README.md) compared two pure-Rust rasterizers.

## Decision

Rasterize glyphs with `swash` on both platforms, from memory-mapped fonts. This replaces
`crossfont` in the architecture. Hinting is on. Font discovery stays free of C libraries.

## Consequences

- Fonts add about 0.2–2.3 MB in the spike, before a real cache. The design's estimate was 1 MB.
- One rasterizer means the same text on Linux and macOS, and no C parser for untrusted font files.
- Text will not match native CoreText or FreeType output. Nobody has compared them yet, so users
  may find it less familiar, especially on low-DPI Linux.
- Fontconfig is also a C library, so Pinfold cannot use it. Font lookup relies on config paths and
  known directories, and may miss what the desktop's font settings would pick.
- `mmap` of font files goes through `pinfold-sys`.
