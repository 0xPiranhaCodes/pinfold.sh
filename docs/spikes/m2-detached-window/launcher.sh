#!/bin/sh
# Stands in for the launcher: start a detached "core", which starts the window, then exit.
here=$(dirname "$0")
/usr/bin/python3 -I -c '
import os, sys, time
if os.fork(): os._exit(0)
os.setsid()
if os.fork(): os._exit(0)
time.sleep(1)
os.execv(sys.argv[1], [sys.argv[1], "--size", "700x300"])
' "$here/pinfold" </dev/null >/dev/null 2>&1
exit 0
