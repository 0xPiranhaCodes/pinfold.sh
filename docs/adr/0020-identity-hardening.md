# ADR-0020: Caller-identity hardening from the M0 spike

- **Status:** Accepted
- **Date:** 2026-10-31

## Context

The [caller-identity spike](../spikes/m0-caller-identity/README.md) passed for the API,
and it found attacks that skip the API. On Linux, a same-user process could copy pane B's
master out of the core unless the core was non-dumpable. Same-user ptrace of pane
processes and writes to their terminals also worked. This ADR amends ADR-0004 and
ADR-0010. The maintainer delegated this decision after reviewing the spike's proposals.

## Decision

1. The core hardens itself at startup, before opening any PTY: `PR_SET_DUMPABLE(0)` on
   Linux and `PT_DENY_ATTACH` on macOS.
2. On Linux the core reads `SO_PEERPIDFD` at accept time, and refuses identity if the peer
   has gone. If the kernel lacks it, the core falls back to the pid lookup.
3. A caller with no controlling terminal is unidentified and gets no pane scopes. An MCP
   server must stay in its agent's session to act for that pane.
4. The threat model states that same-user ptrace and terminal writes are not stopped
   without a sandbox.
5. Sandboxed agents are launched inside their pane's session. A sandbox option that starts
   a new session is not used, and the core must instead block TIOCSTI by other means.

## Consequences

The core cannot be debugged with an ordinary attach, so debugging needs a build or startup
option that keeps it attachable. Point 2's fallback leaves older Linux kernels with the
pid-reuse race. Which kernels and whether macOS needs `LOCAL_PEERTOKEN` are still to
settle. Point 5 needs testing with real bubblewrap and `sandbox-exec`, because the spike
only used `unshare`. `PT_DENY_ATTACH` was proposed from the spike but not tested in it.
