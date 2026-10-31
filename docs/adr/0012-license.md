# ADR-0012: Dual MIT OR Apache-2.0

- **Status:** Accepted
- **Date:** 2026-10-31

## Context

Dual licensing is the Rust ecosystem norm. `alacritty_terminal`, `crossfont` and `winit`
are Apache-2.0 only, which is compatible as a dependency. Code forked from
`alacritty_terminal` (ADR-0006) keeps its license, so it cannot be offered under MIT.

## Decision

Pinfold is `MIT OR Apache-2.0`. The one exception is any crate holding code derived from
`alacritty_terminal`, which stays Apache-2.0 only and keeps its notices. All dependencies
must have compatible licenses, enforced by `cargo-deny`.

## Consequences

A binary containing the forked crate can be used only under terms that satisfy
Apache-2.0 for that code, even though the rest is dual-licensed.
