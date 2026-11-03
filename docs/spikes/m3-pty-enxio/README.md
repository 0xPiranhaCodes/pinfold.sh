# PTY opens that fail with ENXIO on macOS

CI's macOS runners failed pane spawns now and then with `Os { code: 6, "Device not
configured" }` (ENXIO), or `"The shell would not start"` where the core hides the error.
The cause is a race in XNU's `bsd/kern/tty_ptmx.c`:

1. `ptmx_clone` picks the first free slot, or the slot past the end when every slot is
   in use.
2. `ptmx_get_ioctl` drops `DEVFS_LOCK` to allocate, and grows the slot vector only if
   `pis_free == 0` when it takes the lock again.
3. If another PTY closes in between, a slot is free, the vector does not grow, the chosen
   minor is out of range, and `posix_openpt` returns ENXIO.

Opening again takes the freed slot, so `Pty::spawn` retries once on ENXIO. A table that is
truly full (`kern.tty.ptmx_max`) refuses the second open too.

## Results

`run.sh` churns PTY masters from 8 threads (160,000 opens a run). On an 8-CPU Apple
Silicon Mac, macOS 27, `ptmx_max` 511, on 2026-11-03:

| run | ENXIO without retry | ENXIO with one retry |
|-----|---------------------|----------------------|
| 1   | 2                   | 0 (1 other failure, errno not yet printed) |
| 2–4 | 0, 0, 0             | 0, 0                 |

The race is rare, as on CI. The fourth retry run was stopped by hand at load average 284.

`stress.sh` runs the whole `pinfold-core` lib suite in the background each round and the
three tests that flaked on CI in the foreground, under 6 `yes` loaders, 50 rounds. The
machine's load average was 250–330 before and 370–415 after, from other work too.

| | before | after |
|---|---|---|
| `family::…a_bad_cwd_or_a_cap_refuses_and_changes_nothing`, alone | 0/50 | 0/50 |
| `io::…a_search_that_finds_nothing_ends_and_bad_ones_never_start`, alone | 0/50 | 0/50 |
| `io::…sends_past_20_a_second_or_a_full_queue_are_refused`, alone | 0/50 | 0/50 |
| background suite rounds that failed | 1/50 (the family test, "The shell would not start", `family.rs:247`) | 0/50 |

The single tests alone did not reproduce the flake in either run; only the full suite's
PTY churn did, once. One failure in 50 is weak evidence on its own; the kernel source and
`race.c` are the stronger part.
