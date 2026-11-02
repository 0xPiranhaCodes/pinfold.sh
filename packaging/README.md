# Packaging

The Homebrew formula, AppImage and .deb recipes, and the macOS signing and entitlements
arrive in M5.

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
