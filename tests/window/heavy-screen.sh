#!/bin/sh
# Stands in for the shell in `cargo xtask mem-gate`: draws a heavy screen, then waits.
# heavy-screen.txt is plain, bold and italic ASCII with hanzi, kanji, kana and hangul taken in
# order from GB 2312, JIS X 0208 and KS X 1001, so each screen holds many different glyphs.
# Text printed before the window sizes the PTY would land in scrollback, not on the screen.
until [ "$(stty size)" != "24 80" ]; do sleep 0.1; done
sleep 1
rows=$(stty size | cut -d' ' -f1)
# The lines are wider than any screen in the gate, so stop them wrapping.
printf '\033[?7l'
head -n "$((rows - 1))" "$(dirname "$0")/heavy-screen.txt"
exec sleep 3600
