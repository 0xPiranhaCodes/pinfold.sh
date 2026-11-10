# ADR-0047: Keeping other processes' environment from a sandboxed agent on macOS

- **Status:** Accepted
- **Date:** 2026-11-09, accepted 2026-11-10 by the maintainer

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

## Options

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

## Decision

The maintainer's decision of 2026-11-10: a warning on macOS, which no option above listed,
plus option 1 as an opt-in that is off by default. Option 3 waits until after v0.1, as a possible
opt-in. Options 2 and 4 are not taken.

**The warning.** On macOS only, when the core launches a sandboxed agent (`agent.launch` of a
profile with `sandbox = true`, as `pinfold agent <name>` calls it), it checks the names in its
own environment and in the environment it gives panes with `secret_name`. If any match and
`strip_secrets` is off, the core tells the window once per core session, and the window shows a
one-time notice drawn by PinFold outside pane text, as ADR-0044's notice is:

> On macOS, sandboxed agents can read N secret-looking variables in your other panes: NAME1,
> NAME2, … (strip_secrets = true removes them)

N is the count of distinct names. The notice lists a capped number of names, then `…`. The core
logs the names at `warn`, and never a value: no value is logged, shown, sent over the link or
stored. Linux shows no warning, as it is not affected. The check sees only the environment the
core started with and gives panes. It cannot see a variable that a shell exports after it
starts, by hand or from its startup files, so no notice does not mean no secret in a pane.

**The opt-in.** `strip_secrets`, a top-level bool in `pinfold.toml`, default `false`. With it
on, the `pinfold` launcher starts the core, and the core starts every pane, sandboxed or not,
without the variables `secret_name` matches. `keep_env` still applies only inside a sandboxed
profile and cannot bring back a stripped variable. What `KERN_PROCARGS2` shows of the core is
the environment it was given at `exec`, which later changes in the process do not alter, so the
launcher must strip before it executes the core. The core removing variables from itself once
running would not hide them. A core that finds `strip_secrets` on but secret-looking names in
its own environment, such as one not started by the launcher, logs the names, never values. What
it breaks: a token exported before launching PinFold, such as `GITHUB_TOKEN` set in the shell
that runs `pinfold`, no longer reaches any pane. Load such tokens from the shell's startup
files instead (`~/.zshrc`, `~/.bashrc`), which a pane's shell reads itself. A token a shell
exports there is still in that shell's environment, which a sandboxed agent can read on macOS,
and neither the opt-in nor the warning sees it. The opt-in works on Linux too, though Linux
does not need it.

**Why not option 2.** It protects the core, but not the panes' shells, and those are where the
realistic leak is: a token exported in a shell, or inherited by every pane from the shell that
started PinFold. Its private channel to unsandboxed panes would also be new state to protect.

**Option 3 later.** Running sandboxed agents as another user is the only option that also covers
the user's processes outside PinFold. Its helper with root rights or user setup, file ownership
and changes to how the API knows the caller do not fit v0.1. It stays future work, as an opt-in
if people ask for it.

## Consequences

- The window gains a one-time notice, sent over the link like ADR-0044's, so the link version
  rises and ADR-0043 gains a row.
- `pinfold-config` gains `strip_secrets`. The launcher and the core's pane spawn filter the
  environment with `sandbox::secret_name`, in one place each.
- With the opt-in on, a person who relied on exporting tokens before launching PinFold must
  move them to shell startup files, where they are again readable by a sandboxed agent on macOS.
  The opt-in protects only the environment PinFold hands out.
- Processes outside PinFold, such as another terminal app's shells, stay readable on macOS.
  The threat model, `docs/beta/known-issues.md` and the README say so.
- We would revisit if macOS gains a sandbox operation that refuses `KERN_PROCARGS2` for other
  processes alone, which `tests/macos/sandbox-procargs.sh` would then show as `HIDDEN`, or if
  people ask for option 3.
