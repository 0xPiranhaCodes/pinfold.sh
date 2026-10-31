#!/bin/sh
# Reproduces results.md. Needs cargo, and Docker for the Linux run.
# Usage: docs/spikes/m0-process-split/run.sh
# On macOS this opens windows on the display for about five minutes.
set -eu
cd "$(dirname "$0")"

TARGET="${TMPDIR:-/tmp}/pinfold-spike-split"
cargo build --quiet --release --target-dir "$TARGET"
"$TARGET/release/measure"
echo
docker build -q -t pinfold-spike-split . >/dev/null
docker run --rm --user 1000:1000 -v "$PWD":/spike:ro -w /spike \
  -e HOME=/tmp -e CARGO_HOME=/tmp/cargo pinfold-spike-split sh -c '
    cargo build --quiet --release --target-dir /tmp/target
    Xvfb :9 -screen 0 1920x1080x24 >/dev/null 2>&1 &
    sleep 1
    DISPLAY=:9 WINIT_UNIX_BACKEND=x11 /tmp/target/release/measure'
