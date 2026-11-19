# ADR-0052: Session replay: a timeline of records per agent run, drawn in cells

- **Status:** Accepted (accepted by the maintainer, 2026-11-18)
- **Date:** 2026-11-14
- **Amended 2026-11-19**, decided by the coordinator for the maintainer: where slice 1 keeps the
  timeline and how its file is named; see "Amendment 2026-11-19" at the end.
- **Amended 2026-11-19 again**, decided by the coordinator for the maintainer: how slice 2 joins
  a timeline to its ADR-0050 run, records restores and approvals, and what its mem-gate rows
  hold; see "Amendment 2026-11-19: slice 2" at the end.

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

Option A, with the maintainer's answers of 2026-11-18 folded in (see "Decisions on the open
questions") and the maintainer's rule for this work: no resident memory growth. Option A's ring
therefore lives in a small file of fixed-size records on disk, not in the core's memory: the core
keeps only a run's counters, and the window asks for the rows it shows. Tracked in
[#450](https://github.com/0xPiranhaCodes/pinfold.sh/issues/450).

### 1. Whose timeline

Every run of [ADR-0050](0050-rewind-an-agents-changes.md) has one timeline, kept in its run
record. A pane that [ADR-0051](0051-needs-you-inbox-and-alerts.md) lists but `agent.launch` did
not open (its agent reported `pane.status` from a plain shell pane) gets a run record at its
first report, with no checkpoints, so its commands, calls and states are kept too (decision 1). Tool panes and plain shell panes without a report never have one. The run's pane
is the one `agent.launch` opened or the one that reported; a run ends when its pane's program
exits or the pane closes, and stays reachable for this core's life within ADR-0050's cap of 64
runs, oldest closed first.

### 2. What is recorded

An **entry** is 16 bytes: a kind, a time in whole milliseconds since the run started (`u32`,
saturating), and fields the kind defines. It holds no string. The kinds:

| Kind | Written when | Fields | Source and its label |
| --- | --- | --- | --- |
| `command` | The shell's OSC 133 `D` closes a command | The stored-row number the cursor was on at the `B` mark (`Scrollback::end()` plus the cursor's line, `u32`), the duration from `C` to `D` in ms (`u32`), the exit status (`Option<u8>`) | Marks. Output can print them (T4), so the API says `source: "marks"` and the window says so once in its footer |
| `api` | `audit::reply` records a call whose caller or target is the run's pane, and the call writes or acts (`pane.send`, `pane.spawn`, `pane.close`, `agent.launch`, `pane.status`, `notify`, `tool.open`, `checkpoint.take`, `checkpoint.restore`, `session.*`), or was refused or failed. Read-only calls that succeed (`pane.wait`, `pane.read`, `pane.list`, `pane.last_output`, `events.subscribe`, `audit.read`, `checkpoint.list`, `run.timeline`) are not recorded (decision 3) | The audit `seq` (low 32 bits), the method (an index into the `Method` table, `u8`), the outcome (`ok`, `refused`, `error`), the target pane (`u16`) | The audit log, which already holds no text |
| `approval` | A window prompt about the run's pane, as caller or target, is answered | The `ApprovalAction` (`u8`), the answer (`once`, `thirty_minutes`, `denied`, `expired`), the other pane (`u16`), the grant id (`u32`) | The window's key, through `Grants::answer` |
| `state` | The tracker's shown status changes through a `pane.status` report or an agent CLI's OSC 7501 or OSC 9 signal ([ADR-0046](0046-agent-panes-read-needs-you-from-output.md)) | The status (`u8`), whether it was reported | Not written for a change the shell's marks make, which the `command` entry already says |
| `checkpoint` | ADR-0050 takes one for this run | `n` (`u16`), the reason (`u8`), whether it is complete, and how many paths differ from the launch checkpoint (`u32`) | ADR-0050's `pinfold checkpoint take`, whose JSON line gains `changed`: one `diff-tree -r --name-only` between refs in the store, no work tree (decision 8; ADR-0050 section 7 lists it) |
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

- **On disk, not in memory.** Each run's entries are a ring file,
  `<state>/timeline/<run>`, mode 0600 in a 0700 directory checked as ADR-0050's store is: a
  16-byte header (head, count, dropped) and at most `timeline.entries` (1,024) slots of 16 bytes,
  16 KiB, written in place with one `pwrite` per entry, as the audit file is written. When a run
  ends its file is cut to its newest 128 entries, 2 KiB. With ADR-0050's 64 runs, the disk in all
  is at most 1 MiB. It holds no text: the same 16-byte entries the API returns.
- **In the core,** per run: the summary counters and two times (44 bytes), the file's head and
  dropped count (8 bytes), and the file descriptor while the run is live, 56 bytes, at most 3.5 KiB
  for 64 runs. No entry is kept in memory; `run.timeline` and the window read the file when asked.
- **Its life is the core's.** The timeline directory is emptied when the core starts and a run's
  file is deleted when ADR-0050 evicts the run. A restart restores shells, not agents (ADR-0034),
  so a run's timeline ends with the core, as the run does; keeping it across a restart is a later
  ADR (decision 10). Two things do outlive it: the audit file, when `api.audit_log` is on, and
  the checkpoints in ADR-0050's store.
  Each pane of the snapshot gains an optional `summary` object of the counters above, about
  160 bytes, no text, so the `was running` strip can say
  `○ was running: claude · 12 commands · 3 approvals · 8 files changed · 1 error` (decision 4).
  An old snapshot without it parses as before.
- **Caps on reads.** A label read (section 5) decodes at most 64 stored rows per request.
  `run.timeline` returns at most 1,000 entries per call, read from the file, 16 KiB transient.
  The window asks for at most 64 entries at a time, the rows on screen.

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
  the person can still see what changed around then (decision 9).

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
  | `api` | `api` · the method · `ok`, `refused` or `error`, and `→ pane N` when it named another pane. Only calls that write or act, and refusals and errors, are recorded; `ok` rows are hidden by default and `a` shows them |
  | `state` | The status's glyph, colour and word, then `reported` or `from output` in `chrome.dim` |
  | `checkpoint` | `files` · `8 changed since launch` · the reason (`launch`, `done`, `exit`, `api`, `view`, `restore`), and `incomplete` in yellow when it is |
  | `exit` | `exited` · `status 1` or `signal 9` |

  The **label** of a command is the text of its stored row from the `B` mark's column to the
  row's end, at most 80 cells, read by the core from the pane's own scrollback when the window
  asks for the rows on screen (section 6), passed through the same cleaning as a `pane.status`
  message (control and bidi characters dropped, `top_bar`'s `safe`), and cut to the space left
  with `…`. It is never stored, never in the API, and absent for an evicted row. It is the pane's
  own text shown in the pane's own window, which the person sees by scrolling anyway (decision
  2).
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
  the newest row, as a log viewer does. The window holds only the page on screen, at most 64
  entries, and asks for the next page as the cursor moves; it frees them when the overlay closes. The summary line and the live `running` row tick once a
  second on the clock's existing tick, only while open.
- **In the inbox** ([ADR-0051](0051-needs-you-inbox-and-alerts.md) section 4): the row's
  message column shows the summary line when the pane has no `pane.status` message, and `t`
  opens the timeline. The Agents board in `docs/design/ui/` gains both, in the slice that draws
  them.

### 6. Link

Version 18: [ADR-0050](0050-rewind-an-agents-changes.md) takes 16 and
[ADR-0051](0051-needs-you-inbox-and-alerts.md) 17. A release that includes all three moves the
link from 15 to 18, more than one apart, and restarts the session per
[ADR-0043](0043-window-serves-the-previous-link-version.md), which gains the row for 18. Each
message carries `since` 18.

| Message | Direction | Content |
| --- | --- | --- |
| `Notice::Summary { pane, started: u32, ended: bool, commands, failed, approvals, denied, changed, errors: u32 }` | core → window | On a change of any counter, coalesced to one per pane per second. `started` is seconds ago, as ADR-0051's `since` is, so the sides share no clock |
| `Request::Timeline { pane, watch: bool, before: Option<u32>, count: u8 }` | window → core | Opens, pages or closes the overlay: at most 64 entries before `before`, newest if absent. The core answers with `Notice::Timeline` read from the run's file and, while watched, sends `Notice::TimelineEntry` for each new entry, coalesced per 100 ms |
| `Notice::Timeline { pane, run: u32, started, dropped: u32, total: u32, entries: [Entry] }` and `Notice::TimelineEntry { pane, entry }` | core → window | Entries as section 2 encodes them, 16 bytes each on the wire, at most 64 a page; no labels |
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
| `run.timeline` | `pane`, `count` (1–1,000, default 200), `before` (an entry's `seq`, for paging), `kinds` (a subset of the six, default all) | The pane itself (`self`), a pane for its `children`, or a terminal outside every pane (`outside`), not asked, as `audit.read` is not: it returns metadata and no way to run anything (ADR-0042). Any other pane's timeline is refused, never askable (decision 6) | `{ run, pane, started_ms, ended_ms, summary: {...}, dropped, entries: [...] }`. Each entry has `seq`, `at_ms`, `kind`, and the kind's fields as JSON numbers and fixed enums; a `command` has `source: "marks"` and `held: bool`. No string field but enums |

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
enabled = true   # keep a timeline for every agent run: 56 bytes in memory, the rest on disk
entries = 1024   # per live run on disk, 64 to 4096; a closed run keeps 128
```

It is on by default because its resident cost is a fixed 56 bytes a run and no text; the entries
are on disk. With `enabled = false` no run record gets a file or counters, the inbox shows
messages only, and prefix `t` flashes `timelines are off in pinfold.toml`. ADR-0050's run records and
checkpoints are unaffected.

### 9. Platforms

None in the core or the API. The window's keys are the same on macOS and Linux; the overlay
is cells on the renderer each already has. The only platform text in the view is a
checkpoint entry's store, which is ADR-0050's, and it is not shown.

## Threat-model changes

Each naming ADR-0052:

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
- **T9** (secrets at rest), residual noted: the timeline is written to disk only as 16-byte
  records with no text, 0600 in a 0700 directory under the state directory, emptied when the
  core starts, plus the snapshot's counters, no text; the audit file is as today. Test:
  `the_timeline_file_holds_only_entries_and_is_emptied_at_start`.
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
| Core, per run | Counters and two times 44 B, file head and dropped 8 B, a descriptor while live | 56 B; ≤ 3.5 KiB at 64 runs | New row **Core, 8 agent panes with full timelines**: `--idle 8` with agent profiles whose shell prints 1,100 OSC 133 command pairs each, so every ring fills and evicts, measured after settle; ≤ 56 B a run over the 8-agent-pane row, and the same after the shells exit |
| Core, 8 idle panes | No run, so nothing | 0 | The existing row must hold unchanged |
| Core, a read | One page of ≤ 64 entries for the window, ≤ 1,000 for `run.timeline`, read from the file and freed after the reply; a label read decodes one block per distinct block among ≤ 64 rows, as a frame of history costs (ADR-0029) | ≤ 16 KiB, and ≤ 64 KiB a block, transient | Not gated; the reactor stall is bounded as a history read is |
| Link | `Notice::Timeline` ≤ 64 × 16 B + head; `Notice::TimelineLabels` ≤ 64 × 84 B; `Notice::Summary` 40 B, ≤ 1 a second a pane | ≤ 1.1 KiB per page, 5.4 KiB per label batch | Within the 2 MiB write queue cap of ADR-0032 |
| Window, overlay open | ≤ 64 entries × 16 B, 64 labels × 80 B, row buffers reused as the inbox's are, freed on close | ≤ 6.2 KiB | The window rows gain a `timeline: true` case beside `menu: true` and ADR-0051's `inbox: true`, against the same window budget |
| Snapshot | ≤ 64 panes × ~160 B of counters | ≤ 10 KiB, under the 1 MiB cap | A size test, not mem-gate |
| Disk | One ring file per run: 16 KiB live, 2 KiB closed | ≤ 1 MiB at 64 runs, emptied at core start | A disk test, not mem-gate |

These are type sizes and caps, not measurements. Slice 1 and slice 5 each paste their mem-gate
table, and every other slice names its row. The core budget of 4 MB (ADR-0021) is unchanged.

## Implementation plan

Each slice is about 400 changed lines, one concern, with its tests. Slice 1 shares the run
record with ADR-0050's slice 5: whichever lands first creates `crates/pinfold-core/src/run.rs`,
and the other adds to it.

1. **Core: the ring file and the marks.** `crates/pinfold-core/src/timeline.rs`: `Entry` (16
   bytes, asserted), `Timeline` (the ring file, its header, `Summary`), `[timeline]` in
   `pinfold-config`. The VT engine keeps the cursor at the `B` mark beside `C` and `D` and
   reports a closed command as row, duration and exit; the tracker writes `command`, `exit` and
   `state` entries. Tests: the ring drops the oldest and counts it; 10,000 fake commands stay at
   the cap; a non-zero exit counts once in `failed` and `errors`; a mark-made state writes no
   `state` entry; a report does; `enabled = false` keeps nothing; the directory is checked and
   emptied at start; a symlinked file is refused. mem-gate: the full-timelines core row.
2. **Core: calls, approvals and checkpoints.** `audit::reply` writes an `api` entry for a call
   whose caller or target has a run; `Grants::answer` and expiry write `approval` entries;
   ADR-0050's take writes `checkpoint` entries and its JSON line gains `changed`. Tests: a
   `pane.send:other` asked and allowed once shows as one `approval` and one `api` entry; a
   refused call is recorded with its outcome; a run's `api` entries match `audit.read` filtered
   by its pane; `pane.wait` and `pane.read` that succeed write nothing, refused ones do; a
   `pane.status` report from a plain shell pane opens a run with no checkpoints. mem-gate: the
   full-timelines row holds with a polling agent.
3. **API and CLI.** `run.timeline` with its schema, scopes, audit and MCP tool `run_timeline_v1`;
   `pane.list` `summary`; `pinfold ctl timeline` and the `summary` column. Tests: `self`,
   `children` and an outside terminal are answered and not asked, another pane and a caller with
   no terminal are refused; the grammar test of T2; paging with `before`; `kinds` filters;
   schema validation of a recorded session. mem-gate: the row after 100 `run.timeline` calls of
   1,000 entries, unchanged after settle.
4. **Link.** `Notice::Summary`, `Request::Timeline`, `Notice::Timeline` and `TimelineEntry`,
   `Request::TimelineLabels` and `Notice::TimelineLabels`, `Request::RevealStored`,
   `Request::OpenTool.checkpoint`; the version raised, ADR-0043's row. Tests: round trips of
   every message; an older window gets no notice and its requests are never sent; a label is
   cleaned of control and bidi bytes and cut at 80 cells; an evicted row has an empty label and
   its reveal is refused; a `RevealStored` lands on the row `shown_index` names at two widths;
   paging returns at most 64. mem-gate: link and core rows unchanged at idle.
5. **Window: the overlay.** `crates/pinfold-ui/src/timeline.rs`: prefix `t`, the palette entry,
   drawing, keys, the API-call toggle, Enter and `d`, live append, the older-core flash.
   Tests: golden images at 80 and 120 columns with every kind, a hostile label and an evicted
   row; every `state` row has glyph, colour and word; a paste is dropped; a narrow window drops
   the label then the time; `n`/`p` skip to errors; output under the overlay answers nothing;
   mem-gate `timeline: true` row, ≤ 6.2 KiB over the window row, zero after close.
6. **Inbox, snapshot and docs.** The inbox row's summary and its `t`; the snapshot's `summary`
   and the `was running` strip's line; the Agents board; `docs/configuration.md`,
   `docs/api/README.md`, the threat-model rows from proposed to built with their test names;
   the core mem-gate row. Tests: a snapshot round-trips with and without `summary`; a version 1
   snapshot without it parses; goldens of the strip and the inbox with a summary. mem-gate: the
   window `inbox: true` case with summaries, and the snapshot size test.

## Decisions on the open questions

The maintainer answered each on 2026-11-18.

1. **Whose timeline:** both `agent.launch` runs and panes whose agent reports `pane.status`, the
   latter with no checkpoints. ADR-0051 lists both, so both should say what they did.
2. **Command labels:** yes, the command's own line, read live from the pane's scrollback, never
   stored or sent over the API. It is the pane's own text in its own window.
3. **API calls:** record approvals, refusals and calls that write or act, not read-only polling
   such as `pane.wait` or `pane.read`. A polling agent then cannot fill the ring with noise.
4. **Counters in the snapshot:** yes, so the `was running` strip says what the run did. About
   160 bytes a pane and no text.
5. **Key:** prefix `t` and a palette entry, as the tools use prefix keys; ADR-0051 moved the inbox
   to prefix keys too.
6. **Another pane's timeline:** refused. It would be the first approval for metadata, for little
   gain.
7. **Caps:** as proposed, 1,024 live entries, 128 closed, 1,000 per call, 80-cell labels, 64 labels
   a request. On disk they cost no resident memory.
8. **ADR-0050's take gains `changed`:** yes, one `diff-tree` in the store per checkpoint, so the
   core counts nothing.
9. **Evicted output:** a flash and `d` to the nearest checkpoint. The entry still says what ran
   and how it ended.
10. **Surviving a restart:** a later ADR, together with ADR-0050's question 4.

## Consequences

Coming back to an agent, the person opens one view and reads what it ran, what failed, what it
asked, when it waited and what it changed, in time order, and jumps to the output that is still
there or the diff that always is. Everything the view shows is already parsed by the core, so
the cost is 56 bytes a run in the core, a 16 KiB file of records per run on disk, and an overlay
in the window, with no text kept. The inbox's rows say what each run did, not only what state it is in. An agent can
read its own record over the API, and `pinfold ctl` the person's.

The timeline is as honest as its sources: commands and exit codes come from marks a program
can print, a jump is a hint, and evicted output is gone. It ends with the core, as the run
does. The core gains six entry kinds, one method, five link messages and a small ring file per
run. Revisit when spill files arrive, which would let a transcript replace the jump for
evicted output; when runs outlive a restart in ADR-0050; or if agents stop using shells, which
would leave the command column to their own tool calls.

## Amendment 2026-11-19: slice 1, the ring file as built

Decided by the coordinator for the maintainer while building slice 1.

- **Where.** ADR-0050's run record exists only with `[rewind]` on for the profile, and lives in
  the core's event loop, not in the `Reactor` that reads marks. Section 8 makes `[timeline]`
  independent of `[rewind]`. So the timelines live in the `Reactor`
  (`crates/pinfold-core/src/timeline.rs`), one per agent run: each pane `agent.launch` opens
  from a profile, with `[rewind]` on or off. At most 64, as ADR-0050 keeps; at 64 the oldest
  closed one is deleted with its file, and with all 64 live a new run gets none. Slice 2's
  reporting panes and the join to ADR-0050's run id come with `run.rs`.
- **Name.** `<state>/timeline/r<pane>-<n>`, `n` this core's count of launches, so a pane that
  starts another run gets a new file and the old ring is closed first. (Corrected by slice 2:
  ADR-0050's run ids are `r<core start>-<n>` with their own count, so the names share a shape,
  not a key; the join is by pane.)
- **Header.** Written when the file is made and when the run closes. Each entry is exactly one
  `pwrite`; while a run is live the core's own head is the truth, and a read decodes only the
  slots the core counts. A record that does not decode, or is short, is skipped.
- **Core record, 56 bytes, asserted.** The summary's eight counters, `started` in whole seconds
  since the core started, as ADR-0051's `since` counts, and `ended` in milliseconds after that
  second (40 B); the launch count `n` and `dropped` (8 B); the pane and the head as `u16` each,
  the pane as the `api` entry already holds it (4 B); and the file, open only while the run is
  live (4 B). An entry's `at` counts from the second the run started. A pane numbered past
  65,535 gets no timeline.
- **Exit.** Written when the pane's output ends, with the status `try_wait` has then, or when
  the pane is dropped while its program runs. A program whose end of output comes before it can
  be reaped is recorded as its pane closing.
- **Mem-gate.** `--idle 8` with an agent profile whose program prints 1,100 OSC 133 command
  pairs, live and after it exits, and the same output with `[timeline]` off shown beside them.
  The two rows are held to #476's 5.00 MB launch ceiling, as ADR-0050's run-record row is: main,
  with no timeline code, already reads 4.38 MB there, from the agent launches and their output.
  The 56 B a run is far below a sample's noise, so it is held by the asserted type sizes, and
  the off row shows the difference.

## Amendment 2026-11-19: slice 2, calls, approvals and checkpoints as built

Decided by the coordinator for the maintainer while building slice 2.

- **Join.** A timeline is joined to its ADR-0050 run by pane: the `Run`'s pane and the
  newest timeline of that pane. Pane numbers are never reused within a core and each
  `agent.launch` opens its own pane, so no field is added to `Run`, which stays 48 bytes.
- **Calls.** The core reads the audit log once a turn, before the reactor's turn, from the
  `seq` after the last one it read, and writes an `api` entry into the live run of the
  caller's pane and, for a call that succeeded or came from outside every pane, of the
  target's: a refused or failed call from a pane is the caller's alone, so no pane can flush
  another's ring with calls that do nothing, and a refused prompt shows in the target's run as
  its `approval` entry. The method is an index into `timeline::METHODS`, the
  audit's 23 names, to which new methods are appended. A call that succeeds is recorded only
  if that table marks it as writing or acting; `hello`, `pane.list`, `events.subscribe`,
  `pane.read`, `pane.last_output`, `pane.wait`, `checkpoint.list`, `audit.read`,
  `grants.list` and `workspace.trust_state` are not. The audit log records `ok` and `refused`
  today, so `error` is kept for when it records failures.
- **Approvals.** An entry is written only when a prompt ends, never when it is asked: allowed
  once, allowed 30 minutes, denied, or expired, and a prompt released because its window
  closed counts as expired. A prompt the core denies because no window can show it is
  recorded as denied. A read or send prompt is written into the caller's run and the
  target's, a `Rewind` prompt into its run alone, and a trust or owner prompt into none.
- **Checkpoints and restores.** There is no seventh kind. Every take ADR-0050 answers, launch,
  done, exit, API and view, writes a `checkpoint` entry with its line's `changed`, which
  becomes the summary's. A restore writes a `checkpoint` entry with reason `restore`, `n`
  from its `restore_checkpoint`, and the count it undid, restored plus deleted, in the field
  `changed` holds elsewhere; the summary's `changed` stays as it was. An exit take lands after
  its run closed, so the closed file is reopened, checked as a read is, for that one record.
- **Mem-gate.** Serving API calls keeps about 1.7 MB in the core on main, with timelines on or
  off ([#502](https://github.com/0xPiranhaCodes/pinfold.sh/issues/502)), which puts any row with
  a polling agent over #476's 5.00 MB ceiling. The three slice-1 rows therefore stay as they
  were, and two information rows beside them run the same agents, each then calling
  `pinfold ctl list` 1,000 times, with `[timeline]` on and off. Those 8,000 calls write no entry.
