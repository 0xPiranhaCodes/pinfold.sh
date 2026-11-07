#!/bin/sh
# Writes packaging/macos/dmg.DS_Store, the Finder layout dmg.sh puts on the .dmg: PinFold.app
# on the left, an arrow's width from the Applications link on the right. Finder writes the file,
# so this needs a Mac with a screen, and Terminal allowed to control Finder (System Settings >
# Privacy & Security > Automation). Run it from the repository root once, and commit the file:
#   sh packaging/macos/dmg-layout.sh
set -eu

work=$(mktemp -d)
trap 'hdiutil detach -quiet -force "$work/mnt" 2>/dev/null || true; rm -rf "$work"' EXIT
mkdir -p "$work/stage/PinFold.app"
ln -s /Applications "$work/stage/Applications"
hdiutil create -quiet -volname PinFold -srcfolder "$work/stage" -format UDRW "$work/rw.dmg"
hdiutil attach -quiet -mountpoint "$work/mnt" "$work/rw.dmg"
osascript - "$work/mnt" <<'SCRIPT'
on run argv
  tell application "Finder"
    set theVolume to POSIX file (item 1 of argv) as alias
    open theVolume
    set theWindow to container window of theVolume
    set current view of theWindow to icon view
    set toolbar visible of theWindow to false
    set statusbar visible of theWindow to false
    set bounds of theWindow to {200, 120, 740, 460}
    set theOptions to icon view options of theWindow
    set arrangement of theOptions to not arranged
    set icon size of theOptions to 128
    set position of item "PinFold.app" of theVolume to {140, 160}
    set position of item "Applications" of theVolume to {400, 160}
    update theVolume without registering applications
    delay 2
    close theWindow
  end tell
end run
SCRIPT
sync
cp "$work/mnt/.DS_Store" packaging/macos/dmg.DS_Store
echo packaging/macos/dmg.DS_Store
