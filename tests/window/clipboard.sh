#!/bin/sh
# Runs the clipboard tests that need a display, under Xvfb with xclip as one owner (#310).
# One at a time, as they share the X server's CLIPBOARD selection.
# Usage: tests/window/clipboard.sh
set -eu
cd "$(dirname "$0")/../.."
docker build -q -t pinfold-window tests/window >/dev/null
docker run --rm --user 1000:1000 -v "$PWD":/w:ro -w /w \
  -e HOME=/tmp -e CARGO_HOME=/tmp/cargo -e CARGO_TARGET_DIR=/tmp/target \
  -e RUSTUP_HOME=/usr/local/rustup -e LC_ALL=C.UTF-8 pinfold-window \
  sh -c 'Xvfb :9 -screen 0 640x480x24 >/dev/null 2>&1 & sleep 1
    DISPLAY=:9 cargo test --locked -p pinfold-sys --features clipboard clipboard -- --nocapture --test-threads=1' 
