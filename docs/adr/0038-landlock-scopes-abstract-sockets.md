# ADR-0038: Landlock scopes Linux sandboxed agents' abstract sockets

- **Status:** Proposed
- **Date:** 2026-11-04

## Context

A Linux sandboxed agent keeps the host's network namespace, as bubblewrap's `--unshare-net`
would also cut the network agents need. Abstract Unix sockets live in that namespace, not in
the file system, so no mount hides them: the agent could connect to X11's
`@/tmp/.X11-unix/X0` or a session bus on `unix:abstract=` (#161). Seccomp cannot read the
`sockaddr` that `connect` is given. Landlock ABI 6 (Linux 6.12) adds
`LANDLOCK_SCOPE_ABSTRACT_UNIX_SOCKET`: a process in a Landlock domain cannot connect to an
abstract socket bound outside it. The `landlock` crate would add a dependency, which #161 says
needs the maintainer's approval, for three system calls.

## Decision

`pinfold_sys::landlock` makes the raw `landlock_create_ruleset` and `landlock_restrict_self`
system calls through libc, with the constants and the 24-byte `landlock_ruleset_attr` copied
from `linux/landlock.h`.

- `abi()` asks the kernel for its Landlock ABI version. `ENOSYS` (no Landlock) and `EOPNOTSUPP`
  (turned off at boot) mean version 0. Any other failure is an error, and the launch is refused.
- At ABI 6 or later, `sandbox::command` makes a ruleset that handles no file system or network
  access and scopes abstract sockets only. In the child, before `bwrap` is executed, it sets
  `no_new_privs`, which Landlock needs, and restricts itself. bwrap and the agent then share one
  domain, so the agent can still use abstract sockets its own processes bind.
- If making or applying the ruleset fails at ABI 6 or later, the launch fails: the agent never
  runs unsandboxed.
- Below ABI 6 the launch goes ahead as before, and the core's log says once that abstract
  sockets stay reachable. `docs/configuration.md` documents the gap.
- The working directory check also refuses a directory that is an absolute `PATH` entry or
  holds one, as given or resolved, so a program the agent writes there never runs from the
  user's `PATH`. Relative entries name no one directory and are ignored.

## Consequences

On Linux 6.12 and later the agent reaches abstract sockets only of its own sandbox. Pathname
sockets, such as the API's, socket pairs and caller identity are unchanged. A setuid `bwrap`
runs without its setuid rights under `no_new_privs`, so it needs unprivileged user namespaces
like any other; bwrap sets `no_new_privs` for the agent anyway. The ruleset handles no file
system access, so bwrap can still mount. Revisit this to let the same ruleset limit writes
(#161's git work), to scope signals (`LANDLOCK_SCOPE_SIGNAL`, also ABI 6), or if older kernels
must be closed too, at which point `--unshare-net` with a network proxy is the alternative.
