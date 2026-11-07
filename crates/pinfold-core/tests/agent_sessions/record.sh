#!/bin/sh
# Re-records the agent sessions in this directory: `record.sh [claude-code] [codex]`, both
# by default. See CONTRIBUTING.md.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
# A new project each time, so each agent asks whether to trust it.
project=$(cd "$(mktemp -d /tmp/pinfold-demo.XXXXXX)" && pwd -P)
trap 'rm -rf "$project"' EXIT
cd "$project" && git init -q && echo "# demo" > README.md

for agent in "${@:-claude-code codex}"; do
  for agent in $agent; do
    rm -f "$project/hello.txt"
    case $agent in
      claude-code) set -- \
        'send:claude --setting-sources project --strict-mcp-config --permission-mode manual --model haiku "Run this shell command and nothing else: touch hello.txt"\r' \
        'expect:trust this folder' 'sleep:2' 'send:\x1b[B' 'sleep:1' 'send:\r' \
        'expect:Tab to amend' 'sleep:3' 'send:1' 'expect:· done' 'sleep:2' \
        'send:\x03' 'expect:again' 'send:\x03' 'expect:133;B' \
        'send:claude --model no-such-model -p hi\r' ;;
      codex) set -- \
        'send:codex -a on-request -s read-only "Run touch hello.txt with escalated permissions, asking me to approve it. Nothing else."\r' \
        'expect:Trust this folder' 'sleep:2' 'send:\r' \
        'expect:proceed' 'sleep:2' 'send:y' 'expect:state=done' 'sleep:2' \
        'send:\x03' 'expect:133;B' \
        'send:codex -a untrusted hi\r' ;;
      *) echo "unknown agent: $agent" >&2; exit 2 ;;
    esac
    python3 "$here/record.py" "$here/$agent.rec" "$project" 'expect:133;B' "$@" 'expect:133;B'
  done
done
