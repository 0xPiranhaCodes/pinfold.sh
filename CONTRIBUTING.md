# Contributing to Pinfold

Thanks for your interest. Pinfold is pre-alpha, so the code is young and the
design documents carry much of the weight. Please read
[`docs/design/`](docs/design/) and [`docs/adr/`](docs/adr/) before proposing
large changes.

## Development setup

You need [rustup](https://rustup.rs). `rust-toolchain.toml` selects the exact
toolchain, so there is nothing else to pin by hand.

```sh
git clone https://github.com/0xPiranhaCodes/pinfold.sh.git pinfold
cd pinfold
cargo build
cargo xtask --help
```

On macOS a `cargo` build lacks the hardened runtime, so its window shows a notice and keeps
`▲ unhardened` in the tab row ([ADR-0044](docs/adr/0044-cores-without-the-hardened-runtime-warn.md)).
To run a source build as hardened as `PinFold.app`, sign it after each build, with no Apple
account (use `target/debug` for a debug build):

```sh
codesign --force --sign - --options runtime target/release/pinfold-core target/release/pinfold-window
```

Optional tools, used by the checks below:

```sh
cargo install --locked cargo-deny cargo-audit cargo-vet cargo-fuzz typos-cli
```

## The workspace

| Crate            | Purpose                                                       |
| ---------------- | ------------------------------------------------------------- |
| `pinfold-core`   | Daemon library: PTY, `VtEngine`, mux, sessions, status engine |
| `pinfold-proto`  | API types, JSON Schemas, version negotiation (no I/O)         |
| `pinfold-mux`    | Layout model: session, tab, split tree, pane (no I/O)         |
| `pinfold-api`    | Socket server, scopes and approvals, audit log                |
| `pinfold-render` | `Renderer` trait, CPU renderer, glyph cache, optional GL      |
| `pinfold-ui`     | Window process: winit, chrome as cells, input, IME            |
| `pinfold-tools`  | Markdown reader, file manager, diff viewer                    |
| `pinfold-config` | TOML schema, validation, hot reload, permission checks        |
| `pinfold-sys`    | The only crate allowed `unsafe`                               |
| `pinfold-cli`    | The `pinfold`, `pinfold-core` and `pinfold-window` binaries   |

## Running the checks

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check
cargo audit
cargo vet --locked
```

### Fuzzing

Fuzz targets live in `fuzz/` and need `cargo-fuzz` and the nightly toolchain pinned in
`xtask/src/main.rs`:

```sh
rustup toolchain install nightly-2026-10-15 --profile minimal
cargo +nightly-2026-10-15 fuzz run vt_input -- -dict=fuzz/vt.dict -max_total_time=60
```

`cargo xtask fuzz-smoke` runs every target for 30 seconds, which is what CI does on each
PR. Nightly runs it for 30 minutes with `FUZZ_SECONDS=1800`. Any crash is a release blocker.
A target with a folder in `fuzz/seeds/` starts from those committed inputs as well as
`fuzz/corpus/`.

### The memory gate

`cargo xtask mem-gate` measures real processes against the budgets in
[ADR-0021](docs/adr/0021-memory-targets-m0.md). It prints a table and fails if a budget is
exceeded or a measurement fails. If your change affects memory, paste that table into your
PR. Budgets that nothing can measure yet show as `not measured` with the issue that adds
them, and those rows prove nothing. On Linux it needs an X server, such as `xvfb-run`.

The macOS window row means little on hosted CI, which reads about 30 MB where an M1 Pro with a
Retina display reads about 51 MB. For a change that can affect it, run the gate on a real Mac.

Window rows read a `pinfold-window` started by `pinfold-core --window <panes> <size>`, which is a
core without hardening whose window has none either, as Linux hides a hardened process from
`/proc`. So the window and the core are read as separate processes. A last row reads the core
beside such a window, against the core's budget. Run the gate with `XDG_CONFIG_HOME` set to an
empty directory, as your own config changes what a window costs.

Each window has a second row, `with text`, against the same budget. It runs
`tests/window/heavy-screen.sh` as the window's shell, which fills the screen from
`heavy-screen.txt` with plain, bold and italic ASCII and CJK, then waits. The row needs a CJK
font: if the window opened none of the fallbacks in `pinfold-render`, its result says
`no CJK font` instead of `pass`, and the gate does not fail for that. Over budget still fails.
The container in `tests/window/` installs no fonts, and the CI jobs install none either.

Three more rows per window load it the way a long session does, against the same budget.
`with a full glyph cache` runs `tests/window/full-cache.sh`, which pages through all of
`heavy-screen.txt` a screen at a time, with lines wrapped so no glyph is clipped. That text holds
more distinct characters than Metal's atlas holds, which a test in `xtask/src/mem_gate.rs`
checks, so the cache is full when the window is read. `with 8 tabs` opens eight shells, so the
tab bar draws eight tabs. `with its context menu open` runs the heavy screen with
`PINFOLD_MEASURE_MENU` set, which a measured window, and only a measured one, takes as asking for
its right-click menu to open in the middle of the grid. The Dock menu has no row: the Dock draws
it, and no script can right-click the Dock. A pane's scrollback has no row in the window, as the
window only holds the rows on screen; the core's `Scrollback at its cap` row covers it.

### The soak

`cargo xtask soak --minutes 60` runs one core for an hour, as nightly does under Xvfb. It runs
the xtask binary as the core's shell: the first pane drives the session through the API, and
every pane it spawns is a plain `/bin/sh`. Each round of at least a second opens a tab, splits
it twice and closes all three, checks with `pane.list` that no kept pane was lost, and times
five API round trips in a shell pane, from `pane.send` until `pane.read` shows the line. Each
period gets a fresh shell pane for them, as `pane.read` reads the whole history and would slow
with it. Two other panes
print `seq 1 50000` every second for the whole run. Every fifth round kills the window,
as a crash would, and opens a new one with `window.open`. Config, state, data and the socket
live in a temporary directory, so your own session is never touched.

The run is cut into ten periods, and the table gives each one's round trip p50 and p99 and the
core's memory at its end, read as the memory gate reads it. The first period is warm-up. It fails
if the core grows by more than `GROWTH_LIMIT` (1 MB) past warm-up, if a period's round trip p99
passes `ROUND_TRIP_LIMIT` (33 ms), if one takes over 2 s, or if a pane, the window or the core is
lost. The limits and their reasons are in `xtask/src/soak.rs`. The round trip is not the
keystroke-to-echo budget in `docs/design/architecture.md`, which `cargo xtask latency` checks: it
adds two API calls per poll and a read of the pane's history, and catches the core slowing over a
long session rather than one keystroke's time.

A weekly run soaks for 300 minutes, as long as a hosted runner allows. For longer, or on your own
machine, `tests/window/soak.sh <minutes>` runs the soak on Linux in the window image, and with
`HEAP=1` it attaches heaptrack to the core and prints its live heap through the run.
[`benches/soak.md`](benches/soak.md) has an eight-hour run: the core's growth levels off, and
its live heap does not grow.

`tests/window/run.sh` drives the real processes in a container: it types into a shell under
Xvfb through `pinfold`, kills the window and reattaches, kills the core and checks that the window
says so, and starts a window under headless Weston. `crates/pinfold-cli/tests/session.rs` covers
the same session without a display, using a client on the real link in place of the window.
Its helper processes take their directory from `helper`, which notes a panic in `failed` there,
and write what a test waits on with `note` (or the shell's `into`), which renames a finished file
into place. `Session::wait_for` then fails at once with the helper's panic instead of after its
120 s patience.

Every number in the docs or a PR must come from committed code. If you cannot
reproduce it with a script in this repository, leave it out.

## Testing expectations

Every PR that changes behavior needs tests. This list says what exists today; a layer
we want but do not have links to its issue.

- **Unit tests** sit next to the code. Tests that need a real core, window link or PTY
  go in the crate's `tests/`, such as `crates/pinfold-cli/tests/session.rs`.
  `crates/pinfold-cli/tests/repeat.sh RUNS [arguments]` runs that binary in a loop and counts
  failed runs, for measuring a flake. On macOS, Gatekeeper checks each unsigned program the
  tests place in a temporary folder and sometimes kills one at exec, about once in 100 runs
  of `session` on an arm64 Mac (#288); `repeat.sh` lists each refusal from the system log.
  Add the app your terminal runs in to System Settings > Privacy & Security > Developer
  Tools to stop those checks.
- **Property tests** (`proptest`) cover the config parser (`crates/pinfold-config`), the
  window link decoder (`crates/pinfold-proto/src/link.rs`), the layout tree
  (`crates/pinfold-mux`), the VT engine on arbitrary bytes split across reads and resizes
  (`crates/pinfold-core/src/vt.rs`), the core's window link under any commands
  (`crates/pinfold-core/src/link.rs`), the scrollback encoding round-trip
  (`crates/pinfold-core/src/scrollback.rs`) and the scrollback against a stock grid, and
  reflow (`crates/pinfold-core/tests/`).
- **Fuzz targets** in `fuzz/fuzz_targets/` cover the VT input path (`vt_input`), the
  window link decoder (`link`), the control socket's JSON-RPC requests (`api_request`,
  `api_params`), the paste sanitizer (`paste`), session snapshots (`session_snapshot`),
  repository layouts (`workspace_layout`) and the user config parser (`config`). The
  markdown renderer and unified-diff parser have none yet
  ([#235](https://github.com/0xPiranhaCodes/pinfold.sh/issues/235)).
- **Snapshot tests** (`insta`) cover OSC 133 and OSC 7 parsing
  (`crates/pinfold-core/tests/shell_marks.rs`) and the VT grid after plain text, wrapping,
  a scroll region, the alternate screen, SGR attributes and wide characters
  (`crates/pinfold-core/tests/grid_snapshots.rs`, files in `tests/snapshots/`). Run
  `INSTA_UPDATE=always cargo test` to rewrite a changed snapshot, then review the diff.
  The window's chrome as cells, from pane borders, the bar and its status line, an
  approval prompt and the right-click menu, is in `crates/pinfold-ui/src/snapshots.rs`;
  the bar's snapshot is per system, so a change to it needs `@macos` and `@linux` rewritten.
  `every_api_method_replies_as_its_snapshot` in `crates/pinfold-cli/tests/session.rs`
  snapshots the JSON-RPC reply of each API method from a real core, from a pane and from
  a terminal outside every pane, with times, hashes, paths and audit sequence numbers
  replaced by their key names. Tool panes are still to come
  ([#233](https://github.com/0xPiranhaCodes/pinfold.sh/issues/233)).
- **Golden images** of renderer output are in `crates/pinfold-ui/src/golden.rs`: plain text,
  wide characters and the cursor on one, bold, italic, inverse and colours, pane borders, the
  bar, an approval prompt, and the first screen of each built-in tool (`md`, `files`, `diff`),
  whose output goes through a core VT engine first, drawn by the CPU renderer's painter at 26 pixels (13 points at
  2x) with the bundled font alone, so no installed font changes them; it has no CJK, so wide
  characters draw as its missing-glyph box. Each is compared with its PNG in `src/golden/`,
  and a channel may differ by 2, as only the system's `powf`, in the sRGB tables, can differ
  by platform; with none they matched exactly on all three CI test runners. A failure writes
  `NAME.actual.png` and `NAME.diff.png`, with differing pixels red, beside the reference. Run
  `PINFOLD_UPDATE_GOLDEN=1 cargo test -p pinfold-ui golden` to rewrite them, then look at the
  images. The bar has one per system, as for its snapshot; take `@linux` from a Linux run. The
  renderer draws no underline and only a block cursor, so there are no images of those yet.
- **Recorded agent sessions** of Claude Code and Codex CLI are in
  `crates/pinfold-core/tests/agent_sessions/`, raw PTY output with timing from a zsh that
  sends OSC 133 marks. `tests/agent_sessions.rs` replays each through the VT engine and
  status engine, one read at a time, and compares the statuses shown with `NAME.states`; it
  also fails on a fixture over 200 KB or one holding a home path, email address, `sk-` key,
  or the host or user name. To re-record, run `./record.sh` there (or `./record.sh codex`
  for one) with the CLIs installed and logged in, update the table in its `README.md`, and
  read the new `.rec` before committing it. Each run costs a few cents of model use.
- **Approval-prompt tests** prove that only the window's keyboard answers a prompt:
  `no_api_method_can_answer_an_approval_prompt` in `crates/pinfold-api/src/control.rs`,
  the tests in `crates/pinfold-ui/src/approval.rs`, and
  `only_the_window_answers_cross_pane_actions_and_ctl_revokes_grants` in
  `crates/pinfold-cli/tests/session.rs`.
- **Security tests**: the control table in the
  [threat model](docs/design/threat-model.md#controls-and-their-tests) names the test for
  each control of T1–T10, or says it is untested
  ([#234](https://github.com/0xPiranhaCodes/pinfold.sh/issues/234)).
- **Conformance**: `tests/conformance/esctest.sh` runs esctest2 against the VT engine
  and compares the tests that pass with `tests/conformance/esctest-pass.txt`. A listed
  test that fails exits 1; a newly passing one is printed, and `--update` rewrites the
  list. It needs `git` and `python3`, fetches esctest2 (GPL-2.0, run as a separate
  program, never vendored) at a pinned commit into `target/esctest/`, and takes about
  2 minutes. The log is `target/esctest/esctest.log`. To rerun some tests, run
  `target/debug/esctest-driver python3 target/esctest/esctest2-*/esctest/esctest.py
  --include REGEX --no-print-logs --logfile FILE`. CI runs it on Linux, and the list comes from Linux.
  `esctest-driver` answers esctest's DECRQCRA screen reads from the engine's grid,
  characters only. vttest is not run: it draws screens for a person to judge and has no
  pass or fail output to script.

## Commits and pull requests

- **Keep PRs small:** ideally under about 400 changed lines, one concern each.
- **Branch per change:** open PRs into `main`. Direct pushes to `main` are not
  allowed.
- **Conventional Commits:** use them for commit messages and PR titles, for example
  `feat(api): add hello handshake`. Common types are `feat`, `fix`, `docs`,
  `refactor`, `perf`, `test`, `build`, `ci` and `chore`. Commits are kept on merge, so
  each one should read well in the changelog. PR titles are checked in CI too.
- **Fill in the PR template:** the memory-impact and security-impact sections are
  required, even if the answer is "none".
- **Update `CHANGELOG.md`:** add a line under `[Unreleased]` for any user-visible
  change.

### Developer Certificate of Origin

We use the [DCO](https://developercertificate.org), not a CLA. Sign off each commit:

```sh
git commit -s
```

This adds a `Signed-off-by:` line, which certifies that you have the right to submit
the work under the project's licenses. CI rejects unsigned commits. If you forget,
`git commit --amend -s` fixes the latest commit.

Dependabot cannot sign off, so its commits skip this check, and every other check
still applies. A commit is exempt only when the pull request was opened by
`dependabot[bot]` and the commit's author email is
`49699333+dependabot[bot]@users.noreply.github.com`. Anyone can forge an author
email, but not who opened the pull request, so a commit you push onto a Dependabot
pull request still needs your sign-off. `.github/scripts/test-check-dco.sh` tests
these cases.

AI assistants cannot certify the DCO. If a tool wrote part of your change, you are
the one signing off for it, so review it as your own work.

### Licensing of contributions

Unless you state otherwise, contributions are dual-licensed as `MIT OR Apache-2.0`,
with no additional terms. The license is declared by SPDX identifiers in `Cargo.toml`.
Per-file license headers are not required. Code derived from `alacritty_terminal`
is the exception and stays Apache-2.0 only
([ADR-0012](docs/adr/0012-license.md)). `vendor/README.md` says how to change it.

### New dependencies

Justify every new dependency in its PR: why it is needed, its license, maintenance
status, `unsafe` usage and memory cost. Check the current version on crates.io and
docs.rs first. A dependency that pulls in an async runtime, a GUI toolkit or a C
library needs maintainer approval before work starts.

`cargo vet --locked` must pass. [`docs/security/dependency-review.md`](docs/security/dependency-review.md)
says which criteria apply, whose audits we import, how to review build scripts, proc
macros, native code and `unsafe`, and when a temporary exemption is allowed.

## ADRs and RFCs

- **ADR (Architecture Decision Record):** if a future contributor would ask "why?"
  about a choice, record it. Copy [`docs/adr/0000-template.md`](docs/adr/0000-template.md),
  take the next number, and open it with your PR.
- **RFC:** for a large or cross-cutting change, open a GitHub Discussion in the Ideas
  category before writing code. Once the direction is agreed, the outcome becomes an
  ADR.

ADRs are immutable once accepted. To change one, add a new ADR that supersedes it.
The binding decisions, the security model, and the license and governance files can
change only with maintainer approval.

## What "good first issue" means here

An issue labeled `good first issue` is one a newcomer can finish in a few hours. It
names the files involved, says what "done" looks like, and touches neither
`pinfold-sys` nor the security model. If you are stuck on one, ask on the issue;
that means the issue needs better notes.
