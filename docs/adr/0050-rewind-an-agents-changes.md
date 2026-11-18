# ADR-0050: Rewind an agent's changes from checkpoints in a PinFold-owned git store

- **Status:** Accepted (accepted by the maintainer, 2026-11-18)
- **Date:** 2026-11-14
- **Amended 2026-11-18**, decided by the coordinator for the maintainer, after the spike in
  [`docs/spikes/rewind/`](../spikes/rewind/README.md) failed this ADR's gates as first written.
  Sections 1, 2, 3, 5 and 7, the memory table and slices 0, 2 and 3 change; see "Amendment
  2026-11-18" at the end.

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
plan has measured it. The maintainer's answers to the open questions are folded in and listed
under "Decisions on the open questions", and so is the maintainer's rule for this work: no
resident memory growth, so the core keeps fixed-size run records and reads everything else from
the store on demand. Tracked in [#442](https://github.com/0xPiranhaCodes/pinfold.sh/issues/442).

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
  order, streaming, and reuses the old blob id for a file whose stat is unchanged, so it reads
  only changed files. Its commit starts from the tree of the checkpoint whose list it merged,
  the store's newest, and lists only what differs from it: `M` for a path added or changed, `D`
  for one gone. It names no parent, so a pruned checkpoint is reachable from no later one
  (amendments). A store with no checkpoint yet lists every covered file. Memory stays
  flat in the tree's size. A first
  checkpoint reads every covered file: it never takes stat data or blob ids from the repository's
  own `.git/index`, which an unsandboxed agent can forge (decision 3).
- One writer at a time per store: `<store>/pinfold.lock`, mode 0600, opened without following
  a link and held with `flock`, so the kernel drops it when the take dies; a second take or
  prune refuses at once (amendment of slice 3).

### 2. Who takes it, and how git is run

`pinfold checkpoint take|list|plan|restore|prune`, hidden subcommands of the existing `pinfold` binary
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

The walk is one thread and stays in path order. Files to read go through a bounded queue of 512
records to `min(4, available CPUs)` reader threads, each with 64 KiB buffers and its own
`git fast-import --quiet --depth=0`, and the stat list is written in walk order as their answers
come back, so memory does not grow with the tree. Once every reader's pack is in the store, one
more `fast-import` writes the commit. `pinfold checkpoint take` is a short-lived process, not the
core, so these threads do not touch ADR-0007. Their git processes are bounded: the sum of the
concurrent `git` peaks of a take must stay within 1.25 times the peak of the single importer of
this ADR as first written, on the same tree (amendment).

The take and its git children run at background priority, so a checkpoint never makes the
machine feel slow. On macOS the take is started with a QoS clamp of utility, as
`taskpolicy -c utility` starts a command, and its children inherit the clamp; on Linux the take
sets nice 10 (`setpriority`) and I/O priority class idle (`ioprio_set`) for itself before it
starts any git. Each is a `pinfold-sys` wrapper.

Git is the first `git` on an absolute `PATH` entry, as for the diff viewer. On macOS, when that is
`/usr/bin/git`, PinFold first checks that the Command Line Tools are installed (`xcode-select -p`
names a directory holding `usr/bin/git`) and otherwise treats git as missing, so running it never
opens Apple's install dialog; the diff viewer's lookup in `git.rs` gets the same check (decision
5). With git missing, no checkpoint is taken and the pane's strip says `no checkpoint · git not
found`.

Git is reached through a new entry point beside `git::run`, `git::store(store, args, stdin)`,
that keeps `run`'s environment scrubbing, absolute-`PATH` lookup, timeout, output cap and
`OVERRIDES`, and adds `GIT_DIR=<store>`, `GIT_CONFIG_GLOBAL=/dev/null` and
`GIT_CONFIG_NOSYSTEM=1`, with no work tree. The store's own `config` is written by PinFold at
creation and names no command: `core.compression = 1`, as deltas come later from `repack`.
Only these subcommands run there: `fast-import --quiet --depth=0` (with `feature done`),
`diff-tree`, `diff` between two commits (with `--no-ext-diff --no-textconv --no-color`),
`cat-file --batch`, `for-each-ref`, `update-ref -d`, `repack -a -d -q`, `prune` and
`rev-list --objects --all --disk-usage` (amendment of slice 3). None of them reads the working directory.

### 3. What a checkpoint skips

Each skipped path is listed in the commit message as not covered. A restore never writes or
deletes a path that the checkpoint it restores did not cover.

| Skipped | Why |
| --- | --- |
| Every entry named `.git`, ignoring ASCII case, and on macOS any name that git's `is_hfs_dotgit` would take for it | Git's own data; restoring it is how a repository gets hooks. Commits the agent made are not undone; the docs say so. |
| A directory holding a `.git` below the working directory, a nested repository or submodule | Its files belong to another repository's history. Not covered in v1; the diff viewer on a checkpoint lists each skipped one as `nested repository, not covered` (decision 6). |
| Paths ignored by the `.gitignore` files in the tree, with `rewind.include_ignored = false`, except files whose name matches `.env*`, which are covered in every directory the walk enters. An ignored directory is not entered, so a `.env*` inside one is not covered | Build output and dependencies, often most of a tree's size. Matched by PinFold's walker from the tree's `.gitignore` files, read as data with the walk's caps; no git runs over the tree for it. A `.env*` file is covered even when ignored: an agent that overwrites one loses the person's secrets for good, the worst loss a rewind can prevent, and the copy stays in the private store (decision 2). |
| A regular file over `rewind.max_file_mib` (16 MiB) | Large binaries. |
| A regular file with more than one hard link | It may be a link to a file outside the directory, such as a hidden credential, whose text a checkpoint would copy and a restore write back. |
| Fifos, sockets and devices | Not files. |
| Anything outside the working directory | Not watched. The docs say an unsandboxed agent's writes elsewhere, such as to `~/.bashrc`, cannot be rewound. |
| Everything past `rewind.max_files` (200,000) files and links, directories not counted, or `rewind.max_new_mib` (2048 MiB) of new content | Bounds time and disk. The checkpoint is marked incomplete; a restore from it deletes nothing. At the first values, 100,000 entries and 256 MiB, a launch checkpoint of the Linux tree stopped after 33,969 of 86,663 files; whole, it fits `wait_s` (amendment). |

### 4. When checkpoints are taken

- **Launch.** `agent.launch` takes checkpoint 0 after its directory checks and before the agent's
  PTY opens, and defers its reply until then. If it is not done within `rewind.wait_s` (10 s) the
  process is killed and the agent starts without one; the strip under the pane says
  `no checkpoint · took over 10 s` in yellow until a later checkpoint of the run succeeds, and
  `agent.launch` answers with `checkpoint: null`. It is not an agent state, so it gets none of
  ◆ ● ✓ ✕ ○ (decision 1). `pinfold agent` prints "taking a checkpoint" while
  it waits.
- **Done and exit.** When the run's status becomes ✓ done, at most once in 60 s, and when its
  program exits. These mark where the agent stopped, for section 6.
- **On demand.** `checkpoint.take` (section 7), at most once in 30 s per run.
- **Before a restore**, always, so a rewind can itself be rewound.

### 5. Keeping and pruning

A run keeps at most `rewind.keep_per_run` (20) checkpoints: past it, the oldest after the launch
checkpoint goes. A checkpoint older than `rewind.keep_days` (14) goes. When all stores together
exceed `rewind.max_store_mib` (2048; 2 GiB and 14 days are defaults, both configurable, decision
8), closed runs go oldest first, then a live run's checkpoints
other than its launch one and its newest. A live run's launch checkpoint is never pruned: if the
cap cannot be met without it, new checkpoints are refused, and the reason is shown. So an agent
cannot push its own launch checkpoint out by taking checkpoints. `pinfold checkpoint prune` runs
after each take: it deletes refs and stat files, then `prune` in the store. It runs
`repack -a -d -q`, which costs 10-15 s on a Linux-sized store, only when the store holds more
than 32 packs or the packs written since its last repack exceed 512 MiB, and at background
priority like a take (amendment).

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
- Nested repositories and submodules the checkpoint skipped are listed at the end of the file
  list as `nested repository, not covered`, so the person knows a rewind leaves them as they are.
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
| `checkpoint.take` | `pane` | An agent pane for its own run (`self`), a pane for runs of its `children`, or a terminal outside every pane. Once in 30 s per run. | `{ checkpoint, files, new_bytes, complete, changed }`, `changed` being how many paths differ from the launch checkpoint, one `diff-tree -r --name-only` in the store, which [ADR-0052](0052-session-replay-a-timeline-per-agent-run.md) records |
| `checkpoint.list` | `pane` | As `checkpoint.take`, with no rate limit | Each checkpoint's id, reason, time, counts and `complete`, read on demand from the store by `pinfold checkpoint list` (`for-each-ref`), never cached in the core. Metadata only, never a path. |
| `checkpoint.restore` | `pane`, `checkpoint`, `shown` (the view checkpoint), `paths` (at most 1,000; all if absent) | Only the person's own callers: a tool pane (the diff viewer) and a terminal outside every pane. An agent pane is refused, for its own run and its children's, before any prompt (decision 7). Always waits on the window's `Rewind` prompt; one waiting prompt per caller | `{ restored, deleted, skipped, restore_checkpoint }`, or refused |

`tool.open` gains `checkpoint` for `diff`. A sandboxed agent still cannot call `tool.open`. A
closed run stays reachable from the pane that launched it, as one of its `children`, for this
core's life only; runs from before a core restart are not reachable in v1 (decision 4). The core
keeps per run a fixed-size record and no list: the run id, its pane, the store key, the newest
`done` or `exit` checkpoint's `n` and a `missing` reason, 48 bytes, for at most 64 runs, 3 KiB.
Everything else, checkpoint lists, counts and paths, is read from the store when asked.

The window link gains `ApprovalAction::Rewind` (`is_once`, `since` link version 16, the next free
one), whose texts are the directory, the summary line and the paths, all escaped by the core, and
`Notice::Checkpoint { pane, missing: u8 }` for the strip's `no checkpoint`. The window's existing
flash carries the result. ADR-0043 gains the row for 16: the core never sends either to a version
15 window, so a restore from a terminal outside every pane is refused with "approval needs an open
PinFold window" and the strip shows nothing. [ADR-0051](0051-needs-you-inbox-and-alerts.md) takes
17 and [ADR-0052](0052-session-replay-a-timeline-per-agent-run.md) 18, so a release that includes
all three moves the link from 15 to 18, more than one apart, and restarts the session per
[ADR-0043](0043-window-serves-the-previous-link-version.md).

Config, read when the core starts, as agent profiles are:

```toml
[rewind]
enabled = true          # a checkpoint at every agent launch
on_done = true          # and when the run's status becomes done
wait_s = 10             # how long a launch waits for its checkpoint
include_ignored = false # skip what the tree's .gitignore files ignore, except .env*
max_file_mib = 16       # larger files are not covered
max_files = 200000      # files and links walked per checkpoint
max_new_mib = 2048      # new content per checkpoint
max_store_mib = 2048    # all stores together, 2 GiB
keep_days = 14          # older checkpoints are pruned
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
| Git | The first on an absolute `PATH` entry; `/usr/bin/git` only once the Command Line Tools are found, so no install dialog opens | The first on an absolute `PATH` entry |

Sandboxed and unsandboxed runs are treated the same: checkpoints and restores always run outside
the sandbox, from PinFold's own binary, on paths inside the working directory the core recorded at
launch.

## Threat-model changes

The threat model gains, each naming ADR-0050:

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
  changed since shown; it always takes a checkpoint first; only the person's own callers (a tool
  pane, a terminal outside every pane) may ask for a restore, never an agent pane; every restore
  waits on a window prompt whose text comes from the store, not the caller; the store is hidden
  from sandboxed agents.
  Not covered: an unsandboxed agent, running as the person, can change or delete its own
  checkpoints, so a rewind is a convenience, not a backup.
- **T9**, residual noted: a checkpoint keeps copies of covered files, a deleted secret file among
  them, for up to `keep_days`, in a 0700 store under the state directory. Ignored files are not
  copied by default, except `.env*` files, which are always copied into that private store so an
  agent's overwrite of one can be undone.
- **Authorization** gains the `checkpoint.*` rows; `checkpoint.restore` is denied to agent panes.

## Memory

| Metric | Target | How `cargo xtask mem-gate` checks it |
| --- | --- | --- |
| Core, 8 idle panes | Unchanged: no run, no record, and `[rewind]` adds nothing until an agent launches | The existing row must hold unchanged |
| Core, 8 agent panes plus 64 run records | ≤ 4 MB, the existing core budget; 48 B a record, 3 KiB at the cap, and no checkpoint list kept | A new row, filling the records through `checkpoint.take` on a scratch directory, then `checkpoint.list` on each run, measured after settle |
| Each agent pane in the core | ≤ 250 KB, unchanged | The existing pane row |
| A `pinfold checkpoint take` process | ≤ 4 MB at any tree size, as the walk and the stat merge stream with 64 KiB buffers | A new row: a take over a generated tree of 100,000 files, then a second take with 1% changed |
| `git fast-import` and `repack` | Not PinFold's process, and mem-gate does not gate it (decision 9). The sum of a take's concurrent git peaks stays within 1.25 times the single importer's on the same tree: on the Linux tree 152 MB against 268 MB on macOS and 325 MB against 280 MB on Linux (amendment) | The spike's `measure.sh --peaks` |
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
   `cp -R`. It also reports, not gates, `git fast-import`'s peak memory (decision 9), and the time
   a first take would save by reading the real `.git/index`, which v1 does not do (decision 3).
   Corpora: PinFold at a fixed commit, `torvalds/linux` at tag `v6.12`, and a generated tree of 100,000 small files, 20 files of 20 MB and an ignored
   directory of 200,000 files. Results go in its README with the script beside them. This
   decision stands if a launch checkpoint of the Linux tree takes ≤ 10 s, an unchanged one
   ≤ 1 s, the store is at most half the covered size, and the prototype stays ≤ 4 MB. Otherwise
   this ADR is revised before slice 1, with option B on macOS the first alternative. mem-gate:
   none; the spike's own table is its memory evidence. Done in PR #451: as written the first
   take failed on both platforms; the amendment below takes the spike's changes, with which every
   gate passes.
1. **`pinfold-sys`: descriptor-relative file access.** `openat`, `fstatat`, `readlinkat`,
   `unlinkat`, `symlinkat`, `renameat` and `mkdirat` wrappers, each with a `// SAFETY:` comment.
   Tests: a component swapped for a link is refused; a rename over a link replaces the link, not
   its target. mem-gate: the existing core and pane rows hold unchanged (no resident state).
2. **`pinfold-tools`: the walker and `git::store`.** Skips, caps (`max_files` counting files
   and links only), hard links, the `.git` aliases, nested repositories, `.gitignore` matching
   with `.env*` covered in every directory the walk enters and ignored directories not entered,
   and the stat merge; the fast-import stream, with `min(4, available CPUs)` reader threads each
   feeding its own `fast-import --depth=0` through the 512-record queue, and a commit on the
   previous checkpoint listing only `M` and `D`; background priority for the take and its git
   (QoS utility on macOS, nice 10 and I/O class idle on Linux, through `pinfold-sys`); the macOS
   Command Line Tools check before `/usr/bin/git`, for `git::store` and the diff viewer's
   `git::run` alike. `getattrlistbulk` on macOS stays in reserve for the walk, used only if the
   take row or a regression puts the unchanged Linux-tree take over 1 s. Tests: each skip and cap
   fails without it; an ignored `.env.local` is covered and a `.env` in an ignored directory is
   not; a commit built on its parent has the tree of a take from scratch; the hostile work tree of
   T5 leaves no marker; with the Command Line Tools absent (a stubbed `xcode-select`) git counts
   as missing and `/usr/bin/git` never runs; a `.gitignore` fuzz target joins `fuzz-smoke`.
   mem-gate: the existing diff row holds.
3. **`pinfold checkpoint take` and `prune`.** Store creation with `core.compression = 1` and its
   refusals, the lock, commit metadata, pruning order, and `repack` only past 32 packs or 512 MiB
   written since the last one. Tests: a live launch checkpoint is never pruned; the cap refuses new
   checkpoints instead; a symlinked or open store is refused. mem-gate: new row, a take over a
   generated tree of 100,000 files, then one with 1% changed, ≤ 4 MB, with the reader threads.
4. **`pinfold checkpoint plan` and `restore`.** Tests: a planted directory link, file link and hard
   link; a file changed since shown is skipped; an incomplete checkpoint deletes nothing; the
   restore checkpoint restores the state before the restore. mem-gate: the take row's
   `restore` variant, ≤ 4 MB.
5. **Core: runs and taking.** Run records, the launch checkpoint with its wait, done and exit
   checkpoints, `checkpoint.take` and `checkpoint.list` with schemas, scopes, rate limits, audit
   and MCP tools, `pinfold checkpoint list`, and the `[rewind]` config. Session tests: one with
   a sandboxed agent that cannot read its store; a launch whose checkpoint takes past `wait_s`
   starts the agent and answers `checkpoint: null`. mem-gate: the new 64-run-record core row,
   ≤ 3 KiB over the 8-agent-pane row.
6. **Core, link and window: `checkpoint.restore`.** Link version 16 with `ApprovalAction::Rewind`
   and `Notice::Checkpoint`, ADR-0043's row; the plan, the prompt, the result flash, the strip's
   `no checkpoint`. Tests: only the window answers; with no window it is refused; an agent pane
   asking for its own run's restore is refused before any prompt; output that prints the prompt's
   text answers nothing; a snapshot of the prompt and of the strip at 80 columns. mem-gate: the
   window rows gain a `rewind: true` case, within the window budget.
7. **Diff viewer and CLI.** `tool.open`'s `checkpoint`, `pinfold diff --checkpoint`, the `later`
   mark, `u` and `U`, `pinfold rewind` and the palette entry. Snapshots and an end-to-end session
   test: launch, agent edits, add and delete, `U`, `y`, tree as before; nested repositories are
   listed as not covered. mem-gate: the diff row run with `--checkpoint`, ≤ 27 MB.
8. **Docs and known issues.** `docs/configuration.md`, the threat model's rows pointed at their
   tests, and known issues: commits not undone, writes outside the directory, nested repositories
   and submodules, runs from before a core restart. mem-gate: the full table, every row above,
   pasted in the PR.

## Decisions on the open questions

The maintainer answered each on 2026-11-18.

1. **A launch checkpoint past `wait_s`:** the agent starts without one, and the pane's strip says
   `no checkpoint`. A slow disk should not stop the person's work, and the strip makes the gap
   visible.
2. **Ignored files:** left out by default, but files matching `.env*` are always covered. They
   are the worst loss case, and the copy stays in the private store.
3. **Reading the real `.git/index`:** not in v1. An unsandboxed agent can forge it; the spike
   measures what it would save.
4. **Runs from before a core restart:** this core's life only in v1. Reaching older runs is a
   later ADR, with ADR-0052's question 10.
5. **The Command Line Tools on macOS:** checked before `/usr/bin/git` runs, so no install dialog
   opens, and the diff viewer's same exposure is fixed in slice 2. A dialog from a background
   take would be a prompt nobody asked for.
6. **Nested repositories and submodules:** skipped in v1, and the diff viewer says so. Their files
   belong to another repository's history.
7. **Who may restore:** only the person's own callers, tool panes and terminals outside every
   pane. An agent may not undo runs through `checkpoint.restore`, so a prompt-injected agent
   cannot ask the person to wipe what it did.
8. **Disk defaults:** 2 GiB in all and 14 days, both configurable. Disk, not memory, holds the
   checkpoints, and these bound it.
9. **`git fast-import`'s peak:** measured in the spike and reported, not gated by mem-gate. It is
   git's short-lived process, not PinFold's.

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

## Amendment 2026-11-18

Decided by the coordinator for the maintainer, under the maintainer's rule for this work (no
memory growth) and the spike's numbers ([`README.md`](../spikes/rewind/README.md),
[`results.md`](../spikes/rewind/results.md)). As first written, the Linux tree's first take took
46.29 s on macOS and 56.43 s on Linux against 10 s, and its unchanged take 1.02 s on macOS against
1 s. One `git fast-import` at git's default zlib level, trying a delta for every blob, was the
bottleneck, then reading from one thread on APFS. The spike's six recommendations are taken:

1. **Parallel readers, under a memory bound** (section 2). `min(4, available CPUs)` reader
   threads, each with its own `fast-import`, fed by the one in-order walk through a 512-record
   queue, and one more `fast-import` for the commit. The sum of concurrent git peaks must stay
   within 1.25 times the single importer's peak on the same tree. `measure.sh --peaks`, on the
   Linux tree, largest over six takes: macOS (`phys_footprint`) 152 MB with four readers against
   268 MB, a bound of 335 MB; Linux (RSS) 325 MB against 280 MB, a bound of 350 MB, the commit's
   importer alone after the readers. **Four readers** are chosen: two lower nothing on Linux and
   take 10.18 s on macOS, over the gate.
2. **`--depth=0` and `core.compression = 1`** (sections 2 and 5). Deltas come from
   `repack -a -d -q` in `prune`, run only past 32 packs or 512 MiB written since the last one, as
   it costs 10.5 s (macOS) and 15.5 s (Linux) on a Linux-sized store. The store is 0.25 of
   covered bytes against 0.21.
3. **A commit on its parent, listing only `M` and `D`** (section 1). Git's share of an unchanged
   take falls to 8-114 ms; the trees match a take from scratch.
4. **`max_new_mib = 2048` and `max_files = 200000`, counting files and links only** (section 3
   and the config). At 256 MiB the Linux tree's launch checkpoint was incomplete, so a restore
   from it could delete nothing; whole, it takes 5-7 s and about 350 MB of store.
5. **`.env*` is covered in every directory the walk enters; an ignored directory is not
   entered** (section 3), as the generated tree's ignored `build/` of 200,000 files showed.
6. **`getattrlistbulk` on macOS stays in reserve** for the walk (slice 2), as the unchanged take
   is the walk and passes narrowly.

Also decided: **the take and its git run at background priority** (section 2), QoS utility on
macOS and nice 10 with I/O class idle on Linux, so a checkpoint never makes the machine feel
slow. Measured with four readers on the Linux tree, it keeps every gate: first take 6.20 s
against 5.95 s at normal priority on macOS and 4.71 s against 5.16 s on Linux; unchanged 0.98 s
against 0.91 s on macOS, the narrowest margin, and 0.44 s on Linux. The Linux numbers come from an
OrbStack VM on the same Mac whose `none` I/O scheduler ignores I/O priority, so the idle class
is unmeasured there; bare-metal Linux and CI were not used. `wait_s` stays the backstop.

Decision 3 (no `.git/index`), option A and the 4 MB prototype bound stand: the prototype peaked at
3.49 MB with four readers at background priority.

Also decided during slice 2, by the coordinator for the maintainer:

- **Timeouts in `git::store`.** `diff-tree`, `diff`, `cat-file`, `for-each-ref` and `update-ref`
  keep `git::run`'s 10 s. `repack -a -d -q` and `prune` get 300 s, as the amendment measured
  `repack` at 10.5 s and 15.5 s on a Linux-sized store; they run at background priority after a
  take, off the launch path. `fast-import` is streamed and has no cap of its own: it is killed with
  the take, and with `feature done` a killed take leaves no partial commit.
- **Two more bounds on the walk** (section 3), each skipped and listed as not covered: a directory
  more than 64 levels below the working directory, as the walk holds a descriptor for each level
  and macOS starts a process with 256, and a directory of more than 65,536 entries, as the walk
  holds a directory's names while it is inside it. A `.gitignore` over 64 KiB, or past 256 KiB of
  patterns held for the directories the walk is in, is not used, and one whose patterns run past a
  step budget on a path is dropped for the rest of the walk: git's own matcher is exponential on
  lines such as `**/**/**/x`, and dropping covers more, never less.
- **The store is written, not `git init`ed.** PinFold creates it with mode 0700 and writes `HEAD`,
  `config` (`core.bare`, `core.compression = 1`), `objects/` and `refs/` itself, as `init` and
  `config` are not on the store's list of subcommands. The refusals and the lock stay in slice 3.
- **QoS utility on macOS** is applied by `pinfold_sys::background()`, which re-executes the take in
  place, same pid and descriptors, with `posix_spawnattr_set_qos_clamp_np` and
  `POSIX_SPAWN_SETEXEC`, as `taskpolicy -c utility` does: a thread's QoS is not inherited by the
  git it starts. On Linux it sets nice 10 and I/O class idle before any thread starts.

## Amendment 2026-11-18, slice 3

Decided by the coordinator for the maintainer while building `pinfold checkpoint take` and
`prune`, as three sentences above could not all hold:

1. **No parent** (section 1). A commit that names the previous checkpoint as its parent keeps
   every older checkpoint reachable from the newest, so deleting refs and running `prune` would
   free nothing and neither `keep_days` nor the cap could work. The commit instead starts from
   the previous checkpoint's tree (`M 040000 <tree> ""` in the fast-import stream) and lists
   only `M` and `D`, so the speed of amendment 3 above stands and the tree is the same. Diffs
   between checkpoints are `diff-tree` or `diff` of two commits, which need no parent. Test:
   `over_the_cap_closed_runs_go_first_and_their_objects_are_freed_by_the_repack`.
2. **Space comes back at a repack** (section 5). A take's objects go into packs, or stay loose
   under fast-import's 100-object limit, and `prune` frees only loose ones. So `keep_per_run`
   and `keep_days` delete refs and stat lists, then `prune`; their space in packs returns at the
   next repack past 32 packs or 512 MiB. Over `max_store_mib`, `prune` deletes in section 5's
   order until `git rev-list --objects --all --disk-usage` of what each changed store keeps,
   plus its stat lists, fits, then repacks each store it changed whatever its packs. If the cap
   cannot be met without a live run's launch checkpoint, it writes `<state>/checkpoints/full`,
   and every take refuses with `over max_store_mib` until a prune meets the cap. `rev-list`
   joins `git::store`'s list with fixed arguments and `repack`'s 300 s.
3. **The lock is `flock`** (section 1). A lock file made with `O_EXCL` outlives a take the core
   kills at `wait_s`, and would refuse every later take. `File::try_lock` on
   `<store>/pinfold.lock` is dropped by the kernel instead; a second take refuses at once with
   "another checkpoint is being taken or pruned in this store", and a prune skips a locked
   store, counting its size.

Until slice 5 reads `[rewind]`, `pinfold checkpoint prune` takes the live runs and the limits
as flags: `--live <run>`, `--max-store-mib`, `--keep-days` and `--keep-per-run`, defaults 2048,
14 and 20. The core runs it after each take, and after a take refused as over the cap.

## Amendment 2026-11-18, slice 4

Decided by the coordinator for the maintainer while building `pinfold checkpoint plan` and
`restore`, as section 6's parent rule could not hold beside section 3 and T11:

1. **A parent goes only through the deletion rule** (section 6). Read literally, "a parent that
   is now a symbolic link or a file where `C` has a directory is removed" would delete one when
   `C` is incomplete, one planted after `N` was shown, and a regular file no checkpoint covers,
   such as one over 16 MiB, which would then be in no checkpoint, the restore checkpoint
   included. Such a parent is removed only if it is in `N` as that link or file, unchanged since
   shown, and `C` is complete and covered it. Deletions run before writes, and a path above a
   chosen one counts as chosen, so the parent goes first. Otherwise the path under it is skipped
   and reported as changed since shown. Links are never followed either way.
2. **Changed since shown** is the entry's kind and mode, then the git blob id of what is on disk
   now, against its entry in `N`; a matching stat alone is not trusted.
3. **A checkpoint that skipped more paths than it lists** (`more`) covers no path it does not
   hold, so a restore from it deletes nothing, as from an incomplete one.
4. **What `plan` prints.** `{restore, delete, bring_back, not_covered, later, complete, paths,
   more}`: `not_covered` counts paths only `N` holds that a restore leaves alone; `later` counts
   paths changed since the run's newest `done` or `exit` checkpoint before `N`; `paths` holds up
   to 10 `[path, what, later]`, each cut to 256 bytes, fewer if the line would pass the 4 KiB the
   core reads, and `more` the rest. A restore treats a `later` path like any other: the caller
   picks the paths. Its line is `{restored, deleted, skipped, skipped_paths,
   restore_checkpoint}`, `skipped_paths` up to 10 `[path, "changed" or "failed"]`.
5. **Memory stays flat.** Diffs are written by git into a file in the store that is unlinked at
   once (`git::store_to`) and read back streaming, and blobs come through one `cat-file --batch`,
   so neither a diff nor a file is ever held whole.
6. **`plan` takes no lock**, as it only reads. A prune that removed either checkpoint before or
   while it read makes it fail with "checkpoint `<run>/<n>` no longer exists", never a partial
   count. `restore` reads the diff again under the lock and never trusts a plan's counts.

## Amendment 2026-11-18, slice 5

Decided by the coordinator for the maintainer while building the core's runs and taking, as
section 7's 48-byte record holds a store key but no path, and later takes need the directory:

1. **The store records its directory** (sections 1 and 7). A take given `-- <dir>`, as at
   launch, writes the resolved path once to `<store>/dir` (mode 0600, `O_EXCL`, never through a
   link). Every later take is `take <run> <reason> --key <key>`: it reads `dir` without following
   a link and refuses it, never falling back to the path as given, unless the path resolves to
   itself and its SHA-256 still gives the key, so a directory swapped for a link after launch is
   refused. The core computes the key itself at launch, so a launch take killed at `wait_s`
   leaves later takes working once the store exists.
2. **Takes number themselves** (section 1). `take` has no `<n>`: under the lock it takes the
   run's newest in the store plus one, or 0, as `restore` does. `[rewind]`'s caps reach it as
   `--max-files`, `--max-file-mib`, `--max-new-mib` and `--include-ignored`, and `prune` gets
   `[rewind]`'s values instead of its defaults.
3. **`list <run> --key <key>`** prints `{checkpoints, more}`, each
   `{checkpoint, reason, time, files, new_bytes, complete}`, newest first, cut to 4 KiB with
   `more` counting the oldest left out. No path.
4. **Bounds.** A launch take is killed at `wait_s` (1 to 60 s, as a client waits 62 s for an answer), done and exit takes at 120 s, an API take at 60 s, as its caller waits 62 s, the
   prune after a take at 900 s (its `repack` and `prune` have 300 s each inside), a list at
   10 s.
5. **The record** is the core's start, the store key as 8 bytes, the run's `n`, its pane, the
   pane that launched it, the newest `done` or `exit` checkpoint's `n`, the second of the last
   done and API take (`u32` each), `missing` and flags: 48 bytes, asserted. The launching pane
   is how a closed run stays reachable from it (decision 4). At 64 records the oldest closed
   run's is reused; with 64 live runs a launch starts with `checkpoint: null`, missing
   `too many runs`.
6. **Two workers at most**, ADR-0007's pool, each waiting on one child. Launches and API calls
   wait in a queue of 16, a launch still starting its agent at `wait_s` from its request; past
   the queue an API call is refused as busy and a launch starts its agent at once with
   `checkpoint: null`, missing `busy`, never blocking. Done and exit takes wait as flags in
   their run's record.
7. **Scopes** (section 7). A caller in a pane reaches a run if it is the run's pane, reaches
   that pane as `children` while it is open, or launched it; a terminal outside every pane
   reaches any run of this core.

## Amendment 2026-11-18, slice 6 as built

Decided by the coordinator for the maintainer while building `checkpoint.restore`, the prompt
and the strip:

1. **`plan` and `restore` take `--key <key>`** and read the directory from `<store>/dir`, as a
   later take does (slice 5, decision 1). The core always passes `--key`; `-- <dir>` stays for a
   person at a terminal, with the same refusals.
2. **`plan` names what the prompt shows**: its line adds `dir`, cut to 256 bytes, and checkpoint
   `c`'s `reason` and `time` from its commit message, a reason outside take's list read as
   `unknown`. All three come from the store, never from an agent's text, and are cut before the
   line is fitted to 4 KiB. The core escapes each text and cuts it to 256 characters again.
3. **`Notice::Restored { undid, skipped, error }`** carries the result flash: `undid` is
   restored plus deleted, and `error`, at most 200 bytes, is one of the core's fixed phrases,
   never a path or a word of a child's output, so a hostile commit message cannot reach it.
4. **Who may ask** (section 7). A tool pane reaches any run of this core, as a terminal outside
   every pane does. An agent pane, and a plain shell pane, are refused before any prompt, even
   for their own run. Every restore still waits on the window's prompt whoever asks, so a tool
   pane adds reach but no authority.
5. **Waiting.** The plan has 10 s, the prompt 60 s and the restore 60 s, so the server and the
   client's `ask` wait `RESTORE_WAIT`, 132 s. A client that gives up first leaves no restore
   unseen: an unanswered prompt closes at 60 s, and a restore that runs still reaches the
   window's flash.
6. **The strip** under an agent's pane reads `no checkpoint · <missing>` in the muted chrome
   colour, not in yellow as sections 4 and 6 say: in Graphite Grid ANSI yellow is `f5b544`,
   the ◆ needs-you amber, whose meaning is reserved, and the strip is information, not a call
   to act. The core tells the window
   each live run's `missing` once and again when it changes, and again to a new window; the
   window keeps a pane's entry only while it is not 0, at most one per pane.
7. **Link version 23.** `ApprovalAction::Rewind`, `Notice::Checkpoint { pane, missing }` and
   `Notice::Restored` have `since` 23, as 22 is the inbox's slice 10. ADR-0043 has the row.

## Amendment 2026-11-19, slice 7 as built

Decided by the coordinator for the maintainer while building the diff viewer on a checkpoint
and `pinfold rewind`:

1. **The viewer's arguments** (section 6). The core starts `pinfold diff --checkpoint -- <key>
   <C> <N> <pane>`: the store by key, as slice 6's `plan` and `restore` take it, `C` and `N` as
   `<run>/<n>`, and the agent's pane, which `checkpoint.restore` names. `tool.open`'s
   `checkpoint` is `C`; the caller must reach the run as `checkpoint.take` does (slice 5,
   decision 7), and a run whose agent never started is refused, both before the view take. The
   view take shares `checkpoint.take`'s once in 30 s per run, so no caller takes checkpoints
   faster this way; only the window's own entry, the person's, is not limited. The
   server waits 62 s for it, as for a take. A session restore never brings a checkpoint viewer
   back, as its view checkpoint lived with it.
2. **The diff** is `git diff --no-renames` of the two commits through `git::store`, so each
   file is one path a restore can name; the parser, caps and escaping are the existing ones.
3. **`later`** marks each file `diff-tree -r --name-only -z` lists between the run's newest
   `done` or `exit` checkpoint before `N` and `N`, streamed from an unlinked file in the store,
   as `plan` reads it.
4. **Nested repositories** come from the `skipped` lists of `C`'s and `N`'s commit messages,
   reason `nested`, and follow the files. Selecting one says a rewind leaves it; `u` on one is
   refused in the viewer.
5. **`U`** sends no `paths`, meaning all, only when no file is `later` and the diff shows every
   file; otherwise the shown files not `later`, at most 1,000, by name. The viewer refuses it
   past that, when every file is `later`, or when the store's `later` list could not be read,
   so it fails closed and never sends an empty list.
6. **The view checkpoint goes when the viewer quits** with `q`, or fails to load, unless `u` or
   `U` asked for a restore: its ref and stat list are deleted under the store's lock, waited for
   up to 2 s, as the prune after every take holds it. A viewer killed by closing its pane leaves
   it to `keep_per_run`.
7. **`pinfold rewind [pane] [--checkpoint <n>] [--right | --down | --tab]`**: the run in the
   caller's own pane, else in the agent pane focused last, as the viewer's `a` picks one; since
   the run's oldest listed checkpoint, its launch one, unless `--checkpoint` names another. The
   tool's `path` is the caller's working directory, which the viewer never reads.
