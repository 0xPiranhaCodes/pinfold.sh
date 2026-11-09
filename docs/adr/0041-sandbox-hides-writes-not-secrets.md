# ADR-0041: The sandbox hides credential stores, secret variables and the clipboard, not the network

- **Status:** Accepted, by the maintainer's decision of 2026-11-09 (#328)
- **Date:** 2026-11-05

## Context

A sandboxed agent profile ([ADR-0020](0020-identity-hardening.md) point 5) limits what the
agent writes and which terminals it can reach. It does not limit most of what the agent reads,
or where it connects (#216):

- The macOS profile starts from `(allow default)` and denies only terminals by name, TIOCSTI,
  writes outside the working and temporary directories, git's control files, Unix sockets but
  the API's, and the hidden directories (`crates/pinfold-core/src/sandbox.rs`, `PROFILE`,
  `WRITES`, `GIT`, `SOCKETS` and `macos_args`).
- The Linux one starts from `--ro-bind / /`, with an empty directory over each hidden one, and
  has no `--unshare-net` (`linux_args`, and the test `linux_covers_existing_paths_and_keeps_the_session`).
- The hidden directories are `~/.ssh`, the config file's directory and PinFold's state
  directory (`pinfold_config::private_dirs`).
- The agent loses only `SSH_AUTH_SOCK`, `GPG_AGENT_INFO` and `DBUS_SESSION_BUS_ADDRESS` from its
  environment (`SOCKET_VARS`); every other variable, such as a token, reaches it.

So a sandboxed agent can read `~/.aws`, `~/.config/gh`, `~/.netrc`, `~/.kube`, `~/.docker`,
`~/.gnupg` and browser profiles, can read and write the macOS
clipboard, which `(allow default)` leaves reachable, and can connect anywhere over TCP or UDP.
An agent that a prompt injection steers can therefore send those credentials off the machine.
The threat model never promised otherwise, but "sandboxed" reads as "cannot read my
credentials".

## Decision

For the macOS beta the sandbox stays as it is, and the docs say what it does not do.

For v0.1 I decided on 2026-11-09 (#328):

1. **Reads: a denylist.** The sandbox hides the common credential stores, which
   `pinfold_config::credential_stores` lists and `private_dirs` adds to the hidden set:
   `~/.aws`, `~/.azure`, `~/.docker`, `~/.gnupg`, `~/.kube`, `~/.netrc`, `gcloud` and `gh` in
   `~/.config` and `$XDG_CONFIG_HOME`, and the browser profile directories of this system. On
   macOS `sandbox-exec` denies reading each; on Linux bubblewrap mounts an empty directory over
   each directory and `/dev/null` over each file. A store that does not exist breaks nothing.
   A store not on the list stays readable; an allowlist may come later as an opt-in.
2. **Environment: strip secret-looking variables.** Before exec the core removes every
   variable whose name looks like a secret, such as `*_TOKEN`, `*_KEY`, `*_SECRET` and `AWS_*`.
   The agent profile names the few variables it keeps, such as its own API key. The core never
   logs a variable's value.
3. **Clipboard: blocked.** On macOS the profile denies `mach-lookup` of the pasteboard service.
   On Linux the agent cannot reach the X11 or Wayland sockets, nor gets the variables that name
   them.
4. **Network: open**, and the docs say so plainly. Agents need their model's API, package
   registries and git hosts. A proxy that allows named hosts needs its own ADR.

Each control has a test that runs the real `sandbox-exec` and bubblewrap.

## Consequences

A sandboxed agent can no longer read the listed stores, see secret-looking variables the
profile does not keep, or use the clipboard directly. An agent CLI that read one of those stores, or
needs a stripped variable, breaks until the profile keeps the variable or the person runs it
unsandboxed; each hidden store has a known-issues entry. Stores not on the list, and anything
the agent can reach over the network, are not protected: an agent a prompt injection steers can
still send what it can read off the machine. We would revisit the network choice if a proxy
that fits ADR-0007 is found, or if agent CLIs gain a mode that talks only to their own API.
