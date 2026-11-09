#!/bin/sh
# Runs check-dco.sh on commits in a scratch repository.
set -eu
script="$(cd "$(dirname "$0")" && pwd)/check-dco.sh"
repo=$(mktemp -d)
trap 'rm -rf "$repo"' EXIT
cd "$repo"
git init -q
git -c user.name=base -c user.email=base@example.com commit -q --allow-empty -m base
bot='dependabot[bot] <49699333+dependabot[bot]@users.noreply.github.com>'
human='Human <human@example.com>'
# commit AUTHOR [git commit flags]: adds an empty commit by AUTHOR.
commit() {
  author=$1
  shift
  git -c user.name=x -c user.email=x@example.com commit -q --allow-empty \
    --author="$author" -m change "$@"
}
# expect STATUS PR_AUTHOR: runs the check on base..HEAD, then drops those commits.
expect() {
  if PR_AUTHOR=$2 sh "$script" "$base" HEAD > /dev/null; then got=0; else got=1; fi
  git reset -q --hard "$base"
  if [ "$got" != "$1" ]; then echo "FAIL: case $case exited $got, expected $1" >&2; exit 1; fi
}
base=$(git rev-parse HEAD)

case='bot commit on a bot PR'
commit "$bot"
expect 0 'dependabot[bot]'

case='forged bot author on a human PR'
commit "$bot"
expect 1 'mallory'

case='human commit without sign-off on a bot PR'
commit "$bot"
commit "$human"
expect 1 'dependabot[bot]'

case='human commit with sign-off on a bot PR'
commit "$human" --trailer "Signed-off-by: $human"
expect 0 'dependabot[bot]'

case='human commit without sign-off on a human PR'
commit "$human"
expect 1 'human'

case='bot-like email with a suffix on a bot PR'
commit 'dependabot[bot] <49699333+dependabot[bot]@users.noreply.github.com.evil>'
expect 1 'dependabot[bot]'

echo "check-dco: ok"
