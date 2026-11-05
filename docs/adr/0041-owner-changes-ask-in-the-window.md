# ADR-0041: Owner changes ask in the window, owner reads do not

- **Status:** Proposed
- **Date:** 2026-11-05

## Context

The core takes a caller whose terminal and session match no pane for a person at a terminal
outside PinFold, and lets it call the owner methods. A pane's program can be such a caller by
starting a new PTY, for example `script -q /dev/null pinfold ctl session save` ([#217]). The
threat model asked for a window confirmation of owner actions, which #100 meant to give and did
not.

A sandboxed agent cannot open a new terminal: `openpty` fails under the macOS profile and under
bubblewrap's empty `/dev/pts` (`a_sandboxed_agent_cannot_open_a_new_terminal`). An unsandboxed
pane's program can, and it runs as the user.

## Decision

`session.save`, `workspace.open` and `grants.revoke` from a terminal outside every pane park on a
one-shot owner prompt, drawn in window chrome like the trust prompt: it names the method and
what it touches, `y` runs it once, `n` or Esc refuses, and it gives no lasting grant. With no
window it is refused. The parked call waits in `Grants`, so the reactor never blocks (ADR-0007).

`audit.read`, `grants.list` and `workspace.trust_state` keep the terminal check and are not
asked. They return metadata, never pane text or a way to run anything. `trust.json` and an
audit file are already readable by any unsandboxed process of the user's, and a sandboxed agent
cannot reach them through a new terminal. Asking would cost `pinfold up` a third prompt and make
`pinfold ctl log` and `pinfold ctl grants` need an open window.

## Consequences

`pinfold up` asks once for the open, and twice for a layout whose `run` lines are not yet
trusted. `pinfold ctl session save` and `pinfold ctl grants revoke` need an open window. An
unsandboxed pane's program can still read grants, the audit log and trust state through a new
terminal. Revisit if a read starts returning pane text, or if the core can tell a person's
terminal from a pane's descendant, for example by its process ancestry.

[#217]: https://github.com/0xPiranhaCodes/pinfold.sh/issues/217
