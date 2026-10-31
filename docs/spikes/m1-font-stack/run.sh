#!/bin/sh
# Reproduces results.md. Needs cargo, python3, and Docker for the Linux run.
# Usage: docs/spikes/m1-font-stack/run.sh
set -eu
cd "$(dirname "$0")"

TARGET="${TMPDIR:-/tmp}/pinfold-spike-fonts"
cargo build --quiet --release --target-dir "$TARGET"
echo "# macOS"
echo
python3 measure.py "$TARGET/release" /System/Library/Fonts/Menlo.ttc:0 "/System/Library/Fonts/STHeiti Light.ttc:0"
echo
echo "# Linux"
echo
docker build -q -t pinfold-spike-fonts . >/dev/null
docker run --rm --user 1000:1000 -v "$PWD":/spike:ro -w /spike \
  -e HOME=/tmp -e CARGO_HOME=/tmp/cargo pinfold-spike-fonts sh -c '
    cargo build --quiet --release --target-dir /tmp/target
    python3 measure.py /tmp/target/release \
      /usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf:0 \
      /usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc:0'
