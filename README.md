# Pinfold

![status: pre-alpha](https://img.shields.io/badge/status-pre--alpha-orange)
![license: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)

A memory-first terminal for agentic coding, written in Rust.

> **Pre-alpha.** Pinfold does not run yet. This repository holds the design, the
> decisions and the project scaffolding. Everything below describes what we are
> building, not what exists today.

<!-- Screenshot placeholder: replace with a PNG rendered from docs/design/ui/Main.dc.html. -->
_Screenshot to come. The design direction, "Graphite Grid", is in
[`docs/design/ui/`](docs/design/ui/)._

## What it is

Pinfold is a standalone desktop terminal for Linux (X11 and Wayland) and macOS.
It has a built-in multiplexer (tabs, splits, detach and attach, so no tmux is
needed). A markdown reader, a ranger-style file manager and a git diff viewer are
planned for v0.2.
Coding agents can drive panes through a local, permissioned API (a CLI and an MCP
server). It warns you when an agent needs you, and it restores sessions after a
reboot.

## Goals

1. **The smallest memory footprint in its class.** Every subsystem has a budget,
   and CI will fail any change that exceeds one.
2. **Security.** Pinfold gives agents a programmable channel into your shells, so
   that channel is scoped, audited and gated by prompts that programs cannot fake.
   See the [threat model](docs/design/threat-model.md), including what these
   controls do not do.
3. **Crash isolation.** A small core process owns your PTYs. A crash in the window
   or the built-in tools never kills a running agent.

## Non-goals for v1

- Ligatures and inline images
- An embedded scripting runtime (plugins are external processes that use the API)
- A blocks-style, browser-like UI
- Remote SSH multiplexing, or any TCP listener
- Windows support (platform code stays behind `cfg` and traits so it can come later)
- Screen-reader support ([ADR-0015](docs/adr/0015-accessibility-v1.md))

## Memory targets

These targets come from [ADR-0021](docs/adr/0021-memory-targets-m0.md), after the
[M0 measurements](docs/spikes/m0-report.md). They are targets for a product that does not
exist yet: the spikes behind them left out fonts and a real renderer, so real numbers will be
higher.

| Metric                                 | Linux                       | macOS                        |
| -------------------------------------- | --------------------------- | ---------------------------- |
| Core, 8 idle panes, no scrollback      | ≤ 4 MB                      | ≤ 4 MB                       |
| Each idle pane, no scrollback          | ≤ 250 KB                    | ≤ 250 KB                     |
| 10k lines of typical text              | ≤ 1 MB                      | ≤ 1.5 MB                     |
| Cold scrollback per pane, hard cap     | 2 MB                        | 2 MB                         |
| Window process                         | ≤ 8 MB + 2 × 4 B × pixels   | ≤ 30 MB + 2 × 4 B × pixels   |

Memory is measured as PSS on Linux and `phys_footprint` on macOS. macOS costs more because an
empty window takes about 19 MB there.

## Install

**Not yet released.** There are no packages or binaries to install.

## Documentation

- [Architecture](docs/design/architecture.md) and its
  [review](docs/design/architecture-review.md). Where they disagree the review
  wins, and the [ADRs](docs/adr/) override both.
- [Roadmap](docs/roadmap.md)
- [Threat model](docs/design/threat-model.md)
- [UI design](docs/design/ui/)

## Contributing and security

- Read [CONTRIBUTING.md](CONTRIBUTING.md) to get started.
- Report vulnerabilities as described in [SECURITY.md](SECURITY.md), not in public
  issues.
- This project follows the [Code of Conduct](CODE_OF_CONDUCT.md). Decisions are
  made as described in [GOVERNANCE.md](GOVERNANCE.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option. Any fork of `alacritty_terminal` code
stays Apache-2.0 only ([ADR-0012](docs/adr/0012-license.md)).

The bundled JetBrains Mono font is under the
[SIL Open Font License 1.1](crates/pinfold-render/fonts/OFL.txt).

The name and logo are covered by [TRADEMARK.md](TRADEMARK.md).
