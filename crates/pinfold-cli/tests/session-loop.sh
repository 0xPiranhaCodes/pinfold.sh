#!/bin/sh
# Runs `cargo test -p pinfold-cli --test session` RUNS times, COPIES at once, and counts the
# runs that failed and the tests that failed in them (#353).
#
#   crates/pinfold-cli/tests/session-loop.sh [runs] [copies]
#
# Before #353 two tests that passed one name to `Session::new` shared a directory, and the
# one that started second deleted the first one's socket and snapshot or killed its core.
set -eu
runs=${1:-25}
copies=${2:-4}
cd "$(dirname "$0")/../../.."
cargo test -q -p pinfold-cli --test session --no-run 2>/dev/null
out=target/session-loop
rm -rf "$out"
mkdir -p "$out"
seq "$runs" | xargs -P "$copies" -I{} sh -c \
    'cargo test -q -p pinfold-cli --test session >"$0/run{}.txt" 2>&1 && rm "$0/run{}.txt"; true' "$out"
failed=$(find "$out" -name 'run*.txt' | wc -l | tr -d ' ')
echo "runs=$runs failed=$failed"
grep -ho "^thread '[^']*'" "$out"/run*.txt 2>/dev/null | sort | uniq -c || true
