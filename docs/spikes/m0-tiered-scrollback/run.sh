#!/bin/sh
# Reproduces results.md. Needs cargo, curl, and Docker for the Linux run.
# Usage: docs/spikes/m0-tiered-scrollback/run.sh
set -eu
cd "$(dirname "$0")"
./setup.sh

cargo run --quiet --release --target-dir "${TMPDIR:-/tmp}/pinfold-spike-scrollback"
echo
docker run --rm --user 1000:1000 -v "$PWD":/spike:ro -w /spike \
  -e HOME=/tmp -e CARGO_HOME=/tmp/cargo rust:1-slim \
  cargo run --quiet --release --target-dir /tmp/target
