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
| `pane.read`        | Version 2, a pane in reach  | [`pane.read.schema.json`](pane.read.schema.json)             |
| `pane.last_output` | Version 2, a pane in reach  | [`pane.last_output.schema.json`](pane.last_output.schema.json) |
| `pane.send`        | Version 2, a pane in reach  | [`pane.send.schema.json`](pane.send.schema.json)             |
| `pane.wait`        | Version 2, a pane in reach  | [`pane.wait.schema.json`](pane.wait.schema.json)             |
| `pane.spawn`       | Version 2 and its own pane  | [`pane.spawn.schema.json`](pane.spawn.schema.json)           |
| `pane.close`       | Version 2, a spawned pane   | [`pane.close.schema.json`](pane.close.schema.json)           |
| `events.subscribe` | Version 2 and a terminal    | [`events.subscribe.schema.json`](events.subscribe.schema.json) |
| `pane.status`      | Version 2 and its own pane  | [`pane.status.schema.json`](pane.status.schema.json)         |
| `audit.read`       | Version 2, outside any pane | [`audit.read.schema.json`](audit.read.schema.json)           |
| `session.save`     | Version 2, outside any pane | [`session.save.schema.json`](session.save.schema.json)       |
| `workspace.open`   | Version 2, a terminal, no pane | [`workspace.open.schema.json`](workspace.open.schema.json) |
| `grants.list`      | Version 2, outside panes    | [`grants.list.schema.json`](grants.list.schema.json)         |
| `grants.revoke`    | Version 2, outside panes    | [`grants.revoke.schema.json`](grants.revoke.schema.json)     |

A pane in reach is the caller's own, the `self` scope, or one it spawned, or one they spawned in
turn, the `children` scope. `pane.spawn` opens a split or tab running the user's shell, never a
program the caller names, and a program in that pane is that pane, not its parent.

`pane.read`, `pane.last_output` and `pane.send` on any other pane park the call for up to 60
seconds and ask in trusted window chrome. `y` allows the batch once, `a` grants that caller,
target and action for 30 minutes, and `n` or Esc denies it. Allow keys count only once the prompt
has been on screen in a focused window for half a second; denial always counts. Pane text is
muted while a prompt is up, and at most 16 prompts wait: past that a new one is denied. Related calls share a prompt, which takes a new id as each joins,
so a key pressed for the smaller batch approves nothing. The prompt shows every send text whole
with its byte length; one too long for the window offers only denial. Without an open window the
call is refused. Cross-pane reads are redacted. Reads return at most 1,000,000 bytes, keeping the
newest. A send carries at most 4096 bytes, and a pane takes at most 20 sends a second.

`grants.list` and `grants.revoke` are accepted only from an identified terminal outside every
pane. Listing is read-only and revocation only removes authority, so neither raises another
approval prompt. `pinfold ctl grants [--json]` lists lasting grants and `pinfold ctl grants
revoke <id>` revokes one.

A connection takes many requests, answered one at a time in order, each line at most 32 KiB.
It must send `hello` within 2 seconds of connecting and end each line within 2 seconds of its
first byte. It is closed after 60 seconds with no request in flight, wait or subscription, and
the time starts again after each answer. A malformed request closes it, and so does a request
the core took too long to answer; a refusal, such as "the core is busy", does not.

`pane.wait` waits on a pane in reach for at most 10 minutes, and the core parks at most 64
waits. `events.subscribe` sends `event` notifications on its connection. A connection is closed
if 2 MiB of replies queue unread, or if the core has more events for it than it can queue, so a
subscriber that was closed may have missed events.

`pane.status` lets an agent say what state its own pane is in: `needs-you`, `running`, `done`,
`error` or `idle`, with a message of at most 200 characters. A report outranks the pane's OSC 133
marks, which output can fake, so `pane.list` and `pane.status` events carry `source`: `marks` or
`reported`. The window shows the state as glyph, colour and word, and the message only in its
own chrome.

The core records each request it answers in a hash-chained audit log: when, the caller's pane
(or outside a pane, or unidentified), the method, the target pane, and whether it was allowed,
with a short reason. It never records pane contents or sent text, only a send's size in bytes.
Requests the API thread answers itself are not recorded: a malformed line, an unsupported
`hello` version, and "the core is busy".
The log is a 256 KiB ring in memory, which is the source of truth; `api.audit_log` in the
[config file](../configuration.md) also writes it to disk. `audit.read` returns at most 1000 of
the newest entries and whether the chain verifies. Only a terminal outside any pane may call it,
so an agent in a pane cannot read what other agents did.

`pinfold ctl log` prints `audit.read` a line per entry, `--json` its result, `-n <count>` the
newest entries and `--pane <pane>` only calls by or to that pane. `pinfold ctl log --verify
<file>` checks a written log and its rotated `.1`, and fails if the chain is broken. A chain
cannot show entries cut from its end.

`pinfold ctl list` prints `pane.list` as plain text, with `*` by the caller's own pane, and
`pinfold ctl list --json` prints its result. Its title, directory and status come from what
programs print, which can fake them: read them as hints, not facts. `pinfold ctl send <pane>
<text>` types into a pane. `pinfold ctl session save`, from a terminal that is no pane, writes the
session's snapshot and prints its path. A core that refuses version 2 is older than the client, and
`pinfold ctl` says to quit PinFold so that a new core starts.

`pinfold up [dir]` reads `<dir>/.pinfold/layout.toml`, the current directory's by default,
starts a core and a window as `pinfold` does, and sends `workspace.open` with each pane's split
and cwd. A second run for the same directory focuses its first open tab. `run` lines are not
sent: they wait for workspace trust, and `pinfold up` says how many there are.

## MCP

`pinfold mcp` is an MCP server over stdio (protocol version 2025-06-18) and a thin client of
this API. Its stdout carries only newline-delimited JSON-RPC, and a line over 1,000,000 bytes is
refused. Its tools are `pane_list_v1`, `pane_read_v1`, `pane_last_output_v1`, `pane_send_v1`,
`pane_spawn_v1`, `pane_close_v1` and `pane_wait_v1`, named with their version as ADR-0009 asks,
each taking its method's params and returning the method's result as JSON text.
`events_next_v1` takes `events.subscribe`'s `events` and an optional `timeout_ms` (default
30,000, at most 600,000), and returns the next such event, or `{"timeout": true}`. MCP
clients rarely pass notifications to the model, so it waits rather than streams (ADR-0035), and
events between two calls are not kept.
`pane_list_v1` adds `caller_pane`, the caller's own pane. A refusal, or no running core, is a tool
result with `isError`. Each call opens its own connection, so the core identifies the caller by
the server's controlling terminal: start it from an agent running in a PinFold pane, as Claude
Code does with this in `.mcp.json`, or any client with the same command:

```json
{ "mcpServers": { "pinfold": { "command": "pinfold", "args": ["mcp"] } } }
```

A read returns at most 1,000,000 bytes of text, so read a long history with `lines`.
