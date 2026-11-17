# ADR-0051: A "needs you" inbox across every agent, with optional alerts

- **Status:** Accepted (accepted by the maintainer, 2026-11-18)
- **Date:** 2026-11-14

## Context

**Who has the problem.** Someone running several coding agents at once, in several tabs and
splits. ADR-0046 made a pane show ◆ needs you while its agent CLI waits at its own prompt, but
the window still shows only one state per tab, and only for the pane that names it:
`Notice::Status` is sent only for that pane (`pinfold-core/src/link.rs`, `state.naming`), so a
second agent in a split that needs you shows nothing until you look at its tab. The Dock menu
(`pinfold-ui/src/dock_menu.rs`) lists the same tab states. There is no place to see every agent
at once, no key to go to the one that waits, and no alert once you have walked away from the
machine.

**What exists.**

- `pinfold-core/src/status.rs`: each pane's `Tracker` holds its state, whether the agent reported
  it (`reported`) or output did, and whether the pane is an agent profile's (`agent`, ADR-0046).
  Alerts are rate-limited per pane to one per `ALERT_INTERVAL` (10 s), counted apart for API and
  output. The pane keeps the last `pane.status` message, sanitized and at most `MAX_MESSAGE`
  (200) characters. Nothing records when the state last changed.
- `pane.list` already gives every pane's state and source; the message only to its own pane
  ("a message is the caller's own business: other panes' stay in the window",
  `pinfold-core.rs`).
- `pinfold-ui/src/notify.rs` raises a notification when a tab comes to need you unseen: one per
  tab per 60 s, one per 10 s in all, PinFold's own text. `pinfold-sys/src/notify.rs` does it on
  macOS (Dock bounce, plus `NSUserNotificationCenter` where it can). Linux has the tab mark only;
  the code's `shortcut:` comment says a freedesktop notification "needs a D-Bus client crate".
  Errors never alert.
- `docs/design/ui/Agents.dc.html` already draws this view: "AGENTS · needs you first", one row
  per pane numbered tab·pane, with label, last status line and time in state, ⏎ to jump,
  `⌃⇧w next waiting agent`, and a footer "5 agents · 1 needs you". `architecture.md` names the key
  `next_waiting = "C-S-w"` and an `[alerts]` table with `desktop` and `webhook`.
- Issue [#264](https://github.com/0xPiranhaCodes/pinfold.sh/issues/264) holds threat T9's
  placeholder: no webhook exists, and the PR that adds one must send status and never pane text,
  with a test that fails if that control breaks. Its other half, spill files under an in-memory
  key, is unrelated to alerts.
- Review finding M2 (`architecture-review.md`) is open: `notify-rust` pulls in `zbus` and an
  executor thread on Linux, against the no-async rule.

**Constraints.** The core has no async runtime: `mio` and a fixed set of threads, plus a lazy
worker of at most two threads for blocking jobs (ADR-0007). It must never panic on external input.
Budgets: core ≤ 4 MB with 8 idle panes, ≤ 250 KB per idle pane (ADR-0021). All UI is cells
(ADR-0005, ADR-0011); menus run existing commands (ADR-0033); prompts PinFold draws sit outside
pane text. `webhook.*` is never over the API (threat model, Authorization). One core holds the
user's one session in v0.1 (ADR-0034, point 8), so "every tab of the session" is every pane the
user has.

**Crates checked on 2026-11-14.** `zbus` 6.3.0: its default `default-rt` feature brings its own
async runtime (`zruntime`), the other choice is `tokio`. `notify-rust` 4.19.0: Linux backend is
`zbus` 6 or `dbus` 0.9, macOS needs `mac-notification-sys`. `dbus` 0.9.12 links libdbus through
`libdbus-sys`, a C library. `ureq` 3.5.2: blocking, but HTTPS needs `rustls`, whose default
provider is `ring` (C and assembly), or `native-tls` (OpenSSL on Linux). Each needs the
maintainer's consent under CLAUDE.md, so the decision below uses none of them.

## Options

**A. Inbox in the window, alerts in the window and the core, no new crate (recommended).** The
core tells the window every agent pane's state and how long it has held it; the window draws the
inbox, the key and desktop notifications, with a small hand-written D-Bus client on Linux. The
core sends webhooks by running the system `curl` with fixed options on its worker thread. Costs:
about 300 lines of D-Bus wire code to fuzz and keep; webhooks need `curl` installed, which macOS
always has and some minimal Linux installs lack.

**B. As A, with crates.** `notify-rust` for desktop alerts and `ureq` with `rustls` for webhooks.
Less of our own code, but an async runtime in the window on Linux (`zbus`) or a C library
(`dbus`, `ring`), TLS inside the one long-lived process that must never panic, and roughly a
megabyte of code in `pinfold-core`. Needs the maintainer under CLAUDE.md for each crate.

**C. Webhooks as a plugin, not in PinFold.** ADR-0011 makes plugins external processes over the
API: a script reads `events.subscribe` and posts. No HTTP code at all. But `events.subscribe`
needs a caller in a terminal (PTY identity), so the script must live in a pane and dies with it;
the script can call `pane.read` and post pane text, so T9's "status only" stops being a property
PinFold enforces; and #264's promise is left unmet. The inbox and desktop half is unaffected.

**D. Inbox only.** Draw the view and the key, keep today's alerts. Smallest, but leaves Linux
without notifications, errors without alerts, and #264 open with nothing to point at.

## Decision

Option **A**, with the maintainer's answers of 2026-11-18 folded in (see "Decisions on the open
questions"): Linux notifications go through `notify-send` rather than a hand-written D-Bus
client, keys are prefix keys in `[keys]`, and webhooks may use plain `http` to loopback.
Webhooks fold in #264's T9 half; spill files stay in #264. A review of competing terminals added
four parts, sections 6 to 9: answering an agent's permission prompt from the inbox, richer rows,
agents running outside PinFold, and program notifications and progress.

The maintainer's rule for this work holds throughout: no resident memory growth. The core keeps
fixed-size fields and no text; anything else is computed when the inbox is open and dropped when
it closes; whatever costs memory or reads more than PinFold's own state is off by default. Tracked
in [#447](https://github.com/0xPiranhaCodes/pinfold.sh/issues/447).

### 1. What the core keeps

- **Listed panes.** A pane is in the inbox when it was launched from an agent profile
  (`Tracker::agent`) or has had a `pane.status` report since it opened (a new `Tracker::listed`
  flag, never cleared by `clear_report`). Tool panes never are (ADR-0045). A plain shell pane is
  not, so every failed `ls` does not reach the inbox.
- **Time in state and run time.** `Tracker` gains `since: u32` and `started: u32`, whole seconds
  since the core started, saturating: `since` set whenever the shown state changes and never by a
  repeat of the same state, `started` when the pane's program starts. 4 bytes each, not a 16-byte
  `Instant`.
- **Profile,** for the webhook and Approve: the `agent.launch` profile as a `u8` index into the
  config's profile table, read when the core starts, never its name as a string. 1 byte.
- **Progress,** section 9: one `u8`.
- Nothing else. The message is the one the pane already keeps. Branch, context use, steps and
  agents outside PinFold are read when the inbox opens and dropped when it closes (sections 7
  and 8).

### 2. Link (version 16)

Reserved as 17, after [ADR-0050](0050-rewind-an-agents-changes.md)'s 16, but this ADR's first
slice reached the link first and took 16, the next version not yet on `main`; the others take the
next free versions as they land. A release that includes all three moves the link from 15 more
than one apart and restarts the session per
[ADR-0043](0043-window-serves-the-previous-link-version.md), which gains the row for 16.

- `Notice::Status` gains `since: u32` and `started: u32`, whole seconds in the state and in the
  run when sent, saturating, and `naming: bool`. The core sends it for every listed pane and for
  each tab's naming pane, on any change of state, source or message, as today. The window turns
  the seconds into `Instant`s, so the two sides share no clock.
- `Notice::Webhook { host: &str, sender: bool, failed: u32 }`, sent after the hello when a
  webhook is configured and again when `failed` changes: the URL's host only (section 5 limits it
  to `A-Z a-z 0-9 . - : [ ]`), whether `curl` was found, and how many alerts were given up on.
- `Request::Inbox { open: bool }`: the window says when the inbox opens and closes. While open,
  and only then, the core answers with `Notice::InboxExtra` (sections 7 and 8) and refreshes it
  every 5 s.
- `Request::Answer { pane, choice: Approve | Interrupt }` (section 6), sent only after the
  person confirms in the window.
- `Notice::Toast { pane, kind: u8, text: &str }` (section 9) and `Notice::Progress { pane,
  progress: u8 }`.
- A jump is `SwitchTab` then `Focus`, which exist. Opening a tab in a folder is the existing new
  tab request with a directory.

A version 15 window gets what it gets today, the naming pane's status only, and never sends the
new requests: prefix `!` and `w` flash `the inbox needs the core updated`.

### 3. API

- `pane.list` gains `since` and `started`, seconds in the current state and in the run, for
  every pane, under the scopes it already needs. Messages stay as they are: the caller's own only.
- No new method or scope. There is no `alerts.*`, `webhook.*` or answering method: alert
  destinations stay config-file only, as the Authorization table says, and Approve and Interrupt
  come only from a person's key in the window (section 6).

### 4. The inbox, drawn in cells

- **Open:** prefix `!` (`keys.inbox`), and an "Agents" entry in the palette (`palette::COMMANDS`).
  It is an overlay drawn after the panes, like the palette and the right-click menu, so every key
  goes to it and program output can neither fake nor answer it.
- **Rows,** one per listed pane of every tab, ordered ◆ needs you, ✕ error, ● running, ✓ done,
  ○ idle, and within a state the longest-held first; then agents outside PinFold (section 8).
  Each row is, left to right: the glyph in its state colour (`theme.states`), the **word** in the
  same colour, padded to 9 cells ("needs you"), `tab·pane` numbers, the tab's label
  (`top_bar::label`, which already drops control, bidi and state glyphs), the git branch, the
  last message, and on the right the run time and the time in state. A state from output shows
  "from output" in `chrome.dim` where the message goes (ADR-0046, point 3). The message is filtered
  again with `top_bar`'s `safe` and cut to the space left with `…`. The word is drawn in every row:
  the board shows glyphs only, but CLAUDE.md asks for glyph, colour and word, so the board is
  updated in the slice that draws it.
- **Second line,** only with `[inbox] agent_files = true` (section 7): context use as `ctx 64%`
  and the last three steps, each cut to fit, in `chrome.dim`.
- **Time:** `m:ss` under an hour, then `3h07`, then `99h+`. The view ticks once a second only
  while open, on the clock's existing tick.
- **Header and footer:** "AGENTS · needs you first", and "N agents · M needs you" (plus
  "webhook: K failed" in red when `failed` > 0, and "webhook: no curl" when `sender` is false).
  No rows: "No agent panes". Footer keys: `⏎ jump · y approve · i interrupt · esc`.
- **Keys:** ↑↓ move, Enter jumps to the row's pane and closes (on an outside agent, opens a tab
  in its folder), `y` Approve and `i` Interrupt on a ◆ row (section 6), Esc or prefix `!` closes.
  A paste is dropped. Mouse: a click on a row jumps.
- **Size:** at most 100 columns wide and the window's rows less 4, at least 40×6; past that it
  scrolls. Narrower windows drop the steps line, then the message, the branch and the label.
- **Memory:** the rows exist only while the inbox is open, built from the window's `Notice::Status`
  state and the core's `InboxExtra`, and are freed when it closes.
- **Next needs you:** prefix `w` (`keys.next_waiting`), from anywhere in the window, focuses the
  ◆ pane waiting longest; pressed again on a ◆ pane, the next one in inbox order. With none it
  does nothing and flashes "Nothing needs you" in the row under the pane, as other notes do.
  Palette entry "Next Needs You".
- **Keys config.** `[keys]` gains `inbox = "!"` and `next_waiting = "w"`, each a single key
  pressed after the prefix. Neither clashes with an existing prefix key (`c x n p 0-9 | % - "`,
  arrows, `z d f g m`) or ADR-0052's `t`; the config refuses a key already bound. Ctrl+Shift
  chords are not used, as they take keys from programs; `architecture.md`'s `C-S-w` is replaced.

### 5. Alerts

**Config,** a new `[alerts]` table in `pinfold-config`, with `deny_unknown_fields`:

| Key | Default | Meaning | Read |
| --- | --- | --- | --- |
| `desktop` | `["needs-you", "error"]` | States that raise a desktop notification; `"needs-you"`, `"error"` or none | By the window, hot-reloaded |
| `webhook` | `""` (off) | An `https://` URL, or `http://` to `127.0.0.1` or `[::1]`, to POST to | By the core, when it starts |
| `webhook_states` | `["needs-you", "error"]` | States that are posted | By the core, when it starts |
| `webhook_format` | `"json"` | `"json"`, or `"text"` for ntfy-style relays | By the core, when it starts |
| `program_notifications` | `false` | Show OSC 9, 99 and 777 text as toasts (section 9) | By the window, hot-reloaded |
| `progress` | `true` | Show OSC 9;4 progress on the tab and the Dock (section 9) | By the window, hot-reloaded |

✕ error now notifies by default. The core takes no hot reload (`pinfold-core.rs`, "Read once"),
so a webhook change applies to the next session; that also satisfies "security-relevant config
changes do not apply on hot reload until you confirm" without a new prompt. A webhook configured
when a window links is shown once in it, as ADR-0044's notice is: "Alerts for ◆ and ✕ go to
`<host>`", so an agent that edited the file cannot add one unseen.

**Desktop notifications** stay in the window, keyed by pane rather than tab index (fixing
`notify.rs`'s `shortcut:`), with today's limits: one per pane per 60 s, one per 10 s in all. Text
is PinFold's: "`<label>` needs you" or "`<label>` error". A listed pane alerts when it comes to
that state while not shown in a focused window. Clicking goes to the pane where the system
reports clicks.

- **macOS:** unchanged mechanism (ADR-0046): Dock bounce, and `NSUserNotificationCenter` where
  it can.
- **Linux:** `notify-send`, the first on an absolute `PATH` entry, found once when the window
  starts, run with a cleared environment but `DBUS_SESSION_BUS_ADDRESS`, `DISPLAY`,
  `WAYLAND_DISPLAY` and `XDG_RUNTIME_DIR`, fixed arguments (`--app-name=PinFold --expire-time=10000
  --`) and PinFold's text last, never through a shell. At most one runs at a time; one due while
  another runs is dropped. It is optional: without it the tab mark stays the only signal, as
  today, and the log says so once. No hand-written D-Bus and no new crate; clicks are not handled
  on Linux in v1.

**Webhooks,** from the core, since the point is to reach someone with no window open.

- **What is posted.** With `webhook_format = "json"`, one JSON object, `Content-Type:
  application/json`, built by PinFold from fixed fields only:
  `{"pinfold":1,"state":"needs-you","source":"agent","agent":"claude","host":"studio","tab":2,"pane":7,"at":1763112000,"since":1763111958}`.
  `state` is a `Status::name`; `source` is `agent` or `output`; `agent` is the profile's name from
  the config, looked up by the index of section 1 when sending, or `null`; `host` is the machine's
  hostname, read when sending, cut to 64 bytes of `A-Z a-z 0-9 . -` (anything else drops it), so
  people with several machines can tell them apart; `at` and `since` are Unix seconds. With
  `"text"`, `Content-Type: text/plain` and one line of the same fields in PinFold's words,
  `◆ needs you · claude · tab 2 pane 7 · studio`, which ntfy shows as is. Never the message, the
  tab label (it comes from OSC 7, which output sets), a title, a command, a directory, pane text,
  a program notification's text or an environment value. A test feeds a pane hostile output and
  a message holding a secret-looking token and checks the body against a fixed grammar of these
  fields, in both formats.
- **URL rules,** checked when the config is read; a bad URL turns webhooks off with a logged
  reason: at most 2048 bytes; scheme `https://`, or `http://` only when the host is `127.0.0.1` or
  `[::1]` (a local relay; never a name, as `localhost` can resolve elsewhere); no userinfo (`@` in
  the authority); host of `A-Z a-z 0-9 . -` or a bracketed IPv6 literal, optional `:port`
  1–65535; path and query of printable ASCII without `"`, `\` or `#`. A secret in the path or
  query, as an ntfy topic is, stays out of logs, the window and the process table: logs and the
  notice name the host only.
- **Sending.** A job on the core's worker (ADR-0007), started lazily, exiting after 60 s idle. It
  runs `curl` found once at core start: `/usr/bin/curl` on macOS, the first `curl` on an absolute
  `PATH` entry on Linux. If none is found, webhooks are off, the log says why once, and the
  window's notice says "webhook: no curl". The environment is cleared except `HTTPS_PROXY`,
  `https_proxy`, `ALL_PROXY`, `all_proxy`, `NO_PROXY` and `no_proxy`. The argument list is fixed,
  `curl --disable --config -`, so `~/.curlrc` is ignored and the URL never reaches the process
  table; standard input carries the config: `url`, `data`, `header`, `user-agent = "pinfold"`,
  `proto = "=https"` (`"=http"` for a loopback URL), `max-redirs = 0`, `connect-timeout = 5`,
  `max-time = 10`, `silent`, `output = "/dev/null"`, `write-out = "%{http_code}"`. Values are
  quoted by one function the URL rules and payload grammar make simple (no `"` or `\` in the URL;
  the payload's own `"` escaped), with a test. Standard error is discarded; at most 16 bytes of
  standard output are read; a child still running after 15 s is killed.
- **Retries.** A 2xx is done. Connect, DNS, TLS or timeout failures (curl exit 6, 7, 28, 35, 52,
  56) and HTTP 429 or 5xx retry after 5 s and then 30 s, three tries in all; anything else is
  given up at once. Given-up alerts count into `failed` and log at `warn` with host, status and
  try, never the URL.
- **Limits.** A webhook follows the tracker's alerts, which are already one per pane per 10 s; on
  top, one per pane per 60 s and at most 10 in any 10 minutes in all. The queue holds 16 fixed
  records of 20 bytes (state, source, profile index, tab, pane, two times), the body built only
  when sent; past 16 the oldest is dropped and counted. A ◆ from output can still post (ADR-0046
  lets an agent pane's output say needs you), labelled `"source":"output"`, within these limits.
  It posts always, whether or not a window shows the pane: the core cannot tell whether the
  person is at the machine.

### 6. Answering an agent's permission prompt from the inbox

On a ◆ row of an agent profile's pane, `y` is **Approve** and `i` is **Interrupt**:

- **What is sent.** Approve sends the profile's `approve` key, set per profile in the config
  (`[agents.claude] approve = "Enter"`, or `"y"`, or `"1"`: one key name from the window's existing
  key table); a profile without one offers Interrupt only. Interrupt sends Ctrl+C. One key, never
  text the agent or its output chose.
- **Who.** Only a person's key in the window, on a row of the open inbox, then the confirmation.
  There is no API method for it, so no agent or tool can ask, and the window never sends
  `Request::Answer` on its own.
- **Confirmation.** PinFold's own prompt, drawn by the window outside pane text as the trust
  prompt is: `◆ needs you` in amber, "Send `Enter` to `claude` in tab 2 · pane 7?" (or "Send
  Ctrl+C to …"), `y` sends once, `n` or Esc cancels. Never a lasting grant.
- **Checks in the core,** on `Request::Answer`: the pane still exists, is still ◆ needs you, is an
  agent profile's pane, and is not protected (`api.protect`, the same match as for API sends); any
  failure refuses with a reason the window flashes, and nothing is sent. The key goes into the
  pane's input as typed keys do.
- **Memory:** nothing kept; the prompt lives while shown.

### 7. Richer rows

- **Always, at no resident cost:** the run time (`started`, section 1) and the git branch. The
  branch is read when the inbox opens and every 5 s while open, by the core, from `HEAD` in the
  `.git` directory found walking up from the pane's directory as the system reports it (never
  OSC 7): at most 256 bytes read, a `ref: refs/heads/` line taken as data, cut to 40 bytes of
  printable ASCII, a detached head shown as its first 7 hex digits. No git runs. It travels in
  `Notice::InboxExtra` and is not kept.
- **Opt-in, `[inbox] agent_files = false` by default:** context use and the last three steps of
  Claude Code and OpenCode, read from their own local session files: for Claude Code the newest
  `*.jsonl` under `~/.claude/projects/` for the pane's directory, for OpenCode the newest session
  under its storage directory (`~/.local/share/opencode/storage/` on Linux, the same under
  `~/Library/Application Support` where it keeps it on macOS).
  - Read by `pinfold agents peek`, a hidden subcommand in `pinfold-tools` that the core starts as
    it starts the checkpoint subcommands of ADR-0050: fixed arguments, the directories after
    `--`, only while the inbox is open, at most one at a time, killed after 2 s. It opens each
    file read-only with `O_NOFOLLOW`, reads at most its last 64 KiB, parses each JSON line
    defensively (bounded depth, unknown fields ignored, a bad line skipped), and prints one JSON
    line per pane of at most 1 KiB: `ctx` as a whole percent or none, and up to three steps, each
    a tool name or the first words of a step, cleaned of control and bidi characters and cut to
    80 cells. A crash or a timeout costs only that process: the rows show no second line.
  - The core forwards the line in `Notice::InboxExtra` and keeps nothing. No text leaves the
    machine: never in a webhook, a notification, the API or a log, which records only that a peek
    ran and how long it took.
- A fuzz target, `agent_session_tail`, covers the parser.

### 8. Agents running outside PinFold

With the same opt-in, `[inbox] agent_files = true`, `pinfold agents peek` also lists sessions
whose file changed in the last 10 minutes and whose directory no PinFold pane is in. Each becomes
a row after PinFold's own: `● running` when its file changed in the last 30 s, `○ idle`
otherwise, glyph, colour and word as always, then `outside PinFold`, the agent's name (Claude
Code or OpenCode, PinFold's words), its directory escaped and shortened from the left with `…`,
and the time since its file changed. Enter on it asks "Open a tab in `<dir>`?" and, on `y`, opens
a new tab there through the existing new tab request; the core checks the directory is an existing
absolute directory, as OSC 7's rule is, and refuses otherwise. At most 16 such rows; they live
only while the inbox is open. The directory comes from the session file, so it is a hint, never
used for anything but that tab.

### 9. Program notifications and progress

- **Toasts, opt-in** (`[alerts] program_notifications = false` by default, as the threat model
  keeps program text out of alerts unless the person asks). When on, OSC 9 text, OSC 99 (kitty's
  notifications: title and body, other keys ignored, never its query or action replies) and OSC
  777 `notify` are shown through the same alert path. The parser already holds the sequence within
  its 4 KB cap; the core cleans the text of control and bidi characters, cuts it to 120 cells,
  forwards it in `Notice::Toast` and keeps nothing. Limits: one per pane per 10 s, as the tracker's
  alerts, and one per 2 s in all; the rest are counted, not queued. The window draws a toast as
  PinFold chrome in the top right of the pane, 5 s, `<label> says:` in `chrome.dim` before the
  text, so it can never look like a system or PinFold prompt; unfocused, the same words go to the
  desktop notification, keyed and limited as above. A toast is never a state: it gets none of
  ◆ ● ✓ ✕ ○, and in an agent pane OSC 9 still also means needs you (ADR-0046). Never in a webhook,
  the API or a log.
- **Progress, on** (`[alerts] progress = true`): OSC 9;4 states (hide, normal with a percent,
  error, indeterminate, paused) are kept as one `u8` per pane, no text, and sent in
  `Notice::Progress` on change, coalesced to one a second per pane. The window draws it as a thin
  bar under the tab's label, in `states.running` blue, or `states.error` red for error, and on
  macOS as a progress bar on the Dock tile (the focused tab's, or the furthest-along). Linux shows
  the tab only. A pane whose program exits clears it. It is on by default because it costs one
  byte a pane and keeps no text.

### 10. Threat model

Rows added or changed, each naming ADR-0051: T9's threat row gains the webhook controls above;
the Controls table splits T9's "Spill files and webhooks" into spill files (still #264) and
webhooks with the tests named in slice 5; T4's window-notification row gains ✕ and Linux; T1
gains Approve and Interrupt (section 6); T6 and T9 gain agent session files (section 7); T4 and
the OSC 9 / 777 hardening row gain program notifications (section 9); T10 gains the
`agent_session_tail` fuzz target; Secure defaults gain the webhook destination, program
notification text and agent session files; Trust boundaries name the one kind of output that
leaves the machine.

### 11. Memory

| Where | What | Cost | mem-gate |
| --- | --- | --- | --- |
| Core, per pane | `since` 4 B, `started` 4 B, `listed` 1 B, profile index 1 B, progress 1 B | 11 B; ≤ 704 B at `MAX_PANES` (64) | "Core, 8 idle panes" and "Each idle pane" hold unchanged |
| Core, webhook | 16 queued records of 20 B, one 64 KiB-stack thread while busy | 320 B; the thread's touched stack while it lives | New "Core, 8 agent panes listed, a webhook after one alert" row, to `https://127.0.0.1:9`, measured after the worker exits |
| Core, inbox open | Branch reads and the peek's lines, forwarded, not kept | 0 resident; ≤ 64 KiB transient | The listed row with the inbox opened and closed, unchanged after close |
| `curl`, `notify-send`, `pinfold agents peek` | Separate short-lived processes | Not in the core's or window's footprint; peek ≤ 4 MB | A peek row over a 64 MiB session file, ≤ 4 MB |
| Window, inbox open | One row per listed pane, buffers reused as `Dock::update` does, freed on close | ~160 B a row; ≤ 74 KB at 64 rows with steps | `inbox: true` case beside `menu: true` |
| Window, inbox closed | Nothing | 0 | The existing window rows hold |
| Window, a toast | One at a time, ≤ 120 cells | ≤ 1 KB while shown | `toast: true` case |

These are type sizes and caps, not measurements; each slice pastes its row.

### 12. Slices

Each about 400 changed lines, one concern, with its tests and its mem-gate row:

1. **Core state and link 16.** `since`, `started`, `listed`, profile index; `Notice::Status`
   fields for every listed pane; `pane.list` `since` and `started`; ADR-0043 row. Tests: `since`
   moves only on a change of state; listed rules; link round trip; a version 15 window gets only
   naming statuses. mem-gate: idle rows unchanged, ≤ 11 B a pane.
2. **The inbox.** `pinfold-ui/src/inbox.rs`, `Request::Inbox`, prefix `!`, `keys.inbox`, palette
   entry, jump. Tests: ordering; every row has glyph, colour and word; "from output"; hostile
   label and message drawn safe; 64 rows scroll; narrow window; a paste is dropped; golden image;
   the Agents board shows the word. mem-gate: `inbox: true`, and zero after close.
3. **Next needs you and desktop config.** Prefix `w`, `keys.next_waiting` and the clash check,
   `[alerts] desktop` with ✕ on by default, limits keyed by pane, `docs/configuration.md`. Tests:
   cycle order; nothing waiting flashes; a key already bound is refused; limits; error text is
   PinFold's. mem-gate: window rows unchanged.
4. **Linux notifications.** `notify-send` lookup, environment, fixed arguments, one at a time.
   Tests with a stub `notify-send` on an absolute `PATH` recording its arguments: fixed
   arguments, text last after `--`, none found logs once and leaves the tab mark. mem-gate:
   window rows unchanged.
5. **Webhook config and payload.** `alerts.webhook`, `webhook_states`, `webhook_format`, URL
   rules with loopback `http`, payload builder with `host`, curl config quoting. Tests:
   `a_webhook_body_holds_only_status_fields_whatever_the_pane_printed` in both formats, a URL
   rule table (userinfo, `http` to a name, `http://localhost`, `http://127.0.0.1` allowed, a
   fragment, a quote, a control character, 2049 bytes), quoting of every value the grammar allows.
   mem-gate: none resident; the config's size test.
6. **Webhook sender.** Worker job, `curl` lookup and its absence, environment, retries, limits,
   queue, `Notice::Webhook` and the window's notice and footer. Tests with a stub `curl`: fixed
   arguments, no URL in them, retry schedule, given-up count, no `curl` turns webhooks off with a
   log line; threat-model rows point at their tests, closing #264's webhook half. mem-gate: the
   webhook row.
7. **Approve and Interrupt.** Profile `approve` key, `y` and `i` in the inbox, the confirmation,
   `Request::Answer` and the core's checks. Tests: only the window's key after `y` sends; a
   protected pane, a pane no longer ◆, and a non-profile pane are refused and nothing is sent;
   output that prints the prompt's text answers nothing; no API method sends it; golden of the
   prompt. mem-gate: window `answer: true` case.
8. **Branch and run time.** The `HEAD` read and `Notice::InboxExtra`'s branch, the run-time
   column. Tests: a hostile `HEAD` (control bytes, 10 KB, a symlink) is cut, cleaned or skipped;
   OSC 7 never chooses the directory. mem-gate: the inbox-open core row.
9. **Agent session files.** `[inbox] agent_files`, `pinfold agents peek`, the second line. Tests
   on recorded Claude Code and OpenCode files and hostile ones (deep nesting, huge lines, bidi,
   a symlinked file); nothing in logs; off reads no file; fuzz target `agent_session_tail` in
   `fuzz-smoke`. mem-gate: the peek row.
10. **Agents outside PinFold.** Outside rows, Enter to open a tab in the folder after its
    confirmation, the directory check. Tests: a session in a PinFold pane's directory is not
    listed twice; a missing or relative directory is refused; at most 16 rows. mem-gate: the
    `inbox: true` case with 16 outside rows.
11. **Program notifications.** OSC 99 parsing, OSC 9 and 777 text kept to the cap, `Notice::Toast`,
    the toast and its desktop path, `[alerts] program_notifications`. Tests: off shows nothing
    and keeps no text; hostile text is escaped and cut; a flood toasts once per pane per 10 s; a
    toast never sets a state; never in a webhook body. mem-gate: `toast: true`, core idle rows
    unchanged.
12. **Progress.** OSC 9;4 parsing to one byte, `Notice::Progress`, the tab bar, the Dock tile on
    macOS. Tests: every state; a malformed percent is clamped; exit clears it; goldens of the tab.
    mem-gate: core idle rows unchanged.

## Decisions on the open questions

The maintainer answered each on 2026-11-18.

1. **✕ error desktop alerts:** on by default. An error is the other state that needs the person.
2. **Webhook config:** read when the core starts only, in v1. A reload would need a window
   prompt for little gain.
3. **Sender:** the system `curl`, fixed arguments, the URL on standard input; without `curl`,
   webhooks are off and the log says why. No TLS or C library in the core.
4. **Linux notifications:** `notify-send`, optional when absent; no hand-written D-Bus. Less code
   to own and fuzz, and no resident thread.
5. **When to post:** always. The core cannot tell whether the person is at the machine.
6. **Payload:** JSON with a `host` field, plus `webhook_format = "text"` for ntfy-style relays.
   People with several machines can tell them apart, and ntfy shows the text as is.
7. **Keys:** `[keys]` entries with prefix defaults, `!` for the inbox and `w` for the next ◆, not
   Ctrl+Shift chords that take keys from programs. Neither clashes with an existing prefix key.
8. **Plain `http`:** only to `127.0.0.1` and `[::1]`; everything else `https`. A local relay needs
   it, and nothing leaves the machine unencrypted.
9. **Several cores:** later, with named sessions (ADR-0034, point 8).

## Consequences

Every agent the user runs is in one view, ◆ first, with its branch and run time, and one key
reaches the one that waits; from the inbox the person can approve or interrupt it without leaving
their place, always behind PinFold's own prompt. Agents run outside PinFold show up too, for those
who opt in. Linux gets notifications without a new crate or thread, errors alert, programs can
toast and show progress on the tab and the Dock. A phone can hear that an agent needs you, and
what it hears is fixed fields a test checks, never text from a pane.

The core keeps 11 bytes a pane and no text; everything else is read while the inbox is open and
dropped when it closes. PinFold now owns `curl` and `notify-send` invocations that must stay
hardened, a session-file reader for two agents' formats that will change under it, and the
core's first network-facing behaviour, though through a separate process. Webhooks need `curl`,
and a change needs a new session. Revisit the sender if `curl` is missing for enough Linux users,
the session-file reader if Claude Code or OpenCode publish a stable status interface, and the
listing rule if agents often start in plain shell panes without hooks.
