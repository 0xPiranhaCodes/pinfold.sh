#!/bin/sh
# Builds Pinfold.app from binaries already built. Run from the repository root:
#   sh packaging/macos/bundle.sh <pinfold> <pinfold-core> <output directory>
set -eu

pinfold=$1
core=$2
app=$3/Pinfold.app
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
sed "s/@VERSION@/$version/g" packaging/macos/Info.plist > "$app/Contents/Info.plist"
plutil -lint -s "$app/Contents/Info.plist"
cp "$pinfold" "$core" "$app/Contents/MacOS/"
cp packaging/macos/AppIcon.icns crates/pinfold-render/fonts/OFL.txt LICENSE-MIT LICENSE-APACHE \
  "$app/Contents/Resources/"
# shortcut: an ad-hoc signature, enough to run here but not past Gatekeeper elsewhere, Developer ID and notarization arrive in M5.
codesign --force --sign - "$app/Contents/MacOS/pinfold-core"
codesign --force --sign - "$app"
echo "$app"
