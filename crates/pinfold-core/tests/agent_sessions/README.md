# Recorded agent sessions

Real sessions of agent CLIs, replayed by `tests/agent_sessions.rs` through the VT engine
and status engine. `NAME.rec` is the raw PTY output, one read at a time, as
`DELAY_MS LENGTH\n` and then LENGTH bytes. `NAME.states` lists the statuses the pane shows
while it replays, in order.

| Fixture | CLI | Recorded | Size |
| --- | --- | --- | --- |
| `claude-code.rec` | Claude Code 2.1.311, `--model haiku` | 2026-11-07 | 10,951 bytes |
| `codex.rec` | codex-cli 0.166.0, its default model | 2026-11-07 | 32,470 bytes |

Both were recorded on macOS 26 with zsh 5.9 and Python 3.14 by `record.sh`, at 24 rows by 80
columns, in a new git repository in `/tmp` holding only a one-line `README.md`. `record.py`
runs `zsh -d -i` with a bare environment (`PATH`, `HOME`, `USER`, `LANG`, `TERM`) and a
prompt that sends OSC 133 `A`, `B`, `C` and `D;status` as shell integration would. Each
session, in order:

1. Starts the agent with a prompt asking it to `touch hello.txt` (running).
2. Trusts the folder, then approves the command at the agent's permission prompt (needs
   you). Claude Code runs with `--setting-sources project --strict-mcp-config
   --permission-mode manual`, so no user settings or MCP servers show and it asks first.
   Codex runs with `-a on-request -s read-only` and is asked to request approval.
3. Waits for the agent to finish, then quits it with Ctrl-C, and the agent exits 0 (done).
4. Runs a command that fails: `claude --model no-such-model -p hi` exits 1 and
   `codex -a untrusted hi` exits 2 (error).

After recording, `record.py` replaces session IDs with zeros and fails if the recording
holds the home directory, user name, host name, `/Users/`, `/home/`, an email address or an
`sk-` key. The test checks the same patterns, the size, and the host and user it runs as.

## What the pane shows

Both sessions replay as `idle, running, done, running, error`. Needs you is not among
them: at the permission prompts the pane says running, as OSC 133 marks never say needs
you (threat model T4); an agent says it with `pane.status`. Both CLIs send signals of their
own that Pinfold does not read ([#276](https://github.com/0xPiranhaCodes/pinfold.sh/issues/276)).
The shell sends `D` with `A`, and `B` 0–2 ms later in a separate read, so a finish stays
shown at the prompt until the next command runs.

## Re-recording

`./record.sh` re-records both, or `./record.sh codex` one. It needs the CLIs installed and
logged in, spends a few cents of model use, and adds one trusted folder to each CLI's
settings. Update the table above, check what changed with `cargo test -p pinfold-core --test
agent_sessions`, and read the new `.rec` before committing it.

## Why the output may be committed

A recording is the CLI's own interface text: its banner, menus, prompts and spinner words,
plus the model's short replies to the prompts above. It is kept to one short task per CLI,
about 11 KB and 32 KB, the least that shows each state; it holds no source code, system
prompt or documentation of either CLI, and serves only to test interoperability, as a
terminal emulator's test corpus would. Claude Code is proprietary (© Anthropic) and Codex
CLI is Apache-2.0.
