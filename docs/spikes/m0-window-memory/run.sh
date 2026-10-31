#!/bin/sh
# Reproduces results.md. Needs cargo, and Docker for the Linux runs.
# Usage: docs/spikes/m0-window-memory/run.sh
# On macOS this opens windows on the display for about two minutes.
set -eu
cd "$(dirname "$0")"

TARGET="${TMPDIR:-/tmp}/pinfold-spike-window"
cargo build --quiet --release --target-dir "$TARGET"
"$TARGET/release/measure"
echo
docker build -q -t pinfold-spike-window . >/dev/null
docker run --rm --user 1000:1000 -v "$PWD":/spike:ro -w /spike \
  -e HOME=/tmp -e CARGO_HOME=/tmp/cargo pinfold-spike-window sh linux.sh
