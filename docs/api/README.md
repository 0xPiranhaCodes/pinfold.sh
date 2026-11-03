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
| `pane.read`        | Version 2 and a caller pane | [`pane.read.schema.json`](pane.read.schema.json)             |
| `pane.last_output` | Version 2 and a caller pane | [`pane.last_output.schema.json`](pane.last_output.schema.json) |
| `pane.send`        | Version 2 and a caller pane | [`pane.send.schema.json`](pane.send.schema.json)             |
| `pane.wait`        | Version 2 and its own pane  | [`pane.wait.schema.json`](pane.wait.schema.json)             |
| `events.subscribe` | Version 2 and a terminal    | [`events.subscribe.schema.json`](events.subscribe.schema.json) |
| `grants.list`      | Version 2, outside panes    | [`grants.list.schema.json`](grants.list.schema.json)         |
| `grants.revoke`    | Version 2, outside panes    | [`grants.revoke.schema.json`](grants.revoke.schema.json)     |

A caller reads and sends to its own pane under the `self` scope. Another pane parks the call for
up to 60 seconds and asks in trusted window chrome. `y` allows the batch once, `a` grants that
caller, target and action for 30 minutes, and `n` or Esc denies it. Keys count only after the
prompt has been shown for half a second. Related calls share a prompt, which takes a new id as
each joins, so a key pressed for the smaller batch approves nothing. The prompt shows every send
text whole with its byte length; one too long for the window offers only denial.
Without an open window the call is refused. Cross-pane reads are redacted. Reads return at most
2,000,000 bytes, keeping the newest. A send carries at most 4096 bytes, and a pane takes at most
20 sends a second.

`grants.list` and `grants.revoke` are accepted only from an identified terminal outside every
pane. Listing is read-only and revocation only removes authority, so neither raises another
approval prompt. `pinfold ctl grants [--json]` lists lasting grants and `pinfold ctl grants
revoke <id>` revokes one.

A connection takes many requests, answered one at a time in order, each line at most 32 KiB.
It must send `hello` within 2 seconds of connecting and end each line within 2 seconds of its
first byte. It is closed after 60 seconds with no request in flight, wait or subscription, and
the time starts again after each answer. A malformed request closes it, and so does a request
the core took too long to answer; a refusal, such as "the core is busy", does not.

`pane.wait` waits on the caller's own pane for at most 10 minutes, and the core parks at most 64
waits. `events.subscribe` sends `event` notifications on its connection. A connection is closed
if 2 MiB of replies queue unread, or if the core has more events for it than it can queue, so a
subscriber that was closed may have missed events.

`pinfold ctl list` prints `pane.list` as plain text, with `*` by the caller's own pane, and
`pinfold ctl list --json` prints its result. Its title, directory and status come from what
programs print, which can fake them: read them as hints, not facts. `pinfold ctl send <pane>
<text>` types into a pane. A core that refuses version 2 is older than the client, and
`pinfold ctl` says to quit PinFold so that a new core starts.
