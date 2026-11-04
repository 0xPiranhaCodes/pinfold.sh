# PinFold

![status: beta](https://img.shields.io/badge/status-beta%2C%20macOS%20only-orange)
![license: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)

A memory-first terminal for agentic coding, written in Rust.

> **Beta, macOS only.** The first beta is for testing, and parts described below,
> such as Linux packages and the v0.2 tools, are not built yet. Read the
> [known issues](docs/beta/known-issues.md) before you report a bug.

<!-- Screenshot placeholder: replace with a PNG rendered from docs/design/ui/Main.dc.html. -->
_Screenshot to come. The design direction, "Graphite Grid", is in
[`docs/design/ui/`](docs/design/ui/)._

## What it is

PinFold is a standalone desktop terminal for Linux (X11 and Wayland) and macOS.
It has a built-in multiplexer (tabs, splits, detach and attach, so no tmux is
needed). A markdown reader, a ranger-style file manager and a git diff viewer are
planned for v0.2.
Coding agents can drive panes through a local, permissioned API (a CLI and an MCP
server). It warns you when an agent needs you, and it restores sessions after a
reboot.

## Goals

1. **The smallest memory footprint in its class.** Every subsystem has a budget,
   and CI will fail any change that exceeds one.
2. **Security.** PinFold gives agents a programmable channel into your shells, so
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
| Window process                         | ≤ 8 MB + 2 × 4 B × pixels   | ≤ 33 MB + 2 × 4 B × pixels   |

Memory is measured as PSS on Linux and `phys_footprint` on macOS. macOS costs more because an
empty window takes about 19 MB there.

## Install the beta

1. Download `PinFold.dmg` from the beta on the
   [Releases](https://github.com/0xPiranhaCodes/pinfold.sh/releases) page.
2. Open it and drag PinFold to Applications. The app is signed and notarized, so macOS asks
   once whether to open an app from the Internet.
3. To use `pinfold` from any terminal, link the command into a directory on your `PATH`:

   ```sh
   sudo mkdir -p /usr/local/bin
   sudo ln -sf /Applications/PinFold.app/Contents/MacOS/pinfold /usr/local/bin/pinfold
   ```

   The command finds the rest of PinFold beside the file the link points to, so link it, do
   not copy it.

**Requirements:** macOS on Apple Silicon or Intel; the app is universal. No minimum version is
enforced yet. The automated tests run on macOS 26.6 and the app has been run by hand on macOS 27
([#190](https://github.com/0xPiranhaCodes/pinfold.sh/issues/190)).

**Where PinFold keeps things:**

| What                                     | Where                                      |
| ---------------------------------------- | ------------------------------------------ |
| Config                                   | `~/.config/pinfold/pinfold.toml`           |
| Saved session, workspace trust, audit log | `~/Library/Application Support/PinFold`   |
| Agents' shadow git directories           | `~/.local/share/pinfold`                   |
| Socket and the log, `default.log`        | `$TMPDIR/pinfold-$(id -u)`                 |

`XDG_CONFIG_HOME` and `XDG_DATA_HOME` move the first and third.
[docs/configuration.md](docs/configuration.md) explains each of them.

### Uninstall

Close every pane, with `⌘W` or `exit`, so the session and its core end;
`pgrep -x pinfold-core` then prints nothing. Then:

```sh
sudo rm -f /usr/local/bin/pinfold
rm -rf /Applications/PinFold.app ~/.config/pinfold "$HOME/Library/Application Support/PinFold" \
  ~/.local/share/pinfold "$TMPDIR/pinfold-$(id -u)"
```

The second command also removes your config, the workspaces you trusted, and agents' commits
that you have not fetched. Leave out the paths you want to keep.

## Documentation

- Beta: [known issues](docs/beta/known-issues.md) and the
  [manual checks](docs/beta/manual-checks.md) the maintainer runs before each beta
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
