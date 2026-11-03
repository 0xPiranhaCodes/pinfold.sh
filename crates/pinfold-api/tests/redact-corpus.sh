#!/bin/sh
# Prints ordinary terminal output with no secrets in it, as a negative corpus for
# redaction. Run from the repository root.
set -eu
git log --stat -n 300
git log --format='%H %an %s' -n 2000
cat Cargo.lock
cargo tree --workspace
ls -la / /usr/bin
env | grep -v -i -E 'key|token|secret|pass|auth|cred' || true
cargo build --workspace -v 2>&1 | tail -n 200
