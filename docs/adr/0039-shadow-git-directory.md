# ADR-0039: Linux sandboxed agents commit into a shadow git directory

- **Status:** Proposed
- **Date:** 2026-11-04

## Context

On Linux, #160 binds a sandboxed agent's whole `.git` read-only, so the agent cannot commit
(#161). Git makes its locks (`index.lock`, `HEAD.lock`, ref locks) in `.git` itself, next to
the files that point git at other code: `config`, `config.worktree`, `commondir` and `gitdir`.
Landlock cannot split those. Its `MAKE_*` rights "only apply to the content of a directory",
and its rules key on existing file hierarchies, so allowing `.git/index.lock` to be created
also allows `.git/commondir`. A linked worktree's directory holds `HEAD`, `index` and
`commondir` together, so another layout of the same repository does not split them either. A
seccomp user-notify supervisor could check names, but it races on path arguments, has many
system calls and io_uring to cover, and would add a core thread (ADR-0007). A core-side commit
would run git outside the sandbox over a tree the agent wrote, where `core.hooksPath`,
`.gitattributes` filters and `core.fsmonitor` can name the agent's code.

## Decision

The agent's git uses a separate git directory, the shadow, and the user takes its commits with
a fetch.

- On Linux, if the working directory has a `.git` directory, `sandbox::command` sets
  `GIT_DIR` to the shadow and `GIT_WORK_TREE` to the working directory, and binds the shadow
  writable. The real `.git` stays read-only as before.
- The shadow is `$XDG_DATA_HOME/pinfold/agents/<profile>/<key>` (default
  `~/.local/share/pinfold`), where the key is the first 16 hex digits of the SHA-256 of the
  resolved working directory. It is not named `.git`, so plain git discovery never finds it.
  A profile name that is not one plain path component gets no shadow.
- At the first launch the core makes it mode 0700. It starts at the real `HEAD`, with the
  branch's commit from a loose or packed ref, or the detached commit, and with a copy of the
  real index. The core reads those only from files that really are inside the working
  directory's `.git`, as the open file's path in `/proc/self/fd` shows. An agent can make a
  `.git` where there was none, with links to hidden files, and the shadow must not show it
  their text. `objects/info/alternates` names the real objects, unresolved, so nothing is
  copied. The core
  never writes into an existing shadow, which the agent may have changed. It refuses the launch
  if the shadow is a symlink, not a directory, another user's, or open to group or others.
- The user fetches into remote-tracking refs, never into local branches:
  `git fetch --no-tags <shadow> '+refs/heads/*:refs/remotes/agent-<profile>/*'`. A local
  fetch runs `upload-pack` in the shadow, which runs none of its hooks and ignores repository
  config that names commands, such as `uploadpack.packObjectsHook`. The real-bwrap test plants
  a hook set, `core.hooksPath`, `core.fsmonitor`, a `filter` driver with an `info/attributes`
  rule and `uploadpack.packObjectsHook` in the shadow from inside the sandbox. It checks that
  none runs on that fetch or on `git status` in the real worktree. A negative control shows
  the same config does run under `git --git-dir=<shadow> status`.
- No API method is added. A core-assisted fetch is a later, separate decision. macOS is
  unchanged, as `sandbox-exec` already lets the agent commit there.

## Consequences

Agents commit with plain git on Linux, and no byte they write runs with the user's rights
unless the user runs git with the shadow as its git directory. The shadow is not hidden, so
other sandboxed agents can read, but not write, each other's shadows. Those hold the project
data the agents can already read from the projects themselves. A shadow starts at the commit of
its first launch and is kept after that: the user deletes it to start again from a newer
commit. The fetch reads a repository the agent controls, like any untrusted remote: git's handling
of its refs, objects and `alternates` is the exposure, and what it brings lands only in
remote-tracking refs for review, never in the agent's view. A working directory whose `.git` is a file, such as a linked worktree, gets no shadow,
and the agent there cannot commit. A real index in split-index form refers to a shared index
that the shadow lacks, and the agent must rebuild its index with `git reset`. Revisit this if
git gains per-file lock locations, or if a core-assisted fetch is approved.
