# Roadmap

Pinfold has no dates, because no schedule has been measured. The order comes from the
[M0 report](spikes/m0-report.md) and [ADR-0024](adr/0024-tools-in-v0-2.md).

| Milestone | Scope | Release |
| --- | --- | --- |
| Phase 0 | Repository, CI, decisions | Done |
| M0 | Risk spike on four bets | Done |
| M1 Daily driver | VT, CPU renderer on Linux, Metal renderer on macOS, fonts, IME, selection, search, OSC 133 and 7, config, escape-sequence hardening, tiered scrollback with a byte cap, `mem-gate` | Done |
| M2 Multiplexer | Core daemon, tabs, splits, prefix keys, detach and attach, reconnecting window, core hardening, reflow of cold scrollback | Done |
| M3 Agent API | Protocol, `hello` handshake, `pinfold ctl`, `pinfold mcp`, scopes, approvals, protected panes, redaction, audit log, sandbox, status engine and alerts, in the order of [ADR-0032](adr/0032-api-thread-and-m3-order.md) | Done |
| M5 Sessions | Snapshots, restore rules, workspace trust, `pinfold up`, packaging, signed releases, in the order of [ADR-0034](adr/0034-m5-sessions-plan.md) | Done, for v0.1, macOS and Linux |
| M4 Built-in tools | Markdown reader, file manager, diff viewer, each with its sanitizing and hardened git invocation ([ADR-0045](adr/0045-tools-are-programs-in-their-own-pane.md)) | Done, for v0.2 |

A beta for macOS only comes before v0.1 ([ADR-0031](adr/0031-macos-beta-first.md)). Linux stays in
CI until then, and its desktop checks block v0.1 rather than the beta. v0.1.0-beta.1 to beta.4
have shipped, and beta.4 includes the built-in tools. v0.1 waits on the Linux desktop checks in
[#47](https://github.com/0xPiranhaCodes/pinfold.sh/issues/47), and the manual Mac checks in
[#213](https://github.com/0xPiranhaCodes/pinfold.sh/issues/213) are still open. The sandbox gap in
[#331](https://github.com/0xPiranhaCodes/pinfold.sh/issues/331) is documented for v0.1, not fixed
([ADR-0047](adr/0047-macos-sandbox-reads-other-processes-environment.md)).

**Next (proposed).** A "needs you" inbox across every agent pane, a key to the next ◆, and
optional desktop and status-only webhook alerts ([ADR-0051](adr/0051-needs-you-inbox-and-alerts.md)).
