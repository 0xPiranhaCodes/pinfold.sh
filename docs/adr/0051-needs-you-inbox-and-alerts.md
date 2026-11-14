# ADR-0051: A "needs you" inbox across every agent, with optional alerts

- **Status:** Proposed
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

Option **A**. Webhooks fold in #264's T9 half; spill files stay in #264.

### 1. What the core keeps

- **Listed panes.** A pane is in the inbox when it was launched from an agent profile
  (`Tracker::agent`) or has had a `pane.status` report since it opened (a new `Tracker::listed`
  flag, never cleared by `clear_report`). Tool panes never are (ADR-0045). A plain shell pane is
  not, so every failed `ls` does not reach the inbox.
- **Time in state.** `Tracker` gains `since: Instant`, set whenever the shown state changes and
  never by a repeat of the same state. 16 bytes (`size_of::<Instant>()` on both platforms, rustc
  1.87).
- **Profile name,** for the webhook only: the `agent.launch` profile's name, cut to 32 bytes of
  `A-Z a-z 0-9 _ -` (anything else drops the name), as `Option<Box<str>>`, 16 bytes plus at most
  32 on the heap.
- Nothing else. The message is the one the pane already keeps.

### 2. Link (version 16, or the next free one when this lands; ADR-0050 may take 16)

- `Notice::Status` gains `since: u32`, whole seconds in the state when sent, saturating, and
  `naming: bool`. The core sends it for every listed pane and for each tab's naming pane, on any
  change of state, source or message, as today. The window turns `since` into an `Instant`, so
  the two sides share no clock. A version 15 window gets what it gets today, the naming pane's
  status only; ADR-0043 gains the row.
- `Notice::Webhook { host: &str, sender: bool, failed: u32 }`, sent after the hello when a
  webhook is configured and again when `failed` changes: the URL's host only (§5 limits it to
  `A-Z a-z 0-9 . - : [ ]`), whether `curl` was found, and how many alerts were given up on.
- No new request. A jump is `SwitchTab` then `Focus`, which exist.

### 3. API

- `pane.list` gains `since`, seconds in the current state, for every pane, under the scopes it
  already needs. Messages stay as they are: the caller's own only.
- No new method or scope. There is no `alerts.*` or `webhook.*` method: alert destinations stay
  config-file only, as the Authorization table says.

### 4. The inbox, drawn in cells

- **Open:** Ctrl+Shift+A on every system, as the palette is Ctrl+Shift+P, and an "Agents" entry
  in the palette (`palette::COMMANDS`). It is an overlay drawn after the panes, like the palette
  and the right-click menu, so every key goes to it and program output can neither fake nor
  answer it.
- **Rows,** one per listed pane of every tab, ordered ◆ needs you, ✕ error, ● running, ✓ done,
  ○ idle, and within a state the longest-held first. Each row is, left to right: the glyph in
  its state colour (`theme.states`), the **word** in the same colour, padded to 9 cells ("needs
  you"), `tab·pane` numbers, the tab's label (`top_bar::label`, which already drops control,
  bidi and state glyphs), the last message, and the time in state right-aligned. A state from
  output shows "from output" in `chrome.dim` where the message goes (ADR-0046, point 3). The
  message is filtered again with `top_bar`'s `safe` and cut to the space left with `…`. The word
  is drawn in every row: the board shows glyphs only, but CLAUDE.md asks for glyph, colour and
  word, so the board is updated in the slice that draws it.
- **Time:** `m:ss` under an hour, then `3h07`, then `99h+`. The view ticks once a second only
  while open, on the clock's existing tick.
- **Header and footer:** "AGENTS · needs you first", and "N agents · M needs you" (plus
  "webhook: K failed" in red when `failed` > 0, and "webhook: no curl" when `sender` is false).
  No rows: "No agent panes".
- **Keys:** ↑↓ move, Enter jumps to the row's pane and closes, Esc or Ctrl+Shift+A closes. A
  paste is dropped. Mouse: a click on a row jumps.
- **Size:** at most 80 columns wide and the window's rows less 4, at least 40×6; past that it
  scrolls. Narrower windows drop the message column, then the label.
- **Next needs you:** Ctrl+Shift+W from anywhere in the window, not only the inbox, focuses the
  ◆ pane waiting longest; pressed again on a ◆ pane, the next one in inbox order. With none it
  does nothing and flashes "Nothing needs you" in the row under the pane, as other notes do.
  Palette entry "Next Needs You".

### 5. Alerts

**Config,** a new `[alerts]` table in `pinfold-config`, with `deny_unknown_fields`:

| Key | Default | Meaning | Read |
| --- | --- | --- | --- |
| `desktop` | `["needs-you"]` | States that raise a desktop notification; `"needs-you"`, `"error"` or none | By the window, hot-reloaded |
| `webhook` | `""` (off) | An `https://` URL to POST to | By the core, when it starts |
| `webhook_states` | `["needs-you", "error"]` | States that are posted | By the core, when it starts |

The default keeps today's behaviour. The core takes no hot reload (`pinfold-core.rs`, "Read
once"), so a webhook change applies to the next session; that also satisfies "security-relevant
config changes do not apply on hot reload until you confirm" without a new prompt. A webhook
configured when a window links is shown once in it, as ADR-0044's notice is: "Alerts for ◆ and ✕
go to `<host>`", so an agent that edited the file cannot add one unseen.

**Desktop notifications** stay in the window, keyed by pane rather than tab index (fixing
`notify.rs`'s `shortcut:`), with today's limits: one per pane per 60 s, one per 10 s in all. Text
is PinFold's: "`<label>` needs you" or "`<label>` error". A listed pane alerts when it comes to
that state while not shown in a focused window. Clicking goes to the pane.

- **macOS:** unchanged mechanism (ADR-0046): Dock bounce, and `NSUserNotificationCenter` where
  it can.
- **Linux:** a blocking D-Bus client written for this, in `pinfold-sys/src/notify/linux.rs`
  behind the same `Notifier`, on one window thread (64 KiB stack, a bounded channel of 8). It
  reads `DBUS_SESSION_BUS_ADDRESS` and takes only `unix:path=` and `unix:abstract=` (std's
  `SocketAddrExt`); authenticates with SASL `EXTERNAL` and `pinfold_sys::effective_uid`, which the peer
  UID check already uses, so no new `unsafe`; sends `Hello`, `AddMatch` for `ActionInvoked` and
  `NotificationClosed`, and `org.freedesktop.Notifications.Notify` (`susssasa{sv}i`: "PinFold",
  0, "", the text, "", `["default", "Go to pane"]`, no hints, -1). It reads replies of at most
  64 KiB, bounds-checked, ignores anything it does not expect, and maps a returned id to the pane
  for clicks (at most 32 ids kept). No bus, a refused call or a malformed reply: log once at
  `warn`, and the tab mark stays the only signal, as today. A fuzz target covers the reply parser.

**Webhooks,** from the core, since the point is to reach someone with no window open.

- **What is posted.** One JSON object, `Content-Type: application/json`, built by PinFold from
  fixed fields only:
  `{"pinfold":1,"state":"needs-you","source":"agent","agent":"claude","tab":2,"pane":7,"at":1763112000,"since":1763111958}`.
  `state` is a `Status::name`; `source` is `agent` or `output`; `agent` is the profile name from
  §1, or `null`; `at` and `since` are Unix seconds. Never the message, the tab label (it comes
  from OSC 7, which output sets), a title, a command, a directory, pane text, the hostname or an
  environment value. A test feeds a pane hostile output and a message holding a secret-looking
  token and checks the body against a fixed grammar of these fields.
- **URL rules,** checked when the config is read; a bad URL turns webhooks off with a logged
  reason: at most 2048 bytes; scheme `https://`; no userinfo (`@` in the authority); host of
  `A-Z a-z 0-9 . -` or a bracketed IPv6 literal, optional `:port` 1–65535; path and query of
  printable ASCII without `"`, `\` or `#`. A secret in the path or query, as an ntfy topic is,
  stays out of logs, the window and the process table: logs and the notice name the host only.
- **Sending.** A job on the core's worker (ADR-0007), started lazily, exiting after 60 s idle. It
  runs `curl` found once at core start: `/usr/bin/curl` on macOS, the first `curl` on an absolute
  `PATH` entry on Linux. The environment is cleared except `HTTPS_PROXY`, `https_proxy`,
  `ALL_PROXY`, `all_proxy`, `NO_PROXY` and `no_proxy`. The argument list is fixed,
  `curl --disable --config -`, so `~/.curlrc` is ignored and the URL never reaches the process
  table; standard input carries the config: `url`, `data`, `header`, `user-agent = "pinfold"`,
  `proto = "=https"`, `proto-redir = "=https"`, `max-redirs = 0`, `connect-timeout = 5`,
  `max-time = 10`, `silent`, `output = "/dev/null"`, `write-out = "%{http_code}"`. Values are
  quoted by one function the URL rules and payload grammar make simple (no `"` or `\` in the
  URL; the payload's own `"` escaped), with a test. Standard error is discarded; at most 16 bytes
  of standard output are read; a child still running after 15 s is killed.
- **Retries.** A 2xx is done. Connect, DNS, TLS or timeout failures (curl exit 6, 7, 28, 35, 52,
  56) and HTTP 429 or 5xx retry after 5 s and then 30 s, three tries in all; anything else is
  given up at once. Given-up alerts count into `failed` and log at `warn` with host, status and
  try, never the URL.
- **Limits.** A webhook follows the tracker's alerts, which are already one per pane per 10 s;
  on top, one per pane per 60 s and at most 10 in any 10 minutes in all. The queue holds 16;
  past it the oldest is dropped and counted. A ◆ from output can still post (ADR-0046 lets an
  agent pane's output say needs you), labelled `"source":"output"`, within these limits.

### 6. Threat model

Rows added or changed, each marked "proposed by ADR-0051" until built: T9's threat row gains the
webhook controls above; the Controls table splits T9's "Spill files and webhooks" into spill
files (still #264) and webhooks with the tests named in slice 5; T4's window-notification row
gains ✕ and Linux; T10 gains the D-Bus reply parser's fuzz target; Secure defaults gain "Webhook
destination: none, config file only, read when the core starts"; Trust boundaries name the one
kind of output that leaves the machine.

### 7. Memory

| Where | What | Cost |
| --- | --- | --- |
| Core, per pane | `since` 16 B, `listed` 1 B, profile name 16 B + ≤ 32 B | ≤ 72 B; under 5 KB at `MAX_PANES` (64) |
| Core, webhook | 16 queued jobs of ≤ 256 B, one 64 KiB-stack thread while busy | ≤ 4 KB heap; the thread's touched stack while it lives |
| `curl` | A separate process for ≤ 15 s per try | Not in the core's footprint; measured once, not gated |
| Window | One row per listed pane, buffers reused as `Dock::update` does | ~120 B a row typical; ≤ 54 KB at 64 rows of 200-character messages |
| Window, Linux | D-Bus thread, 64 KiB stack, ≤ 64 KiB read buffer, 32 ids | < 140 KB, only once a notification is sent |

These are type sizes and caps, not measurements. `cargo xtask mem-gate` checks them: "Core, 8
idle panes" and "Each idle pane" must hold unchanged; a new "Core, 8 agent panes listed, a
webhook after one alert" row runs `--idle 8` with agent panes and a webhook to
`https://127.0.0.1:9` that fails fast, measured after the worker exits, against `CORE_LIMIT`; and
the window rows gain an `inbox: true` case beside `menu: true`, against the same window budget.
Slice 2 and slice 5 each paste their table.

### 8. Slices

Each about 400 changed lines, one concern, with its tests:

1. **Core state and link.** `since`, `listed`, profile name; `Notice::Status` with `since` and
   `naming` for every listed pane; `pane.list` `since`; ADR-0043 row. Tests: `since` moves only
   on a change of state; listed rules (agent, reported, never a tool or plain shell); link round
   trip; a version 15 window gets only naming statuses.
2. **The inbox.** `pinfold-ui/src/inbox.rs`, Ctrl+Shift+A, palette entry, jump. Tests: ordering;
   every row has glyph, colour and word; "from output"; hostile label and message drawn safe;
   64 rows scroll; narrow window; a paste is dropped; golden image; mem-gate `inbox` row; the
   Agents board shows the word.
3. **Next needs you and desktop config.** Ctrl+Shift+W, `[alerts] desktop`, ✕ notifications,
   limits keyed by pane, `docs/configuration.md`. Tests: cycle order; nothing waiting flashes;
   limits; error text is PinFold's.
4. **Linux notifications.** The D-Bus client and its click path. Tests: marshalling against
   bytes from the spec; a fake bus on a socket pair scripting auth, `Hello`, `Notify` and
   `ActionInvoked`; malformed replies; fuzz target `dbus_reply` in `fuzz-smoke`. A run against a
   real `dbus-daemon --session` where CI has one.
5. **Webhook config and payload.** `alerts.webhook` and `webhook_states`, URL rules, payload
   builder, curl config quoting. Tests:
   `a_webhook_body_holds_only_status_fields_whatever_the_pane_printed`, a URL rule table
   (userinfo, `http`, a fragment, a quote, a control character, 2049 bytes), and quoting of every
   value the grammar allows.
6. **Webhook sender.** Worker job, `curl` lookup, environment, retries, limits, queue,
   `Notice::Webhook` and the window's notice and footer. Tests with a stub `curl` on an absolute
   `PATH` that records its arguments and standard input and exits as scripted: fixed arguments,
   no URL in them, retry schedule, given-up count, no `curl`; mem-gate webhook row; threat-model
   rows move from "proposed" to built with their test names, closing #264's webhook half.

### Open questions for the maintainer

1. **✕ error on the desktop by default?** Proposed off, keeping today's behaviour.
2. **Webhook only at core start,** or a window-confirmed reload so a change does not need a new
   session?
3. **`curl` as the sender.** It adds a runtime need on minimal Linux installs; the alternative is
   `ureq` with `rustls`, whose `ring` is C and assembly, needing your consent.
4. **Hand-written D-Bus** (about 300 lines, fuzzed) or spawning `notify-send`, which is simpler,
   is not always installed, and gives clicks only in newer libnotify?
5. **Post always, or only when no window shows the pane?** Proposed always, as the core cannot
   tell whether you are at the machine.
6. **Payload shape.** JSON suits relays; ntfy shows it raw. Add a `webhook_format = "text"`, or a
   hostname field for people with several machines?
7. **Keys.** Ctrl+Shift+A and Ctrl+Shift+W, fixed as the palette's is, or `[keys]` entries? Ctrl+
   Shift+W stops reaching the pane as Ctrl+W with Shift held.
8. **Plain `http://127.0.0.1`** for a local relay: allow, or keep `https` only?
9. **Several cores.** When named sessions arrive (ADR-0034, point 8), should one inbox list every
   core's agents? This ADR covers the one core v0.1 has.

## Consequences

Every agent the user runs is in one view, ◆ first, and one key reaches the one that waits. Linux
gets notifications without a new crate, and errors can alert. A phone can hear that an agent
needs you, and what it hears is fixed fields a test checks, never text from a pane.

PinFold now owns a small D-Bus client and a `curl` invocation that must stay hardened, and the
core gains its first network-facing behaviour, though through a separate process. Webhooks need
`curl`, and a change needs a new session. Revisit the sender if `curl` is missing for enough
Linux users, the D-Bus client if it grows past notifications, and the listing rule if agents
often start in plain shell panes without hooks.
