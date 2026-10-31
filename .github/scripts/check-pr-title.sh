#!/bin/sh
# Fails unless the argument is a Conventional Commit title.
set -eu
types='build|chore|ci|docs|feat|fix|perf|refactor|revert|style|test'
if ! printf '%s\n' "$1" | grep -Eq "^($types)(\([a-z0-9-]+\))?!?: .+"; then
  echo "::error::PR title must be a Conventional Commit, such as: feat(api): add hello handshake"
  exit 1
fi
