# Roadmap

Pinfold has no dates, because no schedule has been measured. The order comes from the
[M0 report](spikes/m0-report.md) and [ADR-0024](adr/0024-tools-in-v0-2.md).

| Milestone | Scope | Release |
| --- | --- | --- |
| Phase 0 | Repository, CI, decisions | Done |
| M0 | Risk spike on four bets | Done |
| M1 Daily driver | VT, CPU renderer on Linux, Metal renderer on macOS, fonts, IME, selection, search, OSC 133 and 7, config, escape-sequence hardening, tiered scrollback with a byte cap, `mem-gate` | |
| M2 Multiplexer | Core daemon, tabs, splits, prefix keys, detach and attach, reconnecting window, core hardening, reflow of cold scrollback | |
| M3 Agent API | Protocol, `hello` handshake, `pinfold ctl`, `pinfold mcp`, scopes, approvals, protected panes, redaction, audit log, sandbox, status engine and alerts | |
| M5 Sessions | Snapshots, restore rules, workspace trust, `pinfold up`, packaging, signed releases | v0.1 |
| M4 Built-in tools | Markdown reader, file manager, diff viewer, each with its sanitizing and hardened git invocation | v0.2 |
