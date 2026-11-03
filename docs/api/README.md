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
| `session.save`     | Version 2, outside any pane | [`session.save.schema.json`](session.save.schema.json)       |

For now a caller reads and sends only to the pane it runs in, the `self` scope, and any other
pane is refused. Reads return at most 2,000,000 bytes, keeping the newest. A send carries at
most 4096 bytes, and a pane takes at most 20 sends a second.

`pinfold ctl list` prints `pane.list` as plain text, with `*` by the caller's own pane, and
`pinfold ctl list --json` prints its result. Its title, directory and status come from what
programs print, which can fake them: read them as hints, not facts. `pinfold ctl send <pane>
<text>` types into a pane. `pinfold ctl session save`, from a terminal that is no pane, writes the
session's snapshot and prints its path. A core that refuses version 2 is older than the client, and
`pinfold ctl` says to quit PinFold so that a new core starts.
