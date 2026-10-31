# ADR-0011: One TOML file, no embedded scripting

- **Status:** Accepted
- **Date:** 2026-10-31

## Context

An embedded VM costs memory even when unused, and scripts in the core widen the trusted
code.

## Decision

Configuration is one hot-reloaded TOML file. There is no embedded scripting VM. Plugins
are external processes that use the API.

## Consequences

Plugins cost nothing until they run. Security-relevant changes wait for confirmation in
the window instead of applying on reload (ADR-0010).
