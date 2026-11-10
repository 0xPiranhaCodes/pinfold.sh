#!/bin/sh
# Runs pinfold-core's library tests RUNS times, COPIES at once so the copies and the other
# tests in each load the machine, and counts the runs in which
# `the_users_shell_reports_its_path` failed (#342). Further arguments go to the test binary:
#
#   crates/pinfold-core/tests/login-path-loop.sh [runs] [copies] [test args...]
#
# `... 50 1 the_users_shell` runs the test alone; `... 50 8 --skip a_blocked_turn_wakes` runs
# it under the rest of the suite, without the hang of #387. Before #342 the test gave the
# user's interactive login zsh LOGIN_WAIT, 10 s, and under `... 50 8` failed 50 of 50 runs
# with `TimedOut: your shell took over 10 s to report its PATH`.
set -eu
runs=${1:-50}
copies=${2:-8}
[ $# -ge 2 ] && shift 2 || shift $#
cd "$(dirname "$0")/../../.."
cargo test -q -p pinfold-core --lib --no-run 2>/dev/null
out=target/login-path-loop
rm -rf "$out"
mkdir -p "$out"
seq "$runs" | xargs -P "$copies" -I{} sh -c \
    'cargo test -q -p pinfold-core --lib -- "$@" >"$0/run{}.txt" 2>&1 && rm "$0/run{}.txt"; true' \
    "$out" "$@"
failed=$(find "$out" -name 'run*.txt' | wc -l | tr -d ' ')
path=$(grep -l "the_users_shell_reports_its_path --- FAILED" "$out"/run*.txt 2>/dev/null | wc -l | tr -d ' ')
echo "runs=$runs copies=$copies failed=$failed path_test_failed=$path"
grep -h -A3 "the_users_shell_reports_its_path' panicked" "$out"/run*.txt 2>/dev/null || true
