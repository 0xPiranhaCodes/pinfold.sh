#!/bin/sh
# Fails unless the CI workflow's latest push run on commit $1 concluded success. Waits while that
# run is queued or running, or has not started yet, for at most $CI_WAIT_SECONDS (default 3600).
# Needs gh with GH_REPO set and a token that can read actions.
set -eu
sha=$1
deadline=$(($(date +%s) + ${CI_WAIT_SECONDS:-3600}))
while :; do
  # The newest push run first; a re-run keeps its run and changes its conclusion.
  run=$(gh api "repos/$GH_REPO/actions/workflows/ci.yml/runs?head_sha=$sha&event=push&per_page=1" \
    --jq '.workflow_runs[0] | select(.) | "\(.status) \(.conclusion) \(.html_url)"')
  case $run in
    "completed success "*)
      echo "CI passed on $sha: ${run##* }"
      exit 0
      ;;
    completed*)
      echo "::error::CI did not pass on $sha: $run"
      exit 1
      ;;
  esac
  if [ "$(date +%s)" -ge "$deadline" ]; then
    echo "::error::no finished CI push run on $sha in time${run:+: $run}"
    exit 1
  fi
  echo "waiting for CI on $sha: ${run:-no push run yet}"
  sleep "${CI_POLL_SECONDS:-60}"
done
