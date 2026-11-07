# Agent hooks: ◆ needs you from Claude Code and Codex

A pane shows ◆ needs you when its agent says so with `pane.status`. Agent CLIs don't call the
API themselves, but both run a command on events, and `pinfold ctl status <state>` reports the
state of the pane it runs in. PinFold knows the pane by its terminal, so the hook needs no ID
or token. Outside a PinFold pane the command fails and changes nothing.

A pane launched from an agent profile also reads codex-cli's own OSC 7501 states, without a
hook ([ADR-0046](../adr/0046-agent-panes-read-needs-you-from-output.md)). PinFold never reads
terminal titles, so Claude Code needs the hook in any pane. A reported state outranks what the
pane's output says until the agent reports another.

## Claude Code

Merge [`claude-code.json`](claude-code.json) into `~/.claude/settings.json`, or a project's
`.claude/settings.json`:

```json
{
  "hooks": {
    "Notification": [
      { "hooks": [{ "type": "command", "command": "pinfold ctl status needs-you" }] }
    ],
    "UserPromptSubmit": [
      { "hooks": [{ "type": "command", "command": "pinfold ctl status running" }] }
    ],
    "PostToolUse": [
      { "matcher": "*", "hooks": [{ "type": "command", "command": "pinfold ctl status running" }] }
    ],
    "Stop": [
      { "hooks": [{ "type": "command", "command": "pinfold ctl status done" }] }
    ]
  }
}
```

`Notification` fires when Claude Code asks for permission or waits for your input, so the
pane needs you. A tool that ran after you approved it, or a new prompt, makes it running again,
and the end of a turn makes it done.

## Codex

Add [`codex.toml`](codex.toml)'s line to `~/.codex/config.toml`:

```toml
notify = ["sh", "-c", "pinfold ctl status done", "pinfold-notify"]
```

Codex runs `notify` when a turn completes, with a JSON argument appended. Under `sh -c` that
argument becomes `$1`, which the command ignores, so it never becomes the message. codex-cli
0.166.0 calls `notify` only at the end of a turn, so in a plain shell pane this shows done;
needs you at its approval prompt comes from its OSC 7501 states in an agent profile's pane.

## Tested

`the_documented_agent_hooks_set_their_panes_status` (`crates/pinfold-cli/tests/session.rs`)
reads both files, runs each command the way its CLI would, in a real core's pane, and expects
`needs-you, running, done, running, done`, each with `source: "reported"`.
