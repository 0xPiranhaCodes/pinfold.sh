# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Entries are generated from Conventional Commits with git-cliff
([ADR-0014](docs/adr/0014-changelog-tool.md)).

## [Unreleased]

### Added

- Project governance, security policy, contribution guide and license files.
- Architecture decision records ADR-0001 to ADR-0027.
- Threat model with the PTY-based identity design.
- Roadmap, and memory targets updated from the M0 measurements.
- `cargo xtask mem-gate` now measures the core on Linux and macOS.
- Font stack spike, and pure-Rust fonts with `swash`.
- Glyph cache capped at 1 MB, and CPU drawing of cell rows.
- A scrollback store with a 2 MB cap per pane, holding what leaves a pane's screen.
- `cargo xtask mem-gate` measures 10k lines of typical text and a pane at its scrollback cap.
- Fuzzing of the terminal input path, on each PR and nightly.
- Function keys, modifier combinations and application cursor mode, as xterm sends them.
- Paste from the clipboard, with a confirmation for anything that ends a line.
- A paste sanitizer in the core, and an input queue of 1 MiB so a large write is not cut.
- Input method composition, drawn at the cursor.
- Synchronized updates end on their own after 150 ms and hold at most 256 KiB per pane.
- Limits on OSC strings, titles and hyperlinks, and replies only in fixed formats.
- Mouse reporting, and the wheel as arrow keys on the alternate screen.
- A fork of `alacritty_terminal` that hands rows leaving the screen to their owner.
- Metal renderer on macOS, with CPU drawing as its fallback.
- macOS window budget raised to 30 MB plus pixels.
- A window that runs a real shell, drawn by the CPU renderer on Linux.
- Coloured pane frames, plus input and resize in the I/O loop.
- JetBrains Mono as the bundled default font, under the SIL OFL 1.1.
- One I/O loop for every PTY, with fair read budgets.
- PTY spawning, and the `VtEngine` trait over `alacritty_terminal`.
- Cargo workspace with empty crates, `cargo xtask`, CI and a dry-run release workflow.
- M0 spike results for caller identity, tiered scrollback, the process split and window memory, with the M0 report.

### Fixed

- Closing a pane with unread output can no longer hang on macOS.
- A window narrowed to one column with a wide character on screen no longer exhausts memory.
- An unterminated OSC string can no longer grow the core without limit.
