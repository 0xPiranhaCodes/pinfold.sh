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

| Method        | Needs                    | Schema                                         |
| ------------- | ------------------------ | ---------------------------------------------- |
| `window.open` | Any `hello`              | Takes nothing, returns `{}`                    |
| `pane.list`   | Version 2 and a terminal | [`pane.list.schema.json`](pane.list.schema.json) |
| `pane.wait`   | Version 2, in a pane     | [`pane.wait.schema.json`](pane.wait.schema.json) |
| `events.subscribe` | Version 2 and a terminal | [`events.subscribe.schema.json`](events.subscribe.schema.json) |

`pane.wait` waits on the caller's own pane for at most 10 minutes, and the core parks at most 64
waits. `events.subscribe` sends `event` notifications on its connection. A connection with a
wait or a subscription stays open past the usual limit, and is closed if 2 MiB of replies
queue unread.

`pinfold ctl list` prints `pane.list` as plain text, with `*` by the caller's own pane, and
`pinfold ctl list --json` prints its result. A core that refuses version 2 is older than the
client, and `pinfold ctl` says to quit PinFold so that a new core starts.
