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
ditto "$app" "$stage/PinFold.app"
ln -s /Applications "$stage/Applications"
hdiutil create -quiet -volname PinFold -srcfolder "$stage" -format UDZO -ov "$dmg"
codesign --force --sign "$identity" "$timestamp" "$dmg"
codesign --verify --strict "$dmg"
echo "$dmg"
