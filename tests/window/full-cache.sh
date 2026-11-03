#!/bin/sh
# Stands in for the shell in `cargo xtask mem-gate`: pages through all of heavy-screen.txt, a
# screen at a time, so the window draws more glyphs than its cache holds, then waits.
# Text printed before the window sizes the PTY would land in scrollback, not on the screen.
until [ "$(stty size)" != "24 80" ]; do sleep 0.1; done
sleep 1
rows=$(stty size | cut -d' ' -f1)
columns=$(stty size | cut -d' ' -f2)
text="$(dirname "$0")/heavy-screen.txt"
lines=$(wc -l < "$text")
# Lines wrap, so every glyph is drawn. Each is 384 columns wide, and a wide glyph at the edge can
# take one more row, so a page never scrolls a line off before it is drawn.
page=$(((rows - 1) / (384 / columns + 2)))
[ "$page" -ge 1 ] || page=1
start=1
while [ "$start" -le "$lines" ]; do
  printf '\033[H\033[2J'
  sed -n "${start},$((start + page - 1))p" "$text"
  start=$((start + page))
  # Long enough for the window to draw this page.
  sleep 0.2
done
exec sleep 3600
