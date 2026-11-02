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

`tests/hardening/audit.sh` has since tested point 1 on macOS 27.0.1 (arm64, SIP on, Developer
Mode off) and Linux 7.2 (arm64, `ptrace_scope` 0), and the threat model holds the results.

- On Linux `PR_SET_DUMPABLE(0)` does what point 1 wants: `ptrace` and `pidfd_getfd` were
  refused, where an unhardened core gave up 8 descriptors.
- On macOS `PT_DENY_ATTACH` stops `ptrace` only, and the attacker gets `SIGSEGV`. It does not
  stop `task_for_pid`: an ad-hoc signed process with the debugger entitlement read a hardened
  core's memory. Point 1 alone does not meet its aim on macOS. The hardened runtime refused
  that process, so `Pinfold.app` now has it. Builds from `cargo` do not.
- Neither state reaches a pane, on either system, and calling `harden` twice is harmless.
- Under a debugger a macOS core or window exits with status 45 at `harden`, not with a signal.
  The startup option this ADR calls for is `pinfold-core --idle <panes>`. It ran under a tracer,
  and a debugger with the entitlement attached to it, in a `cargo` build. A bundle's core cannot
  be attached to, as the hardened runtime refuses that too.
