#!/bin/sh
# Runs `cargo xtask soak` on Linux in the window image, as nightly does under Xvfb, for runs longer
# than a hosted runner's six hours. With HEAP=1, root attaches heaptrack to the core and prints the
# live heap's lowest and highest bytes per tenth of the run. Writes soak.log (and core.gz) to OUT.
# Usage: [HEAP=1] OUT=<directory> tests/window/soak.sh <minutes>
set -eu
cd "$(dirname "$0")/../.."
OUT="${OUT:-${TMPDIR:-/tmp}/pinfold-soak}"
mkdir -p "$OUT" && rm -f "$OUT/done"
docker build -q -t pinfold-window tests/window >/dev/null
git archive -o "$OUT/src.tar" HEAD
packages=procps debug=
if [ "${HEAP:-}" = 1 ]; then
  # Line tables give heaptrack call sites, and stay out of a plain run's binary.
  packages="procps heaptrack gdb"
  debug="-e CARGO_PROFILE_RELEASE_CORE_STRIP=false -e CARGO_PROFILE_RELEASE_CORE_DEBUG=line-tables-only"
fi
# The soak runs as uid 1000 in a 0700 home; a named volume keeps the build between runs.
# shellcheck disable=SC2086 # $debug is zero or more -e flags
docker run --rm --cap-add SYS_PTRACE --security-opt seccomp=unconfined \
  -v "$OUT/src.tar":/src.tar:ro -v "$OUT":/out -v pinfold-soak-target:/home/u/t \
  -e RUSTUP_HOME=/usr/local/rustup -e MINUTES="$1" -e HEAP="${HEAP:-}" -e PACKAGES="$packages" \
  $debug pinfold-window sh -c '
  set -e
  apt-get update -qq >/dev/null && apt-get install -y -qq $PACKAGES >/dev/null 2>&1 &&
  useradd -u 1000 -M -d /home/u u && mkdir -p -m 700 /home/u/w && tar -xf /src.tar -C /home/u/w &&
  chown -R 1000:1000 /home/u
  (setpriv --reuid=1000 --regid=1000 --clear-groups env HOME=/home/u CARGO_HOME=/home/u/cargo \
    CARGO_TARGET_DIR=/home/u/t sh -c "cd /home/u/w && (Xvfb :99 -screen 0 1920x1080x24 &) &&
    DISPLAY=:99 cargo xtask soak --minutes $MINUTES" > /out/soak.log 2>&1; touch /out/done) &
  if [ "$HEAP" = 1 ]; then
    until pid=$(pgrep -x pinfold-core); do sleep 1; done
    cd /out && heaptrack -o /out/core -p "$pid" >/dev/null
    heaptrack_print -f /out/core.gz --print-massif /out/massif.out -n 0 >/dev/null
    awk -F= -v tenth=$((MINUTES * 6)) "/^time=/ { t = int(\$2 / tenth) }
      /^mem_heap_B=/ { if (!(t in lo) || \$2 < lo[t]) lo[t] = \$2; if (\$2 > hi[t]) hi[t] = \$2 }
      END { print \"| Tenth | Live heap low MB | Live heap high MB |\n| --- | --- | --- |\";
        for (i = 0; i < 10 && i in lo; i++) printf \"| %d | %.2f | %.2f |\n\", i + 1, lo[i] / 1e6, hi[i] / 1e6 }" \
      /out/massif.out > /out/heap.md
  fi
  until [ -e /out/done ]; do sleep 5; done'
sed -n '/^Soak of/,$p' "$OUT/soak.log"
[ "${HEAP:-}" = 1 ] && cat "$OUT/heap.md"
grep -q '^\*\*FAIL' "$OUT/soak.log" && exit 1
exit 0
