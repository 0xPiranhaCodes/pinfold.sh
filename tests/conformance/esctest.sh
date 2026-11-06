#!/bin/sh
# Runs esctest against pinfold-core's VT engine and compares the tests that pass with
# esctest-pass.txt. A listed test that fails exits 1; a test that newly passes is reported.
# With --update, writes the tests that pass to the list instead.
#
# esctest2 is GPL-2.0 and is fetched at a pinned commit, never vendored or linked.
set -eu

ESCTEST_REPOSITORY=https://github.com/ThomasDickey/esctest2.git
ESCTEST_COMMIT=2798f12149a19c3295e9b4853ab2da4b2eff1b2b

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
target="$root/target/esctest"
source="$target/esctest2-$ESCTEST_COMMIT"
list="$here/esctest-pass.txt"

if [ ! -f "$source/esctest/esctest.py" ]; then
    rm -rf "$source"
    git init -q "$source"
    git -C "$source" fetch -q --depth 1 "$ESCTEST_REPOSITORY" "$ESCTEST_COMMIT"
    git -C "$source" checkout -q FETCH_HEAD
fi

cargo build -q --manifest-path "$root/Cargo.toml" -p pinfold-tests --bin esctest-driver
"$root/target/debug/esctest-driver" python3 "$source/esctest/esctest.py" \
    --expected-terminal xterm --no-print-logs --logfile "$target/esctest.log" >/dev/null

summary=$(grep 'tests passed' "$target/esctest.log" || true)
if [ -z "$summary" ]; then
    echo "esctest stopped before its summary; see $target/esctest.log" >&2
    exit 1
fi
total=$(grep -c '^Run test: ' "$target/esctest.log")
grep -B1 '^Passed\.$' "$target/esctest.log" | sed -n 's/^Run test: //p' | LC_ALL=C sort \
    >"$target/passing.txt"
echo "esctest: $(wc -l <"$target/passing.txt" | tr -d ' ') of $total tests pass ($summary)"

if [ "${1:-}" = --update ]; then
    cp "$target/passing.txt" "$list"
    exit 0
fi
newly_passing=$(LC_ALL=C comm -13 "$list" "$target/passing.txt")
newly_failing=$(LC_ALL=C comm -23 "$list" "$target/passing.txt")
if [ -n "$newly_passing" ]; then
    echo "Newly passing; add them with tests/conformance/esctest.sh --update:"
    echo "$newly_passing"
fi
if [ -n "$newly_failing" ]; then
    echo "Newly failing; the log is $target/esctest.log:" >&2
    echo "$newly_failing" >&2
    exit 1
fi
