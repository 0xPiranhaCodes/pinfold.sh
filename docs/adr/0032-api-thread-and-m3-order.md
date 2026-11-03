# ADR-0032: The API thread and the order of M3

- **Status:** Accepted
- **Date:** 2026-11-03

## Context

M2 left a control socket that serves one peer at a time, for at most 2 seconds and 8 KiB,
and takes only `hello` and `window.open` (ADR-0028). M3 turns it into the agent API
(ADR-0009). That needs connections that stay: `pane.wait` blocks for minutes,
`events.subscribe` streams, and an MCP server holds one connection for its whole life. A serial
loop would let one waiting caller stall every other caller.

Identity is settled: callers are mapped from their peer pid to a controlling terminal and
session, and there are no tokens (ADR-0004, ADR-0020, the threat model). Panes, their PTYs and
their screens live on the reactor's thread, and only that thread spawns (see
`crates/pinfold-cli/src/bin/pinfold-core.rs`). ADR-0007 allows one API thread and forbids an async
runtime.

## Decision

1. **One API thread with its own `mio` poll.** It accepts, reads and writes every API
   connection without blocking. Connections are capped at 16 in total, and at 4 per caller
   identity, with all unidentified callers sharing one cap of 4. Past a cap a new peer is closed
   at once, before anything is read. A connection with no request in flight, wait or
   subscription for 60 seconds is closed, so idle peers cannot hold the slots.
2. **Identity at accept, on the API thread.** It checks the peer UID, then reads the peer pid
   (`SO_PEERCRED` on Linux, `LOCAL_PEERPID` on macOS), and the pid's controlling terminal and
   session. Where the kernel has `SO_PEERPIDFD`, the core takes that descriptor too and only
   uses it to check that the process it pins is still alive after the lookup, so a reused pid
   is refused; the descriptor is never read as a pid. That pair is fixed for the
   connection. The reactor maps it to a pane on each request, so a pane that closes stops
   matching. A caller with no terminal is unidentified (ADR-0020 point 3).
3. **The reactor answers.** The API thread parses and checks a request's shape, then sends
   it with the caller's identity over a bounded channel and wakes the reactor. The reactor
   checks scope, acts, and replies over a second bounded channel, waking the API thread. A full
   channel refuses the request with "busy" rather than queueing. Nothing on the API thread
   reads a pane.
4. **Waits live in the reactor** as parked requests with a deadline, at most 64 at a time. A
   connection that closes drops its waits.
5. **Framing stays newline-delimited JSON-RPC 2.0.** A line may be up to 1 MB, 10^6 bytes,
   as the threat model's limits say. A connection's read buffer grows to the line it holds and shrinks after it, so idle
   connections cost little. Writes queue per connection with a 2 MiB cap, and a peer that will
   not read past the cap is closed.
6. **`hello` version 2** replies with the protocol version, the methods this caller may call
   and the caller's pane id, or null when unidentified. The core still takes version 1, which
   allows only `window.open`, for the two minor versions ADR-0009 gives a deprecation. The
   launcher keeps sending version 1, so a new `pinfold` still opens a window on a core started
   by an older one. A client that needs version 2, such as `pinfold ctl`, tells a person whose
   core refuses it to quit PinFold so that a new core starts.
7. **`pinfold ctl` is a subcommand of `pinfold`** and a thin client. It prints plain text by
   default and JSON with `--json`. `pinfold mcp` is a thin client too, but its stdout carries
   only the MCP protocol, and anything for a person goes to stderr.

M3 lands in this order, each step a PR of its own:

1. The API thread on `mio`, with `hello` and `window.open` unchanged in behaviour.
2. Caller identity at accept, with the pane mapping, tested from inside and outside a pane.
3. `hello` version 2, `pane.list` and `pinfold ctl list`.
4. `pane.read`, `pane.last_output` and `pane.send` for the `self` scope, with the limits.
5. `pane.spawn` and the `children` scope.
6. `pane.wait` and `events.subscribe`.
7. Approvals drawn in the window, protected panes, newline splitting and redaction.
8. The audit log, `pinfold mcp`, `pane.status` and alerts, the sandbox, and API fuzzing.

## Consequences

The core gains one thread's poll and up to 16 connection buffers. An idle connection holds
only its small buffers, but a peer that sends a 1 MB line holds 1 MB until the line ends, so
16 such peers could hold about 16 MB plus their write queues. The memory gate measures the
idle case. A later step can lower the line cap if no method needs it.

Steps 1 to 3 touch the same files, so they run one after another. From step 4 on, methods
can be built in parallel because each is its own handler in the reactor.

Revisit point 1 if 16 connections is too few for an orchestrator with many MCP servers, and
point 2 if `LOCAL_PEERTOKEN` turns out to be needed on macOS.
