#!/bin/sh
# Runs `cargo test -p pinfold-sys`, its `many_ptys_open_at_once` load and #354's session test
# RUNS times, COPIES at once, so many PTYs open together, and counts each kind of failure (#383).
#
#   crates/pinfold-sys/tests/pty-loop.sh [runs] [copies]
#
# Under this load macOS fails an open of /dev/ptmx with errno -6, its private EREDRIVEOPEN,
# once the open has lost the race for a PTY slot ten times; the kernel logs "vn_open_auth:
# reached max_retries error -6". Before #383 `Pty::spawn` opened at most twice and returned
# `Os { code: -6 }`; now a fourth loss in a row is the `ResourceBusy` "took every slot".
# grantpt can also restart forever in the kernel when other PTYs' masters close before their
# replicas, as a pane's does: a command past `limit` seconds is killed with its process group
# and counted as hung.
set -eu
cd "$(dirname "$0")/../../.."
out=target/pty-loop
limit=300
if [ "${1:-}" = --run ]; then
    # One run, numbered $2: a command that fails leaves its output as <kind><run>.txt, and one
    # that hung as hung-<kind><run>.txt.
    run() {
        name=$1
        shift
        status=0
        perl -e '$limit = shift; $pid = fork;
            if (!$pid) { setpgrp; exec @ARGV or exit 127 }
            $SIG{ALRM} = sub { kill "KILL", -$pid; exit 124 }; alarm $limit;
            waitpid $pid, 0; exit($? >> 8)' "$limit" "$@" >"$out/$name.txt" 2>&1 || status=$?
        case $status in
        0) rm "$out/$name.txt" ;;
        124) mv "$out/$name.txt" "$out/hung-$name.txt" ;;
        esac
    }
    run "sys$2" cargo test -q -p pinfold-sys
    run "load$2" cargo test -q -p pinfold-sys --lib -- --ignored --exact \
        pty::tests::many_ptys_open_at_once
    run "cli$2" cargo test -q -p pinfold-cli --test session -- --exact \
        a_configured_protect_pattern_refuses_a_send_to_a_pane_that_runs_it
    exit 0
fi
runs=${1:-50}
copies=${2:-8}
cargo test -q -p pinfold-sys --no-run 2>/dev/null
cargo test -q -p pinfold-cli --test session --no-run 2>/dev/null
rm -rf "$out"
mkdir -p "$out"
seq "$runs" | xargs -P "$copies" -n 1 "$0" --run
count() { find "$out" -name "$1" | wc -l | tr -d ' '; }
echo "runs=$runs copies=$copies"
for kind in sys load cli; do
    echo "$kind: failed=$(count "$kind*.txt") hung=$(count "hung-$kind*.txt")" \
        "lost_slot=$(grep -l -e 'code: -6' -e 'took every slot' "$out/$kind"*.txt 2>/dev/null |
            wc -l | tr -d ' ')"
done
