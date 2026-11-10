#!/bin/sh
# Prints one line per run of a core beside a measuring window on Linux: its PSS in MB of 10^6
# bytes, as the gate's row "Core beside that window, 8 idle tabs" reads it, and the kB of PSS
# in the heap, in other anonymous memory and in files. Ends with the lowest, median and highest.
# Usage, from the repository root, after `cargo xtask mem-gate` has built the binaries:
#   xvfb-run -a -s "-screen 0 1920x1080x24" docs/spikes/core-beside-window/sample.sh [runs] [settle seconds] [core arguments]
# The core arguments default to `--window 8 1920x1080`; `--idle 8` reads a core with no window.
set -eu
runs=${1:-10}
settle=${2:-7}
if [ "$#" -gt 2 ]; then shift 2; else set -- --window 8 1920x1080; fi
core_binary=${CARGO_TARGET_DIR:-target}/release-core/pinfold-core
dir=$(mktemp -d /tmp/pfc.XXXXXX)
trap 'rm -rf "$dir"' EXIT
run=0
while [ "$run" -lt "$runs" ]; do
    run=$((run + 1))
    window=
    if [ "$1" = --window ]; then
        mkfifo "$dir/line"
        XDG_RUNTIME_DIR="$dir" "$core_binary" "$@" >"$dir/line" 2>/dev/null &
        core=$!
        read -r window <"$dir/line"
        rm "$dir/line"
    else
        XDG_RUNTIME_DIR="$dir" "$core_binary" "$@" >/dev/null 2>&1 &
        core=$!
    fi
    sleep "$settle"
    awk -v run="$run" '
        /^[0-9a-f]+-[0-9a-f]+ / { kind = $6 == "[heap]" ? "heap" : $6 == "" || $6 ~ /^\[/ ? "anon" : "file" }
        /^Pss:/ { pss[kind] += $2; total += $2 }
        END { printf "%d pss %.2f heap %d anon %d file %d\n", run, total * 1024 / 1e6, pss["heap"], pss["anon"], pss["file"] }
    ' "/proc/$core/smaps" | tee -a "$dir/readings"
    # shellcheck disable=SC2086 # no window leaves nothing to kill
    kill "$core" $window 2>/dev/null || true
    wait "$core" 2>/dev/null || true
done
sort -n -k3 "$dir/readings" | awk '{ pss[NR] = $3 } END { print "lowest", pss[1], "median", pss[int((NR + 1) / 2)], "highest", pss[NR] }'
