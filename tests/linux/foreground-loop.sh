#!/bin/bash
# Runs one pinfold-sys test many times, several copies at once so they load the machine, and
# counts passes and failures (#336). It runs by hand on Linux, or from a Mac in Docker:
#
#   bash tests/linux/foreground-loop.sh [rounds] [copies] [test]
#   docker run --rm -v "$PWD":/src -w /src rust:1.99.1 bash tests/linux/foreground-loop.sh
#
# Before #336 the default test read a program's arguments while `exec` was setting them out
# and failed with `Some([])`.
set -u

ROUNDS=${1:-100}
COPIES=${2:-16}
TEST=${3:-foreground::tests::the_foreground_process_has_its_name_arguments_and_user}

BIN=$(cargo test -q -p pinfold-sys --lib --no-run --message-format=json 2>/dev/null |
    grep -o '"executable":"[^"]*"' | cut -d'"' -f4 | head -1)
[ -x "$BIN" ] || { echo "no pinfold-sys test binary" >&2; exit 1; }

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
pass=0
fail=0
for _ in $(seq "$ROUNDS"); do
    for copy in $(seq "$COPIES"); do
        "$BIN" --exact "$TEST" -q >"$WORK/$copy" 2>&1 &
    done
    for copy in $(seq "$COPIES"); do
        if wait -n; then pass=$((pass + 1)); else fail=$((fail + 1)); fi
    done
    grep -h -A2 panicked "$WORK"/* | grep -v '^--$' | sort | uniq -c
done
echo "$TEST: passes=$pass failures=$fail"
[ "$fail" -eq 0 ]
