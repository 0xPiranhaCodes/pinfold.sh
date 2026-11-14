# ADR-0050: Rewind an agent's changes from checkpoints in a PinFold-owned git store

- **Status:** Proposed
- **Date:** 2026-11-14

## Context

An agent PinFold launches (`pinfold agent <profile>`, `agent.launch`,
[ADR-0036](0036-launching-agent-profiles.md)) edits, adds and deletes files in its working
directory, through its own tools and through every command it runs. When a run goes wrong, the
person has to find and undo those changes by hand. Git helps only partly: the changes mix with
the person's own uncommitted work, untracked files the agent added or deleted are not in any
commit, an unsandboxed agent can commit, reset or check out over what was there, and a directory
outside a repository has nothing. Some agent CLIs keep their own undo, but only of edits made
through their own file tools, not of what their shell commands change, and each in its own
format. PinFold launches the agent, so it is the one place that can record the directory before
the agent touches it, for every profile alike, sandboxed or not.

The hard part is doing that without running the agent's code. The core, the window and the tools
run outside the sandbox, as the person. [ADR-0039](0039-shadow-git-directory.md) found that git
run over a tree the agent wrote can run code the agent named: hooks, `core.hooksPath`, a
`.gitattributes` filter whose driver the configuration names, and `core.fsmonitor`. The tools'
git entry point (`crates/pinfold-tools/src/git.rs`, `run` and `OVERRIDES`,
[ADR-0045](0045-tools-are-programs-in-their-own-pane.md) section 4) turns each of these off for
`diff` and `status`, and `nothing_a_hostile_repository_configures_runs` proves it, but it still
reads the repository's own configuration and the person's, and lets git walk the work tree. A
restore is worse: it writes as the person into a directory where the agent may have planted
symbolic links, for example `src` replaced by a link to `~/.ssh`, or a hard link to
`~/.bashrc`.

Other constraints: the core is the only long-lived process and must never panic on external input;
it has no async runtime and a fixed set of threads ([ADR-0007](0007-concurrency.md),
[ADR-0048](0048-pty-opens-on-a-spawn-thread-with-a-deadline.md)); its budget is 4 MB, 250 KB a pane
([ADR-0021](0021-memory-targets-m0.md)); all UI is cells ([ADR-0005](0005-renderer.md),
[ADR-0011](0011-configuration.md)); a decision the person makes is asked in window chrome outside
pane text, so output cannot fake or answer it ([ADR-0042](0042-owner-changes-ask-in-the-window.md));
and the sandbox hides PinFold's state directory from the agent
([ADR-0041](0041-sandbox-hides-writes-not-secrets.md), `pinfold_config::private_dirs`). No
measurement of checkpoint time or disk use exists yet; this ADR names the spike that makes one
before slice 1 relies on a guess.

## Options

| Option | For | Against |
| --- | --- | --- |
| **A. A git commit in a PinFold-owned bare store, written by `git fast-import` from PinFold's own walk (recommended)** | Git only ever reads a byte stream on standard input: it never sees the work tree, its `.git`, its `.gitattributes` or the person's configuration, so there is nothing to override. Objects are content-addressed, compressed and shared between checkpoints, so a second checkpoint costs only what changed. The diff viewer already parses `git diff`, and `git diff <then> <now>` between two commits in the store also never touches the work tree. No new crate. | Needs `git` on `PATH`, as the diff viewer already does. A first checkpoint reads every file it covers. Packs and pruning are git's, run in the store. |
| B. A plain copy of the tree into PinFold's state directory, cloned where the file system can (`clonefile` on APFS, `FICLONE` on btrfs and XFS) | Runs no git at all; a clone is near free in time and disk on APFS. | ext4, the common Linux file system, cannot clone, so each checkpoint costs the tree's full size again. No sharing between checkpoints. The diff needs `git diff --no-index` over two directories, or a new diff source. |
| C. PinFold's own content-addressed store (files named by SHA-256, a manifest per checkpoint) | No git; shares content between checkpoints. | A new store, manifest and pruner to write and fuzz; no compression without a new crate; the diff viewer needs a non-git input. |
| D. `git stash create` or a commit in the person's own repository | Least code. | Runs git, with the repository's hooks and configuration, over the tree the agent wrote: the exact exposure of ADR-0039. Only works inside a repository, and puts objects in it. |

A file-system snapshot (APFS, btrfs, ZFS) needs root or one file system, and is not considered.

## Decision

Option A. Each part below is what slice 1 onwards builds, once the spike of the implementation
plan has measured it.

### 1. What a checkpoint is and where it lives

- A **run** is one `agent.launch`. Its id is `r<core start, Unix seconds>-<n>`. A **checkpoint**
  is a commit at `refs/pinfold/<run>/<n>` in the **store** of the run's working directory, with
  `n` counting from 0, the launch checkpoint.
- The store is a bare git repository at `<state>/checkpoints/<key>`, where `<state>` is
  `pinfold_config::state_dir()` (`~/Library/Application Support/PinFold` on macOS,
  `$XDG_STATE_HOME/pinfold`, default `~/.local/state/pinfold`, on Linux) and `<key>` is the
  first 16 hex digits of the SHA-256 of the resolved working directory, as ADR-0039's shadow.
  Made mode 0700; refused, never repaired, if it is a symlink, not a directory, another user's or
  open to group or others. The state directory is already hidden from sandboxed agents, so they
  can neither read nor change their checkpoints.
- The commit's tree holds each covered file as a blob, mode 100644 or 100755, and each symbolic
  link as a 120000 blob of its target text, never followed. Its message is one JSON object: the
  run, `n`, the reason (`launch`, `done`, `exit`, `api`, `view` or `restore`), the time, counts of files
  and new bytes, whether it is complete, and the paths it did not cover (below), at most 1,000,
  with a flag if there were more.
- Beside it, `stat/<run>-<n>` holds, sorted by path, each covered file's path, size, modification
  time, inode, change time and blob id. The next checkpoint merges its walk with this list in
  order, streaming, and gives git the old blob id (`M 100644 <id> <path>`) for a file whose stat
  is unchanged, so it reads only changed files. Memory stays flat in the tree's size.
- One writer at a time per store, by a lock file made with `O_EXCL`.

### 2. Who takes it, and how git is run

`pinfold checkpoint take|plan|restore|prune`, hidden subcommands of the existing `pinfold` binary
with their code in `pinfold-tools`, do all the work. The core starts its sibling binary as it does
for `tool.open`, never through `PATH`, with fixed arguments and its own values after `--`, waits
on a thread of ADR-0007's bounded pool (at most two of these processes at once, one per store),
and reads one JSON line of at most 4 KiB from it. The walk, the tree and git's output are never
parsed in the core, and a crash costs only that process.

The walk is PinFold's own, through new `pinfold-sys` functions that open each component relative
to a directory descriptor with `O_NOFOLLOW` (`openat`, `fstatat` with `AT_SYMLINK_NOFOLLOW`,
`readlinkat`), starting from the working directory opened once with `O_DIRECTORY | O_NOFOLLOW`.
Each file is read through its open descriptor and checked with `fstat` before and after; one that
changed while read is read once more, then left uncovered.

Git is reached through a new entry point beside `git::run`, `git::store(store, args, stdin)`,
that keeps `run`'s environment scrubbing, absolute-`PATH` lookup, timeout, output cap and
`OVERRIDES`, and adds `GIT_DIR=<store>`, `GIT_CONFIG_GLOBAL=/dev/null` and
`GIT_CONFIG_NOSYSTEM=1`, with no work tree. The store's own `config` is written by PinFold at
creation and names no command. Only these subcommands run there: `fast-import --quiet` (with
`feature done`), `diff-tree`, `diff` between two commits (with `--no-ext-diff --no-textconv
--no-color`), `cat-file --batch`, `for-each-ref`, `update-ref -d`, `repack -a -d -q` and
`prune`. None of them reads the working directory.

### 3. What a checkpoint skips

Each skipped path is listed in the commit message as not covered. A restore never writes or
deletes a path that the checkpoint it restores did not cover.

| Skipped | Why |
| --- | --- |
| Every entry named `.git`, ignoring ASCII case, and on macOS any name that git's `is_hfs_dotgit` would take for it | Git's own data; restoring it is how a repository gets hooks. Commits the agent made are not undone; the docs say so. |
| A directory holding a `.git` below the working directory, a nested repository or submodule | Its files belong to another repository's history (open question 6). |
| Paths ignored by the `.gitignore` files in the tree, with `rewind.include_ignored = false` | Build output and dependencies, often most of a tree's size, and secrets such as `.env`. Matched by PinFold's walker from the tree's `.gitignore` files, read as data with the walk's caps; no git runs over the tree for it. |
| A regular file over `rewind.max_file_mib` (16 MiB) | Large binaries. |
| A regular file with more than one hard link | It may be a link to a file outside the directory, such as a hidden credential, whose text a checkpoint would copy and a restore write back. |
| Fifos, sockets and devices | Not files. |
| Anything outside the working directory | Not watched. The docs say an unsandboxed agent's writes elsewhere, such as to `~/.bashrc`, cannot be rewound. |
| Everything past `rewind.max_files` (100,000) entries or `rewind.max_new_mib` (256 MiB) of new content | Bounds time and disk. The checkpoint is marked incomplete; a restore from it deletes nothing. |

### 4. When checkpoints are taken

- **Launch.** `agent.launch` takes checkpoint 0 after its directory checks and before the agent's
  PTY opens, and defers its reply until then. If it is not done within `rewind.wait_s` (10 s) the
  process is killed, the agent starts without one, and the window flashes `✕ error · no
  checkpoint: took over 10 s` (open question 1). `pinfold agent` prints "taking a checkpoint" while
  it waits.
- **Done and exit.** When the run's status becomes ✓ done, at most once in 60 s, and when its
  program exits. These mark where the agent stopped, for section 6.
- **On demand.** `checkpoint.take` (section 7), at most once in 30 s per run.
- **Before a restore**, always, so a rewind can itself be rewound.

### 5. Keeping and pruning

A run keeps at most `rewind.keep_per_run` (20) checkpoints: past it, the oldest after the launch
checkpoint goes. A checkpoint older than `rewind.keep_days` (14) goes. When all stores together
exceed `rewind.max_store_mib` (2048), closed runs go oldest first, then a live run's checkpoints
other than its launch one and its newest. A live run's launch checkpoint is never pruned: if the
cap cannot be met without it, new checkpoints are refused, and the reason is shown. So an agent
cannot push its own launch checkpoint out by taking checkpoints. `pinfold checkpoint prune` runs
after each take: it deletes refs and stat files, then `repack -a -d -q` and `prune` in the store.

### 6. Seeing and undoing

- `pinfold rewind` in a pane, or "Rewind agent changes" in the palette on an agent pane, opens the
  diff viewer through `tool.open { tool: "diff", checkpoint }`, by default on the run's launch
  checkpoint. The core first takes a `view` checkpoint `N` of the tree as it is now, then starts
  `pinfold diff --checkpoint <store> <C> <N>`. The viewer runs `git diff <C> <N>` through
  `git::store`, so its parser, caps, escaping and snapshots are the existing ones. Its stage,
  revert and send-hunk keys are off in this mode. A view checkpoint is pruned when the viewer
  exits, unless a restore used it.
- A file whose content in `N` differs from the run's newest `done` or `exit` checkpoint was changed
  after the agent stopped, probably by the person. The file list marks it with the word `later`
  in yellow. It is not an agent state, so it gets none of ◆ ● ✓ ✕ ○.
- `u` undoes the file at the cursor and `U` undoes every file, except those marked `later`, which
  only `u` undoes one at a time. Each calls `checkpoint.restore`, which waits on the window.
- **The confirmation** is a new approval kind, `Rewind`, drawn by the window outside pane text like
  the trust prompt: `◆ needs you` in amber, then "Undo changes in `<dir>` since `<time>`
  (`<reason>`)", the caller, counts ("restore 5 · delete 3 the agent added · bring back 1 it
  deleted · keep 2 changed later"), and up to 10 paths, escaped as `Screen::put` escapes them, a
  flagged one in red, then "and N more". `y` runs it once, `n` or Esc refuses, and it is never a
  lasting grant. The counts come from `pinfold checkpoint plan` (`diff-tree --name-status -z` in
  the store), not from the caller. With no window it is refused.
- **The restore** is `pinfold checkpoint restore`. It takes the `restore` checkpoint, then for
  each path compares the file to its entry in `N`, by stat and then by blob id, and skips it as
  "changed since shown" if it differs. Otherwise it makes the path match `C`:
  - Each parent is opened from the working directory's descriptor with `O_NOFOLLOW |
    O_DIRECTORY`. A parent that is now a symbolic link or a file where `C` has a directory is
    removed itself, with `unlinkat`, and made again as a directory; a link is never followed.
  - A file is written as a new file in its parent (`openat` with `O_CREAT | O_EXCL |
    O_NOFOLLOW`, mode 0600), filled, given 0644 or 0755 less the umask with `fchmod`, then
    `renameat` over the name. The rename replaces the entry, so a hard link or symbolic link
    there is dropped and what it pointed to is never written. A symbolic link is made with
    `symlinkat` and renamed the same way.
  - A path in `N` but not in `C` is deleted with `unlinkat`, only if `C` covered it and was
    complete. A directory left empty that `C` lacked is removed.
  - It reports counts, skipped paths and the restore checkpoint's id. The window flashes
    `✓ done · undid 8 files, 1 skipped` in green or `✕ error · <reason>` in red.
- A restore while the run is ● running is allowed; the prompt adds "the agent is still running,
  and may change these files again".

### 7. API, link and config

New methods, each with a JSON Schema, an MCP tool and an audit entry (method, scope, target pane,
no paths):

| Method | Parameters | Who may call | Result |
| --- | --- | --- | --- |
| `checkpoint.take` | `pane` | An agent pane for its own run (`self`), a pane for runs of its `children`, or a terminal outside every pane. Once in 30 s per run. | `{ checkpoint, files, new_bytes, complete }` |
| `checkpoint.list` | `pane` | As `checkpoint.take`, with no rate limit | Each checkpoint's id, reason, time, counts and `complete`. Metadata only, never a path. |
| `checkpoint.restore` | `pane`, `checkpoint`, `shown` (the view checkpoint), `paths` (at most 1,000; all if absent) | Any caller that may list the run, including a tool pane. Always waits on the window's `Rewind` prompt; one waiting prompt per caller | `{ restored, deleted, skipped, restore_checkpoint }`, or refused |

`tool.open` gains `checkpoint` for `diff`. A sandboxed agent still cannot call `tool.open`. A
closed run stays reachable from the pane that launched it, as one of its `children`, for this
core's life; the core keeps at most 64 runs of at most 20 checkpoints in memory.

The window link gains `ApprovalAction::Rewind` (`is_once`, `since` the next link version), whose
texts are the directory, the summary line and the paths, all escaped by the core. The window's
existing flash carries the result.

Config, read when the core starts, as agent profiles are:

```toml
[rewind]
enabled = true          # a checkpoint at every agent launch
on_done = true          # and when the run's status becomes done
wait_s = 10             # how long a launch waits for its checkpoint
include_ignored = false # skip what the tree's .gitignore files ignore
max_file_mib = 16       # larger files are not covered
max_files = 100000      # entries walked per checkpoint
max_new_mib = 256       # new content per checkpoint
max_store_mib = 2048    # all stores together
keep_days = 14
keep_per_run = 20

[agents.claude]
rewind = true           # per profile; false takes no checkpoints for it
```

### 8. Platforms

| | macOS | Linux |
| --- | --- | --- |
| Store | `~/Library/Application Support/PinFold/checkpoints/<key>` | `$XDG_STATE_HOME/pinfold/checkpoints/<key>` |
| `.git` aliases skipped | ASCII case and HFS+ ignorable code points | ASCII case |
| Sandboxed agent | Can read `.git`'s control files but not write them (`GIT`); unchanged | `.git` read-only, commits in the shadow (ADR-0039); unchanged, as checkpoints skip `.git` |
| Git | The first on an absolute `PATH` entry; `/usr/bin/git` without the Command Line Tools would open an install dialog (open question 5) | The first on an absolute `PATH` entry |

Sandboxed and unsandboxed runs are treated the same: checkpoints and restores always run outside
the sandbox, from PinFold's own binary, on paths inside the working directory the core recorded at
launch.

## Threat-model changes

The threat model gains, each marked "proposed by ADR-0050":

- **T5**, control added: checkpoints never run git over the working tree. PinFold walks it without
  following links and feeds `git fast-import` in a store with no global or system configuration.
  Test: the hostile repository of `nothing_a_hostile_repository_configures_runs`, plus a work tree
  with its own `.gitattributes` filter, `core.fsmonitor`, hooks and a nested repository whose
  configuration names commands; a take, plan, diff and restore leave no marker.
- **T11, new: a rewind writes where the agent pointed it, or undoes what it should not.** An agent
  plants a link (`src` → `~/.ssh`), a hard link to a file outside, or a file changed during the
  walk; or a caller asks for a restore the person did not mean. Controls: the walk and the restore
  never follow a link; hard-linked files are not covered; a restore writes a new file and renames
  it over the name; it deletes only covered paths of a complete checkpoint and skips a file
  changed since shown; it always takes a checkpoint first; every restore waits on a window prompt
  whose text comes from the store, not the caller; the store is hidden from sandboxed agents.
  Not covered: an unsandboxed agent, running as the person, can change or delete its own
  checkpoints, so a rewind is a convenience, not a backup.
- **T9**, residual noted: a checkpoint keeps copies of covered files, a deleted secret file among
  them, for up to `keep_days`, in a 0700 store under the state directory. Ignored files, such as
  `.env` in most repositories, are not copied by default.
- **Authorization** gains the three `checkpoint.*` rows.

## Memory

| Metric | Target | How `cargo xtask mem-gate` checks it |
| --- | --- | --- |
| Core, 8 idle panes, plus 64 run records of 20 checkpoints | ≤ 4 MB, the existing core budget, with a target of 2 KiB a record, 128 KiB at the cap | A new row variant of the core row, filling the records through `checkpoint.take` on a scratch directory |
| Each agent pane in the core | ≤ 250 KB, unchanged | The existing pane row |
| A `pinfold checkpoint take` process | ≤ 4 MB at any tree size, as the walk and the stat merge stream with 64 KiB buffers | A new row: a take over a generated tree of 100,000 files, then a second take with 1% changed |
| `git fast-import` and `repack` | Not PinFold's process; the spike reports their peak, not gated (open question 9) | — |
| Diff viewer on a checkpoint | ≤ 3 MB + 3 × git output, at most 27 MB, the existing diff row | The existing row, run with `--checkpoint` |
| Window with a `Rewind` prompt | The existing window budget: at most 10 paths of 256 bytes | The existing window rows |
| Disk | ≤ `max_store_mib`, 2 GiB by default | A pruning test, not mem-gate |

## Implementation plan

0. **Spike, before slice 1.** `docs/spikes/rewind/`: a prototype of the walker and fast-import
   stream in `src/`, about 200 lines, and `measure.sh`, which on macOS (APFS) and Linux (ext4 in
   CI, btrfs where there is one) measures, median of 3, warm cache: wall time and peak memory
   (`/usr/bin/time -l` or `-v`) of the prototype and of `git fast-import`; `du -sk` of the store
   after the take and after `repack`; the same for a second take with nothing changed and with 1%
   of files changed; and, as baselines, `cp -c -R` on macOS, `cp -R --reflink=auto` and plain
   `cp -R`. Corpora: PinFold at a fixed commit, `torvalds/linux` at tag `v6.12`, and a generated tree of 100,000 small files, 20 files of 20 MB and an ignored
   directory of 200,000 files. Results go in its README with the script beside them. This
   decision stands if a launch checkpoint of the Linux tree takes ≤ 10 s, an unchanged one
   ≤ 1 s, the store is at most half the covered size, and the prototype stays ≤ 4 MB. Otherwise
   this ADR is revised before slice 1, with option B on macOS the first alternative.
1. **`pinfold-sys`: descriptor-relative file access.** `openat`, `fstatat`, `readlinkat`,
   `unlinkat`, `symlinkat`, `renameat` and `mkdirat` wrappers, each with a `// SAFETY:` comment.
   Tests: a component swapped for a link is refused; a rename over a link replaces the link, not
   its target.
2. **`pinfold-tools`: the walker and `git::store`.** Skips, caps, hard links, the `.git` aliases,
   `.gitignore` matching and the stat merge; the fast-import stream. Tests: each skip and cap
   fails without it; the hostile work tree of T5 leaves no marker; a `.gitignore` fuzz target joins
   `fuzz-smoke`.
3. **`pinfold checkpoint take` and `prune`.** Store creation and its refusals, the lock, commit
   metadata, pruning order. Tests: a live launch checkpoint is never pruned; the cap refuses new
   checkpoints instead; a symlinked or open store is refused.
4. **`pinfold checkpoint plan` and `restore`.** Tests: a planted directory link, file link and hard
   link; a file changed since shown is skipped; an incomplete checkpoint deletes nothing; the
   restore checkpoint restores the state before the restore.
5. **Core: runs and taking.** Run records, the launch checkpoint with its wait, done and exit
   checkpoints, `checkpoint.take` and `checkpoint.list` with schemas, scopes, rate limits, audit
   and MCP tools, and the `[rewind]` config. Session tests, one with a sandboxed agent that
   cannot read its store.
6. **Core, link and window: `checkpoint.restore`.** `ApprovalAction::Rewind`, the plan, the prompt,
   the result flash. Tests: only the window answers; with no window it is refused; output that
   prints the prompt's text answers nothing; a snapshot of the prompt at 80 columns.
7. **Diff viewer and CLI.** `tool.open`'s `checkpoint`, `pinfold diff --checkpoint`, the `later`
   mark, `u` and `U`, `pinfold rewind` and the palette entry. Snapshots and an end-to-end session
   test: launch, agent edits, add and delete, `U`, `y`, tree as before.
8. **mem-gate rows and docs.** The rows above, `docs/configuration.md`, the threat model's rows
   moved from proposed to built, and known issues: commits not undone, writes outside the
   directory, nested repositories.

## Open questions for the maintainer

1. When the launch checkpoint takes past `wait_s`, should the agent start without one, as proposed,
   or should the launch be refused?
2. Is leaving ignored files out by default right, given that an agent's edit to an ignored `.env`
   then cannot be rewound?
3. Should a first checkpoint inside a repository read stat data and blob ids from the real
   `.git/index` to skip unchanged tracked files? Faster on large repositories, but it parses a
   file an unsandboxed agent can forge. The spike can measure the gain.
4. Should runs from before a core restart be reachable, which needs the core to list a store's
   refs through `pinfold checkpoint`, or is this core's life enough for a first version?
5. On macOS, should PinFold check for the Command Line Tools before running `/usr/bin/git`, so a
   launch never opens Apple's install dialog? The diff viewer has the same exposure today.
6. Should nested repositories and submodules be covered as plain files?
7. May an agent ask, through `checkpoint.restore`, to undo its own run, behind the same prompt, as
   proposed, or only the person's own callers: tool panes and terminals outside every pane?
8. Are 2 GiB in all and 14 days the right defaults for disk?
9. Should mem-gate also gate the peak of `git fast-import`, which is git's process, not
   PinFold's?

## Consequences

Every agent run gets an undo, sandboxed or not, inside a repository or outside one, at the cost of
one walk of the directory before the agent starts and disk up to a cap. Git never runs over the
tree the agent wrote, so no configuration or attribute there can name a command, and a restore
cannot be steered through a link. The core gains three methods, one approval kind and small run
records, and parses nothing new; the work is in short-lived processes. Rewind needs `git`, does
not undo the agent's commits or its writes outside the working directory, skips ignored and large
files by default, and cannot protect checkpoints from an unsandboxed agent. Revisit if the spike
misses its targets, if a pure-Rust store replaces git for the tools, or if agents start running in
their own copy of the tree, which would make a rewind a discard.
