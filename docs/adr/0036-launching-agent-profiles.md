# ADR-0036: Agent profiles launch by name

- **Status:** Accepted
- **Date:** 2026-11-03

## Context

`[agents.<name>]` profiles in the config give a command, an argv list run with no shell, and
whether it runs sandboxed (`sandbox::command`, ADR-0020 point 5). Nothing started one (part of
issue #106). `pane.spawn` deliberately runs only the user's shell, so a caller of the API can
never name a program (threat model, Authorization). Launching an agent must keep that.

## Decision

A new method, `agent.launch { profile, place, of }`, opens a pane running the profile named
`profile` from the config the core read at start. The caller names a profile, never a program,
argv, environment or directory. An unknown name is refused with "no agent profile by that name
in the config", which does not echo the name into the audit log.

- `place` is `right` (the default), `down` or `tab`. `of` is the pane to split, the caller's
  own by default. Reach and hello rules are exactly `pane.spawn`'s: hello version 2, a caller in
  a pane, `of` in its reach, and the new pane joins the caller's `children` scope.
- The pane runs the profile's argv as it is. With `sandbox = true` it runs through
  `sandbox::command`; a missing or refusing tool refuses the launch, so a sandboxed profile
  never runs unsandboxed.
- The pane starts where `of`'s shell last said it was, by OSC 7, if that is a directory. Any
  program in that pane can print OSC 7, so a sandboxed launch with no such directory is
  refused rather than given one the core chose, and the sandbox validates it once its
  working-directory rule lands.
- It is audited like `pane.spawn`: method `agent.launch`, scope `children`, the new pane as
  target. The profile's name is not recorded.
- `pinfold agent <name> [--right | --down | --tab]` calls it from a pane.

There is no keybinding yet. A key needs a way to pick a profile in the window, which is a
prompt drawn by PinFold outside pane text; that is follow-up work, as is an MCP tool.

## Consequences

Agents start from a pane with one command, sandboxed when the user asked, and the API still
cannot run a program the user did not write into their config. A profile added to the config
needs a core restart, as the core takes no hot reload. A pane whose shell sends no OSC 7 starts
an unsandboxed agent in the core's directory, and cannot start a sandboxed one. Revisit if the
window gets a profile picker, or if callers need per-launch arguments, which would need their
own review against the threat model.
