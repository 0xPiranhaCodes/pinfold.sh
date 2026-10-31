#!/bin/sh
# Drives the real window in a container, and writes a screenshot to OUT.
# Usage: OUT=<directory> tests/window/run.sh
set -eu
cd "$(dirname "$0")/../.."
OUT="${OUT:-${TMPDIR:-/tmp}/pinfold-window}"
mkdir -p "$OUT"
docker build -q -t pinfold-window tests/window >/dev/null
docker run --rm --user 1000:1000 -v "$PWD":/w:ro -v "$OUT":/out -w /w \
  -e HOME=/tmp -e CARGO_HOME=/tmp/cargo -e CARGO_TARGET_DIR=/tmp/target \
  -e RUSTUP_HOME=/usr/local/rustup -e LC_ALL=C.UTF-8 pinfold-window sh tests/window/drive.sh
echo "screenshot: $OUT/x11.png"
