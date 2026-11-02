#!/bin/sh
# Runs inside the container. Types into a real shell under X11, and starts the window on Wayland.
set -eu
cargo build --quiet --locked --release --package pinfold-cli \
  --bin pinfold --bin pinfold-core --bin pinfold-window
PINFOLD=/tmp/target/release/pinfold
CORE=/tmp/target/release/pinfold-core
export SHELL=/bin/bash
export XDG_RUNTIME_DIR=/tmp/xdg
mkdir -p -m 700 "$XDG_RUNTIME_DIR"
pss() { awk '/^Pss:/ { printf "%.2f", $2 * 1024 / 1000000 }' "/proc/$1/smaps_rollup"; }
# The X11 window and its process, once it shows. A hardened process still gives its parent.
window_id() {
  tries=0
  until ID=$(xdotool search --name '^Pinfold$' | head -1) && [ -n "$ID" ]; do
    tries=$((tries + 1))
    [ "$tries" -lt 50 ] || { echo "FAIL: no window"; exit 1; }
    sleep 0.1
  done
  WINDOW=$(xdotool getwindowpid "$ID")
}
parent() { awk '/^PPid:/ { print $2 }' "/proc/$1/status"; }
gone() { ! kill -0 "$1" 2>/dev/null; }

echo "fonts installed: $(find /usr/share/fonts -type f 2>/dev/null | wc -l)"

Xvfb :9 -screen 0 1280x800x24 >/dev/null 2>&1 &
XPID=$!
sleep 1
export DISPLAY=:9
"$PINFOLD"
window_id
sleep 2
DAEMON=$(parent "$WINDOW")
xdotool windowfocus "$ID"
sleep 0.3
xdotool type --delay 15 'echo sum-$((40+2))'
xdotool key Return
sleep 0.5
# shellcheck disable=SC2016
xdotool type --delay 15 'printf "\033[31mred \033[42mgreen\033[0m \033[7minverse\033[0m\n┌──┬──┐\n│ab│cd│\n├──┼──┤\n└──┴──┘\n"'
xdotool key Return
sleep 0.5
import -window root /out/x11.png
# shellcheck disable=SC2016
xdotool type --delay 15 'stty -echo -icanon; printf "\033[?1h"; head -c 17 | od -An -c > /tmp/keys; printf "\033[?1l"; stty sane'
xdotool key Return
sleep 0.5
xdotool key Up F5 ctrl+Left shift+Tab
sleep 0.5
keys=$(tr -s ' ' < /tmp/keys | tr -d '\n')
if [ "$keys" != ' 033 O A 033 [ 1 5 ~ 033 [ 1 ; 5 D 033 [ Z' ]; then
  echo "FAIL: keys reached the program as: $keys"
  exit 1
fi
echo "keys reached the program"
# shellcheck disable=SC2016
xdotool type --delay 15 'stty -echo -icanon; printf "\033[?1000h\033[?1006h"; head -c 38 | od -An -c > /tmp/mouse; printf "\033[?1000l\033[?1006l"; stty sane'
xdotool key Return
sleep 0.5
# Below the top bar, in the pane's first row.
xdotool mousemove --window "$ID" 21 70 click 1 click 4
sleep 0.5
mouse=$(tr -d ' \n' < /tmp/mouse)
# Two wheel reports: winit counts a synthetic wheel button's press and its release.
if [ "$mouse" != '033[<0;1;1M033[<0;1;1m033[<64;1;1M033[<64;1;1M' ]; then
  echo "FAIL: the mouse reached the program as: $mouse"
  exit 1
fi
echo "the mouse reached the program"
# A hostile paste: it tries to end its brackets early. Cancelled once, then confirmed.
printf 'a\033[201~b\nc' | xclip -selection clipboard >/dev/null 2>&1
# shellcheck disable=SC2016
xdotool type --delay 15 'stty -echo -icanon; printf "\033[?2004h"; head -c 21 | od -An -c > /tmp/paste; printf "\033[?2004l"; stty sane'
xdotool key Return
sleep 0.5
xdotool key ctrl+shift+v
sleep 0.5
xdotool key Escape ctrl+shift+v
sleep 0.5
import -window root /out/paste.png
xdotool key Return
sleep 0.5
paste=$(tr -d ' \n' < /tmp/paste)
if [ "$paste" != '033[200~a[201~b\nc033[201~' ]; then
  echo "FAIL: the paste reached the program as: $paste"
  exit 1
fi
echo "the paste reached the program as text, after confirmation"
"$PINFOLD"
sleep 0.5
if [ "$(xdotool search --name '^Pinfold$' | wc -l)" != 1 ] || gone "$WINDOW"; then
  echo "FAIL: a second pinfold did not keep to one window"
  exit 1
fi
echo "a second pinfold kept to one window"
# shellcheck disable=SC2016
xdotool type --delay 15 'echo $$ > /tmp/before; seq 1 100'
xdotool key Return
sleep 0.5
kill -9 "$WINDOW"
sleep 0.5
if gone "$DAEMON"; then
  echo "FAIL: the core died with its window"
  exit 1
fi
"$PINFOLD"
window_id
sleep 2
xdotool windowfocus "$ID"
sleep 0.3
import -window root /out/reattached.png
# shellcheck disable=SC2016
xdotool type --delay 15 'echo $$ > /tmp/after'
xdotool key Return
sleep 0.5
if [ ! -s /tmp/after ] || ! cmp -s /tmp/before /tmp/after; then
  echo "FAIL: a new window did not find the same shell"
  exit 1
fi
echo "a killed window left the pane running, and pinfold reattached"
xdotool key ctrl+d
sleep 1
if ! gone "$WINDOW" || ! gone "$DAEMON"; then
  echo "FAIL: window or core outlived the shell"
  exit 1
fi
echo "window and core exited with the shell"
# Tabs and splits, with real keys.
"$PINFOLD"
window_id
sleep 2
DAEMON=$(parent "$WINDOW")
xdotool windowfocus "$ID"
sleep 0.3
shell_in() {
  # shellcheck disable=SC2016
  xdotool type --delay 15 "echo \$\$ > /tmp/$1; stty size > /tmp/$1.size; clear; echo $1"
  xdotool key Return
  sleep 0.5
  cat "/tmp/$1"
}
LEFT=$(shell_in left)
xdotool key ctrl+a bar
sleep 1
RIGHT=$(shell_in right)
if [ -z "$LEFT" ] || [ -z "$RIGHT" ] || [ "$LEFT" = "$RIGHT" ]; then
  echo "FAIL: a split did not start its own shell: $LEFT $RIGHT"
  exit 1
fi
if cmp -s /tmp/left.size /tmp/right.size; then
  echo "FAIL: a split did not resize its pane: $(cat /tmp/right.size)"
  exit 1
fi
xdotool key ctrl+a minus
sleep 1
BELOW=$(shell_in below)
xdotool key ctrl+a c
sleep 1
TAB=$(shell_in tab)
xdotool key ctrl+a 1
sleep 1
AGAIN=$(shell_in again)
if [ "$AGAIN" != "$BELOW" ] || [ "$TAB" = "$BELOW" ]; then
  echo "FAIL: going back to the first tab did not reach its focused pane: $AGAIN $BELOW $TAB"
  exit 1
fi
xdotool key ctrl+a Up
sleep 0.5
if [ "$(shell_in up)" != "$RIGHT" ]; then
  echo "FAIL: focus did not move up"
  exit 1
fi
import -window root /out/splits.png
echo "a split and a tab opened with real keys, each with its own shell"
# The + button, a shell that exits, a tab's box, and a pane, with the window at 1000x640.
xdotool mousemove --window "$ID" 240 45 click 1
sleep 1
PLUS=$(shell_in plus)
if [ "$PLUS" = "$TAB" ] || [ "$PLUS" = "$RIGHT" ]; then
  echo "FAIL: the + button did not open a tab"
  exit 1
fi
xdotool type --delay 15 'exit'
xdotool key Return
sleep 1
if ! gone "$PLUS"; then
  echo "FAIL: a shell did not exit"
  exit 1
fi
xdotool mousemove --window "$ID" 100 45 click 1
sleep 1
xdotool mousemove --window "$ID" 100 300 click 1
sleep 1
if [ "$(shell_in clicked)" != "$LEFT" ]; then
  echo "FAIL: clicks did not reach the first tab's left pane"
  exit 1
fi
xdotool mousemove --window "$ID" 700 100 click 1
sleep 1
if [ "$(shell_in up)" != "$RIGHT" ]; then
  echo "FAIL: a click did not focus a pane"
  exit 1
fi
echo "clicks open a tab, pick a tab and focus a pane"
# A hidden tab that prints gets its mark, with the pointer resting on it.
xdotool key ctrl+a 2
sleep 0.5
xdotool type --delay 15 '(sleep 2; echo news) &'
xdotool key Return
sleep 0.5
xdotool key ctrl+a 1
xdotool mousemove --window "$ID" 150 45
sleep 3
import -window root /out/states.png
xdotool mousemove --window "$ID" 700 100
sleep 0.5
xdotool windowsize "$ID" 800 500
sleep 1
shell_in small >/dev/null
if cmp -s /tmp/up.size /tmp/small.size; then
  echo "FAIL: a window resize did not reach the pane: $(cat /tmp/small.size)"
  exit 1
fi
echo "a window resize reached the panes: $(cat /tmp/up.size) to $(cat /tmp/small.size)"
# shellcheck disable=SC2016
xdotool type --delay 15 'stty -echo -icanon; head -c 1 | od -An -c > /tmp/doubled; stty sane'
xdotool key Return
sleep 0.5
xdotool key ctrl+a ctrl+a
sleep 0.5
if [ "$(tr -d ' \n' < /tmp/doubled)" != '001' ]; then
  echo "FAIL: the prefix twice reached the program as: $(cat /tmp/doubled)"
  exit 1
fi
echo "the prefix twice reached the program"
xdotool key ctrl+a x
sleep 0.5
xdotool key Escape
sleep 0.5
if gone "$RIGHT"; then
  echo "FAIL: a cancelled prompt closed the pane"
  exit 1
fi
xdotool key ctrl+a x
sleep 0.5
xdotool key Return
sleep 1
if ! gone "$RIGHT"; then
  echo "FAIL: closing a pane left its shell"
  exit 1
fi
echo "x asks first, and closing a pane ends its shell"
# Close what is left, the last by the prompt that says it ends the session.
xdotool key ctrl+a x Return
sleep 1
xdotool key ctrl+a x Return
sleep 1
xdotool key ctrl+a x
sleep 0.5
import -window root /out/last.png
if gone "$TAB" || gone "$DAEMON"; then
  echo "FAIL: the session ended before the last pane closed"
  exit 1
fi
xdotool key Return
sleep 1
if ! gone "$TAB" || ! gone "$WINDOW" || ! gone "$DAEMON"; then
  echo "FAIL: closing the last pane left the session running"
  exit 1
fi
echo "closing the last pane ended the session"
"$PINFOLD"
window_id
sleep 2
DAEMON=$(parent "$WINDOW")
import -window root /tmp/alive.png
kill -9 "$DAEMON"
sleep 1
import -window root /out/ended.png
if gone "$WINDOW"; then
  echo "FAIL: the window died with the core"
  exit 1
fi
if compare -metric AE /tmp/alive.png /out/ended.png null: 2>/dev/null; then
  echo "FAIL: the window did not say the session ended"
  exit 1
fi
echo "a killed core left the window up, saying so"
kill "$WINDOW"
sleep 0.5
# Measured apart from the rest, as a hardened window cannot be read from /proc.
"$CORE" --window 1 1000x640 >/tmp/measured &
MEASURED=$!
sleep 3
WINDOW=$(cat /tmp/measured)
echo "X11 PSS at 1000x640: $(pss "$WINDOW") MB"
kill -9 "$MEASURED" "$WINDOW"
kill "$XPID"

weston --backend=headless --width=1920 --height=1080 --socket=wl-window --idle-time=0 >/tmp/weston.log 2>&1 &
WESTON=$!
sleep 2
env -u DISPLAY WAYLAND_DISPLAY=wl-window "$CORE" --window 1 1920x1080 >/tmp/measured &
MEASURED=$!
sleep 3
WINDOW=$(cat /tmp/measured)
if gone "$WINDOW"; then
  echo "FAIL: window did not start on Wayland"
  exit 1
fi
echo "Wayland PSS at 1920x1080: $(pss "$WINDOW") MB (budget 24.59)"
kill -9 "$MEASURED" "$WINDOW"
kill "$WESTON"
