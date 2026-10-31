# ADR-0007: No async runtime in the core

- **Status:** Accepted
- **Date:** 2026-10-31

## Context

Every thread costs a stack and an allocator arena, and an async runtime adds memory and
complexity. Review finding H1 showed that blocking work could stall every pane, and that
one flooding pane could starve the others.

## Decision

The core uses `mio` with a fixed set of threads: one I/O thread for all PTYs, one mux
thread and one API thread. A bounded worker pool of 1–2 threads, created lazily and
exiting when idle, handles blocking jobs. Each pane gets a fair read budget per wakeup,
and a flooding pane gets backpressure.

## Consequences

Dependencies that bring an executor are excluded, which rules out `notify-rust` with
`zbus` on Linux (finding M2). `cargo-deny` enforces this ban.
