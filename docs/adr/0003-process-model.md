# ADR-0003: Two processes: a tiny core and a window process

- **Status:** Accepted
- **Date:** 2026-10-31

## Context

The architecture proposed one process. Review finding B2 showed that any UI or tool
crash would then kill every agent, and that the trusted code would be the whole binary.
Finding H4 added that a long-lived core needs an upgrade path.

## Decision

`pinfold-core` is a tiny headless daemon that owns PTYs, VT state, the mux, sessions and
the API, with a target of ≤ 4 MB. `pinfold` is the window process, which hosts rendering
and the built-in tools and can exit or restart freely. The core is the only long-lived
process. A crash in the window or tools must never kill a PTY.

We design for live upgrade of the core through PTY fd handover over `SCM_RIGHTS`;
implementing it can wait.

## Consequences

The window process returns all of its memory exactly when it exits. The core and window
need a version check and an IPC boundary. The cost of the second process is measured in
M0 (pass: within 2 MB of one process).
