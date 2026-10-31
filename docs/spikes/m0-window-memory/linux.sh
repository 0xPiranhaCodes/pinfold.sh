#!/bin/sh
# Runs inside the container: measures X11 under Xvfb and Wayland under headless Weston.
set -eu
cargo build --quiet --release --target-dir /tmp/target
MEASURE=/tmp/target/release/measure

Xvfb :9 -screen 0 1920x1080x24 >/dev/null 2>&1 &
XPID=$!
sleep 1
env -u WAYLAND_DISPLAY DISPLAY=:9 COMPOSITOR_PID=$XPID SESSION="X11 (Xvfb)" "$MEASURE"
kill $XPID

export XDG_RUNTIME_DIR=/tmp/xdg
mkdir -p -m 700 "$XDG_RUNTIME_DIR"
weston --backend=headless --width=1920 --height=1080 --socket=wl-spike --idle-time=0 >/tmp/weston.log 2>&1 &
WPID=$!
sleep 2
echo
env -u DISPLAY WAYLAND_DISPLAY=wl-spike COMPOSITOR_PID=$WPID SESSION="Wayland (headless Weston)" "$MEASURE"
kill $WPID
