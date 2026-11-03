#!/bin/sh
# Stands in for the shell in `cargo xtask mem-gate`: pages through all of heavy-screen.txt, a
# screen at a time, so the window draws more glyphs than its cache holds, then waits.
# Text printed before the window sizes the PTY would land in scrollback, not on the screen.
until [ "$(stty size)" != "24 80" ]; do sleep 0.1; done
sleep 1
rows=$(stty size | cut -d' ' -f1)
text="$(dirname "$0")/heavy-screen.txt"
lines=$(wc -l < "$text")
printf '\033[?7l'
start=1
while [ "$start" -le "$lines" ]; do
  printf '\033[H\033[2J'
  sed -n "${start},$((start + rows - 2))p" "$text"
  start=$((start + rows - 1))
  # Long enough for the window to draw this page.
  sleep 0.5
done
exec sleep 3600
