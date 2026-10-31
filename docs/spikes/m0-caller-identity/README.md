# M0 bet 4: caller identity

**Result: pass for the API, with one limit outside it.** On Linux and macOS, a process in
pane A holding pane B's full environment was always identified as pane A or as
unidentified, never as pane B. On Linux with `ptrace_scope` 0, a same-user process can
still ptrace pane B's processes. That is outside the API, and the core cannot stop it.

Raw output is in [`results.md`](results.md). `./run.sh` reproduces it, and three repeat
runs gave the same outcomes.

## How it works

The harness plays the core. It owns two real PTY panes and a Unix socket, and runs the
attacks inside pane A with pane B's environment. A caller counts as a pane only if it
holds that pane's controlling terminal **and** is in its session:

- **Linux:** `SO_PEERCRED`, then `/proc/<pid>/stat`.
- **macOS:** `LOCAL_PEERPID`, then `proc_pidinfo` and `getsid`.

## Findings

| Question | Linux | macOS |
| --- | --- | --- |
| Spoofed environment fools the core | No | No |
| Forked child keeps its pane identity | Yes | Yes |
| Adopt or force-steal pane B's terminal | Blocked | Blocked |
| TIOCSTI into pane B's input | Blocked | Blocked |
| Caller in a child PID namespace keeps its identity | Yes | Not applicable |
| Copy pane B's master out of the core | **Possible unless `PR_SET_DUMPABLE(0)`** | No `pidfd_getfd`; ptrace blocked |
| ptrace the core | **Possible unless `PR_SET_DUMPABLE(0)`** | Blocked |
| ptrace a process in pane B | **Possible at `ptrace_scope` 0** | Blocked |
| Write bytes to pane B's terminal | **Possible** | **Possible** |

The last three need explaining.

- **Stealing the master is the worst case:** it would give the caller pane B's input. The
  core has to set `PR_SET_DUMPABLE(0)` at startup. The first Linux run showed this attack
  as blocked, but only because Docker's seccomp filter hides `pidfd_getfd`. With seccomp
  off, it succeeded.
- **ptrace of pane B:** the attacker could make a process in pane B call the API, and the
  core would correctly see pane B. `PR_SET_DUMPABLE` does not help, because it resets on
  `exec`. Yama `ptrace_scope` 1 should stop it, but that was not tested. Only a sandbox
  stops it for certain.
- **Writing to pane B's terminal:** the bytes arrive at the core as pane B's program output,
  so they reach its VT parser. They can draw on the screen and emit OSC sequences. They do
  not reach pane B's input. Status from output must stay labelled "reported by output".

## Open questions answered

- **Caller with no controlling terminal:** it is unidentified, so it should get no pane
  scopes. An MCP server its agent starts in a new session falls into this case. It will
  need another way to prove its pane, or to stay in its agent's session.
- **Pid reuse:** a caller that exits before lookup was caught on both platforms. On Linux,
  `SO_PEERPIDFD` closes the race, and the spike saw it report the exit. macOS has no such
  check in this spike. `LOCAL_PEERTOKEN` is the likely answer, and it is untested.
- **PID namespaces:** a caller in a child PID namespace kept its identity.

## Security changes

These were accepted in [ADR-0020](../../adr/0020-identity-hardening.md).

1. The core sets `PR_SET_DUMPABLE(0)` at startup on Linux, and `pinfold-core` also tries
   `PT_DENY_ATTACH` on macOS.
2. Use `SO_PEERPIDFD` on Linux, and refuse identity if the peer has gone.
3. Treat callers with no controlling terminal as unidentified, with no pane scopes.
4. State in the threat model that same-user ptrace and terminal writes are not stopped
   without a sandbox.
5. Launch sandboxed agents so that they keep their pane's session. Any sandbox option that
   starts a new session, which is bubblewrap's usual TIOCSTI defence, loses their identity.

## Not tested

- Yama `ptrace_scope` 1 or higher, and real distro kernels. Linux ran on OrbStack's kernel.
- x86_64 Linux and Intel macOS.
- A forced pid-reuse collision, as opposed to a caller that exits.
- Real bubblewrap, Landlock or `sandbox-exec`. The PID-namespace case only uses `unshare`.
- macOS `ptrace` from a differently signed binary. Results apply to this spike's binary.
