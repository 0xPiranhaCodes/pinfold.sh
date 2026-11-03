#!/bin/sh
# Reproduces the table in README.md on an Apple Silicon Mac.
set -eu
cd "$(dirname "$0")"

echo "model: $(/usr/sbin/sysctl -n hw.model)"
echo "macOS: $(sw_vers -productVersion) ($(sw_vers -buildVersion))"
echo "rustc: $(rustc --version)"
echo "frames: ${1:-1000} after ${2:-120} warm-up frames"
cargo run --quiet --release -- "${1:-1000}" "${2:-120}"
