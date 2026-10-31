# Architecture review

Oct 31, 2026 · @Satheesh

**Verdict: approve the direction, but not the build, until three blocking issues are fixed.** The product idea is sound, and memory-first plus agent-first is a real gap in the market. The stack choices (alacritty_terminal, portable-pty, no async runtime, no scripting VM) are right. But three core claims do not hold up:

1. **B1:** the agent permission model can be bypassed by the agents it is meant to contain.
2. **B2:** the one-process design lets any UI or tool crash kill every running agent.
3. **B3:** the 15 MB target does not survive window buffer sizes on HiDPI screens or the per-pane math.

Each one is fixable, and the fixes are below. Reviewed: the whole Architecture tab as of today. The scope covered correctness, internal consistency, feasibility, operability and sequencing. This is a design review, not a code review.

## Blocking findings

### B1. Agents can bypass the capability model, because they run as you

The threat model puts "a process running as your user with no sandbox" out of scope. But every agent inside Pinfold is exactly that. The design's controls only stop an agent that politely uses the API. An agent that uses its own shell gets around all of them:

- **Tokens leak through `/proc`.** `PINFOLD_TOKEN` is in each pane's environment. Any same-user process can read `/proc/<pid>/environ` of the shell in pane 1, take pane 1's token, and use its `self` scope to type into pane 1. Every child the agent runs (npm install scripts, test runners) also inherits the agent's token.
- **The owner token is a file you can read.** `~/.local/state/pinfold/owner.token` is 0600, but the agent is the same user. One `cat` gives it full scope, with no approval prompts.
- **"Config only" is not a boundary.** An agent can edit `pinfold.toml` to add a webhook, turn off redaction or pre-approve scopes, and hot-reload applies it. The audit log file is just as writable, so it is not tamper-evident.
- **Protected panes have a race.** The foreground check runs per call, but one `pane.send` of `ssh prod\nrm -rf ~\n` passes the check while the local shell is in front. The second line is then typed ahead into ssh. Name-based detection also misses `kubectl exec`, `docker exec`, `psql`, `aws ssm` and aliases.

**Fix**

1. **Identify callers by PTY, not by bearer token.** Read the peer's pid from the socket (`SO_PEERCRED` / `LOCAL_PEERPID`), find its controlling terminal and session, and map that to a pane. Drop `PINFOLD_TOKEN` from the environment.
2. **No owner token on disk.** Owner-level actions from outside a pane need a one-time confirmation drawn in the Pinfold window.
3. **Security-relevant config changes need you to confirm them in the window.** That covers approval policy, redaction, webhooks and auto-rerun. A hot-reload shows a diff and waits for a key press. Hash-chain the audit log, and treat the in-memory copy as the source of truth.
4. **Split sends to other panes at newlines.** Re-check the foreground process and wait about 150 ms between lines. Allow protection rules by process name, by command-line pattern and by user rule.
5. **Be honest about what the controls are, and offer real isolation.** Restate the API controls as guardrails against injected instructions, not as a security boundary. Add a `sandbox` option to agent profiles that launches the agent under bubblewrap or Landlock on Linux and `sandbox-exec` on macOS (or the agent CLI's own sandbox mode). The sandbox hides `~/.ssh`, `~/.config/pinfold`, `~/.local/state/pinfold` and other users' panes. Only a sandboxed agent is truly contained.

### B2. One process means one crash kills every agent

The original principle was "closing or crashing the window never kills a running agent". The memory rework merged the window into the core and dropped that guarantee. Now FreeType (C code), the CPU blitter, winit, ratatui tools, syntect regexes and the diff parser all share one address space with every PTY. The build also uses `panic = "abort"`, so a panic anywhere ends the process, closes every PTY master and sends SIGHUP to every agent. It also makes the "small trusted core" in the security design untrue: the trusted code is the whole binary.

**Fix:** go back to two processes, but keep the memory win. Run a **tiny headless core** (PTYs, VT state, mux, API, about 3–4 MB) and a **window process** that also hosts the tools. The core is the only long-lived process. The window process can exit when closed, which returns all of its memory exactly, with no allocator leftovers. In PSS terms the second process costs about 1–2 MB with the window open, because shared libraries are split between the two. That is a small price for crash isolation and a real trust boundary. It also enables **live upgrades**: restart the window process freely, and hand PTY fds to a new core over `SCM_RIGHTS`. Keep `panic = "abort"` only in the core, and make the core's code paths unable to panic on input (fuzzed, no `unwrap` on parsed data).

### B3. The 15 MB target does not survive window buffers or the per-pane math

- **Pixel buffers are double-buffered.** On Wayland, softbuffer uses shared-memory buffers, and the compositor holds one while Pinfold draws the next. On macOS every present copies into a CGImage. At full HD that is about 16 MB, not 8 MB.
- **HiDPI is the normal case on macOS.** A full-screen window on a 14-inch MacBook is about 3024×1890 device pixels, which is 23 MB per buffer. The ≤ 20 MB macOS target cannot be met with CPU drawing at that size.
- **The table's own sum misses the target.** 4 + 2 + 8 + 1 + 0.3 + 0.4 + 0.5 = 16.2 MB at full HD, against a 15 MB target.
- **The per-pane budget is contradicted by the hot window.** 200 hot lines × 120 columns × 24 bytes is 576 KB, before the visible grid. The ≤ 400 KB per pane target cannot hold.
- **alacritty_terminal cannot do tiered scrollback.** Its grid keeps history as cells in its own storage, and it has no hook for lines leaving the hot window. Compressed cold storage means forking its `Grid`, or getting a change accepted upstream. The `VtEngine` trait does not hide this.

**Fix**

1. **Express targets separately from pixels.** For example: core ≤ 4 MB, window process ≤ 6 MB plus about 2 × 4 bytes × window pixels.
2. **Choose the renderer per platform, by measurement.** Keep CPU drawing on Wayland and X11, where foot proves it. Measure a thin Metal path on macOS, where unified memory and the absence of Mesa make the GPU route cheap. Also consider drawing only damaged regions into layer-backed tiles instead of one full buffer.
3. **Store hot rows compactly.** Use 8–12 bytes per cell (codepoint plus a style index), so the per-pane budget becomes about 250 KB.
4. **Budget the fork of alacritty_terminal's grid explicitly in M0.**

## Findings tracker

Fourteen findings: 3 blocking, 4 high, 5 medium and 2 low. Update the Status column as each one is resolved.

| ID  | Severity | Area          | Finding                                                                                                                                                | Recommendation                                                                                                                                                 | Status |
| --- | -------- | ------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------ |
| B1  | Blocker  | Security      | Same-user agents can read tokens from `/proc`, read the owner token file, edit config and race protected panes                                         | Identify callers by PTY session; no tokens in env or on disk; confirm config changes in the window; split sends at newlines; optional sandboxed agent profiles | Decided: ADR-0004, ADR-0010 |
| B2  | Blocker  | Architecture  | One process plus `panic = "abort"`: any UI or tool crash kills every agent, and the trusted code becomes the whole binary                              | Tiny headless core plus a window/tools process; abort only in the core; fd handover for upgrades                                                               | Decided: ADR-0003 |
| B3  | Blocker  | Memory        | Double-buffered and HiDPI pixel buffers, a 16.2 MB table sum, 576 KB of hot lines per pane, no tiered-scrollback hook in alacritty_terminal            | Targets split from pixel area; renderer per platform (Metal on macOS to measure); compact hot rows; plan a grid fork                                           | Decided: ADR-0005, ADR-0006, ADR-0008 |
| H1  | High     | Concurrency   | The main thread runs mux, drawing and tools, so blocking work stalls every pane; one flooding pane can starve the others on the single I/O thread      | A lazy 1–2 thread worker pool that exits when idle; per-pane read budget per wakeup, round-robin; stop reading a pane whose parse queue is full                | Decided: ADR-0007 |
| H2  | High     | Plan          | 20 weeks for one developer is optimistic; security work is not on the roadmap diagram; M3 packs the API and the whole authorization model into 3 weeks | Re-plan at about 40–50 weeks solo, or move the built-in tools to v0.2; put security on the roadmap; make M0 a risk spike                                       | Open   |
| H3  | High     | API           | No protocol versioning or compatibility policy, so agents and plugins will break between releases                                                      | A `hello` handshake with protocol version and capabilities; JSON Schema per method; deprecate over two minor versions; versioned MCP tool names                | Decided: ADR-0009 |
| H4  | High     | Operations    | No upgrade path for a long-lived core: a new binary means a restart that kills every pane                                                              | Covered by B2's fd handover; version check between window and core                                                                                             | Decided: ADR-0003 |
| M1  | Medium   | Agent status  | "Done" is tied to OSC 133, but agent CLIs stay in the foreground, so a finished agent turn never emits it                                              | Per-agent turn states (thinking, needs approval, turn complete) from hooks; OSC 133 only for shell commands                                                    | Open   |
| M2  | Medium   | Dependencies  | `notify-rust` can pull in `zbus` and its executor thread on Linux, against the no-async-runtime rule; on macOS it needs an app bundle                  | A minimal blocking D-Bus call on the worker thread, or `notify-send`; native notifications from the signed macOS bundle                                        | Open   |
| M3  | Medium   | Accessibility | Not mentioned; a CPU-drawn cell grid exposes nothing to screen readers                                                                                 | AccessKit for the focused pane's text, tabs and approval prompts, or an explicit v1 non-goal                                                                   | Proposed: ADR-0015 |
| M4  | Medium   | Testing       | Only memory budgets and fuzzing are specified                                                                                                          | Add VT conformance (vttest, esctest), renderer golden images, recorded agent sessions for status detection, and approval-prompt UI tests                       | Open   |
| M5  | Medium   | Security UX   | Approval prompts invite click-through, and "Allow for this session" is broad                                                                           | Grants per caller, target and action, with a 30-minute expiry; show the exact text to be sent; batch prompts                                                   | Decided: ADR-0010 |
| L1  | Low      | Doc           | Stale "daemon" wording and a few contradictions (see the list below)                                                                                   | Clean up as listed                                                                                                                                             | Open   |
| L2  | Low      | Build         | `opt-level = "s"` cannot apply to "cold code" only; Cargo sets it per crate                                                                            | Per-crate profile overrides: tools and API at "s", core at 3                                                                                                   | Open   |

## Revised M0: a risk spike, about 3 weeks

Before any feature work, M0 should answer the four bets the whole design rests on, with numbers. If a bet fails, the design changes while that is still cheap.

1. **Process split.** Build the core plus window-process skeleton. Measure the PSS of each process idle and with 8 panes. Pass if the two processes together are within 2 MB of one process.
2. **Window memory.** Measure CPU drawing on Wayland, X11 and macOS Retina at 1x, full-screen and half-screen, counting compositor-side buffers. Then measure a minimal Metal path on macOS. Pass if a half-screen Retina window is ≤ 12 MB including buffers. Otherwise Metal becomes the macOS default.
3. **Tiered scrollback.** Prototype compact hot rows and compressed cold blocks on a fork of alacritty_terminal's grid. Pass if 10k lines at 120 columns is ≤ 1 MB and scrolling into cold history takes < 5 ms.
4. **Caller identity.** Prototype pid → controlling-tty → pane mapping on Linux and macOS. Show that a process in pane A cannot act as pane B, even with full knowledge of pane B's environment.

Then re-baseline the targets and the roadmap from the measured numbers, and re-review.

## Doc consistency fixes

- [ ] "Daemon" still appears in the multiplexer, agent API and session sections, after the one-process change. Settle B2 first, then make the wording match it.
- [ ] The Window row in the tech stack lists `glutin`, which is only needed for the optional GL renderer.
- [ ] `syntect` is "dropped after idle" in the tech stack but "freed when the pane closes" in the tools section.
- [ ] The principle "anything a human can do with keys, an agent can do over the socket" contradicts the approval model. Reword it to "... within the scopes you grant".
- [ ] The markdown reader points to "see Risks" for images, but no risk row covers images; that topic is only an open question.
- [ ] The memory table's full-HD total (16.2 MB) is above the 15 MB target in the summary (B3).
- [ ] The per-pane target of ≤ 400 KB conflicts with 200 hot lines at 24 bytes per cell (B3).
- [ ] The roadmap diagram has no security deliverables, though the security section assigns them to every milestone (H2).
