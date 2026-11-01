# Fuzz targets

This folder stays outside the workspace because fuzzing needs a nightly toolchain and
`libfuzzer-sys` compiles C++. See `CONTRIBUTING.md` for how to run it.

`vt_input` plays bytes through a pane's terminal with resizes between reads. It fails on a
panic, an out-of-memory or a hang, if the scrollback passes its cap, or if a stored block
stops decoding.

`paste` runs any text through the paste sanitizer. It fails if a control character other
than tab or a line end comes out, or if the brackets are not exactly at the two ends.

`vt.dict` holds the escape sequences a mutator rarely writes by chance. It uses libFuzzer's
format, which only knows the escapes `\\`, `\"` and `\xNN`.
