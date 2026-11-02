#!/bin/bash
# Attacks real binaries with the tools a same-user process has, and prints what each one got
# (ADR-0020). It runs by hand: crates/pinfold-cli/tests/session.rs holds what CI can check.
#
#   cargo build -p pinfold-cli && bash tests/hardening/audit.sh
#
# On Linux it needs ptrace and pidfd_getfd, which a container's default seccomp profile may stop:
#   docker run --rm --security-opt seccomp=unconfined -v "$PWD":/w:ro -w /w \
#     -e BIN=<dir holding the binaries> pinfold-window bash tests/hardening/audit.sh
#
# On macOS the probe is ad-hoc signed by this script, which needs no admin rights, no Developer
# Mode and no Apple account, and the debugging tools it runs may ask for a password on screen.
set -u

BIN=${BIN:-$PWD/target/debug}
HERE=$(cd "$(dirname "$0")" && pwd)
WORK=$(mktemp -d /tmp/pfaudit.XXXXXX)
MARKER=PFMARK42
PIDS=()
trap 'kill -9 "${PIDS[@]}" 2>/dev/null; rm -rf "$WORK"' EXIT

# Runs a command for at most $1 seconds, and says how it ended.
bounded() {
  local seconds=$1
  shift
  "$@" &
  local pid=$!
  (sleep "$seconds" && kill -9 "$pid") 2>/dev/null &
  local timer=$!
  wait "$pid" 2>/dev/null
  local code=$?
  kill "$timer" 2>/dev/null
  wait "$timer" 2>/dev/null
  if [ "$code" -gt 128 ]; then
    echo "killed by signal $((code - 128))"
  fi
}

# Starts a core from binary $1 with arguments $3.. and shell $2, and sets CORE to its pid. A
# connected socket means it is past hardening.
start() {
  local binary=$1 shell=$2
  shift 2
  local run
  run=$(mktemp -d "$WORK/rXXXX")
  XDG_RUNTIME_DIR=$run SHELL=$shell "$binary" "$@" </dev/null >"$run/out" 2>&1 &
  CORE=$!
  PIDS+=("$CORE")
  for _ in $(seq 200); do
    [ -S "$run/pinfold/default.sock" ] && break
    sleep 0.02
  done
  sleep 0.5
}

printf '#!/bin/sh\nprintf "PFMARK%%d\\n" $((40+2))\necho $$ > "$AUDIT_PANE"\nexec sleep 300\n' \
  >"$WORK/shell.sh"
chmod +x "$WORK/shell.sh"
export AUDIT_PANE=$WORK/pane
cc -w -O1 -o "$WORK/probe" "$HERE/probe.c" || exit 1

macos() {
  # An ad-hoc signed attacker that claims the debugger entitlement.
  cat >"$WORK/debugger.plist" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>com.apple.security.cs.debugger</key><true/></dict></plist>
EOF
  cp "$WORK/probe" "$WORK/debugger"
  codesign -f -s - --entitlements "$WORK/debugger.plist" "$WORK/debugger" 2>/dev/null
  mkdir "$WORK/runtime"
  cp "$BIN/pinfold-core" "$WORK/runtime/"
  codesign -f -s - -o runtime "$WORK/runtime/pinfold-core" 2>/dev/null

  sw_vers -productVersion
  csrutil status
  DevToolsSecurity -status 2>&1 | head -1

  for target in hardened runtime unhardened; do
    case $target in
      hardened) start "$BIN/pinfold-core" "$WORK/shell.sh" ;;
      runtime) start "$WORK/runtime/pinfold-core" "$WORK/shell.sh" ;;
      unhardened) start "$BIN/pinfold-core" "$WORK/shell.sh" --idle 1 ;;
    esac
    echo "== $target core"
    echo "- attach, plain process: $(bounded 10 "$WORK/probe" attach "$CORE" 2>&1)"
    echo "- attach, debugger entitlement: $(bounded 10 "$WORK/debugger" attach "$CORE" 2>&1)"
    echo "- task port, plain process: $(bounded 10 "$WORK/probe" scan "$CORE" $MARKER 2>&1)"
    echo "- task port, debugger entitlement: $(bounded 30 "$WORK/debugger" scan "$CORE" $MARKER 2>&1)"
    echo "- lldb -p: $(bounded 30 lldb --batch -p "$CORE" -o 'process detach' 2>&1 | grep -E 'error:|Process .* (stopped|detached)|Detaching|killed by' | cut -c1-70 | head -2 | tr '\n' ' ')"
    echo "- vmmap: $(bounded 20 vmmap "$CORE" 2>&1 | grep -E 'TOTAL  |cannot|fatal' | head -1 | cut -c1-60)"
    echo "- leaks memgraph holds the marker: $(bounded 30 leaks --outputGraph="$WORK/graph" "$CORE" >/dev/null 2>&1; grep -c -a $MARKER "$WORK"/graph* 2>/dev/null | tail -1)"
    rm -f "$WORK"/graph*
    echo "- gcore: $(bounded 30 gcore -o "$WORK/dump" "$CORE" 2>&1 | head -1)"
    echo "- still running: $(ps -o stat= -p "$CORE")"
    kill -9 "$CORE" 2>/dev/null
    wait "$CORE" 2>/dev/null
  done

  # lldb is run only with -p above: on macOS 27 with these Command Line Tools its own start-up
  # crashes in ObjectFileMachO::ParseSymtab with no core involved, on any target. This drives
  # its debugserver instead, which needs an attach to succeed and leaves the target stopped.
  local debugserver
  debugserver=$(xcode-select -p)/Library/PrivateFrameworks/LLDB.framework/Resources/debugserver
  [ -x "$debugserver" ] || debugserver=$(xcode-select -p)/../SharedFrameworks/LLDB.framework/Resources/debugserver

  cat >"$WORK/pane-debug.sh" <<EOF
#!/bin/bash
exec >"\$AUDIT_PANE.debug" 2>&1
echo "attach to this shell, plain process: \$("$WORK/probe" attach \$\$ 2>&1)"
echo "attach to a program it starts, debugger entitlement: \$("$WORK/debugger" launch /bin/sleep 1 2>&1)"
"$WORK/probe" hold - &
HOLD=\$!
sleep 0.2
"$debugserver" 127.0.0.1:0 --attach=\$HOLD >/dev/null 2>&1 &
SERVER=\$!
sleep 3
echo "debugserver left its target in state: \$(ps -o stat= -p \$HOLD)"
kill -9 \$SERVER \$HOLD
echo "trace a program it starts: \$("$WORK/probe" trace /usr/bin/true 2>&1)"
echo finished
exec sleep 300
EOF
  chmod +x "$WORK/pane-debug.sh"

  echo "== debugging inside a pane of a hardened core"
  start "$BIN/pinfold-core" "$WORK/pane-debug.sh"
  for _ in $(seq 100); do
    grep -q finished "$AUDIT_PANE.debug" 2>/dev/null && break
    sleep 0.1
  done
  grep -v finished "$AUDIT_PANE.debug" | sed 's/^/- /'
  kill -9 "$CORE" 2>/dev/null
  wait "$CORE" 2>/dev/null

  echo "== the same with no core"
  AUDIT_PANE=$WORK/outside "$WORK/pane-debug.sh" </dev/null 2>/dev/null &
  local outside=$!
  PIDS+=("$outside")
  for _ in $(seq 100); do
    grep -q finished "$WORK/outside.debug" 2>/dev/null && break
    sleep 0.1
  done
  grep -v finished "$WORK/outside.debug" | sed 's/^/- /'
  kill -9 "$outside" 2>/dev/null

  echo "== started under a tracer, as lldb starts a program"
  local run
  run=$(mktemp -d "$WORK/rXXXX")
  export XDG_RUNTIME_DIR=$run SHELL=/bin/sh
  echo "- pinfold-core: $(bounded 10 "$WORK/probe" trace "$BIN/pinfold-core" 2>&1)"
  echo "- pinfold-core --idle 1: $(bounded 10 "$WORK/probe" trace "$BIN/pinfold-core" --idle 1 2>&1)"
  echo "- pinfold-window: $(bounded 10 "$WORK/probe" trace "$BIN/pinfold-window" 2>&1)"

  # This opens a real window for a moment.
  echo "== the bundle, with a real window"
  mkdir "$WORK/bundle"
  sh packaging/macos/bundle.sh "$BIN/pinfold" "$BIN/pinfold-core" "$BIN/pinfold-window" "$WORK/bundle" >/dev/null
  local macos=$WORK/bundle/Pinfold.app/Contents/MacOS window
  mkdir "$WORK/config"
  export XDG_CONFIG_HOME=$WORK/config
  for mode in launched adopted; do
    run=$(mktemp -d "$WORK/rXXXX")
    export XDG_RUNTIME_DIR=$run
    if [ $mode = adopted ]; then
      "$macos/pinfold-core" --idle 1 </dev/null >/dev/null 2>&1 &
      PIDS+=("$!")
      for _ in $(seq 200); do
        [ -S "$run/pinfold/default.sock" ] && break
        sleep 0.02
      done
    fi
    "$macos/pinfold"
    echo "- $mode: launcher exited with $?"
    CORE=$(lsof -t "$run/pinfold/default.lock")
    PIDS+=("$CORE")
    window=$(pgrep -P "$CORE" -x pinfold-window)
    PIDS+=("$window")
    echo "- $mode core: $(bounded 10 "$WORK/probe" attach "$CORE" 2>&1)"
    echo "- $mode core, debugger entitlement: $(bounded 10 "$WORK/debugger" attach "$CORE" 2>&1)"
    echo "- $mode window: $(bounded 10 "$WORK/probe" attach "$window" 2>&1)"
    echo "- $mode window, task port: $(bounded 30 "$WORK/debugger" scan "$window" $MARKER 2>&1)"
    kill -9 "$window" "$CORE"
  done
}

linux() {
  uname -sr
  echo "ptrace_scope $(cat /proc/sys/kernel/yama/ptrace_scope 2>/dev/null)"
  for target in hardened unhardened; do
    case $target in
      hardened) start "$BIN/pinfold-core" "$WORK/shell.sh" ;;
      unhardened) start "$BIN/pinfold-core" "$WORK/shell.sh" --idle 1 ;;
    esac
    local pane
    pane=$(cat "$AUDIT_PANE")
    echo "== $target core"
    echo "- owner of /proc/pid/mem, as uid $(id -u): $(stat -c %u "/proc/$CORE/mem")"
    echo "- attach: $(bounded 10 "$WORK/probe" attach "$CORE" 2>&1)"
    echo "- pidfd_getfd: $(bounded 10 "$WORK/probe" copy "$CORE" 2>&1)"
    echo "- pane shell, owner of /proc/pid/mem: $(stat -c %u "/proc/$pane/mem")"
    echo "- pane shell, attach: $(bounded 10 "$WORK/probe" attach "$pane" 2>&1)"
    kill -9 "$CORE" 2>/dev/null
    wait "$CORE" 2>/dev/null
  done

  echo "== debugging inside a pane of a hardened core"
  cat >"$WORK/gdb-shell.sh" <<EOF
#!/bin/sh
exec >"\$AUDIT_PANE.debug" 2>&1
stat -c '%u' /proc/\$\$/mem
"$WORK/probe" launch /bin/sleep 1
echo finished
exec sleep 300
EOF
  chmod +x "$WORK/gdb-shell.sh"
  start "$BIN/pinfold-core" "$WORK/gdb-shell.sh"
  for _ in $(seq 100); do
    grep -q finished "$AUDIT_PANE.debug" 2>/dev/null && break
    sleep 0.1
  done
  sed 's/^/- pane: /' "$AUDIT_PANE.debug"
  kill -9 "$CORE" 2>/dev/null
  wait "$CORE" 2>/dev/null

  echo "== a core started under a tracer"
  local run
  run=$(mktemp -d "$WORK/rXXXX")
  export XDG_RUNTIME_DIR=$run SHELL=/bin/sh
  bounded 20 "$WORK/probe" trace "$BIN/pinfold-core" 2>&1 | sed 's/^/- /'
}

case $(uname -s) in
  Darwin) macos ;;
  *) linux ;;
esac
