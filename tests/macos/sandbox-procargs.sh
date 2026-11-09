#!/bin/bash
# Shows that a process under sandbox-exec can read another process's environment through
# sysctl(KERN_PROCARGS2) (#328, ADR-0047), and which rules stop it. It runs by hand on a Mac:
#
#   bash tests/macos/sandbox-procargs.sh
#
# A program built here, as pinfold-core and a pane's programs are not platform binaries, sleeps
# with a marker variable set. A probe reads its arguments and environment under each profile and
# prints LEAKS, HIDDEN or BROKEN (the probe itself did not run). Platform binaries, such as
# /bin/sleep, return their arguments only, so they would hide the gap.
set -u

WORK=$(mktemp -d)
trap 'kill "$PID" 2>/dev/null; rm -rf "$WORK"' EXIT
printf '#include <unistd.h>\nint main(void) { sleep(60); return 0; }\n' >"$WORK/sleeper.c"
cc -o "$WORK/sleeper" "$WORK/sleeper.c" || exit 1
cat >"$WORK/probe.py" <<'PY'
import ctypes, sys
libc = ctypes.CDLL("/usr/lib/libc.dylib", use_errno=True)
libc.sysctl.argtypes = [ctypes.POINTER(ctypes.c_int), ctypes.c_uint, ctypes.c_void_p,
                        ctypes.POINTER(ctypes.c_size_t), ctypes.c_void_p, ctypes.c_size_t]
mib = (ctypes.c_int * 3)(1, 49, int(sys.argv[1]))  # CTL_KERN, KERN_PROCARGS2
size = ctypes.c_size_t(1 << 18)
buf = ctypes.create_string_buffer(size.value)
libc.sysctl(mib, 3, buf, ctypes.byref(size), None, 0)
print("LEAKS" if b"PINFOLD_PROBE_TOKEN=MARK" in buf.raw[:size.value] else "HIDDEN")
PY
env PINFOLD_PROBE_TOKEN=MARK "$WORK/sleeper" &
PID=$!
sleep 0.5

run() {
    local result
    result=$(sandbox-exec -p "$1" /usr/bin/python3 -I "$WORK/probe.py" "$PID" 2>/dev/null)
    printf '%-8s %s\n' "${result:-BROKEN}" "$1"
}
echo "not sandboxed: $(/usr/bin/python3 -I "$WORK/probe.py" "$PID")"
run '(version 1)(allow default)'
run '(version 1)(allow default)(deny process-info* (target others))'
run '(version 1)(allow default)(deny sysctl-read (sysctl-name "kern.procargs2"))'
run '(version 1)(allow default)(deny sysctl*)'
run '(version 1)(allow default)(deny process-info*)'
