#!/bin/sh
# Prints one line per run of a core beside a 1920x1080 window whose one pane prints 5,000,000
# numbered lines: the core's CPU time and the time the lines took, both in ms.
# Usage, from the repository root, after `cargo xtask mem-gate` has built the binaries:
#   xvfb-run -a -s "-screen 0 1920x1080x24" docs/spikes/core-beside-window/flood.sh [runs]
set -eu
runs=${1:-5}
core_binary=${CARGO_TARGET_DIR:-target}/release-core/pinfold-core
dir=$(mktemp -d /tmp/pff.XXXXXX)
trap 'rm -rf "$dir"' EXIT
# Waits for the window's size first, so every line is drawn at 1920x1080.
printf '#!/bin/sh\nsleep 2\ndate +%%s%%N >%s/start\nseq 1 5000000\ndate +%%s%%N >%s/done\nexec sleep 60\n' "$dir" "$dir" >"$dir/shell"
chmod +x "$dir/shell"
run=0
while [ "$run" -lt "$runs" ]; do
    run=$((run + 1))
    rm -f "$dir/done"
    mkfifo "$dir/line"
    SHELL="$dir/shell" XDG_RUNTIME_DIR="$dir" "$core_binary" --window 1 1920x1080 >"$dir/line" 2>/dev/null &
    core=$!
    read -r window <"$dir/line"
    rm "$dir/line"
    while [ ! -s "$dir/done" ]; do sleep 0.1; done
    # The first field of schedstat is the time on a CPU, in ns.
    awk -v run="$run" -v start="$(cat "$dir/start")" -v done="$(cat "$dir/done")" \
        '{ printf "%d core-cpu-ms %d lines-ms %d\n", run, $1 / 1e6, (done - start) / 1e6 }' "/proc/$core/schedstat"
    kill "$core" "$window" 2>/dev/null || true
    wait "$core" 2>/dev/null || true
done
