#!/bin/sh
# Creates or updates the project's labels. Usage: .github/labels.sh [owner/repo]
set -eu
repo="${1:-0xPiranhaCodes/pinfold.sh}"

while IFS='|' read -r name color description; do
  gh label create "$name" --repo "$repo" --color "$color" \
    --description "$description" --force
done <<'LABELS'
type/bug|D73A4A|Something is broken
type/feature|A2EEEF|New capability
type/docs|0075CA|Documentation only
type/chore|CFD3D7|Build, CI or maintenance
area/core|5319E7|PTY, VT engine, mux, sessions
area/api|5319E7|Agent API, scopes, audit log
area/render|5319E7|Renderer and glyph cache
area/ui|5319E7|Window process, chrome, input
area/tools|5319E7|Markdown reader, file manager, diff viewer
area/config|5319E7|Configuration
area/ci|5319E7|CI, release and packaging
good first issue|7057FF|A newcomer can finish this in a few hours
help wanted|008672|Maintainer welcomes a contribution
security|B60205|Affects the security model
memory|FBCA04|Affects a memory budget
LABELS
