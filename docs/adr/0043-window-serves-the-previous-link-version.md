# ADR-0043: A window serves a core one link version older

- **Status:** Proposed
- **Date:** 2026-11-06

## Context

The core outlives its window and starts the `pinfold-window` beside its own path on every
spawn (ADR-0028), so after an update an old core starts the new window. Until now both sides
demanded the same `link::VERSION`, so any change to the link made the next launch restart the
session ([#196], [#203]). A restart restores layout and directories but ends what ran in the
panes. [#238] raised the version from 9 to 10, so the first beta update would hit it.

Between 9 and 10 exactly one thing changed on the wire (`git diff b44d500^ b44d500 --
crates/pinfold-proto/src/link.rs`): `ApprovalAction::Owner`, tag 3 of the action byte in
`Notice::Approval`, which the core sends since [#217] for an owner method called from a terminal
outside every pane (ADR-0042). No request changed, and no other notice.

The fixed points are what beta.1 already ships: a version 9 core sends its hello at once and
accepts only a hello of 9. Neither can change after the fact.

## Decision

**Hello.** The core sends its hello as soon as the link opens, as before. The window holds its
own hello until the core's has come, then answers with the lower of the two versions and queues
every request behind that answer. That is the version both sides speak for the life of the link.

**Range.** Each side speaks its `VERSION` and `OLDEST = VERSION - 1`:

| Peer's hello | Core (reads the window's) | Window (reads the core's) |
|---|---|---|
| `VERSION + 1` | refused: a window of N+1 answers N | accepted, answered at `VERSION` |
| `VERSION` | accepted | accepted |
| `VERSION - 1` | accepted | accepted, answered at `VERSION - 1` |
| anything else | refused (`Error::Version`) | refused, after saying its own `VERSION` |

A gap of two or more is still [#203]'s path: the window says its own version before it closes,
so the core logs both numbers and the launcher offers the restart, asking first when a pane runs
a program.

**Gating.** Every message or field value that changed in version N carries `since = N`
(`Notice::since`, `ApprovalAction::since`). A sender never sends a message newer than the spoken
version. A receiver that gets one ends the link with `Error::Newer`, as it would for any other
broken message. For 9 and 10:

| Change in 10 | Spoken version 9 |
|---|---|
| `Notice::Approval` with `ApprovalAction::Owner` | the core never sends it: an owner method from outside every pane is refused with "approval needs an open PinFold window", as with no window. It is never run unasked. |

Every other message is unchanged between 9 and 10 and flows both ways at either version.

| Change in 11 | Spoken version 10 |
|---|---|
| `Notice::Unhardened` (ADR-0044) | the core never sends it: a window of 10 shows no notice or marker, and the core's log still says it. |

From 11, `OLDEST` is 10, so a version 9 core from beta.1 restarts the session, as two apart does.

**Raising the version.** A PR that changes the link raises `VERSION`, gives each new message or
value a `since` of the new version, and adds a row to this table saying how the older version
degrades. A change with no safe degradation, such as a new field every frame needs, says so
here, and that update restarts the session.

## Consequences

- An update one link version ahead keeps the running session and its programs, with no restart
  and no question, and the user gets every feature of the older version. A shipped version 9
  core is served by a version 10 window without changing it.
- A core of N also serves a window that answers N-1: a window of N-1 that follows this ADR,
  which only a downgrade starts. A version 9 window from beta.1 still fails against a newer core,
  as it checks for exactly 9.
- Each link change costs one `since` entry and one row here. Both sides keep code for the older
  version for one release only: raising `VERSION` again drops what `OLDEST` no longer covers.
- Feature changes that are not on the wire, such as what a key does in the window, need no
  gating. Behaviour the core decides alone works the same with either window.
- Revisit if updates often skip a version, as two apart still restarts, or if a change cannot
  degrade, which forces a restart anyway.

[#196]: https://github.com/0xPiranhaCodes/pinfold.sh/issues/196
[#203]: https://github.com/0xPiranhaCodes/pinfold.sh/pull/203
[#217]: https://github.com/0xPiranhaCodes/pinfold.sh/issues/217
[#238]: https://github.com/0xPiranhaCodes/pinfold.sh/pull/238
