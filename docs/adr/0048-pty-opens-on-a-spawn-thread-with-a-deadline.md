# ADR-0048: PTYs open on a spawn thread, with a deadline

- **Status:** Accepted (approved by the maintainer on 2026-11-10)
- **Date:** 2026-11-10

## Context

On macOS `grantpt` can stay in the kernel forever
([#392](https://github.com/0xPiranhaCodes/pinfold.sh/issues/392)). `TIOCPTYGRANT` returns
`ERESTART` while the replica's devfs entry is missing, and the kernel restarts the call with
no bound; the entry stays missing for good when `ptmx_get_ioctl` fails to make `/dev/ttysNNN`.
A sample of a test hung for 2 h 42 m shows its thread in `__ioctl` under `open_pair`. Under
`crates/pinfold-core/tests/login-path-loop.sh 50 8` a core test sat 9+ minutes the same way.

`Reactor::spawn` opened each pane's PTY on the reactor thread, for the session's first panes,
a new tab and a split. One stuck grant stopped every pane, the API and the window. `grantpt`
cannot be skipped, since the replica is `root:tty 0620`, and the close order that provokes it
is not ours to choose. Killing a helper process per grant (option 2 in #392) costs a fork per
pane; the maintainer chose a thread.

[ADR-0007](0007-concurrency.md) keeps a fixed set of threads and allows a bounded pool for
blocking jobs.

## Decision

- **Where.** Only the open of a pane's PTY, `pinfold_sys::pty::open_pair` (`posix_openpt`,
  `grantpt`, `unlockpt`, `ptsname_r` and opening the replica), runs on one spawn thread,
  `pinfold-spawn`, which the core starts on its first pane. The reactor hands it a job and
  waits on a channel with `recv_timeout`. Setting the size, the fork and exec, and polling
  stay on the reactor thread, in `Pty::spawn_on`. Linux takes the same path.
- **Deadline: 5 s** (`OPEN_DEADLINE` in `crates/pinfold-core/src/opener.rs`). Twice
  `crates/pinfold-sys/tests/open-latency.sh 8`, 8 copies of 16 threads opening 300 PTYs each,
  next to 8 copies of the core's tests: no open failed, the 99th percentile was at most
  0.53 s and the slowest of 76,800 opens took 1.1 s. An earlier run of the same load saw
  2.4 s. 5 s leaves twice the slowest seen, and bounds how long the reactor stands still for
  a stuck grant.
- **A stuck thread is abandoned.** A thread in the kernel cannot be killed, so the reactor
  drops its end of the channels and leaves the thread, leaked. If its open ever returns, the
  send fails and the pair is dropped, so both descriptors close; the thread then ends. The
  next pane starts a new spawn thread.
- **Cap: 4 abandoned threads** (`MAX_ABANDONED`). While 4 are still stuck, a new pane is
  refused at once, with an error that says PTY opens are stuck in the kernel, and no thread
  is started. A stuck thread that ends frees its place.
- **What the caller sees.** The pane fails to start: `Reactor::spawn` returns `TimedOut`,
  which `pane.spawn`, a new tab and a split report as the program not starting. A restore
  logs `cannot restore the session: timed out` and the core starts fresh. No descriptor and no pane is left behind. Every
  other pane, the API and the window keep running once the deadline passes.

## Consequences

- The core has one more thread once it opens a pane: a 64 KiB stack, of which only the pages
  used are resident. Each abandoned thread keeps its stack and one PTY slot until it ends; the
  cap bounds that at 4.
- A grant that sticks still costs a 5 s pause of the reactor, at most 4 times before panes are
  refused.
- `Pty::spawn` still opens on the calling thread, for tests and callers that can wait.
- Revisit if the pause matters, which would need a pane that exists before its PTY, or if
  macOS grows a bound on `TIOCPTYGRANT`.
