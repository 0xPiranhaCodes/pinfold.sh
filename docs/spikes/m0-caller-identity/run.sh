#!/bin/sh
# Reproduces results.md. Needs cargo, and Docker for the Linux run.
# Usage: docs/spikes/m0-caller-identity/run.sh
# Seccomp is off by default to match a bare host. Docker's filter hides pidfd_getfd.
set -eu
cd "$(dirname "$0")"

native() {
  cargo run --quiet --release --target-dir "${TMPDIR:-/tmp}/pinfold-spike-identity"
}

linux() {
  docker run --rm --user 1000:1000 --cap-drop ALL --security-opt "seccomp=${SECCOMP:-unconfined}" -v "$PWD":/spike:ro -w /spike \
    -e HOME=/tmp -e CARGO_HOME=/tmp/cargo rust:1-slim \
    sh -c 'cargo run --quiet --release --target-dir /tmp/target; rc=$?; echo; \
      echo "kernel $(uname -r), yama ptrace_scope $(cat /proc/sys/kernel/yama/ptrace_scope 2>/dev/null || echo absent)"; exit $rc'
}

native
echo
linux
