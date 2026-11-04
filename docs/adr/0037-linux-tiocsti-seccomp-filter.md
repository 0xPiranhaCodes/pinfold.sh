# ADR-0037: A seccomp filter denies TIOCSTI to Linux sandboxed agents

- **Status:** Proposed
- **Date:** 2026-11-03

## Context

ADR-0020 point 5 keeps a sandboxed agent in its pane's session, so bubblewrap's
`--new-session`, its own fix for TIOCSTI, is ruled out. On macOS the `sandbox-exec` profile
denies the ioctl. Linux 6.2 and later refuse TIOCSTI to unprivileged callers only when
`dev.tty.legacy_tiocsti` is `0`, and some kernels are built with it on: the kernel in the Linux
test container for #157 reports `1`. libseccomp would add a C library, which CLAUDE.md asks the
maintainer about first.

## Decision

`pinfold_sys::seccomp::filter` hand-assembles a 10-instruction classic BPF program from libc's
`sock_filter` and `BPF_*` constants, and bubblewrap installs it with `--seccomp` on an
inherited pipe.

- The program checks `AUDIT_ARCH` first and kills a call made under any other architecture's
  numbering, such as i386's `int 0x80`, because its syscall and ioctl numbers differ and would
  slip past the checks.
- On x86_64 every x32 call (numbers from `0x40000000`) fails with `EPERM` rather than being
  checked, for the same reason.
- `ioctl` with a request whose low 32 bits are `TIOCSTI` fails with `EPERM`. The kernel reads
  the request as 32 bits, so the high half is ignored.
- Everything else is allowed.
- Only x86_64 and little-endian aarch64 have numbers. On any other architecture `filter`
  fails, and the sandboxed pane does not start.

## Consequences

The agent keeps its session and caller identity, and cannot type into its terminal on any
supported Linux kernel. 32-bit and x32 programs cannot run inside a Linux sandbox. A new
architecture needs its `AUDIT_ARCH` value and a test run before it is supported. Revisit this
if Landlock or the kernel gives a per-process TIOCSTI control, or if the filter has to grow
beyond this one call, at which point libseccomp may be worth its C dependency.
