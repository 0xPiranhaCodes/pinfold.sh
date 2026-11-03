#!/bin/bash
# usage: stress.sh <pinfold-core lib test binary> <label> <outdir>
# 50 rounds; each round runs the whole lib suite in the background (PTY churn like CI) and,
# meanwhile, each target test once, all under 6 `yes` loaders. Counts target failures.
bin=$1; label=$2; out=$3
tests=(family::tests::a_bad_cwd_or_a_cap_refuses_and_changes_nothing io::tests::a_search_that_finds_nothing_ends_and_bad_ones_never_start io::tests::sends_past_20_a_second_or_a_full_queue_are_refused)
declare -a fails=(0 0 0)
pids=(); for i in 1 2 3 4 5 6; do yes > /dev/null & pids+=($!); done
for n in $(seq 1 50); do
  "$bin" -q > "$out/$label-suite-$n.log" 2>&1 & suite=$!
  for i in 0 1 2; do
    if ! "$bin" --exact "${tests[$i]}" -q > "$out/$label-$i-$n.log" 2>&1; then
      fails[$i]=$((fails[$i]+1)); grep -h -A1 panicked "$out/$label-$i-$n.log" | tail -1
    fi
  done
  wait $suite || echo "round $n: suite failed: $(grep -h 'FAILED' "$out/$label-suite-$n.log" | head -3 | tr '\n' ' ')"
done
kill "${pids[@]}"
for i in 0 1 2; do echo "$label ${tests[$i]}: ${fails[$i]}/50 failed"; done
