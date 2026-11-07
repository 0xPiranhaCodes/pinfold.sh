#!/bin/sh
# Builds a .dmg holding PinFold.app and a link to /Applications, and signs it:
#   sh packaging/macos/dmg.sh <PinFold.app> <output .dmg> [identity]
# The identity defaults to `-`, an ad-hoc signature; the tag build passes its Developer ID.
set -eu

app=$1
dmg=$2
identity=${3:--}
timestamp=--timestamp
[ "$identity" != - ] || timestamp=--timestamp=none

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
# mktemp makes it 0700, which would be the volume's root.
chmod 755 "$stage"
ditto "$app" "$stage/PinFold.app"
ln -s /Applications "$stage/Applications"
# Finder's icon layout, from dmg-layout.sh once on a Mac with a screen, as CI has none.
[ ! -f packaging/macos/dmg.DS_Store ] || cp packaging/macos/dmg.DS_Store "$stage/.DS_Store"
hdiutil create -quiet -volname PinFold -srcfolder "$stage" -format UDZO -ov "$dmg"
codesign --force --sign "$identity" "$timestamp" "$dmg"
codesign --verify --strict "$dmg"
echo "$dmg"
