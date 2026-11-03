# ADR-0034: The plan for M5, sessions and releases

- **Status:** Proposed
- **Date:** 2026-11-03

## Context

M5 holds snapshots, restore rules, workspace trust, `pinfold up`, packaging and signed releases
([ROADMAP](../ROADMAP.md)). The threat model gives it workspace trust, encrypted spill files,
snapshot redaction, and signed and notarized releases. A macOS beta comes first
([ADR-0031](0031-macos-beta-first.md)), and Developer ID signing blocks it.

The code today: the core holds one `pinfold_mux::Session` in its `Reactor`, and exits when it has
no pane and no window (`crates/pinfold-cli/src/bin/pinfold-core.rs`). A pane keeps the program it
started without its arguments, which can hold secrets (`crates/pinfold-core/src/io.rs`), and its
cwd only as an OSC 7 hint. Scrollback lives in RAM under a 2,000,000-byte cap per pane
(`BYTE_CAP` in `crates/pinfold-core/src/scrollback.rs`, [ADR-0027](0027-scrollback-without-a-hot-tier.md)),
so nothing spills to disk. `pinfold_api::redact`, `sha2`, `serde_json`, `toml` and `lz4_flex` are
already in the workspace. `release.yml` is a dry run that builds Linux tarballs and an ad-hoc
signed universal `PinFold.app` ([ADR-0016](0016-release-tooling.md)).

[`architecture.md`](../design/architecture.md#session-save-and-restore) plans scrollback in
snapshots (zstd, 2,000 lines, 5 MB), an environment allowlist, tool pane state, agent resume
commands and a `restore.auto_rerun` list. Each of those widens what is on disk or what runs
without a key press, and none is needed for v0.1.

## Decision

1. **A snapshot holds the layout and no screen.** Tabs, splits and their shares, zoom, the
   focused pane, and per pane: the OSC 7 cwd, the workspace it came from, and, for a pane Pinfold
   started with a command, that command line. The command passes `pinfold_api::redact` and is cut
   at 4,096 bytes, the `pane.send` cap (`MAX_SEND` in `pinfold_api::control`). No scrollback, no environment, no titles. Tool panes wait
   for v0.2 ([ADR-0024](0024-tools-in-v0-2.md)).
2. **One file, JSON, not encrypted.** `$XDG_STATE_HOME/pinfold/sessions/default.json`, else
   `~/.local/state/pinfold/sessions/default.json`, on both platforms, with a `version` field. The
   directory is 0700 and checked like the control directory, and the file is 0600. A write goes
   to a temp file in that directory, is synced, then renamed. The core reads at most 1 MiB, which
   64 panes (`MAX_PANES` in `pinfold-proto`) with a 4,096-byte command and a 4,096-byte Linux
   `PATH_MAX` cwd stay under. A file that is too big or will not parse is logged by kind, never by
   content, and the core starts fresh. It never panics on it. The snapshot holds nothing secret by
   design, so a key stored on the same disk would add nothing.
3. **No spill files in v0.1.** ADR-0027 keeps scrollback in RAM, so the threat model's encrypted
   spill files have nothing to cover yet. When spill files or scrollback in snapshots come, they
   use ChaCha20-Poly1305 from `chacha20poly1305` 0.11.0 with default features off and `alloc` on,
   a 32-byte key read from `/dev/urandom`, a nonce counted per chunk, and a file unlinked at open
   ([architecture.md](../design/architecture.md#data-at-rest-and-network-egress)). Checked on
   crates.io and docs.rs on 2026-11-03:

   | Crate | License | Maintained | `unsafe` | C code |
   | --- | --- | --- | --- | --- |
   | `chacha20poly1305` 0.11.0 (chosen) | MIT or Apache-2.0 | RustCrypto team, released 2026-06-28 | In the `chacha20` and `poly1305` SIMD backends. `aead`, `cipher` and `universal-hash` forbid it | None. With default features off it does not pull `getrandom` |
   | `orion` 0.18.1 (fallback) | MIT | One maintainer, released 2026-10-29 | Forbidden in `orion`, deps not checked | None |
   | `ring` 0.17.14 (rejected) | Apache-2.0 and ISC | Yes | Yes | C and assembly through a `cc` build script |
   | `zstd` 0.14.3 (rejected, for compression) | BSD-3-Clause | Yes | Yes | `zstd-sys` builds C with `cc` |

   `cpufeatures` and `crypto-common` are already in `Cargo.lock` through `sha2`. ISC and
   BSD-3-Clause are not in `deny.toml`'s allow list, which settles `ring` and `zstd`.
   Compression, if needed, uses `lz4_flex`, already a dependency of `pinfold-core`.
4. **Snapshots are taken** when the window closes, every 60 seconds while something in the
   snapshot changed (the period in `architecture.md`), on SIGTERM and SIGHUP, and on
   `pinfold ctl session save`. An unchanged snapshot is not rewritten, and a core with no change
   arms no timer, so an idle core still sleeps for its full `IDLE` of 3,600 seconds (`pinfold-core.rs`). When the last pane closes, the
   person ended the session, so the file is removed.
5. **Restore runs at core start, and the snapshot is never a source of commands that run alone.**
   A core that finds a snapshot rebuilds its layout instead of opening one shell. Each pane gets a
   fresh shell in its saved cwd if that is still an existing absolute directory, else in `$HOME`.
   A pane that held a command shows, in a strip Pinfold draws outside the pane text,
   `was running: <command>`, and Enter runs it, unless redaction changed the command, when it is
   only shown. The one exception is a pane from a trusted workspace whose layout file still has
   the trusted hash: its `run` line is read from that file again, never from the snapshot, and
   runs. There is no `auto_rerun` list and no agent resume command in v0.1: a person who wants
   `claude --continue` back puts it in a workspace layout.
6. **Workspace trust is keyed to the directory and the file's SHA-256.** A project ships
   `.pinfold/layout.toml`, not `pinfold.toml`, which names the person's own config. The core reads
   the file itself, not a symlink, at most 64 KiB, hashes those bytes with `sha2`, and parses the
   same bytes, so what was trusted is what runs. A layout holds tabs and panes, each pane with an
   optional `split` (`right` or `down`), `cwd` and `run`. A `cwd` must resolve inside the project
   directory. `run` is typed into the pane's shell, as tmuxinator does. A layout cannot set
   environment, agent profiles, keys, hooks or alerts. Trust lives in
   `$XDG_STATE_HOME/pinfold/trust.json` (0600) as canonical directory and hash pairs. A same-user
   program can edit that file, as it can edit `~/.zshrc`, so trust guards against a cloned repo
   (T5), not against code already running as you.
7. **The trust prompt is drawn by the window**, on the approval prompt surface from #100, outside
   any pane text. It shows the directory, whether the file changed since it was trusted, and every
   pane's cwd and `run` line, paged if long. The choices are Trust and run, Shells only, and
   Cancel. No API call can answer it.
8. **`pinfold up [dir]`** reads `<dir>/.pinfold/layout.toml`, with `dir` defaulting to the
   current directory and no search upward. It starts a core if none answers and opens a window,
   as `pinfold` does, then sends `workspace.up`. If tabs from that workspace are open, it focuses
   the first one: that is attach. Otherwise it opens the layout's tabs in the one session, within
   `MAX_TABS` and `MAX_PANES`, after the trust check. It prints nothing on success, and exits 1
   with a reason on a bad file, a refusal or Cancel. Named sessions and a core per session wait
   past v0.1.
9. **Releases.** Tags still come only from the maintainer, and signing runs only on tag pushes,
   in a GitHub environment named `release` that the maintainer guards. macOS: Developer ID with
   the hardened runtime and a timestamp, a `.dmg` made with `hdiutil`, notarized with
   `notarytool` and stapled. All platforms: `SHA256SUMS`, a CycloneDX SBOM from `cargo-cyclonedx`
   0.5.9 (Apache-2.0, a CI tool only), and Sigstore-backed GitHub attestations from
   `actions/attest-build-provenance` v4.2.2, on a draft release the maintainer publishes. Linux
   ships tarballs. Homebrew gets a cask for the signed `.dmg` on macOS, so a Homebrew install keeps
   the hardened runtime, and a formula for the tarball on Linux. `.deb` and AppImage wait past
   v0.1.

M5 lands in this order, each step a PR of its own. Steps marked **(maintainer)** need the
maintainer's credentials or hands.

1. Developer ID signing in `release.yml` on tags, with the hardened runtime. **(maintainer)**
   creates the `release` environment and adds the certificate as secrets.
2. Notarization, stapling and the `.dmg`. **(maintainer)** adds an App Store Connect API key.
3. The draft release: `SHA256SUMS`, SBOM and attestations. **(maintainer)** pushes the beta tag
   and publishes the draft.
4. `pinfold-mux` exports its layout as a plain tree and rebuilds a session from one, checking
   every share and pane id, so a bad file is an error and never a panic.
5. The snapshot file and `pinfold ctl session save`, with redaction, and the amended threat model
   and architecture rows for points 1 to 3.
6. Snapshot triggers: window close, the 60-second timer, SIGTERM and SIGHUP through a self-pipe
   in `pinfold-sys`, and removal when the last pane closes.
7. Restore at core start: layout, panes and cwd.
8. The `was running` strip in the window, and Enter to run.
9. Parsing `.pinfold/layout.toml` in `pinfold-config`, with its caps, path checks and a fuzz target.
10. `pinfold up` and `workspace.up`, opening layouts as shells only, and attach.
11. The trust store and the trust prompt, after which `run` lines run.
12. Restore runs the `run` lines of trusted workspaces whose hash still matches.
13. Linux tarballs on the draft release, the Homebrew cask and formula in `packaging/homebrew/`, and
    `packaging/README.md`. **(maintainer)** creates the tap and pushes the formula and cask.

Steps 1 to 3 block the beta and nothing else does from M5. Steps 4 to 13 and the Linux desktop
checks in #47 block v0.1. The two tracks share no files, so they run in parallel. Steps 4 to 8 and
9 to 12 are two chains, joined only at step 12.

## Consequences

**Memory.** The core holds a 32-byte hash of the last snapshot and a dirty flag. Building one is
transient and bounded by the 1 MiB read cap, a layout read by its 64 KiB cap. The core binary
gains the TOML parser through `pinfold-config`, and step 9 brings the `cargo xtask mem-gate` table
against the 4 MB core budget ([ADR-0021](0021-memory-targets-m0.md)). The 60-second timer runs only
while something changed. No new crate is added in M5.

**Security.** On disk there are only layouts, cwds and redacted commands, 0600 in a 0700
directory. Nothing runs without a key press unless a person trusted that exact file in that
directory. A repo's layout can only open panes and type lines it shows first. Developer ID with
the hardened runtime refuses the task port that reads an ad-hoc signed core's memory
([threat model](../design/threat-model.md), core hardening). A `cargo` or source build stays
exposed, which #109 settles.

This narrows `architecture.md`: no scrollback, environment or tool state in snapshots, no
`auto_rerun`, and no spill files. The threat model's M5 row changes with it, which is a change to
the security model, so it waits for the maintainer to accept this ADR.

Revisit point 1 if people miss scrollback after a reboot, then with point 3's encryption and a key
that does not sit beside the file. Revisit point 8 if one session is too few for people with
several projects open.
