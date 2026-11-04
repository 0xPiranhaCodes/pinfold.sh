# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and
this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Entries are generated from Conventional Commits with git-cliff
([ADR-0014](docs/adr/0014-changelog-tool.md)).

## [Unreleased]

### Fixed

- The workspace trust store is read and written only in a state directory that passes the same owner and mode check as `sessions/`, and `trust.json` is opened without following a symlink, so another user who can write the state directory cannot make a layout's `run` lines type without a prompt. A refused directory trusts nothing, and the core logs why.
- A core that `pinfold up` starts no longer idles forever when its window never opens: with no pane and no window 30 seconds after it starts, it exits ([#177](https://github.com/0xPiranhaCodes/pinfold.sh/issues/177)).
- `pinfold up` with no core running no longer opens an extra shell tab in `$HOME` beside the workspace: the core it starts opens only the layout's tabs, or joins them to a restored session ([#174](https://github.com/0xPiranhaCodes/pinfold.sh/issues/174)).
- On macOS a new pane no longer fails to start, as "Device not configured" or "The shell would not start", when another pane closes at the same moment: opening its PTY is tried once more.
- A sandboxed agent can no longer write outside its working directory and temporary directories, such as to `~/.zshrc`, and Linux takes `bwrap` only from root-owned system directories, not `PATH`, where an agent could plant one. The working directory must be the user's own and private, not home, above it, a dot-directory of home, `~/Library` or inside `.git`, and git cannot be made to run its code: on Linux its `.git` is read-only, so commits happen outside the sandbox, and on macOS the hooks, config and pointers of the repository and its submodules are, and each listed submodule's `.git` stays read-only on both. The agent reaches no Unix socket but PinFold's API socket and gets no `SSH_AUTH_SOCK`, `GPG_AGENT_INFO` or `DBUS_SESSION_BUS_ADDRESS`; on Linux it also gets its own IPC, host name and cgroup namespaces and ends with the core.
- On Linux 6.12 and later, with Landlock ABI 6, a sandboxed agent can no longer connect to abstract Unix sockets bound outside its sandbox, such as X11's or a session bus; older kernels keep them reachable, and the core says so once. A sandboxed agent's working directory may no longer be a `PATH` directory or hold one.
- The core now applies `api.protect` from the config file; before, only the default patterns protected panes.
- `past_its_identitys_cap_a_peer_is_closed_unread` no longer fails on macOS when the core closes the extra peer before the test sets its read timeout.
- The link test that fills every tab and pane uses 128 by 128 panes instead of 1024 by 1024, so it no longer runs past its 20 second limit on CI's Intel Mac.
- The sandbox test keeps its API socket in a short directory under `/tmp`, so it no longer fails with "path must be shorter than SUN_LEN" from a checkout with a long path. Binding a socket path that is too long returns an error, never a panic.
- Session and control tests wait for observable events instead of idle-machine timing under load.
- Missing file and status icons now use installed fallback fonts while preserving the selected text font and terminal cell size.
- macOS title text and the grid's left edge align with the actual native window buttons, accounting for font size and display scale.

### Changed

- `packaging/macos/bundle.sh` takes an optional signing identity, signs the `pinfold` launcher and the app with the hardened runtime as well as the core and window, and checks the result. The release dry run keeps an ad-hoc-signed `PinFold-adhoc.dmg` built by the new `packaging/macos/dmg.sh` instead of a `.zip`.
- `cargo xtask mem-gate` also reads each window with a full glyph cache, with 8 tabs and with its context menu open, and lists the Dock menu as not measured.
- The release dry run can no longer be started by hand. It runs on version tags and on pull requests that change packaging or `release.yml`.
- Refined window chrome with graphite tab surfaces, teal active labels, readable unfocused tabs, quieter search hints, and more space around the terminal.

### Added

- On macOS, PinFold > About PinFold shows how to get started: the keys for tabs and splits, `pinfold agent` and `pinfold up`, the agent state glyphs, where the config file and log are, links to the website, docs and issues, and the licenses. The text is `Credits.rtf` in the app's resources, and `bundle.sh` fails if it is missing or does not parse.
- A `v*` tag creates a draft GitHub release holding `PinFold.dmg`, the Linux tarballs, a CycloneDX SBOM from `cargo-cyclonedx` and `SHA256SUMS` over all of them, with a Sigstore-backed build provenance attestation per file. Only that job can write, it builds nothing, and the maintainer publishes the draft ([#181](https://github.com/0xPiranhaCodes/pinfold.sh/issues/181)).
- A `v*` tag signs the macOS app with the Developer ID, the hardened runtime and a timestamp, and builds `PinFold.dmg`, notarized with `notarytool` and stapled, in a job that reads the signing secrets only from the maintainer-approved `release` environment and builds nothing ([#179](https://github.com/0xPiranhaCodes/pinfold.sh/issues/179), [#180](https://github.com/0xPiranhaCodes/pinfold.sh/issues/180)).
- On Linux a sandboxed agent can commit: into a shadow git directory under `~/.local/share/pinfold/agents`, which borrows the repository's objects and whose hooks and config only the sandbox runs. `git fetch --no-tags <shadow> '+refs/heads/*:refs/remotes/agent-<profile>/*'` brings its commits into remote-tracking refs; the real `.git` stays read-only.
- Right-click on an OSC 8 hyperlink adds Open Link and Copy Link to the menu. Open Link opens `http` and `https` links in the browser through `open` or `xdg-open`, never through a shell. Any other scheme, and web links with characters outside ASCII, ask first in a prompt PinFold draws, with the whole link shown and those characters escaped. Links over 2,048 bytes or that are not URIs are not offered. The window asks the core for the link under the cell, so the link is version 9.
- On macOS, right-clicking the Dock icon offers New Tab, Split Right, Split Down, Detach and one item per tab with its agent state as glyph and word ([#114](https://github.com/0xPiranhaCodes/pinfold.sh/issues/114)).
- A per-pane status state machine combines OSC 133 marks with authoritative API reports and rate-limited alerts.
- The release dry run builds Linux x86_64 and aarch64 tarballs with licenses and the font license, writes a SHA-256 file per artifact, and ships Homebrew formula and cask templates.
- Agent profiles in `[agents.<name>]` may set `sandbox`, which runs the agent under `sandbox-exec` on macOS or bubblewrap on Linux, inside its pane's session, with `~/.ssh`, PinFold's config and state, and other panes' terminals hidden.
- `pinfold agent <name> [--right | --down | --tab]` and the API's `agent.launch` open a pane running an `[agents.<name>]` profile by name, sandboxed if it says so, as a child of the caller (ADR-0036).
- On Linux a sandboxed agent's bubblewrap gets a seccomp filter that fails `ioctl(TIOCSTI)` with `EPERM`, so the agent cannot type into its pane's terminal; on architectures other than x86_64 and aarch64 the agent does not start.
- The control socket identifies each caller at accept from its controlling terminal and session, which the core maps to a pane, and caps peers at 4 per identity, with all unidentified callers sharing one cap (ADR-0032 step 2).
- A spike that answers the macOS Dock menu by adding `applicationDockMenu:` to winit's app delegate, in `docs/spikes/m3-dock-menu`.
- `pane.status` lets an agent report its own pane's state, which outranks OSC 133 marks; `pane.list` and events say whether a status came from marks or a report. Each tab shows its state's glyph in colour, a hidden tab whose pane needs you is marked ◆ in amber, and the title row gives the active state as glyph, colour and word with the agent's message.
- Right-click in a pane opens a menu drawn in cells: Copy, Paste, Select All, Find, Split Right, Split Down, New Tab, Zoom and Close Pane. Arrows, Enter and Esc work it as well as the mouse. When a program has turned on mouse reporting, the click goes to it, and `Shift`+right-click opens the menu (ADR-0033).
- A 256 KiB hash-chained API audit log with optional private, rotating persistence. The file must be a regular file, not a symlink or FIFO, and after a failed write the log stops mirroring to disk and keeps only memory.
- A native menu bar on macOS: PinFold, Shell, Edit, View, Window and Help, holding every Command shortcut, with new Select All, Bigger, Smaller, Actual Size, Settings and Help commands, and items greyed out when they cannot apply (ADR-0033).
- A control-socket connection takes many requests, one at a time: it must say `hello` within 2 s and end each line within 2 s of its first byte, and is closed after 60 s idle. Answers and events reach the API thread over bounded channels, and a subscriber that misses an event is closed (ADR-0032).
- ADR-0033: a native menu bar and Dock menu on macOS, and a right-click menu drawn in cells, all running the same commands as the keys.
- `pane.read`, `pane.last_output` and `pane.send` for a caller's own pane (the `self` scope; other panes are refused): reads are plain text, newest last, at most 2,000,000 bytes, and `last_output` follows OSC 133 marks. A send carries at most 4096 bytes, a pane takes 20 a second, and a full input queue refuses it. `pinfold ctl send` types into a pane. JSON Schemas in [docs/api](docs/api/README.md) (ADR-0032 step 4).
- Secret redaction for cross-pane text, covering private keys, provider tokens, JWTs, password pairs, the password in a URI and high-entropy strings.
- The core serves up to 16 callers of its control socket at once on one `mio` thread, so a caller waiting for a window holds up no one (ADR-0032).
- ADR-0032: the agent API gets one `mio` thread of its own, and M3 lands in eight steps. M1 and M2 are done.
- `pane.wait` waits on the caller's own pane for a command's exit code, the next prompt, any output or a timeout, with at most 64 parked, and `events.subscribe` sends pane opened, closed and status events; a connection that lets 2 MiB queue unread is closed (ADR-0032 step 6).
- Fuzz targets `api_request` and `api_params` feed arbitrary control-socket request lines and `hello` parameters to the API parser, in the fuzz smoke run on each PR and nightly.
- Tests that fail if the core stops refusing a debugger, or a pane's shell starts refusing one, and `tests/hardening/audit.sh`, which attacks real binaries by hand and is where the threat model's table comes from.
- ADR-0031: a macOS beta comes before v0.1, which covers macOS and Linux.
- ADR-0034, proposed: M5 in 13 steps. Snapshots hold the layout and the program each pane started, redacted, not the screen, restore runs only what a person trusted or confirms, and `pinfold up` opens a `.pinfold/layout.toml` trusted by its hash. Signing and notarization come first, for the macOS beta.
- A spike measures what waiting for each Metal frame costs: a median 0.44 to 0.47 ms on a full 200x60 grid on an M1 Pro, so the wait stays rather than spending about 750 KB on more instance buffers.
- The app is called PinFold: `PinFold.app`, the window's title and the Dock.
- API `pane.spawn` opens a split or tab running the user's shell without taking focus, and `pane.close` closes it: a caller reads, sends to, waits on and closes the panes it spawned and theirs, the `children` scope.
- Tab pills stand a little taller than a cell, with their text centred, in any font. A chosen font without `⌘` takes it from the bundled font.
- `hello` version 2 returns the version, the methods a caller may call and its pane id. `pane.list` lists the session's panes (id, title, cwd, command, status, and nothing from a screen) for any caller with a terminal, with a JSON Schema in [docs/api](docs/api/README.md). `pinfold ctl list` prints them, as JSON with `--json`. Version 1 still opens windows and cannot list panes (ADR-0032 step 3).
- Pill tabs in the top bar, with hover, a mark on a hidden tab that printed, a quiet look for an
  unfocused window, and no title row in fullscreen.
- Protected panes: `pane.send` is refused while any process in the pane's foreground group is ssh, mosh, sudo, su or doas, runs as root, cannot be read or matches an `api.protect` pattern, at a password prompt, or when marked. Sends to other panes refuse control characters and go a line at a time, each line at least 150 ms after the last, once the pane's shell holds the foreground again, checked just before it is written.
- Tabs and splits for real. The core owns the session, so a new window finds the same tabs and panes, and only the panes of the active tab are sent to the window, so a hidden tab costs no frame copies. A session holds at most 32 tabs and 64 panes, and past that a command is refused with a note under the pane. Panes are drawn in cells with cell-drawn borders, the focused pane's in the focus accent.
- `pinfold mcp` serves the API's pane tools over MCP on stdio, so an agent in a pane can list, read, wait on and type into its own pane.
- `pinfold mcp` can spawn and close child panes through versioned MCP tools.
- `pinfold mcp` has `events_next_v1`, which waits up to 30 seconds by default for the next pane opened, closed or status event, so an MCP client need not poll (ADR-0035).
- Prefix keys work: `Ctrl+a`, or `keys.prefix`, then `c`, `n`, `p`, `0` to `9`, `|`, `-`, the arrows, `z`, `d` and `x`. `x` asks first, and says so in red when it would end the session. On macOS Command+T opens a tab and Command+1 to Command+9 go to one. A click focuses a pane or picks a tab, and the `+` button opens one. Bindings are in [docs/configuration.md](docs/configuration.md).
- Cross-pane reads and sends ask through keyboard-only window chrome, with batched 30-minute grants that `pinfold ctl grants` lists and revokes.
- Workspace trust: `workspace.ask_trust` shows a project directory and every `run` line in window chrome, and `y` records the directory and layout SHA-256 in a private `trust.json` that `workspace.trust_state` checks. Nothing runs yet. The link's version is now 7.
- `pinfold up` asks for workspace trust when a layout has `run` lines, and a core that opens or restores a workspace types each line into its pane only if `.pinfold/layout.toml`, read again, still has the trusted SHA-256. Snapshots record each workspace's root and hash, never its lines.
- Cross-pane reads and sends ask through keyboard-only window chrome, with batched 30-minute grants that `pinfold ctl grants` lists and revokes. Approved actions and grant changes are recorded in the audit log.
- `pinfold-core` runs as a hardened daemon with a private control socket that takes `hello` and `window.open`.
- `pinfold ctl session save`, from a terminal outside any pane, writes the session's layout and each pane's directory and redacted command to `sessions/default.json` in the state directory, 0600 and atomically, with no scrollback or environment.
- The core also saves the session when its window closes, a minute after activity if the snapshot changed, and on SIGTERM or SIGHUP before it exits, and removes the file once the last pane has closed.
- A core that starts beside a saved session restores its tabs and splits, with a fresh shell in each pane's saved directory, or in `$HOME` if that is gone or anyone but you or root could write it or swap it by a symlink, and never runs a saved command. A snapshot that is a symlink, over 1 MiB, or does not parse or check is renamed to `default.json.bad`, and the core starts fresh.
- A restored pane whose saved command was not its shell shows, while focused, a grey strip under the panes, `○ was running: <command>  Enter run · Esc dismiss`. Enter types the command and a newline into the pane's shell, once the strip has been up in a focused window for 500 ms; an earlier Enter is swallowed. Any other key dismisses it. Only printable ASCII shows as is, and a command cut to fit, changed by redaction or holding a control character offers only Esc. The link is version 8.
- The core records each API request that reaches it in a hash-chained audit log, never pane contents or sent text; `audit.read`, `pinfold ctl log` and `pinfold ctl log --verify` read and check it, and `api.audit_log = true` also writes it to disk.
- `pinfold` is now a launcher: it starts the core if none answers, checks that the socket's owner is you, asks for a window and exits. The window is its own `pinfold-window` process on the link (ADR-0028), so a window that crashes or closes leaves every pane running and `pinfold` reattaches. A second `pinfold` brings the window to the front, and a window whose core dies says the session ended. `pinfold --size` is gone.
- `pane.wait` takes `until: "match"`, a regex of at most 1 KiB over lines printed after the wait began, with ASCII-only `\w`, `\d`, `\s` and case folding, and `until: "idle"`; both take only the caller's own pane, as does `pane.output` in `events.subscribe`; and `pinfold ctl wait` waits from a script, exiting 0, 1 on a timeout or 2 on an error.
- `cargo xtask mem-gate` measures the window and the core as separate processes, and the core beside a window.
- `pinfold-mux` exports a session's layout as a plain tree that names panes by index, and rebuilds a session from one, refusing a bad tree with an error.
- A TOML config file with `font_size` and `macos_option_as_alt`. A bad file means the defaults.
- Workspace layout parsing for `.pinfold/layout.toml`, bounded to 32 tabs, 64 panes and 64 KiB, with unknown keys, symlinks and project-escaping working directories refused.
- `pinfold up [dir]` opens a project's `.pinfold/layout.toml` as shells through the new `workspace.open` API method, and a second run focuses the open tabs instead of opening a copy. `run` lines wait for workspace trust.
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

- macOS: frames share one Metal instance buffer, so the window's idle memory reads steadily.
  The window budget rises to 33 MB plus pixels (ADR-0030).
- A core test that failed on macOS when its stand-in window had already ended.
- `pinfold` waits until the window is on screen, and fails with a short reason if the window cannot run, ends during start or is not ready in time: 10 seconds for a core's first window, so a slow Mac does not report failure for a window that opens, and 2 seconds after that. It used to exit 0 once the process existed, and said only "no window can open".
- Bold `i` and other glyphs of several contours no longer come out thinner than regular.
- A paste or a key no longer vanishes while the core lags. The window waits for the link in order, holding at most about 3 MiB, and past that says so under the pane, as it does for input a stalled program has no room for. A paste over the 1 MiB limit, less its brackets, is refused with a note instead of going missing. The link's version is now 4.
- Fallback font pages no longer count against the window once a frame is drawn.
- A copy of a whole pane can run in steps, which keeps its text out of the core's footprint.
- A core started by `pinfold`, and its windows, write problems to `default.log` in the runtime directory, so a window that dies says why even from the Dock, and a failed `pinfold` names the file. It is private, starts empty with each core, and a panic there prints only its place, as a panic's text can quote a pane.
- A window taller than 24 rows no longer costs the core about 2 MB, as growing a grid makes only the rows it needs.
- The core sends a window only the panes it asked for, so a hidden pane holds no frame.
- The config file is `pinfold.toml`, the name the design gives it.
- Closing a pane with unread output can no longer hang on macOS.
- A window narrowed to one column with a wide character on screen no longer exhausts memory.
- An unterminated OSC string can no longer grow the core without limit.
