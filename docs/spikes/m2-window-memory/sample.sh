#!/bin/sh
# Prints one line per run of an idle measuring window: its footprint and what makes it up.
# Usage, from the repository root, after `cargo xtask mem-gate` has built the binaries:
#   docs/spikes/m2-window-memory/sample.sh [runs] [settle seconds]
set -eu
runs=${1:-10}
settle=${2:-7}
dir=$(mktemp -d /tmp/pfm.XXXXXX)
mkdir "$dir/config"
trap 'rm -rf "$dir"' EXIT
category() {
    grep -m1 "$1" "$dir/footprint" | awk '{print $1 $2 "/" $5 $6}'
}
run=0
while [ "$run" -lt "$runs" ]; do
    run=$((run + 1))
    mkfifo "$dir/line"
    XDG_CONFIG_HOME="$dir/config" XDG_RUNTIME_DIR="$dir" \
        target/release-core/pinfold-core --window 1 1512x1890 >"$dir/line" 2>/dev/null &
    core=$!
    read -r window <"$dir/line"
    rm "$dir/line"
    sleep "$settle"
    footprint --pid "$window" >"$dir/footprint" 2>&1
    kill "$core" "$window" 2>/dev/null || true
    wait "$core" 2>/dev/null || true
    # Each category prints dirty/reclaimable.
    printf '%s footprint %s surfaces %s malloc-small %s driver %s pool %s\n' "$run" \
        "$(grep -m1 -o 'Footprint: [0-9]* MB' "$dir/footprint" | cut -d' ' -f2)" \
        "$(category 'IOSurface')" "$(category 'Malloc Small')" \
        "$(category 'IOAccelerator (graphics)')" "$(category 'Owned physical footprint (unmapped)$')"
done
