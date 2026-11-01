#!/bin/sh
# Runs inside the container. Types into a real shell under X11, and starts the window on Wayland.
set -eu
cargo build --quiet --locked --release --package pinfold-cli --bin pinfold
PINFOLD=/tmp/target/release/pinfold
export SHELL=/bin/bash
pss() { awk '/^Pss:/ { printf "%.2f", $2 * 1024 / 1000000 }' "/proc/$1/smaps_rollup"; }

echo "fonts installed: $(find /usr/share/fonts -type f 2>/dev/null | wc -l)"

Xvfb :9 -screen 0 1280x800x24 >/dev/null 2>&1 &
XPID=$!
sleep 1
export DISPLAY=:9
"$PINFOLD" --size 1000x640 &
WINDOW=$!
sleep 2
ID=$(xdotool search --name '^Pinfold$' | head -1)
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
echo "X11 PSS at 1000x640: $(pss "$WINDOW") MB"
xdotool key ctrl+d
sleep 1
if kill -0 "$WINDOW" 2>/dev/null; then
  echo "FAIL: window outlived its shell"
  exit 1
fi
echo "window exited with its shell"
kill "$XPID"

export XDG_RUNTIME_DIR=/tmp/xdg
mkdir -p -m 700 "$XDG_RUNTIME_DIR"
weston --backend=headless --width=1920 --height=1080 --socket=wl-window --idle-time=0 >/tmp/weston.log 2>&1 &
WESTON=$!
sleep 2
env -u DISPLAY WAYLAND_DISPLAY=wl-window "$PINFOLD" --size 1920x1080 &
WINDOW=$!
sleep 3
if ! kill -0 "$WINDOW" 2>/dev/null; then
  echo "FAIL: window did not start on Wayland"
  exit 1
fi
echo "Wayland PSS at 1920x1080: $(pss "$WINDOW") MB (budget 24.59)"
kill "$WINDOW" "$WESTON"
