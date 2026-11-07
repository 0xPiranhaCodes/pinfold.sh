# ADR-0045: The built-in tools are programs in their own pane

- **Status:** Proposed
- **Date:** 2026-11-07

## Context

[ADR-0024](0024-tools-in-v0-2.md) moved the markdown reader, the ranger-style file manager and
the git diff viewer (M4) to v0.2. Nothing of them exists yet: `crates/pinfold-tools/src/lib.rs`
is one doc comment. `docs/design/architecture.md` describes them as in-process `Tool` panes in
the window, drawn with ratatui through a backend that writes into PinFold's grid, and
[ADR-0003](0003-process-model.md) says the window process hosts them. That design predates
the measurements and controls that bind them now:

- The core must never panic on external input, and a crash in the window or tools must never
  kill a PTY (ADR-0003, [ADR-0017](0017-panic-strategy.md)). The tools parse hostile input:
  repository files, file names and the output of `git` run inside an untrusted repository.
- Plugins are external processes that use the API, and all UI is cells, with no GUI toolkit
  ([ADR-0005](0005-renderer.md), [ADR-0011](0011-configuration.md)).
- The window process has a fixed budget of 8 MB on Linux and 33 MB on macOS beyond its pixels
  ([ADR-0021](0021-memory-targets-m0.md), [ADR-0030](0030-macos-window-fixed-part.md)), and
  "a tool's memory is back with the OS within 2 s of closing it" (`architecture.md`).
- Threat T5 needs one hardened git entry point ([#262]), T6 needs escaped control and bidi
  characters plus size and time caps ([#263]), and the tools owe fuzz targets ([#235]) and
  snapshots ([#233]).

Measurements behind this ADR, from scripts in [`docs/spikes/m4-tools/`](../spikes/m4-tools/):

| Crate (from `deps.sh`) | Crates in its tree | `unsafe` in its own source | `cargo deny` under our `deny.toml` |
| --- | --- | --- | --- |
| `pulldown-cmark` 0.13.6, no default features | 4 | 5 in 13,840 lines | passes |
| `similar` 2.7.0, already in `Cargo.lock` | 1 | 0 | passes |
| `unicode-width` 0.2.3, already in `Cargo.lock` | 1 | 0 | passes |
| `ratatui` 0.30.2 with its crossterm backend | 60 | 0 | fails: `hashbrown` 0.17 is Zlib |
| `crossterm` 0.29.0 | 26 | 17 | passes |
| `syntect` 5.3.0, fancy-regex, no plist or YAML | 28 | 1 | fails: `bincode` 1.3.3, RUSTSEC-2025-0141 unmaintained |
| `ignore` 0.4.34 | 13 | 0 | passes |
| `notify` 8.2.1, kqueue on macOS | 10 | 32 | fails: CC0-1.0 and ISC licences |

None of them pulls in an async runtime or builds C. `syntect`'s default regex engine is
Oniguruma, a C library, so it was measured with `fancy-regex` instead. An idle release
`pinfold mcp` process uses 2.10 MB (`baseline.sh`, median of 3, macOS `phys_footprint`), and
the stripped `pinfold` binary is 0.77 MB.

## Decision

### 1. Process model: each tool is a program running in its own pane

`pinfold md <file>`, `pinfold files [dir]` and `pinfold diff [--staged] [range]` are
subcommands of the existing `pinfold` binary, with their code in `pinfold-tools`. Each is a
full-screen terminal program, like ranger or `less`. Run in a shell pane, it simply runs there.
Opened by key, from the palette or over the API, the core starts it in a new pane.

| Option | Why not |
| --- | --- |
| In the core | Puts markdown, file-name and git-output parsing in the only long-lived process, whose panic ends every PTY, and adds their memory to its 4 MB budget. |
| In the window | A tool panic or stall takes the window down with it; the window's fixed budget absorbs every tool's peak; and its allocator need not return a closed tool's pages, against the 2 s target. ratatui into the window grid also needs an adapter between two cell models. |
| A tool process that draws through a new API method | A new drawing protocol for the window to composite, a trust question about who may draw where, and a decoder to fuzz, when the PTY is already a cell protocol with a fuzzed, filtered parser. |
| **A program in a PTY pane (chosen)** | Costs one core pane, ≤ 250 KB by ADR-0021, plus a process that exists only while the tool is open. |

This crosses no boundary that does not already exist. The tool's output passes the core's
escape-sequence filter and VT engine like any program's. A tool that crashes or hangs costs its
own pane; closing the pane hangs it up. Its memory is returned when the process exits. The
window and the core gain no parsers.

If accepted, this ADR replaces the clause of ADR-0003 that has the window host the built-in
tools, and the in-process `Tool` trait and ratatui backend of `architecture.md`. ADR-0003 is
otherwise unchanged, and neither document is edited until the maintainer accepts this one.

### 2. Drawing, panes, tabs, status and keys

- **Cells.** A tool keeps a `Screen` of cells, each a character and a style, and writes only
  the rows that changed since the last frame, in the alternate screen and inside a synchronized
  update, which the core already bounds to 150 ms and 256 KiB. Colours are the 16 palette
  roles, so the user's theme applies. Links are OSC 8, which the core already limits. There is
  no ratatui: a tool owns its whole pane, and its layouts are columns, lists and scrolling.
- **Pane and tab model.** The core records the pane's kind as `tool` with the tool's name and
  argument, so `pane.list` shows it, the session snapshot restores it by running the same
  tool again, and the tab is titled with the tool and its argument, escaped (section 4).
  Restoring a scroll position waits for a later slice.
- **Status.** A tool pane has no agent state, so the bar shows none of ◆ ● ✓ ✕ ○ for it. A
  tool that cannot go on, such as for a missing file, draws ✕ error in red with the reason and
  exits on the next key. A tool exits on `q`. Either way its pane goes, as any pane goes when
  its program exits, and so does a tool that crashes.
- **Keys.** The window's own bindings (Alt+arrows, the prefix, the palette) are taken first, as
  in any pane. Everything else reaches the tool as terminal input. Every tool uses hjkl and the
  arrows to move, `/` to search, `?` for its keys and `q` to quit. Mouse input uses the SGR
  mode the core already reports.
- **Prompts.** The window draws security prompts (approvals, trust) outside pane text, as now.
  A tool's own confirmations, such as "trash these 3 files", are drawn in its pane. They stay
  trustworthy because file contents and names are escaped before they are drawn, and because no
  API caller can type into a tool pane (section 4).

### 3. Parsing and dependencies

| Need | Choice |
| --- | --- |
| Markdown | `pulldown-cmark` 0.13.6 without default features: MIT, 4 crates, 5 `unsafe` in 13,840 lines, streaming, released 0.13.6 on 2026-10-30. `comrak` builds a whole AST, which costs memory per node. |
| Diff source | The user's `git`, through the one entry point in section 4. `git2` links libgit2, a C library, which would need the maintainer; `gix` is a large tree for the few commands a viewer runs. |
| Diff parser | Our own unified-diff parser, fuzzed. |
| Word highlights | `similar` 2.7.0, already in the lockfile: Apache-2.0, no dependencies, no `unsafe`, with a deadline per diff. |
| Width | `unicode-width` 0.2.3, already in the lockfile. |
| Terminal | Raw mode through a new `pinfold-sys` function, beside its existing `termios` code and with a `// SAFETY:` comment, and our own key decoder, fuzzed. Not `crossterm`: 26 crates for one `termios` call and a decoder. |

Rejected for v0.2: `ratatui` (60 crates and a Zlib licence exception, for layouts a whole-pane
tool does not need); `syntect` (an advisory that fails `cargo deny`, or Oniguruma in C), so code
blocks and previews are one colour, and highlighting gets its own ADR; `ignore` (13 crates,
including crossbeam), since `git status --ignored` already says what is ignored; `notify` (CC0
and ISC licence exceptions), since the tools poll. The only new crates are `pulldown-cmark` and
`unicase`, both MIT; none needs the maintainer.

**Polling.** A program in a pane cannot tell whether it is visible, so watching is opt-in and
cheap. The file manager checks its directory's modification time once a second. The diff viewer
in watch mode (`--watch`, or `w`) re-runs git every 2 s or 20 times its last run's duration,
whichever is longer. Follow mode in the reader checks the file's modification time once a
second.

### 4. Security

**One git entry point (T5, [#262]).** `pinfold_tools::git::run(repo, args)` is the only place
that starts git, and a test fails if `Command::new` appears anywhere else in the crate. Each call:

- clears every inherited `GIT_*` variable, then sets `GIT_TERMINAL_PROMPT=0`,
  `GIT_OPTIONAL_LOCKS=0` and `GIT_NO_LAZY_FETCH=1`, so a partial clone cannot fetch and run a
  remote helper (a git too old to know the variable ignores it, which the test would show);
- passes `--no-pager -c core.fsmonitor=false -c core.hooksPath=/dev/null -c core.pager=cat`,
  and `--no-ext-diff --no-textconv --no-color` to `diff`;
- first reads the repository's configuration with `git config --list -z`, which runs nothing,
  and overrides every `filter.*.clean`, `filter.*.smudge` and `filter.*.process` with an empty
  value and `filter.*.required` with false, since `git diff` runs clean filters on work-tree
  files. Repositories that need a filter, such as Git LFS, show pointer files;
- respects `safe.directory`, and runs with a 10 s timeout and an 8 MB cap on output, after which
  git is killed and the view says it was cut short.

The test builds a repository whose configuration sets each of these to a command that creates a
marker file, then runs every git call the tools make (`diff`, `diff --staged`, `status`,
`apply --cached`, `apply -R`) and fails if a marker exists.

**Escaping (T6, [#263]).** Every string reaches the screen through `Screen::put`, which escapes
it, so there is no unescaped path. C0 controls become `^X`, DEL `^?`, C1 controls `<U+0085>`,
invalid UTF-8 `\xNN`, and the bidi controls (U+061C, U+200E, U+200F, U+202A–U+202E,
U+2066–U+2069) `<U+202E>`. A file name with any of them is flagged red, as the Files board
shows. The core's escape filter stays as a second layer.

**Caps (T6).** Each is a constant with a test that fails if it is removed:

| Tool | Size | Time |
| --- | --- | --- |
| Markdown reader | Files over 5 MB show their first 5 MB as plain text, with a banner | Parsing stops after 2 s and the rest shows as plain text |
| File manager | 10,000 entries per directory, the rest counted; previews read 64 KiB, hex 4 KiB | `git status` 2 s |
| Diff viewer | 8 MB of git output; word highlights only on lines up to 1 KiB | git 10 s; `similar` 50 ms per hunk |

**What the API exposes.** One new method, `tool.open`, with `tool` (`md`, `files` or `diff`),
`path`, `staged`, `range`, `place` and `of`. The threat model already lists it as granted.

- The core starts its sibling `pinfold` binary, found as it finds `pinfold-window`, never
  through `PATH`, with a fixed argument list and the caller's values after `--`.
- `path` must be absolute and exist: a file for `md`, a directory for `files` and `diff`.
  `range` must not start with `-` and may hold only revision characters (`A-Z a-z 0-9 . _ / ~ ^
  @ { } -`), one `..` or `...` at most. Anything else is refused before a process starts.
- `pane.send` to a tool pane is refused for every caller, including the agent that opened it, so
  no API caller can stage, revert, trash or rename. Only the window's keys reach a tool.
- A tool pane is an ordinary API caller with no extra rights. Inserting a path into an agent's
  prompt (`@` in the file manager) and sending a hunk as review feedback are `pane.send` calls
  to another pane, which the window approves as any cross-pane send is.

The file manager has no "run" action in v0.2: Enter opens executables in the pager. `$EDITOR`
runs inside the tool's own pane, which redraws when it exits. Trash is a rename into the
platform's trash folder (`~/.Trash`, or the XDG trash with its `.trashinfo`); across file
systems it refuses rather than copying.

### 5. Memory budgets

Targets for `cargo xtask mem-gate`, measured as PSS on Linux and `phys_footprint` on macOS,
in MB of 10^6 bytes. They are targets until the slice that adds each row measures it, and a
missed target is reported with its number.

| Metric | Target |
| --- | --- |
| A tool process, nothing open | ≤ 3 MB (an idle `pinfold mcp` measured 2.10 MB) |
| Markdown reader | ≤ 3 MB + 3 × file size, at most 18 MB at the 5 MB cap |
| File manager | ≤ 3 MB + 512 B per listed entry, at most 8.12 MB at 10,000 entries |
| Diff viewer | ≤ 3 MB + 3 × git output, at most 27 MB at the 8 MB cap |
| Each tool pane in the core | ≤ 250 KB, the existing pane budget; the alternate screen keeps no scrollback |
| Window with a tool pane open | The existing window budget, unchanged |
| After a tool pane closes | The tool process is gone, and the core is back within its budget in 2 s |

### 6. Tests

- **Fuzz targets ([#235]):** `markdown` (bytes and a width from 1 to 300 never panic, and the
  laid-out cells hold no unescaped control or bidi character), `unified_diff`, and `tool_keys`
  for the key decoder. Each gets seeds in `fuzz/seeds/` and joins `cargo xtask fuzz-smoke`.
- **Snapshots ([#233]):** `insta` snapshots of each tool's `Screen` rows and style runs at 80×24,
  as `pinfold-ui/src/snapshots.rs` does: headings, lists, a table, code, links and a bidi file
  name in the reader; escaped names, badges and the cap message in the file manager; unified,
  side-by-side and cut-short diffs. `tool.open`'s reply joins the API snapshots.
- **Golden images:** one per tool through the CPU renderer, using `pinfold-ui/src/golden.rs`,
  from the tool's output fed through a core VT engine.
- **Security and end to end:** the hostile repository above; one test per escape and per cap;
  a session test where an agent opens a tool, its `pane.send` to it is refused, the tool is
  killed, and every other pane keeps running.

### 7. Slices

Each is one PR of about 400 changed lines, in this order. None is opened as an issue until this
ADR is accepted.

1. `pinfold-tools`: `Screen`, escaping, the row-diff writer and the key decoder; raw mode in
   `pinfold-sys`; the `tool_keys` fuzz target.
2. Markdown reader: layout with `pulldown-cmark`, its caps, `pinfold md`, snapshots and the
   `markdown` fuzz target. Closes [#263].
3. Markdown reader: outline, search, links and follow mode.
4. Core: the `tool` pane kind, `tool.open` with its schema, MCP tool and `pinfold open`, refusing
   `pane.send` to tool panes, restore, and the session test.
5. The git entry point and the hostile-repository test. Closes [#262].
6. Unified-diff parser and the `unified_diff` fuzz target. Closes [#235].
7. Diff viewer: file list and unified hunks, `pinfold diff`, its caps and snapshots.
8. Diff viewer: side by side, word highlights and watch mode.
9. Diff viewer: stage and revert through `git apply`, with a confirmation, and a hunk sent to an
   agent pane.
10. File manager: columns, preview, plain and hex views, its caps, flagged names and snapshots.
    Closes [#233] with the snapshots of the other tools.
11. File manager: git badges, polling, fuzzy jump and bookmarks.
12. File manager: rename, copy, move and trash with confirmations, `$EDITOR`, and `@`.
    "New agent here" needs `agent.launch` to take a directory, an API change for its own slice.
13. Golden images and the mem-gate rows; the tools sections of `architecture.md`, the
    threat-model table rows for T5 and T6, and ADR-0003's pointer to this ADR.

## Consequences

The tools cannot take down the core or the window, and give their memory back by exiting. They
also work in any terminal, over ssh, outside PinFold. The core gains one method and one pane
kind, and the window gains nothing. Only two new crates enter the tree.

v0.2 ships without syntax highlighting, without live file watching, and with Git LFS diffs shown
as pointers. Each tool pane costs a core pane, and every frame passes through the VT parser,
which keeps a tool from drawing outside its pane but adds a copy per frame. Tool confirmations
live in pane text, so their safety rests on escaping and on refusing `pane.send` to tool panes.

Revisit if a frame through the VT engine is visibly slow, if a mem-gate row misses its target,
if `syntect` drops `bincode` or highlighting becomes a requirement, or if third-party tool
panes need a drawing API after all.

[#233]: https://github.com/0xPiranhaCodes/pinfold.sh/issues/233
[#235]: https://github.com/0xPiranhaCodes/pinfold.sh/issues/235
[#262]: https://github.com/0xPiranhaCodes/pinfold.sh/issues/262
[#263]: https://github.com/0xPiranhaCodes/pinfold.sh/issues/263
