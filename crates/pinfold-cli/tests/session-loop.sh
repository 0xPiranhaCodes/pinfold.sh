#!/bin/sh
# Runs `cargo test -p pinfold-cli --test session` RUNS times, COPIES at once, and counts the
# runs that failed and the tests that failed in them (#353). With TEST, runs that test alone (#354).
#
#   crates/pinfold-cli/tests/session-loop.sh [runs] [copies] [test]
#
# Before #353 two tests that passed one name to `Session::new` shared a directory, and the
# one that started second deleted the first one's socket and snapshot or killed its core.
# It builds once and runs the test binary itself: a `cargo test` per copy links the binaries
# again and again, and a copy that links `pinfold` while another test places it fails (#354).
set -eu
runs=${1:-25}
copies=${2:-4}
test=${3:-}
cd "$(dirname "$0")/../../.."
binary=$(cargo test -q -p pinfold-cli --test session --no-run --message-format=json 2>/dev/null |
    sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' | tail -1)
out=target/session-loop${test:+-$test}
rm -rf "$out"
mkdir -p "$out"
since=$(date '+%Y-%m-%d %H:%M:%S')
seq "$runs" | xargs -P "$copies" -I{} sh -c \
    '"$1" -q ${2:+--exact "$2"} >"$0/run{}.txt" 2>&1 && rm "$0/run{}.txt"; true' \
    "$out" "$binary" "$test"
failed=$(find "$out" -name 'run*.txt' | wc -l | tr -d ' ')
echo "runs=$runs failed=$failed"
grep -ho "^thread '[^']*'" "$out"/run*.txt 2>/dev/null | sort | uniq -c || true
if [ "$(uname)" = Darwin ]; then
    # As in `repeat.sh`: what Gatekeeper killed at exec meanwhile (#288). Hard links share one
    # file, so the path named may be another test's or copy's link to it.
    log show --start "$since" --style compact \
        --predicate 'process == "kernel" AND eventMessage CONTAINS "Security policy would not allow"' |
        sed 1d
fi
