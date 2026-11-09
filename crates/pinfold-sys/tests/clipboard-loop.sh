#!/bin/sh
# Runs `cargo test -p pinfold-sys --features clipboard` RUNS times, COPIES at once so the
# copies and the other tests in each load the machine, and counts the runs that failed (#361).
#
#   crates/pinfold-sys/tests/clipboard-loop.sh [runs] [copies]
#
# Before #361 `a_large_clipboard_costs_this_process_the_limit` measured the whole test
# process and gave its read 500 ms; under this load it failed 10 of 50 runs with
# `Err(Empty)`, the read cut off by that deadline.
set -eu
runs=${1:-50}
copies=${2:-8}
cd "$(dirname "$0")/../../.."
cargo test -q -p pinfold-sys --features clipboard --no-run 2>/dev/null
out=target/clipboard-loop
rm -rf "$out"
mkdir -p "$out"
seq "$runs" | xargs -P "$copies" -I{} sh -c \
    'cargo test -q -p pinfold-sys --features clipboard >"$0/run{}.txt" 2>&1 && rm "$0/run{}.txt"; true' "$out"
failed=$(find "$out" -name 'run*.txt' | wc -l | tr -d ' ')
echo "runs=$runs failed=$failed"
grep -l a_large_clipboard "$out"/run*.txt 2>/dev/null | sed 's/^/clipboard test failed: /' || true
