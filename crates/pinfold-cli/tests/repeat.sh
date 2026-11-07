#!/bin/sh
# Runs the `session` test binary RUNS times and counts the failed runs, as for #286 and #288.
# Arguments after RUNS go to the binary, such as `--exact NAME` for one test. On macOS it then
# lists the executables Gatekeeper refused to run meanwhile, which it kills at exec (#288).
#
#   crates/pinfold-cli/tests/repeat.sh 100
#   crates/pinfold-cli/tests/repeat.sh 200 --exact a_window_that_exits_during_start_fails_the_launcher
set -eu
runs=${1:?usage: repeat.sh RUNS [test binary arguments]}
shift
cd "$(dirname "$0")/../../.."
binary=$(cargo test -p pinfold-cli --test session --no-run --message-format=json 2>/dev/null |
    sed -n 's/.*"executable":"\([^"]*\)".*/\1/p' | tail -n 1)
out=target/repeat
mkdir -p "$out"
since=$(date '+%Y-%m-%d %H:%M:%S')
failed=0
run=1
while [ "$run" -le "$runs" ]; do
    if "$binary" "$@" >"$out/run$run.txt" 2>&1; then
        rm "$out/run$run.txt"
    else
        failed=$((failed + 1))
        echo "run $run failed: $out/run$run.txt"
    fi
    run=$((run + 1))
done
echo "runs=$runs failed=$failed"
if [ "$(uname)" = Darwin ]; then
    # Hard links share one file, so the path named may be another test's link to it.
    log show --start "$since" --style compact \
        --predicate 'process == "kernel" AND eventMessage CONTAINS "Security policy would not allow"' |
        sed 1d
fi
