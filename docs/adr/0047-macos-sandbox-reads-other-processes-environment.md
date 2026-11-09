# ADR-0047: Keeping other processes' environment from a sandboxed agent on macOS

- **Status:** Proposed
- **Date:** 2026-11-09

## Context

[ADR-0041](0041-sandbox-hides-writes-not-secrets.md) strips variables whose names look like
secrets from a sandboxed agent's environment. On macOS that strips only the agent's own copy.
`sysctl(KERN_PROCARGS2)` returns the arguments and environment of any process of the same user
that is not a platform binary, and `sandbox-exec` with `(allow default)` does not stop it. So a
sandboxed agent can read the environment of `pinfold-core`, of every pane's shell and of what
runs in those panes, including a `GITHUB_TOKEN` exported in another pane.

`tests/macos/sandbox-procargs.sh` shows it. On macOS 27.0.1 (arm64) a probe reads a marker
variable from a program built on the machine under `(allow default)`, and still does with
`(deny process-info* (target others))`, `(deny sysctl-read (sysctl-name "kern.procargs2"))` or
`(deny sysctl*)`. Only `(deny process-info*)` with no target stops it, and then `python3` itself
cannot run, so no agent CLI would. Platform binaries such as `/bin/sleep` return their arguments
only, which hides the gap from a quick check.

Linux is not affected: bubblewrap gives the agent its own PID namespace (`--unshare-pid`) and
`/proc`, so it sees no process outside its sandbox.

## Decision

Proposed; none of these is built. The options:

1. **Launch panes' shells with a stripped environment.** The core starts every pane, and itself,
   without the variables [`secret_name`](../../crates/pinfold-core/src/sandbox.rs) matches, and a
   shell gets its secrets from its own startup files. That closes the gap for what PinFold starts,
   but breaks every workflow that exports a token before starting PinFold, and does nothing for
   the user's processes outside PinFold, such as another terminal app.
2. **Strip only the core's own environment.** The core re-executes itself, or is launched by the
   launcher, without secret-looking variables, and passes them only to unsandboxed panes through
   a channel the sandbox cannot read, such as a file in the hidden state directory. It protects
   the core, but panes' shells still hold the variables they were given.
3. **Run sandboxed agents as another user.** `KERN_PROCARGS2` refuses another user's processes.
   This is the only option that also covers processes outside PinFold, but it needs a helper
   with root rights or a user the person creates, file ownership in the working directory, and
   changes how the API knows the caller.
4. **A different macOS isolation**, such as a virtual machine through `Virtualization.framework`.
   Strong, but heavy in memory, against the budgets of ADR-0008, and far from what an agent CLI
   in a pane expects.
5. **State the gap and do nothing**, as v0.1 does: the docs tell the person not to export
   secrets in a shell whose user also runs sandboxed agents.

We lean to option 2 for the core plus the documented advice of option 5, and to option 3 as an
opt-in later if people ask for it.

## Consequences

Until a choice is built, on macOS ADR-0041's environment stripping keeps secrets out of the
agent's own environment only, and the threat model, `docs/beta/known-issues.md` and the README
say so. We would revisit if macOS gains a sandbox operation that refuses `KERN_PROCARGS2` for
other processes alone, which `tests/macos/sandbox-procargs.sh` would then show as `HIDDEN`.
