#!/bin/bash
# Runs PinFold from a real .dmg and ejects it under a running core (#279), and prints PASS or FAIL
# for each step. It runs by hand on a Mac, as CI cannot mount images under a session:
#
#   sh packaging/macos/bundle.sh target/release/{pinfold,pinfold-core,pinfold-window} /tmp/pfapp
#   sh packaging/macos/dmg.sh /tmp/pfapp/PinFold.app /tmp/PinFold.dmg
#   bash tests/macos/dmg-eject.sh /tmp/PinFold.dmg [old.dmg]
#
# old.dmg, such as v0.1.0-beta.1's, is the release whose core runs from the image and is
# ejected. Without it the new image's core is. Everything runs in its own runtime directory
# and $HOME, so a PinFold already running is untouched. The installed copy goes in
# ~/Applications-test, never /Applications. Windows open on screen while it runs.
set -u

NEW=$1
OLD=${2:-}
# A socket path must fit in 104 bytes, so not the long $TMPDIR.
WORK=$(cd -P "$(mktemp -d /tmp/pfdmg.XXXXXX)" && pwd)
APPS=$HOME/Applications-test
export HOME=$WORK/home XDG_RUNTIME_DIR=$WORK/run
mkdir -p "$HOME" "$XDG_RUNTIME_DIR" "$APPS"
FAILED=0
MOUNTS=()
trap 'for m in ${MOUNTS[@]+"${MOUNTS[@]}"}; do hdiutil detach -quiet -force "$m"; done
      pkill -9 -f "$WORK/|$APPS/PinFold.app" 2>/dev/null; rm -rf "$WORK" "$APPS/PinFold.app"' EXIT

check() { # what, then a command that succeeds when it holds
  if eval "$2"; then echo "PASS $1"; else echo "FAIL $1"; FAILED=1; fi
}
attach() { # image, mount point
  hdiutil attach -quiet -nobrowse -mountpoint "$2" "$1" && MOUNTS+=("$2")
}
cores() { ps -axo pid=,command= | grep '[p]info''ld-core' | grep -F "$1" ; }

attach "$NEW" "$WORK/new"
check "the image is read-only" "mount | grep -F ' $WORK/new ' | grep -q read-only"
check "the image root is not private" "[ \"\$(stat -f %Lp '$WORK/new')\" = 755 ]"
out=$("$WORK/new/PinFold.app/Contents/MacOS/pinfold" </dev/null 2>&1)
check "the launcher on the image refuses: $out" "[ -n \"\$(printf %s \"\$out\" | grep 'cannot run from')\" ]"
check "no core runs from the image" "[ -z \"\$(cores '$WORK/new/')\" ]"

ditto "$WORK/new/PinFold.app" "$APPS/PinFold.app"
if [ -n "$OLD" ]; then
  attach "$OLD" "$WORK/old"
  stale=$WORK/old
else
  # The refusal is new, so a core on the image can only be started by hand, as beta.1's launcher did.
  stale=$WORK/new
fi
"$stale/PinFold.app/Contents/MacOS/pinfold-core" </dev/null >/dev/null 2>&1 &
sleep 3
"$APPS/PinFold.app/Contents/MacOS/pinfold" </dev/null >/dev/null 2>&1 &
sleep 3
check "a core runs from the image to eject" "[ -n \"\$(cores '$stale/')\" ]"
pkill -f "$stale/PinFold.app/Contents/MacOS/pinfold-window"
hdiutil detach -quiet -force "$stale"
echo "ejected $stale"

start=$(date +%s)
out=$(timeout 60 "$APPS/PinFold.app/Contents/MacOS/pinfold" </dev/null 2>&1)
code=$?
took=$(($(date +%s) - start))
echo "launcher after the eject: exit $code in ${took}s: $out"
check "the launcher returns within 30 s" "[ $code -eq 0 ] && [ $took -le 30 ]"
check "the ejected core is gone" "[ -z \"\$(cores '$stale/')\" ]"
check "a core runs from the installed copy" "[ -n \"\$(cores '$APPS/PinFold.app/')\" ]"
check "a window runs from the installed copy" \
  "pgrep -f '$APPS/PinFold.app/Contents/MacOS/pinfold-window' >/dev/null"
exit $FAILED
