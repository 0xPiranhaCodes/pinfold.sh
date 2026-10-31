# ADR-0019: Spike code lives beside its report

- **Status:** Accepted
- **Date:** 2026-10-31

## Context

M0 needs committed, runnable code with its numbers, but that code is throwaway. Spikes
need `unsafe` and dependencies that the product workspace forbids or has not approved.

## Decision

Each spike is a crate under `docs/spikes/<name>/`, outside the workspace, with its report,
raw results and `run.sh`. Workspace lints and `deny.toml` do not apply to it. Code moves
into product crates only by being rewritten to their rules, with `unsafe` in `pinfold-sys`.

## Consequences

Spikes stay cheap and cannot weaken the product's gates. CI does not build them, so a spike
may stop compiling. Its results stay valid for the commit that produced them.
