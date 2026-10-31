# ADR-0006: VtEngine trait over alacritty_terminal

- **Status:** Accepted
- **Date:** 2026-10-31

## Context

`alacritty_terminal` is mature and pure Rust, but its grid keeps history as cells and has
no hook for lines leaving the hot window (review finding B3).

## Decision

The VT engine sits behind a `VtEngine` trait, with `alacritty_terminal` as the first
implementation. Expect to fork its grid for tiered, compressed scrollback; M0 confirms
this. `libghostty-vt` stays a future option.

## Consequences

A fork brings upkeep and stays Apache-2.0 only (see ADR-0012). The trait does not hide
scrollback storage, so its design must follow what the M0 prototype learns.
