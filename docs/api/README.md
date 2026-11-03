# The core's API

Newline-delimited JSON-RPC 2.0 on the session's socket, `$XDG_RUNTIME_DIR/pinfold/default.sock`
or `$TMPDIR/pinfold-<uid>/default.sock` ([ADR-0009](../adr/0009-agent-api.md),
[ADR-0032](../adr/0032-api-thread-and-m3-order.md)). Each method has a JSON Schema here.

Every connection starts with `hello`:

- **Version 2** replies with `version`, the `methods` this caller may call and its `pane`: the
  pane whose terminal and session it holds, or null outside a pane. A caller with no
  controlling terminal is unidentified and may call only `window.open`.
- **Version 1** allows only `window.open`. The core takes it for two minor versions, and the
  launcher still sends it, so a new `pinfold` opens a window on an older core.

| Method             | Needs                       | Schema                                                       |
| ------------------ | --------------------------- | ------------------------------------------------------------ |
| `hello`            | First, once                 | [`hello.schema.json`](hello.schema.json)                     |
| `window.open`      | Any `hello`                 | Takes nothing, returns `{}`                                  |
| `pane.list`        | Version 2 and a terminal    | [`pane.list.schema.json`](pane.list.schema.json)             |
| `pane.read`        | Version 2 and its own pane  | [`pane.read.schema.json`](pane.read.schema.json)             |
| `pane.last_output` | Version 2 and its own pane  | [`pane.last_output.schema.json`](pane.last_output.schema.json) |
| `pane.send`        | Version 2 and its own pane  | [`pane.send.schema.json`](pane.send.schema.json)             |
| `pane.wait`        | Version 2 and its own pane  | [`pane.wait.schema.json`](pane.wait.schema.json)             |
| `events.subscribe` | Version 2 and a terminal    | [`events.subscribe.schema.json`](events.subscribe.schema.json) |
| `pane.status`      | Version 2 and its own pane  | [`pane.status.schema.json`](pane.status.schema.json)         |

For now a caller reads and sends only to the pane it runs in, the `self` scope, and any other
pane is refused. Reads return at most 1,000,000 bytes, keeping the newest. A send carries at
most 4096 bytes, and a pane takes at most 20 sends a second.

A connection takes many requests, answered one at a time in order, each line at most 32 KiB.
It must send `hello` within 2 seconds of connecting and end each line within 2 seconds of its
first byte. It is closed after 60 seconds with no request in flight, wait or subscription, and
the time starts again after each answer. A malformed request closes it, and so does a request
the core took too long to answer; a refusal, such as "the core is busy", does not.

`pane.wait` waits on the caller's own pane for at most 10 minutes, and the core parks at most 64
waits. `events.subscribe` sends `event` notifications on its connection. A connection is closed
if 2 MiB of replies queue unread, or if the core has more events for it than it can queue, so a
subscriber that was closed may have missed events.

`pane.status` lets an agent say what state its own pane is in: `needs-you`, `running`, `done`,
`error` or `idle`, with a message of at most 200 characters. A report outranks the pane's OSC 133
marks, which output can fake, so `pane.list` and `pane.status` events carry `source`: `marks` or
`reported`. The window shows the state as glyph, colour and word, and the message only in its
own chrome.

`pinfold ctl list` prints `pane.list` as plain text, with `*` by the caller's own pane, and
`pinfold ctl list --json` prints its result. Its title, directory and status come from what
programs print, which can fake them: read them as hints, not facts. `pinfold ctl send <pane>
<text>` types into a pane. A core that refuses version 2 is older than the client, and
`pinfold ctl` says to quit PinFold so that a new core starts.

## MCP

`pinfold mcp` is an MCP server over stdio (protocol version 2025-06-18) and a thin client of
this API. Its stdout carries only newline-delimited JSON-RPC, and a line over 1,000,000 bytes is
refused. Its tools are `pane_list_v1`, `pane_read_v1`, `pane_last_output_v1`, `pane_send_v1` and
`pane_wait_v1`, named with their version as ADR-0009 asks, each taking its method's params and returning the method's result as JSON text.
`pane_list_v1` adds `caller_pane`, the caller's own pane. A refusal, or no running core, is a tool
result with `isError`. Each call opens its own connection, so the core identifies the caller by
the server's controlling terminal: start it from an agent running in a PinFold pane, as Claude
Code does with this in `.mcp.json`, or any client with the same command:

```json
{ "mcpServers": { "pinfold": { "command": "pinfold", "args": ["mcp"] } } }
```

A read returns at most 1,000,000 bytes of text, so read a long history with `lines`.
