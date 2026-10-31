# Pinfold contributor conventions

For AI and human contributors. Keep this file current when a convention changes.

## Decision order

Binding ADRs (`docs/adr/`) override `docs/design/architecture-review.md`, which
overrides `docs/design/architecture.md`. If something is still ambiguous, write an ADR
proposing an answer and ask the maintainer.

Read `docs/design/threat-model.md` before touching the API, input handling or anything
that crosses a trust boundary.

## Stop and ask the maintainer before

- changing an accepted ADR, the security model, or license and governance files
- adding a dependency that pulls in an async runtime, a GUI toolkit or a C library
- creating releases, tags or crates.io publishes
- anything that spends money or needs maintainer credentials

## Code

- `unsafe` lives only in `pinfold-sys`, and every block there has a `// SAFETY:`
  comment. All other crates forbid it.
- `pinfold-core` is the only long-lived process, and it must never panic on external
  input: PTY bytes, API requests, config or files. A crash in the window or tools must
  never kill a PTY.
- The core has no async runtime: `mio` and a fixed set of threads (ADR-0007).
- Errors: `thiserror` in libraries, `anyhow` only in binaries.
- `unwrap`, `expect`, `panic`, `todo`, `dbg!` and `print_stdout` stay in tests and
  binaries' `main`.
- Logging: `tracing` with structured fields. Secrets, tokens and pane contents stay
  out of logs by default.
- Public APIs are small and documented with examples. Hot paths use explicit types.
- Plugins are external processes that use the API. All UI is cells; there is no GUI
  toolkit (ADR-0005, ADR-0011).

## UI

Follow `docs/design/ui/`: one monospace font, chrome drawn as cells. Agent states are
always glyph, colour and word: ◆ needs you (amber), ● running (blue), ✓ done (green),
✕ error (red), ○ idle (grey). Prompts drawn by Pinfold sit outside pane text, so
program output cannot fake them.

## Evidence

Every number in a doc, PR or report comes from committed code, with the script beside
the result. Report missed targets and flaky tests plainly, with the numbers. Check
crates on crates.io and docs.rs before use; versions named in the design docs may be
stale.

## Changes

- Small PRs: about 400 changed lines, one concern, on a branch into `main`. Pushing to
  `main` is the maintainer's job.
- Every PR: Conventional Commit title, DCO sign-off, tests, docs, a `CHANGELOG.md`
  entry, and filled-in memory-impact and security-impact sections.
- Justify each new dependency in its PR: need, license, maintenance, `unsafe`, memory.
- Record choices a future contributor would question as an ADR, from
  `docs/adr/0000-template.md`.
- A PR is done when `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D
  warnings`, `cargo test --workspace` and `cargo deny check` all pass. If it can change
  memory use, include the `cargo xtask mem-gate` table too.

`CONTRIBUTING.md` holds the test matrix and tool setup.
