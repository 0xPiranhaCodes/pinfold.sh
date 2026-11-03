#!/bin/sh
# Builds race.c and runs it once without and once with the single retry Pty::spawn makes.
set -eu
cd "$(dirname "$0")"
echo "macOS: $(sw_vers -productVersion) ($(sw_vers -buildVersion)), $(sysctl -n hw.ncpu) CPUs"
echo "kern.tty.ptmx_max: $(sysctl -n kern.tty.ptmx_max)"
cc -O2 -o "${TMPDIR:-/tmp}/pty-race" race.c
"${TMPDIR:-/tmp}/pty-race"
"${TMPDIR:-/tmp}/pty-race" retry
