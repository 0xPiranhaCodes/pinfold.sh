# ADR-0017: Panic abort only in the core, through a separate profile

- **Status:** Accepted
- **Date:** 2026-10-31

## Context

Review finding B2 says to keep `panic = "abort"` only in the core. Cargo sets `panic` per
profile, not per crate or binary, so it cannot be scoped from `[profile.release]`. `opt-level`
is per crate, so "cold code" means whole crates (finding L2).

## Decision

`[profile.release]` unwinds and builds everything but the core binary. The
`release-core` profile inherits it and sets `panic = "abort"`, and it builds only the
`pinfold-core` binary. The `pinfold-api` and `pinfold-tools` crates use `opt-level = "s"`.

## Consequences

Release builds run Cargo twice. A panic in the core ends every PTY, so the core must not
panic on external input.
