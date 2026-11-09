# Dependency review

`cargo vet --locked` runs in CI (the `vet` job) and fails when a third-party crate
version has no audit, trusted-publisher entry or exemption that meets its criteria. This
page says what those criteria are, whose audits we accept, how to review a crate
yourself, and when an exemption is allowed. `cargo deny` checks licenses and advisories
and `cargo audit` checks known vulnerabilities; neither is a source review.

## Criteria

We use cargo-vet's two built-in criteria.

- `safe-to-deploy`: the crate will not introduce a serious security hole into software
  exposed to untrusted input. Required for everything that can end up in a release
  binary, and for build scripts and proc macros those binaries need, since they run on the
  build machine.
- `safe-to-run`: the crate can run on a developer or CI machine without harming it.
  Required for dev-dependencies, and for everything `xtask`, `pinfold-tests` and
  `pinfold-benches` use, as none of them ships. Their policy is in
  `supply-chain/config.toml`. `fuzz/` is outside the workspace, so cargo-vet does not see
  it.

A crate reachable from both a shipped crate and a test-only one needs `safe-to-deploy`.

## Imported audits

`supply-chain/config.toml` imports the audits of six organisations from cargo-vet's
registry, and `supply-chain/imports.lock` pins what was fetched, so CI does not touch the
network. `cargo vet regenerate imports` refreshes them.

| Source              | What it covers                                                     | Why we trust it                                                    |
| ------------------- | ------------------------------------------------------------------ | ------------------------------------------------------------------ |
| `mozilla`           | Firefox's Rust dependencies: parsers, Unicode, `std`-adjacent crates | Firefox's own review process; Mozilla maintains cargo-vet           |
| `google`            | Chromium and Android crates, many with `ub-risk` notes on `unsafe` | Reviewed by Google's Rust and security teams, with per-crate notes |
| `bytecode-alliance` | Wasmtime's tree: `rustix`, `cc`, proc-macro and FFI crates          | Wasmtime runs untrusted code, so its reviewers audit `unsafe`      |
| `zcash`             | Cryptography and serialisation crates                              | Published review practice for cryptographic code                   |
| `isrg`              | Let's Encrypt and Divvi Up crates: TLS, crypto, `serde`            | Runs public certificate infrastructure                             |
| `embark-studios`    | Game and graphics crates                                           | Public audit set close to the window's dependencies                |

An imported audit counts only for the criteria it names. When an import stops covering a
new version, `cargo vet` fails and `cargo vet suggest` shows the smallest diff to review.

### Trusted publishers

`supply-chain/audits.toml` also trusts six crates.io publishers for named crates: David
Tolnay, Andrew Gallant, Ed Page, Dan Gohman, Thomas de Zeeuw and Manish Goregaokar. We
trust a publisher only when at least one import above already trusts them, and each
entry's notes say whether an import trusts them for that crate or only for others. Each
entry ends a year after it was written. A version published by anyone else is not
covered.

## Reviewing a crate

Run `cargo vet inspect CRATE VERSION`, or `cargo vet diff CRATE OLD NEW` when an audit
exists for an earlier version. Read the source as published, not the repository. Then
record it with `cargo vet certify` and a `notes` line saying what you read and what you
found. An audit done with an AI agent starts its notes with "AI-assisted review (Claude
Code) for the maintainer; not yet spot-checked by a person."; a person removes that marker
once they have spot-checked the audit. Check these first.

- **Build scripts** (`build.rs`): they run with your permissions at build time. They
  should read only `CARGO_*` and target variables, write only under `OUT_DIR`, and not
  open network connections or run programs other than the compiler (`cc`, `pkg-config`).
  Look for downloads, `Command::new`, paths outside the crate, and `include!` of generated
  code you have not read.
- **Proc macros**: they run inside the compiler. They should only transform tokens. Look
  for file, network or environment access, and for generated code that adds `unsafe` the
  caller did not write.
- **Native code and FFI** (`links`, `cc`, `*-sys`, `dlopen`): read every `extern` block
  and the types passed across it, check lengths and null pointers, and note which system
  library is loaded and from where. A crate that vendors C source needs that source read
  too, or it stays exempt.
- **`unsafe`**: read every block. Each needs an invariant the surrounding safe code
  upholds (a bounds check, an initialised length, a validated `str`). Pointer casts,
  `transmute`, `MaybeUninit`, `set_len` and `from_utf8_unchecked` are where bugs hide.
- **Untrusted input**: a crate that parses PTY output, API requests, config or files
  (`vte`, `toml`, `serde_json`, `lz4_flex`) also needs its panics and allocation limits
  checked, because the core must not panic or grow without bound on external input. Say
  in the notes when Pinfold, not the crate, enforces a limit.

## Exemptions

An exemption says a crate version is allowed without review. Each one is a gap. A new
exemption is allowed only when the crate is needed now, no import covers it, and reading
it is out of scope for the PR that adds it. It must:

- name one exact version, never a range;
- use the lowest criteria that passes (`safe-to-run` for test-only use);
- carry `notes` naming who added it and why, and the issue that will review it;
- be called out in the PR's security-impact section.

On pull requests the `vet` job runs `.github/scripts/check-vet-notes.py`, which fails when
the PR adds an `[[exemptions.*]]` or `[[trusted.*]]` entry whose `notes` do not name who
(`by NAME`), why, and the tracking issue (`#N`), for example `"Added by NAME because WHY.
Review in #N."`. An entry counts as added when its crate, criteria and version (or
publisher) are not in the PR's base, so entries already on `main` are not checked, and a
new trusted entry needs the same notes as an exemption.

`cargo vet prune` removes exemptions that audits or imports now cover; run it after
`regenerate imports` and after certifying. Never add an exemption to make CI green for a
crate you could review in the same PR.

The exemption count, by criteria, comes from:

```sh
awk '/^\[\[exemptions/{e=1} e&&/^criteria/{print;e=0}' supply-chain/config.toml | sort | uniq -c
```
