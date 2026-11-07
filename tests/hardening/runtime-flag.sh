#!/bin/sh
# Shows that a process can read its own hardened-runtime flag, and that a cargo build lacks it
# (ADR-0044). It runs by hand on macOS, from the repository root:
#
#   cargo build -p pinfold-cli && sh tests/hardening/runtime-flag.sh
#
# It signs a copy of the probe and of the core ad hoc, which needs no Apple account.
set -eu

BIN=${BIN:-$PWD/target/debug}
WORK=$(mktemp -d /tmp/pfflag.XXXXXX)
trap 'kill $core 2>/dev/null; rm -rf "$WORK"' EXIT
core=

cc -O2 -o "$WORK/probe" tests/hardening/runtime-flag.c
cp "$WORK/probe" "$WORK/probe-runtime"
codesign --force --sign - --options runtime "$WORK/probe-runtime" 2>/dev/null
cp "$WORK/probe" "$WORK/probe-unsigned"
codesign --remove-signature "$WORK/probe-unsigned"

for probe in probe probe-runtime probe-unsigned; do
  echo "== $probe reads itself; codesign: $(codesign -dv "$WORK/$probe" 2>&1 | grep -Eo 'flags=[^ ]+|not signed at all')"
  "$WORK/$probe" || echo "exit $?"
done

# The core as cargo leaves it, then as bundle.sh signs it, read from outside by pid.
sh packaging/macos/bundle.sh "$BIN/pinfold" "$BIN/pinfold-core" "$BIN/pinfold-window" "$WORK" >/dev/null 2>&1
mkdir -m 700 "$WORK/run" "$WORK/home"
for binary in "$BIN/pinfold-core" "$WORK/PinFold.app/Contents/MacOS/pinfold-core"; do
  HOME=$WORK/home XDG_RUNTIME_DIR=$WORK/run "$binary" --idle 1 >/dev/null 2>&1 &
  core=$!
  sleep 1
  echo "== $binary --idle 1; codesign: $(codesign -dv "$binary" 2>&1 | grep -Eo 'flags=[^ ]+')"
  "$WORK/probe" "$core"
  kill "$core"
  wait "$core" 2>/dev/null || true
  core=
done
