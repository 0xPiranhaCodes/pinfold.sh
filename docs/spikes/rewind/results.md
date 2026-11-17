# ADR-0050 spike results

Generated on 2026-11-18 by `measure.sh` and printed by `summarize.py` from
[`results/`](results/), whose JSON lines hold every run. Prototype at this commit.

- **macOS:** MacBook Pro, Apple M1 Pro (8 cores), 16 GB, macOS
  27.0.1, APFS (case-insensitive) on the internal SSD, Apple Git 2.54.0.
- **Linux:** OrbStack 8-CPU, 8 GB Linux 7.2.12 VM on the same Mac, Docker `rust:1.94-bookworm`,
  git 2.39.5, ext4 on a loop-mounted 10 GB image file. Not bare-metal Linux: the image sits on
  the Mac's SSD through the VM's disk, and GitHub's Linux runners were not used.

Each time is the median of 3 runs with a warm cache (every file read once first). "First" is a
take into an empty store, "unchanged" a take with nothing changed, "1% changed" a take after
appending a line to every 100th covered file. Whole-tree takes raise `max_files` and
`max_new_mib` so the take covers the whole tree; the 16 MiB per-file cap stays, so the
generated tree's twenty 20 MB files are never covered. The "Default caps" table is one take with
every ADR-0050 default. MB is 10^6 bytes. "Prototype peak" is `phys_footprint` on macOS
(mem-gate's measure) and peak RSS on Linux (above the PSS mem-gate reads); macOS peak RSS, also
in the JSON, reached 4.18 MB. "git peak" is the largest single git process, from `getrusage`, so RSS on both. Store sizes are
`du -sk`. The `cp` baselines copy everything but `.git`; "clone df" is the change in the file
system's used space, since `du` counts APFS clones at full size.

The `-peaks` sections come from `measure.sh --peaks` (`--linux --peaks` in Docker), a later
run on the Linux tree only, with the same hosts. Every take runs under `spike-rewind peaks`, in
its own process group, and every other process in the group, every git, is sampled every 5 ms:
`phys_footprint` on macOS and RSS on Linux, which counts the pack files fast-import maps.
"Concurrent git peaks" is the largest sum, over the git processes alive at one sample, of each
one's own peak so far (`ri_lifetime_max_phys_footprint`, `VmHWM`); "sampled git sum" is the
largest sum of their current memory. Both are the largest over the three first and three
unchanged takes; a process that exits between two samples loses at most its last 5 ms of growth.
The bound is 1.25 times the "ADR as written" single importer's peak on the same host. "bg" runs
the take at background priority: `taskpolicy -c utility` on macOS, `nice -n 10 ionice -c 3` on
Linux. The Linux VM's block devices use the `none` I/O scheduler, which ignores I/O priority
classes (BFQ and mq-deadline honour them), so there the idle class changed nothing.

## linux-ext4-peaks

Linux 7.2.12-orbstack-00394-gbecdd6130068 #1 SMP PREEMPT Thu Oct 22 22:17:17 UTC 2026 aarch64 GNU/Linux; git version 2.39.5; 8 CPUs

| Config | Readers | Priority | First s | Unchanged s | Concurrent git peaks MB | Sampled git sum MB | Largest git RSS MB | git at once | Prototype peak MB |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| adr | 1 | normal | 45.56 | 0.43 | 280 | 280 | 280 | 2 | 2.57 |
| recommended | 4 | normal | 5.16 | 0.49 | 325 | 325 | 325 | 4 | 2.97 |
| recommended | 2 | normal | 8.60 | 0.42 | 325 | 325 | 325 | 2 | 2.68 |
| recommended | 4 | bg | 4.71 | 0.44 | 326 | 326 | 326 | 4 | 2.87 |
| recommended | 2 | bg | 8.48 | 0.47 | 325 | 325 | 325 | 2 | 2.57 |

Bound: 1.25 × the ADR-as-written single importer's 280 MB = 350 MB.

| Config | Concurrent git peaks MB | Within bound |
| --- | --- | --- |
| recommended-4-normal | 325 | yes |
| recommended-2-normal | 325 | yes |
| recommended-4-bg | 326 | yes |
| recommended-2-bg | 325 | yes |

## linux-ext4

Linux 7.2.12-orbstack-00394-gbecdd6130068 #1 SMP PREEMPT Thu Oct 22 22:17:17 UTC 2026 aarch64 GNU/Linux; git version 2.39.5; 8 CPUs

| Corpus | Config | Covered MB | Files | First s | Unchanged s | 1% changed s | Prototype peak MB | git peak MB | Store after take MB | After repack MB | Store / covered | Repack s |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| pinfold | adr | 6 | 605 | 0.26 | 0.01 | 0.01 | 2.41 | 7 | 3 | 3 | 0.44 | 0.0 |
| pinfold | recommended | 6 | 605 | 0.06 | 0.01 | 0.02 | 2.70 | 8 | 3 | 3 | 0.47 | 0.1 |
| generated | adr | 436 | 100,002 | 21.02 | 0.21 | 0.45 | 2.56 | 131 | 128 | 128 | 0.29 | 0.7 |
| generated | recommended | 436 | 100,002 | 2.46 | 0.24 | 0.35 | 2.97 | 150 | 147 | 147 | 0.34 | 5.4 |
| linux | adr | 1453 | 86,663 | 56.43 | 0.54 | 0.79 | 2.56 | 280 | 296 | 297 | 0.20 | 1.4 |
| linux | recommended | 1453 | 86,663 | 5.18 | 0.51 | 0.75 | 3.11 | 326 | 348 | 334 | 0.24 | 15.5 |

| Corpus | Tree MB (without .git) | `cp` clone s | clone du MB | clone df MB | plain `cp` s | plain du MB |
| --- | --- | --- | --- | --- | --- | --- |
| pinfold | 8 | 0.02 | 8 | 8 | 0.01 | 8 |
| generated | 1876 | 23.87 | 1876 | 1876 | 23.13 | 1876 |
| linux | 1707 | 22.89 | 1707 | 1707 | 16.15 | 1707 |

| Corpus | Default caps: complete | Files | New MB | s |
| --- | --- | --- | --- | --- |
| pinfold | True | 605 | 6 | 0.06 |
| generated | False | 61,502 | 268 | 1.52 |
| linux | False | 33,969 | 268 | 4.36 |

| Corpus | Tracked files | Index read s | First take s | Estimated first take from `.git/index` s | Saved s |
| --- | --- | --- | --- | --- | --- |
| pinfold | 605 | 0.00 | 0.06 | 0.01 | 0.05 |
| linux | 86,680 | 0.10 | 5.18 | 0.61 | 4.57 |

| Gate | ADR as written | Recommended |
| --- | --- | --- |
| Linux tree, first take ≤ 10 s | 56.43 s, **fail** | 5.18 s, pass |
| Linux tree, unchanged ≤ 1 s | 0.54 s, pass | 0.51 s, pass |
| Linux tree, store ≤ half of covered | 0.20, pass | 0.24, pass |
| Prototype ≤ 4 MB, every corpus | 2.56 MB, pass | 3.11 MB, pass |

## macos-apfs-peaks

Darwin 27.0.0 Darwin Kernel Version 27.0.0: Tue Aug 11 21:22:49 PDT 2026; root:xnu-13432.1.9~1/RELEASE_ARM64_T6000 arm64; git version 2.54.0 (Apple Git-157); 8 CPUs

| Config | Readers | Priority | First s | Unchanged s | Concurrent git peaks MB | Sampled git sum MB | Largest git RSS MB | git at once | Prototype peak MB |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| adr | 1 | normal | 45.99 | 0.90 | 268 | 268 | 269 | 2 | 3.03 |
| recommended | 4 | normal | 5.95 | 0.91 | 152 | 152 | 278 | 4 | 3.44 |
| recommended | 2 | normal | 10.18 | 0.92 | 92 | 91 | 277 | 2 | 3.20 |
| recommended | 4 | bg | 6.20 | 0.98 | 156 | 156 | 278 | 4 | 3.49 |
| recommended | 2 | bg | 11.83 | 0.97 | 95 | 95 | 278 | 2 | 3.16 |

Bound: 1.25 × the ADR-as-written single importer's 268 MB = 335 MB.

| Config | Concurrent git peaks MB | Within bound |
| --- | --- | --- |
| recommended-4-normal | 152 | yes |
| recommended-2-normal | 92 | yes |
| recommended-4-bg | 156 | yes |
| recommended-2-bg | 95 | yes |

## macos-apfs

Darwin 27.0.0 Darwin Kernel Version 27.0.0: Tue Aug 11 21:22:49 PDT 2026; root:xnu-13432.1.9~1/RELEASE_ARM64_T6000 arm64; git version 2.54.0 (Apple Git-157); 8 CPUs

| Corpus | Config | Covered MB | Files | First s | Unchanged s | 1% changed s | Prototype peak MB | git peak MB | Store after take MB | After repack MB | Store / covered | Repack s |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| pinfold | adr | 6 | 605 | 0.34 | 0.12 | 0.09 | 2.21 | 9 | 3 | 3 | 0.43 | 0.1 |
| pinfold | recommended | 6 | 605 | 0.15 | 0.06 | 0.12 | 2.56 | 8 | 3 | 3 | 0.46 | 0.1 |
| generated | adr | 436 | 100,002 | 20.68 | 0.40 | 0.57 | 2.33 | 133 | 136 | 129 | 0.31 | 0.5 |
| generated | recommended | 436 | 100,002 | 4.43 | 0.43 | 0.54 | 2.65 | 152 | 149 | 148 | 0.34 | 3.9 |
| linux | adr | 1453 | 86,663 | 46.29 | 1.02 | 1.16 | 3.02 | 247 | 304 | 297 | 0.21 | 1.2 |
| linux | recommended | 1453 | 86,663 | 7.25 | 0.98 | 1.13 | 3.47 | 278 | 357 | 337 | 0.25 | 10.5 |

| Corpus | Tree MB (without .git) | `cp` clone s | clone du MB | clone df MB | plain `cp` s | plain du MB |
| --- | --- | --- | --- | --- | --- | --- |
| pinfold | 8 | 0.22 | 8 | 0 | 0.29 | 8 |
| generated | 1867 | 72.50 | 1867 | 98 | 158.34 | 1867 |
| linux | 1683 | 22.02 | 1683 | 34 | 47.05 | 1683 |

| Corpus | Default caps: complete | Files | New MB | s |
| --- | --- | --- | --- | --- |
| pinfold | True | 605 | 6 | 0.15 |
| generated | False | 61,502 | 268 | 4.09 |
| linux | False | 33,969 | 268 | 2.30 |

| Corpus | Tracked files | Index read s | First take s | Estimated first take from `.git/index` s | Saved s |
| --- | --- | --- | --- | --- | --- |
| pinfold | 605 | 0.03 | 0.15 | 0.09 | 0.06 |
| linux | 86,680 | 0.08 | 7.25 | 1.06 | 6.20 |

| Gate | ADR as written | Recommended |
| --- | --- | --- |
| Linux tree, first take ≤ 10 s | 46.29 s, **fail** | 7.25 s, pass |
| Linux tree, unchanged ≤ 1 s | 1.02 s, **fail** | 0.98 s, pass |
| Linux tree, store ≤ half of covered | 0.21, pass | 0.25, pass |
| Prototype ≤ 4 MB, every corpus | 3.02 MB, pass | 3.47 MB, pass |
