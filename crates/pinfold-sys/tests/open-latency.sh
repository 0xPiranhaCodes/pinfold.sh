#!/bin/sh
# Runs `pty::tests::open_latency` COPIES times at once, 16 threads each, and prints each copy's
# open times: the core's PTY open deadline must sit well past the slowest (ADR-0048).
#
#   crates/pinfold-sys/tests/open-latency.sh [copies]
#
# Eight copies keep at most 128 PTYs open, under macOS's kern.tty.ptmx_max of 511.
set -eu
copies=${1:-8}
cd "$(dirname "$0")/../../.."
cargo test -q -p pinfold-sys --lib --no-run 2>/dev/null
seq "$copies" | xargs -P "$copies" -I{} cargo test -q -p pinfold-sys --lib -- --ignored \
    --exact --nocapture pty::tests::open_latency | grep '^opens='
