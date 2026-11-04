# Packaging

The release workflow is a dry run (ADR-0016). It builds Linux tarballs for x86_64 and aarch64
and the universal macOS app, writes a `.sha256` file beside each archive, and keeps them as
workflow artifacts. It publishes nothing; creating and publishing a release is the maintainer's
job until the M5 publishing step lands.

Each Linux tarball contains `pinfold`, `pinfold-core`, `pinfold-window`, `README.md`, both
project license files and the bundled font's `OFL.txt`. The launcher and window use Cargo's
`release` profile; the core uses `release-core`, whose panic strategy is `abort`. The checksum
file is generated in the same job that built the archive.

The macOS core and window are ad-hoc signed with the hardened runtime until Developer ID
signing, notarization and the `.dmg` release step land. The hardened runtime is needed to refuse
a same-user process a task port (ADR-0020).

`macos/bundle.sh` builds `PinFold.app` from built binaries. `macos/AppIcon.icns` holds the
PinFold logo, which [TRADEMARK.md](../TRADEMARK.md) covers.

```sh
cargo build --release -p pinfold-cli
sh packaging/macos/bundle.sh target/release/pinfold target/release/pinfold-core \
  target/release/pinfold-window target/release
open target/release/PinFold.app
```

`pinfold`, `pinfold-core` and `pinfold-window` must sit in one directory: the launcher and the
core each find the next by looking beside themselves.

Every package must ship `crates/pinfold-render/fonts/OFL.txt`, the license of the bundled font.

On Linux, packages must depend on `libxkbcommon-x11`. `winit` loads it at runtime and panics at
startup without it.

## Homebrew

`homebrew/pinfold.rb` installs the Linux tarball for the host architecture.
`homebrew/pinfold.cask.rb` installs the signed and notarized macOS `.dmg`; do not publish the
cask for an ad-hoc-signed app. Both files contain release-marked version and SHA-256 placeholders.
For a release, replace the version with the tag without its leading `v`, copy each digest from
the archive's `.sha256` file, and test the formula or cask against the release assets.

The maintainer creates and owns the Homebrew tap. They copy the formula and cask into that tap
after publishing the GitHub release.
