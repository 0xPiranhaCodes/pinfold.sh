#!/bin/sh
# The memory a tool process starts from (ADR-0045): the release `pinfold` binary idle in
# `pinfold mcp`, reading from a pipe that stays open. Prints the median of three runs of
# phys_footprint on macOS or PSS on Linux, in MB of 10^6 bytes, as mem-gate does.
set -eu
repo=$(cd "$(dirname "$0")/../../.." && pwd)
cargo build -q --release --manifest-path "$repo/Cargo.toml" -p pinfold-cli --bin pinfold
binary="$repo/target/release/pinfold"
readings=""
for _ in 1 2 3; do
    sleep 30 | "$binary" mcp >/dev/null &
    pid=$!
    sleep 2
    case $(uname) in
    Darwin) bytes=$(footprint -p "$pid" | awk '/phys_footprint:/ {v=$2; u=$3} END {m = u=="KB" ? 1e3 : u=="MB" ? 1e6 : 1; print v * m}') ;;
    *) bytes=$(awk '/^Pss:/ {print $2 * 1024}' "/proc/$pid/smaps_rollup") ;;
    esac
    kill "$pid"
    readings="$readings $bytes"
done
echo "$readings" | tr ' ' '\n' | sed '/^$/d' | sort -n | sed -n 2p | awk '{printf "Idle pinfold mcp: %.2f MB (median of 3, %s)\n", $1 / 1e6, "'"$(uname)"'"}'
ls -l "$binary" | awk '{printf "Binary size: %.2f MB\n", $5 / 1e6}'
