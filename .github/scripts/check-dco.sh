#!/bin/sh
# Fails if a non-merge commit in BASE..HEAD lacks a sign-off from its author.
set -eu
status=0
for commit in $(git rev-list --no-merges "$1..$2"); do
  email=$(git show -s --format=%ae "$commit")
  # The email is attacker-controlled, so it must be matched literally.
  if ! git show -s --format='%(trailers:key=Signed-off-by,valueonly)' "$commit" |
    grep -Fq -- "<$email>"; then
    echo "::error::$commit has no Signed-off-by for $email. Run: git commit --amend -s"
    status=1
  fi
done
exit "$status"
