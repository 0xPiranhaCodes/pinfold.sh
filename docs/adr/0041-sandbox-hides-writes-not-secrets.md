# ADR-0041: For the beta the sandbox limits writes, not reads or the network

- **Status:** Proposed
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

For the macOS beta the sandbox stays as it is, and the docs say what it does not do. The threat
model, the README and `docs/beta/known-issues.md` state that a sandboxed agent can read
credential stores outside the hidden directories, see the variables it inherits, use the macOS
clipboard and reach the network.

For v0.1 we propose that the sandbox hide the common credential stores and the macOS clipboard,
with a test for each on both platforms that runs the real `sandbox-exec` and bubblewrap. The
choices to make then:

1. **Denylist or allowlist for reads.**
   - A denylist adds named stores to `private_dirs`: `~/.aws`, `~/.config/gh`, `~/.netrc`,
     `~/.kube`, `~/.docker`, `~/.gnupg`, `~/.config/gcloud`, `~/.azure`, and the browser
     profile directories. It is a small change to code that exists and keeps every tool
     working, but it misses each store not on the list, and the list needs keeping.
   - An allowlist lets the agent read only the system, its working directory, its toolchains
     and named dotfiles. It hides stores nobody listed, but every agent CLI and toolchain needs
     its own entries, the failures are confusing, and on Linux it replaces `--ro-bind / /` with
     many binds. We lean to the denylist for v0.1 and an allowlist as an opt-in later.
2. **Environment.** Strip variables whose names look like secrets (`*_TOKEN`, `*_KEY`,
   `*_SECRET`, `AWS_*`), or pass only an allowlist. The agent CLI needs its own API key, so the
   profile would name the variables it keeps.
3. **Clipboard.** On macOS deny `mach-lookup` of the pasteboard service. On Linux the clipboard is an X11 or
   Wayland socket, and the tests must show the agent cannot reach either.
4. **Network.**
   - Leave it open, as now: agents need their model's API, package registries and git hosts.
   - Cut it (`--unshare-net`, `(deny network*)`): safe, but no agent CLI that calls a remote
     model works, so the sandbox becomes useless for them.
   - Allow named hosts through a proxy the core runs: the only option that keeps agents working
     and stops exfiltration, but it adds a long-lived network component to the core, which has
     no async runtime (ADR-0007), and TLS hides the paths. We propose leaving the network open
     in v0.1, stated plainly, and deciding on a proxy in its own ADR.

## Consequences

The beta ships no sandbox change, so no new failure for testers, and nobody is told the sandbox
keeps secrets it does not. A sandboxed agent remains able to read and send credentials the
person keeps outside `~/.ssh`; a person who needs that kept from an agent must run it without
those files, or as another user, until v0.1. Hiding more paths later can break an agent that
used them, so each hidden store needs a known-issues entry when it lands. We would revisit the
network choice if a proxy that fits ADR-0007 is found, or if agent CLIs gain a mode that talks
only to their own API.
