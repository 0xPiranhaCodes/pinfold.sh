# ADR-0004: Agent identity comes from the PTY

- **Status:** Accepted
- **Date:** 2026-10-31

## Context

The architecture used bearer tokens. Review finding B1 showed they fail against the
agents they are meant to contain: a same-user process can read `/proc/<pid>/environ`
and any owner token on disk.

## Decision

Map each API caller from peer pid (`SO_PEERCRED` / `LOCAL_PEERPID`) to controlling tty
and session to pane. There are no tokens in environment variables and no owner token on
disk. Owner-level actions from outside a pane need a confirmation drawn in the window.

## Consequences

A pane's environment holds no secret to steal. Callers without a controlling terminal
need a defined treatment, which M0 settles along with pid-reuse races and sandbox PID
namespaces. See the [threat model](../design/threat-model.md).
