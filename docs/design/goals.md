# Goals

What PinFold aims for, what v1 leaves out, and the memory targets behind its budgets.

## Goals

1. **The smallest memory footprint in its class.** Every subsystem has a budget,
   and CI will fail any change that exceeds one.
2. **Security.** PinFold gives agents a programmable channel into your shells, so
   that channel is scoped, audited and gated by prompts that programs cannot fake.
   See the [threat model](threat-model.md), including what these
   controls do not do. A sandboxed agent profile limits what the agent writes and
   which terminals it reaches, and hides the common credential stores, secret-looking
   variables and direct clipboard access, but it can read your other files and reach the
   network ([ADR-0041](../adr/0041-sandbox-hides-writes-not-secrets.md)).
3. **Crash isolation.** A small core process owns your PTYs. A crash in the window
   or the built-in tools never kills a running agent.

## Non-goals for v1

- Ligatures and inline images
- An embedded scripting runtime (plugins are external processes that use the API)
- A blocks-style, browser-like UI
- Remote SSH multiplexing, or any TCP listener
- Windows support (platform code stays behind `cfg` and traits so it can come later)
- Screen-reader support ([ADR-0015](../adr/0015-accessibility-v1.md))

## Memory targets

These targets come from [ADR-0021](../adr/0021-memory-targets-m0.md), after the
[M0 measurements](../spikes/m0-report.md). They are targets for a product that did not
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
