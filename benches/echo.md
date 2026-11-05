# Keystroke-to-echo latency (issue #218)

`cargo xtask latency` runs [`echo.rs`](echo.rs) three times against a `release-core` build of
`pinfold-core`, 200 s each, and fails if a case misses the budget in
`docs/design/architecture.md`: p99 ≤ 16.7 ms, one 60 Hz frame, and no keystroke over 50 ms.

## What it measures

The probe is the core's window, on the real link, and its pane's program. Every 20 ms it sends a
letter and a carriage return as `Input`, and times until the pane's first cell in the frames the
core sends back shows that letter. The pane echoes in raw mode, as a shell's line editor does.
The time covers the link both ways, the reactor, the PTY and the VT engine, and not drawing.

- **Saves.** The pane moves to a new directory every second, so every save the core takes a
  minute after activity writes a new snapshot: three per 200 s run. The `syncs` column counts
  every `fsync` and `F_FULLFSYNC` the core made, including the save when the window closes after
  the last sample.
- **Slow storage.** On macOS the core loads [`slow_fsync.c`](slow_fsync.c) with
  `DYLD_INSERT_LIBRARIES`. It sleeps a fixed `--fsync-ms` before each sync, so a run repeats on
  any Mac, where a throttled disk image would depend on the disk underneath it. Syncs are where
  slow storage stalls a writer. 100 ms stands in for a slow disk; `--fsync-ms` takes any value.
- **API traffic.** With `--api` the pane keeps three connections, one under the per-caller cap,
  reading itself with `pane.read` of 200 lines as fast as the core answers.

One case alone: `cargo build --profile release-core -p pinfold-cli --bin pinfold-core`, then
`cargo run --release -p pinfold-benches --bin echo -- --core target/release-core/pinfold-core
--fsync-ms 100 --seconds 200`.

## Results

Apple M1 Pro, macOS 27.0.1, 2026-11-05, milliseconds.

Before: `Snapshot::write` ran on the reactor's thread.

| fsync delay ms | API traffic | session.save calls | samples | p50 ms | p99 ms | max ms | syncs | API reads/s | budget |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 0 | no | 0 | 7423 | 0.31 | 1.71 | 17.48 | 8 | 0 | met |
| 100 | no | 0 | 7346 | 0.35 | 2.17 | 227.54 | 7 | 0 | missed |
| 0 | yes | 0 | 7134 | 0.07 | 0.14 | 14.74 | 8 | 63163 | met |

After: `Saver` hands each snapshot to a save thread.

| fsync delay ms | API traffic | session.save calls | samples | p50 ms | p99 ms | max ms | syncs | API reads/s | budget |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 0 | no | 0 | 7410 | 0.34 | 1.80 | 10.47 | 8 | 0 | met |
| 100 | no | 0 | 7408 | 0.31 | 2.65 | 12.98 | 6 | 0 | met |
| 0 | yes | 0 | 6980 | 0.07 | 0.12 | 0.20 | 8 | 66818 | met |

A save on slow storage stalled every pane for its two syncs, about 2 × 100 ms plus the write,
so the keystroke that waited behind it took 227.54 ms. On the save thread the same case keeps its
max at 12.98 ms. The API drain met the budget in both builds at about 65,000 reads a second, so
it has no per-turn budget. The p99 is lower under API traffic than at rest; the probe does not
show why.

`session.save` still writes on the reactor's thread: the person asked and approved it, and the
reply carries the path. It first drops any snapshot still waiting and waits out a write under
way, so two writes never overlap. It therefore still stalls every pane on slow storage, and misses
the budget. One 60 s run with a call every 5 s, on the same Mac on 2026-11-06:

`cargo build --profile release-core -p pinfold-cli --bin pinfold-core --bin pinfold`, then
`cargo run --release -p pinfold-benches --bin echo -- --core target/release-core/pinfold-core
--fsync-ms 100 --session-save target/release-core/pinfold --seconds 60`

| fsync delay ms | API traffic | session.save calls | samples | p50 ms | p99 ms | max ms | syncs | API reads/s | budget |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 100 | no | 11 | 2103 | 0.24 | 6.18 | 239.08 | 24 | 0 | missed |

Moving it to the save thread too would mean parking the request until the write ends, as
`pane.wait` parks, and replying then; that is a change to the request path, left for its own PR. `cargo xtask latency` leaves this case out of its gate.
