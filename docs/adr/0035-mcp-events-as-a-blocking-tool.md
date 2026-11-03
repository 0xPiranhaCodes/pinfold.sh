# ADR-0035: MCP clients get events from a tool that waits

- **Status:** Accepted
- **Date:** 2026-11-03

## Context

`events.subscribe` streams pane opened, closed and status events as JSON-RPC notifications on
the caller's connection. `pinfold mcp` exposes only request-and-reply tools, so an MCP client
had to poll `pane_list_v1` to learn that a pane finished or closed (issue #140).

MCP can carry server-to-client notifications, but clients handle them unevenly: few surface a
custom notification or resource update to the model, and most coding agents act only on tool
results. A stream a client drops is an event the agent never sees.

## Decision

`pinfold mcp` adds a tool, `events_next_v1`, rather than bridging the stream into MCP
notifications. It takes `events.subscribe`'s `events` and an optional `timeout_ms`, default
30,000 and at most 600,000, `pane.wait`'s cap. Each call opens a connection, subscribes, and
returns the first event, or `{"timeout": true}`, then closes. It returns exactly what
`events.subscribe` sends that caller: pane numbers, status and exit status, never contents.

## Consequences

Any client that can call a tool can now wait for a pane to change without polling, and
`pinfold mcp` keeps its one-call-one-connection shape with no thread or state of its own.

Events between two calls are lost, as each call subscribes afresh. An agent that must not miss
one checks `pane_list_v1` after a call returns. The default stays under the 60 second tool
timeout some clients apply. Revisit if MCP clients come to surface notifications to the model
reliably, or if agents need every event in order.
