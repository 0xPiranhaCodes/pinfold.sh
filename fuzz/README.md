# Fuzz targets

This folder stays outside the workspace because fuzzing needs a nightly toolchain and
`libfuzzer-sys` compiles C++. See `CONTRIBUTING.md` for how to run it.

`vt_input` plays bytes through a pane's terminal with resizes between reads. It fails on a
panic, an out-of-memory or a hang, if the scrollback passes its cap, or if a stored block
stops decoding.

`paste` runs any text through the paste sanitizer. It fails if a control character other
than tab or a line end comes out, or if the brackets are not exactly at the two ends.

`link` feeds any bytes in chunks to the decoders of the window link, after a good version
check. It fails on a panic, an out-of-memory or a hang, if a decoded message does not encode
back to the bytes it came from, or if an applied frame's cells do not fit its size.

`session_snapshot` parses and checks any bytes as a saved session, as a core does at start. It
fails on a panic, an out-of-memory or a hang, or if a snapshot that passed its check does not
rebuild into a session.

`config` parses any bytes as the user config file. It fails on a panic, an out-of-memory or
a hang. Its seeds in `seeds/config/` are the examples in `docs/configuration.md`.

`tool_keys` decodes any bytes as the keys a tool reads, once whole and once a byte at a time.
It fails on a panic or a hang, or if the two give different keys.

`vt.dict` holds the escape sequences a mutator rarely writes by chance. It uses libFuzzer's
format, which only knows the escapes `\\`, `\"` and `\xNN`.
