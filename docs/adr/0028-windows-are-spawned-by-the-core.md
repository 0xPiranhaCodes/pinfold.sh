# ADR-0028: Only the core starts windows

- **Status:** Accepted
- **Date:** 2026-11-02

## Context

ADR-0003 splits the core from the window, and M2 builds that split. A window reads and writes
every pane, so the link to it carries more power than any API scope. A same-user process could
use a named socket for that link, and ADR-0010 assumes that a prompt-injected agent runs as you.
A peer UID check would let it past every scope. Refusing peers with a pane's terminal fails too:
a process that calls `setsid` has no terminal.

## Decision

A window's link is a Unix socketpair made by the core. The core keeps one end and hands the
other to a window process it starts itself. The link has no name on disk, so it cannot be dialled.

- `pinfold` is a launcher: it starts the core if none is running, asks it to open a window over
  the control socket, and exits.
- The control socket keeps the 0700 directory, 0600 socket and peer UID check. Until M3 it
  accepts only `hello` and `window.open`.
- `window.open` takes no arguments. The core starts the sibling binary it resolved at startup,
  and a request cannot name a path, arguments or environment. A session has one window, so a
  second request brings the first to the front.
- The link is binary, length-prefixed and versioned, with its first message a version check.
  A crash or a bad message from a window must never panic the core.
- The inherited descriptor reaches only the window. Pane children never get it, and the window
  sets it close-on-exec as it starts.
- The window takes the core's hardening too: `PR_SET_DUMPABLE(0)` on Linux and `PT_DENY_ATTACH`
  on macOS, because debugging it would reach the link.

## Consequences

An agent that reaches the control socket can open a window on your screen, and gets no channel
into a pane. That is the accepted cost.

Same-user controls stay guardrails, as the threat model says: only a sandbox contains an agent.
Attach over SSH would need its own decision, because no window can be spawned there.

The [spike](../spikes/m2-detached-window/README.md) shows macOS treating such a window as the
app. Revisit this if it does not come to the front for a person, or misbehaves across Spaces.
