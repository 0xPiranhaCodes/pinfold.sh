# ADR-0018: Workspace mechanics

- **Status:** Accepted
- **Date:** 2026-10-31

## Context

The repository layout puts `tests/` and `benches/` at the root, but Cargo ignores those
folders in a virtual workspace. Fuzzing needs a nightly toolchain. A crate cannot relax
a lint it inherits as `forbid`.

## Decision

- `tests/` and `benches/` are workspace packages, `pinfold-tests` and `pinfold-benches`.
- `fuzz/` stays outside the workspace.
- `pinfold-sys` declares its own `[lints]`, mirroring the workspace table.
- Every package has `publish = false` until a release is planned, so nothing can reach
  crates.io by accident.

## Consequences

Workspace lint changes must be mirrored in `pinfold-sys`.
