# Packaging

The release workflow is hand-written (ADR-0016). It builds Linux tarballs for x86_64 and aarch64
and the universal macOS app, writes a `.sha256` file beside each archive, and keeps them as
workflow artifacts. On a pull request that touches packaging it is a dry run: the app and its
`PinFold-adhoc.dmg` are signed ad hoc and no secret is read. On a `v*` tag it also signs with
the Developer ID and notarizes, as described in [Cutting the beta](#cutting-the-beta). It
publishes nothing; creating and publishing a release is the maintainer's job.

Each Linux tarball contains `pinfold`, `pinfold-core`, `pinfold-window`, `README.md`, both
project license files and the bundled font's `OFL.txt`. The launcher and window use Cargo's
`release` profile; the core uses `release-core`, whose panic strategy is `abort`. The checksum
file is generated in the same job that built the archive.

Every macOS binary and the app are signed with the hardened runtime, ad hoc unless a signing
identity is given. The hardened runtime is needed to refuse a same-user process a task port
(ADR-0020), and notarization needs it too.

`macos/bundle.sh` builds and signs `PinFold.app` from built binaries, then checks the signature
and the hardened runtime. `macos/dmg.sh` puts the app and an `/Applications` link in a signed
`.dmg`. Both take a signing identity as their last argument, defaulting to ad hoc. `macos/AppIcon.icns` holds the
PinFold logo, which [TRADEMARK.md](../TRADEMARK.md) covers.

```sh
cargo build --release -p pinfold-cli
sh packaging/macos/bundle.sh target/release/pinfold target/release/pinfold-core \
  target/release/pinfold-window target/release
open target/release/PinFold.app
sh packaging/macos/dmg.sh target/release/PinFold.app target/release/PinFold.dmg
```

`pinfold`, `pinfold-core` and `pinfold-window` must sit in one directory: the launcher and the
core each find the next by looking beside themselves.

Every package must ship `crates/pinfold-render/fonts/OFL.txt`, the license of the bundled font.

On Linux, packages must depend on `libxkbcommon-x11`. `winit` loads it at runtime and panics at
startup without it.

## Cutting the beta

The maintainer pushes a `v*` tag. The `release` environment holds `MACOS_CERT_P12`,
`MACOS_CERT_PASSWORD`, `APPLE_TEAM_ID`, `ASC_KEY_ID`, `ASC_ISSUER_ID` and `ASC_KEY_P8`, and only
the `macos-sign` job can read them, after the maintainer approves it on the run's page.

That job builds nothing. It takes the universal binaries from the `macos` job, imports the
certificate into a keychain in the runner's temporary directory, signs each binary and then the
app with the team's Developer ID, the hardened runtime and a timestamp, and builds and signs
`PinFold.dmg`. It submits the `.dmg` with `notarytool --wait` and fails, printing the notary
log, unless the answer is `Accepted`. It then staples the ticket, checks it, and keeps
`PinFold.dmg` and its `.sha256` file as the `pinfold-macos-dmg` artifact. The keychain and the
API key file are removed even when a step fails.

To check the `.dmg` on a Mac after downloading it:

```sh
spctl -a -t open --context context:primary-signature -v PinFold.dmg
xcrun stapler validate PinFold.dmg
shasum -a 256 -c PinFold.dmg.sha256
```

`spctl` must say `accepted` and `source=Notarized Developer ID`.

## Homebrew

`homebrew/pinfold.rb` installs the Linux tarball for the host architecture.
`homebrew/pinfold.cask.rb` installs the signed and notarized macOS `.dmg`; do not publish the
cask for an ad-hoc-signed app. Both files contain release-marked version and SHA-256 placeholders.
For a release, replace the version with the tag without its leading `v`, copy each digest from
the archive's `.sha256` file, and test the formula or cask against the release assets.

The maintainer creates and owns the Homebrew tap. They copy the formula and cask into that tap
after publishing the GitHub release.
