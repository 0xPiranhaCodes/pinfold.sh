# ADR-0001: Rust stable, edition 2024, pinned MSRV

- **Status:** Accepted
- **Date:** 2026-10-31

## Context

Contributors and packagers need one known toolchain. Pinfold starts on 2026-10-31,
when current stable is Rust 1.99.1.

Edition is set per crate and does not constrain dependencies, so edition 2024 needs no
dependency to adopt it; dependencies only need an MSRV at or below ours. We checked
`alacritty_terminal` 0.26.0 (edition 2024, MSRV 1.85), `winit` 0.30.13 (2024, 1.86),
`ratatui` 0.30.2 (2024, 1.88), `notify-rust` 4.18.2 (2021, 1.89), `crossfont` 0.9.0
(2021, 1.77), `softbuffer` 0.4.8 (2021, 1.71) and `mio` 1.2.4 (2021, 1.71). The
`portable-pty` 0.9.0 and `mimalloc` 0.1.52 crates declare no MSRV and are edition 2018.

## Decision

Use edition 2024. The MSRV is 1.99.1, pinned in `rust-toolchain.toml` and in
`rust-version` in `Cargo.toml`. Bumping it is a minor-version change, noted in the
CHANGELOG.

## Consequences

Distributions with an older Rust cannot build Pinfold from source until they catch up.
The pinned toolchain makes CI and local builds reproducible.
