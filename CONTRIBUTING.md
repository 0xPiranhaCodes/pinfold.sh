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

> The Cargo workspace arrives in the second Phase 0 PR. Until then only the
> documentation exists.

Optional tools, used by the checks below:

```sh
cargo install --locked cargo-deny cargo-audit cargo-vet cargo-fuzz typos-cli
```

## The workspace

| Crate            | Purpose                                                       |
| ---------------- | ------------------------------------------------------------- |
| `pinfold-core`   | Daemon library: PTY, `VtEngine`, mux, sessions, status engine |
| `pinfold-proto`  | API types, JSON Schemas, version negotiation (no I/O)         |
| `pinfold-api`    | Socket server, scopes and approvals, audit log                |
| `pinfold-render` | `Renderer` trait, CPU renderer, glyph cache, optional GL      |
| `pinfold-ui`     | Window process: winit, chrome as cells, input, IME            |
| `pinfold-tools`  | Markdown reader, file manager, diff viewer                    |
| `pinfold-config` | TOML schema, validation, hot reload, permission checks        |
| `pinfold-sys`    | The only crate allowed `unsafe`                               |
| `pinfold-cli`    | The `pinfold` and `pinfold-core` binaries                     |

## Running the checks

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check
cargo audit
```

### Fuzzing

Fuzz targets live in `fuzz/` and need a nightly toolchain and `cargo-fuzz`:

```sh
cargo +nightly fuzz list
cargo +nightly fuzz run <target> -- -max_total_time=60
```

`cargo xtask fuzz-smoke` runs a short pass over every target, which is what CI
does on each PR. Any crash is a release blocker.

### The memory gate

`cargo xtask mem-gate` runs a scripted headless session and checks every budget in
[ADR-0008](docs/adr/0008-memory-targets.md). It prints a table and fails on a
regression. If your change affects memory, paste that table into your PR.

Every number in the docs or a PR must come from committed code. If you cannot
reproduce it with a script in this repository, leave it out.

## Testing expectations

Every PR that changes behavior needs tests.

- **Unit tests** sit next to the code. Cross-crate integration tests go in `tests/`.
- **Snapshot tests** (`insta`) cover grid states, rendered cell output, tool panes
  and API responses.
- **Golden images** cover renderer output.
- **Property tests** (`proptest`) cover parser input paths, the layout tree and the
  scrollback compression round-trip.
- **Fuzz targets** cover the VT input path, JSON-RPC decoder, markdown renderer,
  unified-diff parser, paste sanitizer and config parser.
- **Conformance** runs vttest and esctest against a tracked pass list.
- **Recorded agent sessions** cover status detection.
- **Approval-prompt UI tests** prove that only the keyboard can answer a prompt.
- **Security tests** give each threat T1–T10 in the
  [threat model](docs/design/threat-model.md) at least one test proving its control
  works.

## Commits and pull requests

- **Keep PRs small:** ideally under about 400 changed lines, one concern each.
- **Branch per change:** open PRs into `main`. Direct pushes to `main` are not
  allowed.
- **Conventional Commits:** use them for commit messages and PR titles, for example
  `feat(api): add hello handshake`. Common types are `feat`, `fix`, `docs`,
  `refactor`, `perf`, `test`, `build`, `ci` and `chore`. PR titles are checked in CI
  because squash merges use them for the changelog.
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

AI assistants cannot certify the DCO. If a tool wrote part of your change, you are
the one signing off for it, so review it as your own work.

### Licensing of contributions

Unless you state otherwise, contributions are dual-licensed as `MIT OR Apache-2.0`,
with no additional terms. The license is declared by SPDX identifiers in `Cargo.toml`.
Per-file license headers are not required. Code derived from `alacritty_terminal`
is the exception and stays Apache-2.0 only
([ADR-0012](docs/adr/0012-license.md)).

### New dependencies

Justify every new dependency in its PR: why it is needed, its license, maintenance
status, `unsafe` usage and memory cost. Check the current version on crates.io and
docs.rs first. A dependency that pulls in an async runtime, a GUI toolkit or a C
library needs maintainer approval before work starts.

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
