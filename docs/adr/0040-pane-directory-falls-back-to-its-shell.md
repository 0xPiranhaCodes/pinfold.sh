# ADR-0040: A pane's directory falls back to its shell's own

- **Status:** Proposed
- **Date:** 2026-11-04

## Context

A pane's directory came only from OSC 7: `agent.launch` started an agent there
([ADR-0036](0036-launching-agent-profiles.md)) and a snapshot saved it for restore
([ADR-0034](0034-m5-sessions-plan.md)). The default macOS zsh sends no OSC 7 in PinFold, as
Apple's `/etc/zshrc` sends it only for `TERM_PROGRAM=Apple_Terminal`, and `shell-integration/`
ships no scripts. So by default an unsandboxed agent started in the core's directory, a
sandboxed one was refused (#197), and every restored pane opened in `$HOME` (#198).

Shipping shell integration and injecting it at spawn, through a `ZDOTDIR` shim for zsh and
`--rcfile` for bash, would fix only the shells it covers and change how every pane's shell
starts. The system already knows each process's working directory.

## Decision

A pane's directory is, in this order:

1. its last OSC 7 directory, which any program in the pane can print (T4);
2. else the working directory of the pane's shell, the session leader the pane started, read
   from the system: `proc_pidinfo(PROC_PIDVNODEPATHINFO)` on macOS, `/proc/<pid>/cwd` on Linux
   (`pinfold_sys::foreground::cwd`, used by `Reactor::cwd`);
3. else none. It is never the core's directory: `agent.launch` with none is refused.

The shell and not the foreground program: OSC 7 is the shell's report, so the shell's own
directory is the same fact read without output, and it is where a restored shell should start.
A foreground program can `chdir` anywhere and says nothing about where the user is working.

Each consumer keeps its checks: the agent's directory must exist and, sandboxed, pass
`sandbox::working_directory`; a restore uses a saved directory only if `snapshot::is_private`.

## Consequences

Agents and restored panes start where the user was with any shell, with no setup. A directory
read from the system is no less trusted than OSC 7, which output can fake, so the sandbox and
restore checks are unchanged. A pane that once received OSC 7 keeps that directory even if its
shell later moves without sending another, as before. `pane.list` and tab titles still show only
OSC 7. Revisit if shell integration ships, or if a pane's shell can be other than its session
leader.
