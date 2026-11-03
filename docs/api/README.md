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
| `audit.read`       | Version 2, outside any pane | [`audit.read.schema.json`](audit.read.schema.json)           |

For now a caller reads and sends only to the pane it runs in, the `self` scope, and any other
pane is refused. Reads return at most 2,000,000 bytes, keeping the newest. A send carries at
most 4096 bytes, and a pane takes at most 20 sends a second.

`pane.wait` waits on the caller's own pane for at most 10 minutes, and the core parks at most 64
waits. `events.subscribe` sends `event` notifications on its connection. A connection with a
wait or a subscription stays open past the usual limit, and is closed if 2 MiB of replies
queue unread.

The core records each request it answers in a hash-chained audit log: when, the caller's pane
(or outside a pane, or unidentified), the method, the target pane, and whether it was allowed,
with a short reason. It never records pane contents or sent text, only a send's size in bytes.
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
<text>` types into a pane. A core that refuses version 2 is older than the client, and
`pinfold ctl` says to quit PinFold so that a new core starts.
