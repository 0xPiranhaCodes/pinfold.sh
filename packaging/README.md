# Packaging

The Homebrew formula, AppImage and .deb recipes, and the macOS bundle and entitlements
arrive in M5.

Every package must ship `crates/pinfold-render/fonts/OFL.txt`, the license of the bundled font.

On Linux, packages must depend on `libxkbcommon-x11`. `winit` loads it at runtime and panics at
startup without it.
