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
back to the bytes it came from, or if an applied frame's cells do not fit its size. Its seeds in
`seeds/link/` are a palette request and a pane's colours notice, which a `pinfold-proto` test
keeps equal to what the encoder writes.

`session_snapshot` parses and checks any bytes as a saved session, as a core does at start. It
fails on a panic, an out-of-memory or a hang, or if a snapshot that passed its check does not
rebuild into a session.

`config` parses any bytes as the user config file, and again as a theme file, whose state
colours it then steps and checks. It fails on a panic, an out-of-memory or a hang. Its seeds in
`seeds/config/` are the examples in `docs/configuration.md`, and `theme.toml` is Graphite Grid
as a theme file.

`ghostty_theme` parses any bytes as a Ghostty theme file, and steps and checks the colours of
one that parses. It fails on a panic, an out-of-memory or a hang. Of its seeds in
`seeds/ghostty_theme/`, which the `pinfold-config` tests also read, `Nord` and `GitHub Dark`
are `ghostty/Nord` and `ghostty/GitHub Dark` of
[iTerm2-Color-Schemes](https://github.com/mbadolato/iTerm2-Color-Schemes) at
`bc5531d8ec26e6031c698c61eb7d82c9a9cd092d`, unchanged: MIT, Copyright (c) 2011 to Present Mark
Badolato, whose `licenses.yml` gives both schemes as MIT, from nordtheme/iterm2 (Copyright (c)
2016-present Sven Greb) and primer/github-vscode-theme (Copyright (c) 2020 Primer). The MIT
text is `LICENSE-MIT`. `other-keys` is our own.

`tool_keys` decodes any bytes as the keys a tool reads, once whole and once a byte at a time.
It fails on a panic or a hang, or if the two give different keys.

`markdown` lays out any bytes as `pinfold md` does, at a width from 1 to 300 taken from the
first two bytes. It fails on a panic or a hang, or if a laid-out line holds a control or bidi
character that `escape` would change.

`unified_diff` parses any bytes as `git diff` output, as `pinfold diff` does. It fails on a
panic or a hang, or if a file's label or a shown line holds a control or bidi character that
`escape` would change. Its seeds in `seeds/unified_diff/` are a change, a rename, a binary
file, a mode change, a missing final newline and a hostile quoted path.

`vt.dict` holds the escape sequences a mutator rarely writes by chance. It uses libFuzzer's
format, which only knows the escapes `\\`, `\"` and `\xNN`.
