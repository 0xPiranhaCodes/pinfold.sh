# ADR-0050 spike: checkpoints through `git fast-import`

**Result: ADR-0050 as written fails the first-take gate on both platforms and the unchanged
gate on macOS; with three changes to how a take runs it passes all four gates on macOS (APFS)
and Linux (ext4).** Slice 0 of
[#442](https://github.com/0xPiranhaCodes/pinfold.sh/issues/442). The ADR should take the
changes listed under "Recommended changes to ADR-0050" before slice 1
([ADR-0050](../../adr/0050-rewind-an-agents-changes.md), implementation plan step 0).

| Gate | Target | macOS APFS, as written | macOS APFS, recommended | Linux ext4, as written | Linux ext4, recommended |
| --- | --- | --- | --- | --- | --- |
| Linux tree, first take | ≤ 10 s | 46.29 s, **fail** | 7.25 s, pass | 56.43 s, **fail** | 5.18 s, pass |
| Linux tree, unchanged take | ≤ 1 s | 1.02 s, **fail** | 0.98 s, pass | 0.54 s, pass | 0.51 s, pass |
| Store after the first take / covered bytes | ≤ 0.5 | 0.21, pass | 0.25, pass | 0.20, pass | 0.24, pass |
| Prototype peak, every take of every corpus | ≤ 4 MB | 3.02 MB, pass | 3.47 MB, pass | 2.56 MB, pass | 3.11 MB, pass |

Two of these pass narrowly and should be read that way: the macOS unchanged take is 0.98 s
(runs of 1.18, 0.98 and 0.98 s), and macOS peak RSS, as opposed to the footprint mem-gate
reads, reached 4.18 MB. The Linux numbers come from an OrbStack VM on the same Mac, not
bare-metal Linux or CI; its first recommended run, right after the `cp` baselines, took
15.66 s before two runs of 5.17 and 5.18 s.

The gates are ADR-0050's: a whole-tree first take of `torvalds/linux` at `v6.12`
(86,663 files, 1.45 GB covered) in at most 10 s, an unchanged take in at most 1 s, a store at
most half the covered bytes, and the prototype at most 4 MB. The other corpora are PinFold at
`8f0bd1f` and the ADR's generated tree (`spike-rewind gen`). "ADR as written" is one
`git fast-import` fed by one thread, git's default zlib level and delta attempts.
"Recommended" is the same walk with the changes below. Memory is the peak of every take of
every corpus: `phys_footprint` on macOS and peak RSS on Linux, which is at least the PSS
mem-gate reads. Full tables, with both RSS and footprint inputs, are in
[`results.md`](results.md).

## What the prototype is

[`src/main.rs`](src/main.rs), `spike-rewind <dir> <store> <n>`, is the walk of ADR-0050
section 3 and the store of section 2:

- The directory is opened once with `O_DIRECTORY | O_NOFOLLOW`; every entry is `fstatat`ed
  with `AT_SYMLINK_NOFOLLOW` and opened with `openat(O_NOFOLLOW)` relative to its parent's
  descriptor. Links are stored as `120000` blobs of their target text, never followed.
- Skipped and listed as not covered: any `.git` (ASCII case), a directory holding a `.git`
  (a nested repository), hard-linked files, fifos, sockets and devices, files over 16 MiB,
  and what the tree's `.gitignore` files ignore, read through `O_NOFOLLOW` as data by the
  spike's own matcher; `.env*` files are covered even when ignored. Past `max_files` entries
  or `max_new_mib` of new content the take stops and is marked incomplete.
- Each file is read through its descriptor, checked with `fstat` before and after, read once
  more if it changed, then left uncovered.
- `stat/spike-<n>` holds each covered file's mode, blob id, size, times and inode in walk
  order. The next take merges it with its walk, streaming, and reuses the blob id of a file
  whose stat is unchanged.
- Git runs with `GIT_DIR=<store>`, `GIT_CONFIG_GLOBAL=/dev/null`, `GIT_CONFIG_NOSYSTEM=1`,
  an empty environment otherwise and no work tree. Only `init --bare --template=`,
  `config core.compression` and `fast-import` run in the store.

Not prototyped: git's `is_hfs_dotgit` aliases on macOS, the lock, pruning, metadata beyond a
JSON message, and restore. Spike code, kept out of the workspace
([ADR-0019](../../adr/0019-spike-code.md)).

## Findings

- **One `git fast-import` is the bottleneck, not the walk.** As written, the Linux tree's
  first take spends 43 s (macOS) and 48–57 s (Linux) of git CPU in a 46–56 s take. Most of it
  is zlib at git's default level and fast-import trying a delta of every blob against the
  previous one, which `--depth=0` turns off; the recommended store is 0.04 of covered bytes
  larger for it. `REWIND_READERS`, `REWIND_COMPRESSION` and `REWIND_FAST_IMPORT_ARGS` set each
  change on its own.
- **On APFS, reading files from one thread is the next floor.** Compare
  `find linux -path linux/.git -prune -o -type f -print0 | xargs -0 cat >/dev/null` with the
  same pipeline through `xargs -0 -n 2000 -P 8`: on this Mac, warm, about 11 s against 3 s,
  in ad-hoc runs not recorded in `results/`. So the reads, not only git, have to be spread.
- **Four reader threads, each with its own fast-import, meet the gate.** The walk stays one
  thread and in order; files to read go to readers through a bounded queue of 512 records,
  and the stat list is written in walk order as answers come back, so memory does not grow
  with the tree. The commit is written by one more fast-import once every reader's pack is in
  the store. Linux tree: 7.25 s on macOS, 5.18 s on Linux. The store holds four packs per
  first take.
- **An unchanged take is the walk.** With the commit built on the previous checkpoint and
  given only what changed, git costs 8–114 ms of CPU and 6 MB; the walk (`walk_ms` in the
  JSON) is 0.90 s of the 0.98 s on macOS, single-threaded `fstatat` of every entry, and
  0.40–0.44 s on Linux. Listing every file again, as the ADR's `M 100644 <id>` wording reads,
  kept the macOS unchanged take over 1 s in development runs, with fast-import touching every
  object; those runs are not in `results/`.
- **The default caps leave the Linux tree incomplete.** `max_new_mib = 256` stops the launch
  take of the Linux tree after 33,969 of 86,663 files (268 MB read), and the generated tree
  after 61,502 of 100,002. A whole-tree take of either fits the time gate, so the cap trades
  coverage for no gain on these trees. `max_files` counts directories too, so the generated
  tree's 100,002 files would also exceed it.
- **`.env*` in an ignored directory is not covered.** Ignored directories are not walked (the
  generated tree's `build/` holds 200,000 files), so the always-covered `.env*` rule applies to
  directories the walk enters. The ignored `.env.local` at the generated tree's root is
  covered.
- **The store is a quarter of what it covers.** 0.20–0.25 for the Linux tree, 0.29–0.34 for
  the generated one and 0.43–0.47 for PinFold, whose bytes include images and fonts. Level 1
  and no deltas cost 0.04 of covered bytes over the ADR as written; `repack -a -d` recovers
  part of it but takes 10.5 s (macOS) and 15.5 s (Linux) on the recommended Linux store,
  against 1.2–1.4 s on one already deltified.
- **Option B is not faster here.** `cp -c -R` of the Linux tree on APFS took 22.0 s, as
  `clonefile` is per file, and used 34 MB; plain `cp -R` took 47.1 s. On ext4, which cannot
  clone, `--reflink=auto` copied in full: 22.9 s and 1,707 MB, each checkpoint. A first take
  with the recommended changes is faster than either and a quarter of the size.
- **Reading `.git/index` would save most of a first take.** The index read takes 0.08–0.10 s,
  after which the Linux tree's first take would cost about an unchanged one: 1.06 s instead of
  7.25 s on macOS and 0.61 s instead of 5.18 s on Linux. It needs the repository's objects in
  the store (alternates or a copy), and an unsandboxed agent can forge the index; the gates
  pass without it, so decision 3 can stand.
- **`git fast-import` peaks at 247–326 MB** on the Linux tree's first take, the largest
  single process, and at about 6 MB for an unchanged take. With four readers, four such
  processes run at once; their sum was not measured. Not gated (decision 9), but it is the
  memory cost a launch puts on the machine.

## Recommended changes to ADR-0050

1. **Section 2: read with four threads, each feeding its own `git fast-import`, then commit
   with one more.** `pinfold checkpoint take` is a short-lived process, not the core, so
   threads do not touch ADR-0007. Its memory bound holds: a fixed queue of 512 records and
   64 KiB buffers per reader, 3.47 MB at most here. Mem-gate's take row should run with the
   readers.
2. **Section 2: run fast-import with `--depth=0`, and write `core.compression = 1` into the
   store's config at creation.** Deltas come later from `repack -a -d` in `prune`, off the
   launch path. Section 5 should run that repack only when the store has more than a few
   packs or crossed a size, not after every take, since it costs 10–15 s on a Linux-sized
   store.
3. **Section 1: a checkpoint's commit names the previous one as its parent and lists only
   paths that changed (`M`) or went (`D`)**, from a streaming merge of the two stat lists,
   instead of every covered file. The trees are identical (checked against a take from
   scratch), and git's share of an unchanged take is 8–114 ms. The
   launch checkpoint of a run has no parent unless the store holds an earlier run's.
4. **Section 3 and the config: `max_new_mib = 2048` and `max_files = 200000`, counting files
   and links only.** At 256 MiB a launch checkpoint of a Linux-sized tree is incomplete, so a
   restore from it can delete nothing; at 2 GiB the Linux tree took 5–7 s and its store
   about 350 MB, within `max_store_mib`. `wait_s` stays the backstop for a slower disk.
5. **Section 3: say that `.env*` files are covered in every directory the walk enters**, and that
   an ignored directory is not entered.
6. **Slice 2: keep `getattrlistbulk` on macOS in reserve for the walk.** The macOS unchanged
   take is 0.98 s against 1 s; mem-gate's take row or a regression would show it first.
   Not prototyped here.

Decision 3 (no `.git/index`) and option A both stand on these numbers.

## Reproducing

```sh
docs/spikes/rewind/measure.sh <scratch-dir> macos-apfs   # macOS, the file system of <scratch-dir>
docs/spikes/rewind/measure.sh --linux                    # Docker, ext4 on a loop device
python3 -I docs/spikes/rewind/summarize.py               # results.md's tables
```

`LINUX_SRC=<clone of linux v6.12>` copies an existing clone instead of downloading one.
Each run needs about 6 GB free and deletes its corpora, copies and stores at the end.
`cargo test` in this directory runs the prototype's self-checks: blob ids match git's, the
walk order, and the `.gitignore` globs.
