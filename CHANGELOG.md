# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Entries are generated from Conventional Commits with git-cliff
([ADR-0014](docs/adr/0014-changelog-tool.md)).

## [Unreleased]

### Added

- Pill tabs in the top bar, with hover, a mark on a hidden tab that printed, a quiet look for an
  unfocused window, and no title row in fullscreen.
- Tabs and splits for real. The core owns the session, so a new window finds the same tabs and panes, and only the panes of the active tab are sent to the window, so a hidden tab costs no frame copies. A session holds at most 32 tabs and 64 panes, and past that a command is refused with a note under the pane. Panes are drawn in cells with cell-drawn borders, the focused pane's in the focus accent.
- Prefix keys work: `Ctrl+a`, or `keys.prefix`, then `c`, `n`, `p`, `0` to `9`, `|`, `-`, the arrows, `z`, `d` and `x`. `x` asks first, and says so in red when it would end the session. On macOS Command+T opens a tab and Command+1 to Command+9 go to one. A click focuses a pane or picks a tab, and the `+` button opens one. Bindings are in [docs/configuration.md](docs/configuration.md).
- `pinfold-core` runs as a hardened daemon with a private control socket that takes `hello` and `window.open`.
- `pinfold` is now a launcher: it starts the core if none answers, checks that the socket's owner is you, asks for a window and exits. The window is its own `pinfold-window` process on the link (ADR-0028), so a window that crashes or closes leaves every pane running and `pinfold` reattaches. A second `pinfold` brings the window to the front, and a window whose core dies says the session ended. `pinfold --size` is gone.
- `cargo xtask mem-gate` measures the window and the core as separate processes, and the core beside a window.
- A TOML config file with `font_size` and `macos_option_as_alt`. A bad file means the defaults.
- Padding around the grid, and on macOS a full-height window under the native traffic lights, with `background_opacity` and `background_blur`.
- Config hot reload: saving the file applies it to the running window, and a bad save keeps the current settings.
- CJK text from a system font, opened only when the bundled font lacks a character.
- Bold and italic drawing, a `font_path` config key, and text blended in linear light.
- Text stays sharp across displays: the font size follows the window's scale factor.
- Project governance, security policy, contribution guide and license files.
- Architecture decision records ADR-0001 to ADR-0028.
- Threat model with the PTY-based identity design.
- Roadmap, and memory targets updated from the M0 measurements.
- `cargo xtask mem-gate` now measures the core on Linux and macOS.
- Font stack spike, and pure-Rust fonts with `swash`.
- Glyph cache capped at 1 MB, and CPU drawing of cell rows.
- A scrollback store with a 2 MB cap per pane, holding what leaves a pane's screen.
- `cargo xtask mem-gate` measures 10k lines of typical text and a pane at its scrollback cap.
- `cargo xtask mem-gate` measures the window with a full screen of bold, italic and CJK text.
- Fuzzing of the terminal input path, on each PR and nightly.
- The core serves a window over a socketpair link (ADR-0028): `Reactor::spawn_window` starts one window, with the link on descriptor 3, and a window that misbehaves or stalls never touches a PTY.
- Function keys, modifier combinations and application cursor mode, as xterm sends them.
- Paste from the clipboard, with a confirmation for anything that ends a line.
- `Pinfold.app` for macOS, with its icon, and a login shell in the home folder when opened from the Dock.
- A top bar: a title row with the local date and time, which moves the window when dragged, and a row of square tabs named after the working directory, with a new tab button.
- Drag to select across history and the screen, Shift-drag past mouse reporting, a copy key, and a search bar.
- Vi mode: keyboard motions, selection and copying across history and the screen.
- `pinfold-mux`, the layout model of sessions, tabs and split panes, which runs nothing yet.
- A paste sanitizer in the core, and an input queue of 1 MiB so a large write is not cut.
- A two-second flash in the row under the pane when a program's OSC 52 clipboard write is blocked.
- Input method composition, drawn at the cursor.
- History reflows with the window: a wrapped line reads at the new width, and a view in history stays on its text (ADR-0029).
- Synchronized updates end on their own after 150 ms and hold at most 256 KiB per pane.
- Limits on OSC strings, titles and hyperlinks, and replies only in fixed formats.
- OSC 133 command marks and the OSC 7 working directory, tracked per pane as hints.
- Mouse reporting, and the wheel as arrow keys on the alternate screen.
- The wheel scrolls through history on the primary screen, and input returns to the live screen.
- Search and selection in the core across scrollback and the screen, a bounded step per I/O pass.
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
- The binary protocol between the core and the window, as pure encoding and decoding with a streaming decoder and a fuzz target.
- The window's end of that link, a client that keeps a local copy of the screen and reports a disconnect as a value.
- Prefix keys as pure logic, with tmux-style tab and split bindings and a `keys.prefix` config key.

### Fixed

- `pinfold` waits until the window is on screen, and fails with a short reason if the window cannot run, ends during start or is not ready after 2 seconds. It used to exit 0 once the process existed, and said only "no window can open".
- Bold `i` and other glyphs of several contours no longer come out thinner than regular.
- Fallback font pages no longer count against the window once a frame is drawn.
- A copy of a whole pane can run in steps, which keeps its text out of the core's footprint.
- A window taller than 24 rows no longer costs the core about 2 MB, as growing a grid makes only the rows it needs.
- The core sends a window only the panes it asked for, so a hidden pane holds no frame.
- The config file is `pinfold.toml`, the name the design gives it.
- Closing a pane with unread output can no longer hang on macOS.
- A window narrowed to one column with a wide character on screen no longer exhausts memory.
- An unterminated OSC string can no longer grow the core without limit.
