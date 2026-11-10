#!/bin/bash
# Shows that `strip_secrets = true` hides a secret exported before launch from a sandboxed probe
# (#331, ADR-0047), where sandbox-procargs.sh shows it leaks. It runs by hand on a Mac, from the
# repository's root:
#
#   bash tests/macos/strip-secrets.sh
#
# For each setting it runs the real launcher with PINFOLD_PROBE_TOKEN=MARK exported, which starts
# the core, which starts one pane. The pane's shell execs a program built here, as a platform
# binary would return its arguments only. A probe under `(allow default)` then reads the core's
# and the pane's environment through sysctl(KERN_PROCARGS2) and prints LEAKS or HIDDEN.
set -u

cargo build -q --bin pinfold --bin pinfold-core || exit 1
TARGET=$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
# Short, as a socket's path has a low limit on macOS.
WORK=$(mktemp -d /tmp/psXXXX)
cleanup() {
    pkill -f "$WORK/bin/pinfold-core" 2>/dev/null
    pkill -f "$WORK/sleeper" 2>/dev/null
    rm -rf "$WORK"
}
trap cleanup EXIT
mkdir -p "$WORK/bin" "$WORK/config/pinfold"
cp "$TARGET/debug/pinfold" "$TARGET/debug/pinfold-core" "$WORK/bin/"
# A window that never says hello: the launcher gives up on it, the core and its pane stay.
printf '#!/bin/sh\nexec sleep 60\n' >"$WORK/bin/pinfold-window"
printf '#include <unistd.h>\nint main(void) { sleep(60); return 0; }\n' >"$WORK/sleeper.c"
cc -o "$WORK/sleeper" "$WORK/sleeper.c" || exit 1
# The core asks the shell for its PATH with `-i -c`; the pane's shell notes itself.
cat >"$WORK/shell" <<SH
#!/bin/sh
case \$1 in -i) exec /bin/sh "\$@";; esac
echo \$\$ > "$WORK/pane"
exec "$WORK/sleeper"
SH
chmod 700 "$WORK/bin/pinfold-window" "$WORK/shell"
cat >"$WORK/probe.py" <<'PY'
import ctypes, sys
libc = ctypes.CDLL("/usr/lib/libc.dylib", use_errno=True)
libc.sysctl.argtypes = [ctypes.POINTER(ctypes.c_int), ctypes.c_uint, ctypes.c_void_p,
                        ctypes.POINTER(ctypes.c_size_t), ctypes.c_void_p, ctypes.c_size_t]
mib = (ctypes.c_int * 3)(1, 49, int(sys.argv[1]))  # CTL_KERN, KERN_PROCARGS2
size = ctypes.c_size_t(1 << 18)
buf = ctypes.create_string_buffer(size.value)
if libc.sysctl(mib, 3, buf, ctypes.byref(size), None, 0) != 0:
    print("NOPROC")
else:
    print("LEAKS" if b"PINFOLD_PROBE_TOKEN=MARK" in buf.raw[:size.value] else "HIDDEN")
PY

probe() {
    local result
    result=$(sandbox-exec -p '(version 1)(allow default)' /usr/bin/python3 -I "$WORK/probe.py" "$1" 2>/dev/null)
    echo "${result:-BROKEN}"
}

for strip in false true; do
    rm -rf "$WORK/run" "$WORK/state" "$WORK/pane"
    mkdir -m 700 "$WORK/run"
    echo "strip_secrets = $strip" >"$WORK/config/pinfold/pinfold.toml"
    env XDG_RUNTIME_DIR="$WORK/run" XDG_CONFIG_HOME="$WORK/config" XDG_STATE_HOME="$WORK/state" \
        HOME="$WORK/state" SHELL="$WORK/shell" PINFOLD_PROBE_TOKEN=MARK \
        "$WORK/bin/pinfold" >/dev/null 2>&1 &
    for _ in $(seq 100); do [ -s "$WORK/pane" ] && break; sleep 0.1; done
    sleep 0.2
    core=$(pgrep -f "$WORK/bin/pinfold-core" | head -1)
    pane=$(pgrep -f "$WORK/sleeper" | head -1)
    printf 'strip_secrets = %-5s core: %-6s pane: %s\n' "$strip" \
        "$(probe "${core:-0}")" "$(probe "${pane:-0}")"
    pkill -f "$WORK/bin/pinfold-core"
    pkill -f "$WORK/sleeper"
    kill %1 2>/dev/null
    wait 2>/dev/null
done
