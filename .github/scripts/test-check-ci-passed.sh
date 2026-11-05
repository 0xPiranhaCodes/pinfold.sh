#!/bin/sh
# Runs check-ci-passed.sh against a fake gh that answers with $FAKE_RUN.
set -eu
script="$(cd "$(dirname "$0")" && pwd)/check-ci-passed.sh"
bin=$(mktemp -d)
trap 'rm -rf "$bin"' EXIT
# shellcheck disable=SC2016 # the fake gh expands $FAKE_RUN, not this script
printf '#!/bin/sh\nprintf "%%s" "$FAKE_RUN"\n' > "$bin/gh"
chmod +x "$bin/gh"
expect() {
  if PATH="$bin:$PATH" GH_REPO=o/r CI_WAIT_SECONDS=0 CI_POLL_SECONDS=0 FAKE_RUN=$2 \
    sh "$script" abc > /dev/null; then got=0; else got=1; fi
  if [ "$got" != "$1" ]; then echo "FAIL: '$2' exited $got, expected $1" >&2; exit 1; fi
}
expect 0 "completed success https://x/runs/1"
expect 1 "completed failure https://x/runs/1"
expect 1 "completed cancelled https://x/runs/1"
expect 1 "in_progress null https://x/runs/1"
expect 1 ""
echo "check-ci-passed: ok"
