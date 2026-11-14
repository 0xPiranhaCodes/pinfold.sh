# ADR-0052: Session replay: a timeline of records per agent run, drawn in cells

- **Status:** Proposed
- **Date:** 2026-11-14

## Context

**Who has the problem.** Someone who left an agent running, in one pane or several, and comes
back. The pane shows the last screen of output, and above it up to 2 MB of scrollback
([ADR-0021](0021-memory-targets-m0.md), `BYTE_CAP` in `crates/pinfold-core/src/scrollback.rs`),
which a chatty run fills in minutes, so the first commands are gone. The person wants to know
what the agent did: which commands it ran and which failed, what it asked to do in other panes
and whether that was allowed, when it stopped to wait, and which files it changed. Today each
of those is somewhere else, or nowhere:

- **Commands.** The shell's OSC 133 marks are read by `crates/pinfold-core/src/marks.rs` into
  one `ShellMarks`: the latest phase, a count of finished commands, the last exit status. The VT
  engine keeps the cursor at the last `C` and `D` marks for `pane.last_output`
  (`crates/pinfold-core/src/vt.rs`, `output`). Nothing keeps the one before.
- **API calls and approvals.** The audit log (`crates/pinfold-api/src/audit.rs`) records every
  answered request: caller, method, target, scope and outcome, hash-chained, in a 256 KiB ring
  shared by every caller, with an optional 5 MiB file. `audit.read` returns the newest 1,000 to a
  terminal outside every pane, filtered by pane. An agent that polls `pane.wait` and `pane.list`
  pushes its own and other agents' earlier entries out of the ring. An approval shows only as the
  outcome's reason text.
- **States.** `crates/pinfold-core/src/status.rs` keeps each pane's current `Status` and where it
  came from; [ADR-0051](0051-needs-you-inbox-and-alerts.md) adds how long it has held it. No
  change is kept.
- **Files.** [ADR-0050](0050-rewind-an-agents-changes.md) takes checkpoints of the run's working
  directory at launch, done, exit, on request and before a restore, and opens the diff viewer
  between two of them. It records what changed, not when, or around which command.

**What the design already fixes.** The core is the one long-lived process, has no async
runtime, and never panics on external input ([ADR-0007](0007-concurrency.md)). Its budget is
4 MB with 8 idle panes ([ADR-0021](0021-memory-targets-m0.md)). All UI is cells, drawn by the
window; prompts and overlays PinFold draws sit outside pane text, so output cannot fake or
answer them ([ADR-0005](0005-renderer.md), [ADR-0011](0011-configuration.md),
[ADR-0042](0042-owner-changes-ask-in-the-window.md)). Scrollback rows are numbered when stored,
and a stored row's number holds across widths and outlives eviction (`Scrollback::end`,
`index`, `stored_row`, `shown_index`; [ADR-0029](0029-history-reflows-at-read-time.md)). The
window already asks the core to scroll a pane to a row (`Request::Reveal`) and to open a tool
(`Request::OpenTool`). A snapshot holds the layout and no screen, and a restore runs shells,
never agents ([ADR-0034](0034-m5-sessions-plan.md) points 1 and 5), so a run ends with its
core. The audit log keeps no pane text, redacts nothing because it holds nothing to redact, and
records a send by its size alone; the threat model's T2 and T9 keep pane text out of anything a
caller reads from another pane or anything written to disk. Status is always glyph, colour and
word (`CLAUDE.md`, `docs/design/ui/`).

**What is pre-decided.** A run is [ADR-0050](0050-rewind-an-agents-changes.md)'s: one
`agent.launch`, `r<core start>-<n>`, with its checkpoints. The inbox is
[ADR-0051](0051-needs-you-inbox-and-alerts.md)'s: one row per listed pane with the last
message. This ADR adds to both and invents neither again. Records, never pane text.

**Crates.** None are needed: the records are small fixed structs, the view is cells, and the
transport is the existing link and API. Nothing was checked on crates.io because nothing is
proposed.

## Options

| Option | For | Against |
| --- | --- | --- |
| **A. Fixed-size records appended in the core as each source already fires, one ring per run; the window draws them as an overlay and jumps the pane or opens the diff viewer; counts go to the inbox; nothing is written to disk (recommended)** | Every source is already parsed in the core: marks, the audit reply, the tracker's transition, the checkpoint's result. 16 bytes an entry, bounded per run and in all. Jumps use stored-row numbers, which already survive eviction and widths. No text is kept, so T2 and T9 do not widen. | A new ring and six entry kinds to keep; the window gains an overlay; a core restart drops the timeline, as it drops the agent. |
| B. Derive the timeline at read time from the audit ring, the checkpoint store and scrollback | No new records. | Scrollback holds cells, not exit codes or durations: marks are consumed as they pass. The audit ring is shared and evicts across callers. States are kept nowhere. The commands, the main column, cannot be built. |
| C. A `pinfold replay` tool in its own pane ([ADR-0045](0045-tools-are-programs-in-their-own-pane.md)) reading a new `run.timeline` method | Consistent with the tools; the core only serves records. | A tool is an API caller: it cannot scroll another pane or open the diff viewer in the window, which is the point of selecting an entry, and `audit.read` refuses a caller in a pane. The interactive half must be the window anyway. The CLI half survives as `pinfold ctl timeline`. |
| D. Record each command's output as a transcript on disk, so evicted output can be replayed | Answers "what did it print", not only "what did it run". | Pane text at rest is T9's asset; ADR-0034 keeps scrollback out of snapshots, and spill files are not built ([#264](https://github.com/0xPiranhaCodes/pinfold.sh/issues/264)). It needs the encryption of ADR-0034 point 3, a new crate, and disk the agent's output sizes. A later ADR, once spill files exist. |

## Decision

Option A.

### 1. Whose timeline

Every run of [ADR-0050](0050-rewind-an-agents-changes.md) has one timeline, kept in its run
record. A pane that [ADR-0051](0051-needs-you-inbox-and-alerts.md) lists but `agent.launch` did
not open (its agent reported `pane.status` from a plain shell pane) gets a run record at its
first report, with no checkpoints, so its commands, calls and states are kept too (open
question 1). Tool panes and plain shell panes without a report never have one. The run's pane
is the one `agent.launch` opened or the one that reported; a run ends when its pane's program
exits or the pane closes, and stays reachable for this core's life within ADR-0050's cap of 64
runs, oldest closed first.

### 2. What is recorded

An **entry** is 16 bytes: a kind, a time in whole milliseconds since the run started (`u32`,
saturating), and fields the kind defines. It holds no string. The kinds:

| Kind | Written when | Fields | Source and its label |
| --- | --- | --- | --- |
| `command` | The shell's OSC 133 `D` closes a command | The stored-row number the cursor was on at the `B` mark (`Scrollback::end()` plus the cursor's line, `u32`), the duration from `C` to `D` in ms (`u32`), the exit status (`Option<u8>`) | Marks. Output can print them (T4), so the API says `source: "marks"` and the window says so once in its footer |
| `api` | `audit::reply` records a call whose caller or target is the run's pane | The audit `seq` (low 32 bits), the method (an index into the `Method` table, `u8`), the outcome (`ok`, `refused`, `error`), the target pane (`u16`) | The audit log, which already holds no text |
| `approval` | A window prompt about the run's pane, as caller or target, is answered | The `ApprovalAction` (`u8`), the answer (`once`, `thirty_minutes`, `denied`, `expired`), the other pane (`u16`), the grant id (`u32`) | The window's key, through `Grants::answer` |
| `state` | The tracker's shown status changes through a `pane.status` report or an agent CLI's OSC 7501 or OSC 9 signal ([ADR-0046](0046-agent-panes-read-needs-you-from-output.md)) | The status (`u8`), whether it was reported | Not written for a change the shell's marks make, which the `command` entry already says |
| `checkpoint` | ADR-0050 takes one for this run | `n` (`u16`), the reason (`u8`), whether it is complete, and how many paths differ from the launch checkpoint (`u32`) | ADR-0050's `pinfold checkpoint take`, whose JSON line gains `changed`: one `diff-tree -r --name-only` between refs in the store, no work tree (open question 8) |
| `exit` | The run's program exits or its pane closes | The exit status or signal (`u8`), which it was | `Pty::try_wait` |

Beside the entries, a **summary** of saturating `u32` counters that eviction never lowers:
`commands`, `failed` (a non-zero exit), `approvals`, `denied`, `calls`, `changed` (the newest
checkpoint's count), `errors` (`failed` plus ✕ error reports), `needs_you` (◆ entries), and
the run's start and end times. The inbox's line is these numbers in PinFold's words.

What is **not** recorded, by design: the command's text, any output, the `pane.status`
message, a title, a directory, a path, the profile name, the approval's texts, or the
keys the person typed. The redaction rule of the audit log applies trivially: there is nothing
to redact, and a test asserts the entry type has no string and is 16 bytes
(`const _: () = assert!(size_of::<Entry>() == 16)`).

### 3. Where, how long and how much

- **In the core,** in the run record: a `VecDeque<Entry>` of at most `timeline.entries`
  (1,024) for a live run, and a count of entries dropped. When a run ends, its ring is cut to
  its newest 128 entries; the summary stays whole. With ADR-0050's 64 runs, the worst case is
  64 live runs at 16 KiB each, 1 MiB, and the typical one 8 live runs plus closed ones at 2 KiB
  each; section 10 gates it.
- **Not on disk.** No file, and no entry in the snapshot. A restart restores shells, not agents
  (ADR-0034), so a run's timeline ends with the core, as the run does. Two things do outlive
  it: the audit file, when `api.audit_log` is on, and the checkpoints in ADR-0050's store.
  Each pane of the snapshot gains an optional `summary` object of the counters above, about
  160 bytes, no text, so the `was running` strip can say
  `○ was running: claude · 12 commands · 3 approvals · 8 files changed · 1 error` (open
  question 4). An old snapshot without it parses as before.
- **Caps on reads.** A label read (section 5) decodes at most 64 stored rows per request.
  `run.timeline` returns at most 1,000 entries per call.

### 4. Finding a command's output again

A `command` entry names a stored row, the one the shell's `B` mark was on when the person, or
the agent, started typing it. Selecting the entry asks the core for that row:

- **Still held:** `Scrollback::index(number)` is some, and `shown_index` turns it into a row at
  the pane's width. The core scrolls the view there as `Request::Reveal` does, closes nothing
  else, and the person reads the command and its output as the pane shows them.
- **Evicted or cleared:** `index` is none, because the row left under the byte cap or the
  program cleared the screen. The window draws the entry with the word `evicted` in
  `chrome.dim` where the label would be, which is not an agent state and so gets none of
  ◆ ● ✓ ✕ ○. Enter on it flashes `output evicted · scrollback is capped at 2 MB`, and `d`
  opens the diff viewer at the nearest checkpoint at or after the entry, when there is one, so
  the person can still see what changed around then (open question 9).

The anchor is a hint. A resize between the mark and the row's move into history reflows the
grid, and output can print marks anywhere, so a jump lands near a command the program claims
to have run, never with any authority. That is the same standing `pane.last_output` has today
(the `shortcut` in `vt.rs`).

### 5. The view, drawn in cells

- **Open:** prefix `t` on the focused pane, and a `Timeline` entry in the palette
  (`palette::COMMANDS`). In the inbox, `t` on a row opens that pane's. A pane with no run shows
  `No agent run in this pane` for a second where other notes flash. It is an overlay drawn after
  the panes, as the palette and the inbox are, so every key goes to it and program output can
  neither fake nor answer it.
- **Header,** two rows: `TIMELINE · <label> · tab 2 · pane 7 · ● running 14m`, the state in
  its glyph, colour and word with the time in state from ADR-0051's `since`; then the summary
  line, `12 commands · 3 approvals · 8 files changed · 1 error`, a zero count left out, and
  `· N older dropped` in `chrome.dim` when the ring evicted. The label is the tab's label
  (`top_bar::label`, already safe), as the inbox uses.
- **Rows,** oldest at the top, newest at the bottom, the cursor on the newest when opened.
  Each row is the time (`HH:MM:SS`, local), then a kind column, then the kind's fields:

  | Kind | Row |
  | --- | --- |
  | `command` | `cmd` · `exit 0 · 2.3 s` · the label (below), or `evicted`. A non-zero exit is drawn in `states.error` red with the word `failed`, and a running command, which is not an entry yet, as the last row from the tracker's live state: `● running · 1m20s` |
  | `approval` | `ask` · `pane.send → pane 3` · `allowed once`, `allowed 30 min` in `chrome.accent`, or `denied` in red. The action's word comes from `ApprovalAction`, never from the prompt's texts |
  | `api` | `api` · the method · `ok`, `refused` or `error`, and `→ pane N` when it named another pane. Hidden by default except `refused` and `error`; `a` shows them all |
  | `state` | The status's glyph, colour and word, then `reported` or `from output` in `chrome.dim` |
  | `checkpoint` | `files` · `8 changed since launch` · the reason (`launch`, `done`, `exit`, `api`, `view`, `restore`), and `incomplete` in yellow when it is |
  | `exit` | `exited` · `status 1` or `signal 9` |

  The **label** of a command is the text of its stored row from the `B` mark's column to the
  row's end, at most 80 cells, read by the core from the pane's own scrollback when the window
  asks for the rows on screen (section 6), passed through the same cleaning as a `pane.status`
  message (control and bidi characters dropped, `top_bar`'s `safe`), and cut to the space left
  with `…`. It is never stored, never in the API, and absent for an evicted row. It is the pane's
  own text shown in the pane's own window, which the person sees by scrolling anyway (open
  question 2).
- **Footer:** `⏎ jump · d diff at checkpoint · a api calls · n/p next/previous error · esc`,
  and under it, in `chrome.dim`, `commands and exit codes come from the shell's marks, which
  output can print`.
- **Keys:** ↑↓ move, PgUp PgDn Home End, `n` and `p` to the next or previous failed command or
  ✕, Enter acts (a command jumps and closes the overlay; a checkpoint opens the diff viewer at
  it; others do nothing), `d` opens the diff viewer at the nearest checkpoint at or after the
  entry, `a` toggles API calls, Esc or prefix `t` closes. A paste is dropped. Mouse: a click
  selects, a second click on the same row acts, the wheel scrolls.
- **Size:** at most 100 columns wide and the window's rows less 4, at least 40×6, scrolling
  past that. Narrower windows drop the label, then the time.
- **Live:** while open, new entries append and the cursor stays where it is unless it was on
  the newest row, as a log viewer does. The summary line and the live `running` row tick once a
  second on the clock's existing tick, only while open.
- **In the inbox** ([ADR-0051](0051-needs-you-inbox-and-alerts.md) section 4): the row's
  message column shows the summary line when the pane has no `pane.status` message, and `t`
  opens the timeline. The Agents board in `docs/design/ui/` gains both, in the slice that draws
  them.

### 6. Link

Version the next free one when this lands (ADR-0050 and ADR-0051 are both taking a version;
[ADR-0043](0043-window-serves-the-previous-link-version.md) gains the row). Each message
carries `since` that version.

| Message | Direction | Content |
| --- | --- | --- |
| `Notice::Summary { pane, started: u32, ended: bool, commands, failed, approvals, denied, changed, errors: u32 }` | core → window | On a change of any counter, coalesced to one per pane per second. `started` is seconds ago, as ADR-0051's `since` is, so the sides share no clock |
| `Request::Timeline { pane, watch: bool }` | window → core | Opens or closes the overlay. The core answers with `Notice::Timeline` and, while watched, sends `Notice::TimelineEntry` for each new entry, coalesced per 100 ms |
| `Notice::Timeline { pane, run: u32, started, dropped: u32, entries: [Entry] }` and `Notice::TimelineEntry { pane, entry }` | core → window | Entries as section 2 encodes them, 16 bytes each on the wire, at most 1,024; no labels |
| `Request::TimelineLabels { pane, rows: [u32] }` | window → core | At most 64 stored-row numbers, the rows on screen, sent after each move, coalesced |
| `Notice::TimelineLabels { pane, labels: [(row: u32, held: bool, text: &str)] }` | core → window | One per row asked, `text` empty when not held, at most 80 cells, cleaned as section 5 says |
| `Request::RevealStored { pane, number: u32 }` | window → core | The core maps a stored row to a shown row and scrolls as `Reveal` does; an evicted row is refused with `Notice::Refused`, which the window flashes |
| `Request::OpenTool` gains `checkpoint: Option<u32>` for `diff` | window → core | ADR-0050's `tool.open { checkpoint }` from a window key; whichever ADR lands first adds it |

At the older spoken version the window never sends the three requests, so prefix `t` and the
palette entry flash `timeline needs the core updated`, and the core never sends the notices,
so the inbox shows messages only. Every other key works.

### 7. API, CLI and MCP

| Method | Parameters | Who may call | Result |
| --- | --- | --- | --- |
| `run.timeline` | `pane`, `count` (1–1,000, default 200), `before` (an entry's `seq`, for paging), `kinds` (a subset of the six, default all) | The pane itself (`self`), a pane for its `children`, or a terminal outside every pane (`outside`), not asked, as `audit.read` is not: it returns metadata and no way to run anything (ADR-0042). Any other pane is refused (open question 6) | `{ run, pane, started_ms, ended_ms, summary: {...}, dropped, entries: [...] }`. Each entry has `seq`, `at_ms`, `kind`, and the kind's fields as JSON numbers and fixed enums; a `command` has `source: "marks"` and `held: bool`. No string field but enums |

`pane.list` gains `summary`, the counters, for a pane with a run and `null` otherwise, under the
scope it already needs: counters are of the sensitivity of the `status` and `exit` it already
gives. `events.subscribe` is unchanged; `pane.wait` already waits for a command to finish.
`run.timeline` has a JSON Schema in `docs/api/`, an audit entry (method, scope, target pane)
and an MCP tool `run_timeline_v1`, as the others are named in `crates/pinfold-cli/src/mcp.rs`,
so an agent can answer "what did you do" from the record rather than its memory.
`pinfold ctl timeline [pane] [--json] [--all]` prints it as a table, `--all` with the API
calls, and `pinfold ctl list` gains a `summary` column.

### 8. Config

Read when the core starts, as `[api]` and the agent profiles are:

```toml
[timeline]
enabled = true   # keep a timeline for every agent run
entries = 1024   # per live run, 64 to 4096; a closed run keeps 128
```

With `enabled = false` no run record gets a ring or counters, the inbox shows messages only,
and prefix `t` flashes `timelines are off in pinfold.toml`. ADR-0050's run records and
checkpoints are unaffected.

### 9. Platforms

None in the core or the API. The window's keys are the same on macOS and Linux; the overlay
is cells on the renderer each already has. The only platform text in the view is a
checkpoint entry's store, which is ADR-0050's, and it is not shown.

## Threat-model changes

Each marked "proposed by ADR-0052" until built:

- **T2** (an agent reads secrets from other panes), control noted: `run.timeline` returns
  numbers and fixed enums, never a command's text, output, message, path or title, for the
  caller's own pane, its children and a terminal outside every pane; another pane's is
  refused. Test: `a_timeline_holds_no_pane_text_whatever_the_pane_printed` feeds a run hostile
  output, a secret-looking `pane.status` message and a command line holding a token, and checks
  every `run.timeline` field against a fixed grammar; `the_entry_type_has_no_string_and_is_16_bytes`.
- **T4** (spoofed status), row added: command entries and their exit codes come from OSC 133,
  which output can print. The API labels them `source: "marks"`, the window says so in the
  overlay's footer, and a `state` entry is written only for a report or an agent CLI's signal
  in an agent pane (ADR-0046), never for a change the marks made. A program can also mark any
  row of its own output as a command's start, so a jump is a hint. Test:
  `fake_marks_make_command_entries_labelled_from_output_and_never_a_state_entry`.
- **T9** (secrets at rest), residual noted: nothing of the timeline is written to disk but the
  snapshot's counters, no text; the audit file is as today.
- **Controls table,** rows added: the overlay is drawn by the window after the panes, so output
  can neither fake nor answer it (`output_that_prints_the_timeline_cannot_answer_it`, a golden
  image with hostile output under it); labels are read live from the pane's own store, cleaned
  as messages are, never kept, never over the API, and absent for evicted rows
  (`a_label_is_cleaned_and_an_evicted_row_has_none`).
- **Authorization table,** row added: `run.timeline` · Granted, for its own pane and its
  children · Read the record of a run's commands, calls, approvals, states and checkpoints,
  never its text.
- **Hardening untrusted input,** OSC 133 row: adds "and timeline anchors; a jump lands where the
  program marked".

## Memory

| Where | What | Cost | How `cargo xtask mem-gate` checks it |
| --- | --- | --- | --- |
| Core, per live run | A ring of 1,024 × 16 B, its `VecDeque` header, 9 counters and two times | ≤ 17 KiB | New row **Core, 8 agent panes with full timelines**: `--idle 8` with agent profiles whose shell prints 1,100 OSC 133 command pairs each, so every ring fills and evicts, measured after settle against `CORE_LIMIT` |
| Core, closed run | 128 entries and the counters | ≤ 2.2 KiB; 56 closed runs ≤ 125 KiB | The same row, after the shells exit |
| Core, 8 idle panes | No run, so nothing | 0 | The existing row must hold unchanged |
| Core, a label read | One decoded block per distinct block among ≤ 64 rows, transient, as a frame of history costs (ADR-0029) | ≤ 64 KiB a block, freed after the reply | Not gated; the reactor stall is bounded as a history read is |
| Link | `Notice::Timeline` ≤ 1,024 × 16 B + head; `Notice::TimelineLabels` ≤ 64 × 84 B; `Notice::Summary` 40 B, ≤ 1 a second a pane | ≤ 22 KiB per open, 5.4 KiB per label batch | Within the 2 MiB write queue cap of ADR-0032 |
| Window, overlay open | ≤ 1,024 entries × 16 B, 64 labels × 80 B, row buffers reused as the inbox's are | ≤ 22 KiB | The window rows gain a `timeline: true` case beside `menu: true` and ADR-0051's `inbox: true`, against the same window budget |
| Snapshot | ≤ 64 panes × ~160 B of counters | ≤ 10 KiB, under the 1 MiB cap | A size test, not mem-gate |
| Disk | Nothing | 0 | — |

These are type sizes and caps, not measurements. Slice 1 and slice 5 each paste their mem-gate
table. The core budget of 4 MB (ADR-0021) is unchanged.

## Implementation plan

Each slice is about 400 changed lines, one concern, with its tests. Slice 1 shares the run
record with ADR-0050's slice 5: whichever lands first creates `crates/pinfold-core/src/run.rs`,
and the other adds to it.

1. **Core: the ring and the marks.** `crates/pinfold-core/src/timeline.rs`: `Entry` (16 bytes,
   asserted), `Timeline` (ring, cap, dropped count, `Summary`), `[timeline]` in
   `pinfold-config`. The VT engine keeps the cursor at the `B` mark beside `C` and `D` and
   reports a closed command as row, duration and exit; the tracker writes `command`, `exit` and
   `state` entries. Tests: the ring drops the oldest and counts it; 10,000 fake commands stay at
   the cap; a non-zero exit counts once in `failed` and `errors`; a mark-made state writes no
   `state` entry; a report does; `enabled = false` keeps nothing.
2. **Core: calls, approvals and checkpoints.** `audit::reply` writes an `api` entry for a call
   whose caller or target has a run; `Grants::answer` and expiry write `approval` entries;
   ADR-0050's take writes `checkpoint` entries and its JSON line gains `changed`. Tests: a
   `pane.send:other` asked and allowed once shows as one `approval` and one `api` entry; a
   refused call is recorded with its outcome; a run's `api` entries match `audit.read` filtered
   by its pane; a `pane.status` report from a plain shell pane opens a run with no checkpoints.
3. **API and CLI.** `run.timeline` with its schema, scopes, audit and MCP tool `run_timeline_v1`;
   `pane.list` `summary`; `pinfold ctl timeline` and the `summary` column. Tests: `self`,
   `children` and an outside terminal are answered and not asked, another pane and a caller with
   no terminal are refused; the grammar test of T2; paging with `before`; `kinds` filters;
   schema validation of a recorded session.
4. **Link.** `Notice::Summary`, `Request::Timeline`, `Notice::Timeline` and `TimelineEntry`,
   `Request::TimelineLabels` and `Notice::TimelineLabels`, `Request::RevealStored`,
   `Request::OpenTool.checkpoint`; the version raised, ADR-0043's row. Tests: round trips of
   every message; an older window gets no notice and its requests are never sent; a label is
   cleaned of control and bidi bytes and cut at 80 cells; an evicted row has an empty label and
   its reveal is refused; a `RevealStored` lands on the row `shown_index` names at two widths.
5. **Window: the overlay.** `crates/pinfold-ui/src/timeline.rs`: prefix `t`, the palette entry,
   drawing, keys, the API-call toggle, Enter and `d`, live append, the older-core flash.
   Tests: golden images at 80 and 120 columns with every kind, a hostile label and an evicted
   row; every `state` row has glyph, colour and word; a paste is dropped; a narrow window drops
   the label then the time; `n`/`p` skip to errors; output under the overlay answers nothing;
   mem-gate `timeline: true` row.
6. **Inbox, snapshot and docs.** The inbox row's summary and its `t`; the snapshot's `summary`
   and the `was running` strip's line; the Agents board; `docs/configuration.md`,
   `docs/api/README.md`, the threat-model rows from proposed to built with their test names;
   the core mem-gate row. Tests: a snapshot round-trips with and without `summary`; a version 1
   snapshot without it parses; goldens of the strip and the inbox with a summary.

## Open questions for the maintainer

1. **Whose timeline.** Only `agent.launch` runs, as ADR-0050 defines a run, or also a pane
   whose agent reports `pane.status` from a plain shell, which ADR-0051 lists? Proposed: both,
   the second with no checkpoints.
2. **Command labels.** The window shows a command's own line, read live from the pane's
   scrollback and never recorded or sent over the API. Is that within "records, never pane
   text", or should the view show only exit codes and times and leave the text to the jump?
3. **API calls.** Record every audited call of the run (proposed, hidden in the view unless
   refused or `a` is pressed), or approvals only? A polling agent fills a 1,024 ring in an hour
   with `pane.wait`.
4. **Counters in the snapshot,** so the `was running` strip can say what the run did, or keep
   the snapshot exactly ADR-0034's?
5. **Key.** Prefix `t` and a palette entry, as the tools use prefix `f`, `g` and `m`, or a
   Ctrl+Shift key as ADR-0051 proposes for the inbox?
6. **Another pane's timeline.** Refused, as proposed, or askable in the window like
   `pane.read:other`, which would make it the first approval for metadata?
7. **Caps.** 1,024 live entries, 128 closed, 1,000 per `run.timeline` call, 80-cell labels,
   64 labels a request. ADR-0050's 64 runs bounds the total.
8. **ADR-0050's take gains `changed`,** one `diff-tree` in the store per checkpoint. Accept
   that small addition there, or count files in the core from `checkpoint.list` instead?
9. **Evicted output.** A flash and `d` to the nearest checkpoint, as proposed, or should an
   evicted command's entry be hidden from the view?
10. **Surviving a restart.** A metadata file per run under the state directory, 16 KiB and no
    text, would let `pinfold ctl timeline` answer after a reboot. Now, or a later ADR with
    ADR-0050's open question 4 on runs from before a restart?

## Consequences

Coming back to an agent, the person opens one view and reads what it ran, what failed, what it
asked, when it waited and what it changed, in time order, and jumps to the output that is still
there or the diff that always is. Everything the view shows is already parsed by the core, so
the cost is a 16 KiB ring per run and an overlay in the window, with nothing new on disk and no
text kept. The inbox's rows say what each run did, not only what state it is in. An agent can
read its own record over the API, and `pinfold ctl` the person's.

The timeline is as honest as its sources: commands and exit codes come from marks a program
can print, a jump is a hint, and evicted output is gone. It ends with the core, as the run
does. The core gains six entry kinds, one method, five link messages and a ring to keep under
the budget. Revisit when spill files arrive, which would let a transcript replace the jump for
evicted output; when runs outlive a restart in ADR-0050; or if agents stop using shells, which
would leave the command column to their own tool calls.
