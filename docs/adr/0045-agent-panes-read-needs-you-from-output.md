# ADR-0045: Agent panes read "needs you" from their CLI's output

- **Status:** Accepted
- **Date:** 2026-11-07
- **Decided by:** the maintainer, on 2026-11-07, in
  [#276](https://github.com/0xPiranhaCodes/pinfold.sh/issues/276#issuecomment-6707901568)

## Context

A pane never shows ◆ needs you while an agent CLI waits at its own approval prompt
([#276](https://github.com/0xPiranhaCodes/pinfold.sh/issues/276)). Only `pane.status` can say
it, and neither CLI calls it. Threat model T4 keeps output from saying it at all, because any
program in a pane can print any escape sequence.

The sessions recorded for #236 (`crates/pinfold-core/tests/agent_sessions/`) show what each CLI
sends instead. codex-cli 0.166.0 sends `OSC 7501;state=idle|working|blocked|done:app=codex`,
with `blocked` while its approval prompt is up, and `OSC 7501;state=clear` on exit. Claude Code
2.1.311 sends only `OSC 7501;?` and spinner titles; with this Mac's user settings it also sent
`OSC 9;Claude is waiting for your input` at the prompt.

The maintainer decided on 2026-11-07:

> Maintainer decision (2026-11-07): yes. A pane should show ◆ needs you while an agent CLI waits
> at its own approval prompt, and PinFold should raise a notification if possible.
> Implementation follows, with an ADR and a threat-model update for T4.

## Decision

1. **Only agent panes read it.** The core reads OSC 7501 `state=blocked`, `working`, `done`,
   `idle` and `clear`, and OSC 9 notifications, only in a pane launched from an agent profile
   by `agent.launch` (ADR-0036). The profile is the opt-in: the user wrote that program into
   their config, and a caller of the API cannot name one. A plain shell pane, from a window
   key, `pane.spawn`, a layout or a restore, keeps today's behaviour whatever it prints.
   Other fields of an OSC 7501 string, its `?` query, and numbered OSC 9 commands such as
   ConEmu's `9;4` progress are ignored.
2. **Blocked or a notification is needs you.** Working is running, done is done, idle is idle,
   and clear hands back to the shell's OSC 133 marks. The CLI's latest signal outranks the
   shell's marks until it clears, its command finishes (the next OSC 133 `D`), or the user
   types or pastes into the pane, which clears a needs you only.
3. **It stays labelled "reported by output"**, as marks are: `pane.list` and events say
   `source: "marks"`, the API's word for output, and the window says "from output". An
   agent's `pane.status` report still outranks all of it until the agent clears it.
4. **Titles are never parsed.** Claude Code without user settings sends only spinner titles,
   the same while running and at its prompt. Instead the docs give a Claude Code
   `Notification` hook and a Codex `notify` setting that call `pinfold` to set `pane.status`,
   with example config the tests run.
5. **Notifications are PinFold's words.** A needs you from output alerts as a report does: the
   pane's tab is marked while hidden, and the window raises a notification when a tab comes to
   need you while hidden or in the background. Its text is written by PinFold, "<tab> needs
   you", and never the OSC 9, OSC 777 or title text the program sent. Limits: the tracker's
   alert interval stays at one per pane per 10 s, with output and API alerts counted apart; the
   window adds one notification per tab per 60 s and one per 10 s across all tabs. Clicking a
   notification goes to its tab where the platform tells the window.
6. **The smallest platform mechanism.** On macOS the window uses `NSUserNotificationCenter` from
   the `objc2-foundation` binding it already links. It is deprecated, but on macOS 27.0.1 it
   delivers from an ad hoc signed bundle, and it needs no entitlement. Outside a bundle, such as
   a `cargo` build, it has no default center, and the window asks for Dock attention with
   `NSApplication.requestUserAttention`. Linux has a TODO behind the same interface, and the
   tab mark alone until then. No GUI toolkit, C library or new crate is added.

## Consequences

An agent CLI that says it is blocked shows ◆ needs you, and replaying the committed codex
session in an agent pane shows it at the approval prompt. Any program the agent runs, or any
text it cats, can now say needs you in that pane, or flood notifications: the per-pane and
window-wide limits bound that, the label says where it came from, and the text a user reads
is PinFold's. A program in a plain shell pane still cannot; the same recorded bytes there never
show needs you.

`NSUserNotificationCenter` may stop delivering in a later macOS. Moving to
`UNUserNotificationCenter` would add `objc2-user-notifications` and needs the signed bundle,
so revisit then, or if users want banners from `cargo` builds. Revisit the opt-in if agents
start in plain shell panes often enough that the hooks are not enough.
