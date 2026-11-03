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
