#!/bin/sh
# Reproduces results.md for one file system. Needs cargo, git, and ~6 GB free on <work>.
# Usage: measure.sh <work-dir> <label>   (writes results/<label>.jsonl beside this script)
#        measure.sh --linux              (Docker: the same run on a loop-mounted ext4 image)
#        measure.sh [--linux] --peaks [<work-dir> <label>]
#                                        (Linux tree only: the sum of concurrent git peaks per
#                                         reader count, at normal and background priority;
#                                         writes results/<label>-peaks.jsonl)
#        measure.sh --real <work-dir> <label>
#                                        (Linux tree only, on this host: the real take,
#                                         `pinfold checkpoint take` (ADR-0050 slice 3), beside
#                                         the spike's recommended take, both at background
#                                         priority, interleaved so they share the machine's
#                                         load; writes results/<label>-real.jsonl)
# LINUX_SRC=<clone of torvalds/linux at v6.12> skips the download.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(git -C "$HERE" rev-parse --show-toplevel)
PINFOLD_COMMIT=8f0bd1f4baf33dee3203c16d5213eff4ebc3895d
mkdir -p "$HERE/results"

if [ "${1:-}" = --linux ]; then
  PEAKS=
  [ "${2:-}" = --peaks ] && PEAKS=--peaks
  LINUX_MOUNT=
  [ -n "${LINUX_SRC:-}" ] && LINUX_MOUNT="-v $LINUX_SRC:/linux-src:ro"
  # shellcheck disable=SC2086
  exec docker run --rm --privileged -v "$REPO":/repo:ro -v "$HERE/results":/results $LINUX_MOUNT \
    rust:1.94-bookworm sh -c '
      set -eu
      truncate -s 10G /ext4.img && mkfs.ext4 -q /ext4.img && mkdir /w && mount -o loop /ext4.img /w
      cp -R /repo/docs/spikes/rewind /spike && rm -rf /spike/results && ln -s /results /spike/results
      git config --global --add safe.directory "*"
      cd /spike && git init -q && LINUX_SRC=$([ -d /linux-src ] && echo /linux-src || true) \
        ./measure.sh '"$PEAKS"' /w linux-ext4'
fi

PEAKS=
if [ "${1:-}" = --peaks ]; then PEAKS=-peaks; shift; fi
if [ "${1:-}" = --real ]; then PEAKS=-real; shift; fi
W=$1
LABEL=$2
OUT="$HERE/results/$LABEL$PEAKS.jsonl"
: > "$OUT"
avail_kb=$(df -Pk "$W" | awk 'NR==2 {print $4}')
[ "$avail_kb" -gt 6000000 ] || { echo "need 6 GB free on $W, have $((avail_kb / 1024)) MB" >&2; exit 1; }

cargo build -q --release --manifest-path "$HERE/Cargo.toml" --target-dir "$W/target"
B="$W/target/release/spike-rewind"
R="$W/target/release/pinfold"
[ "$PEAKS" = -real ] && cargo build -q --release --manifest-path "$REPO/Cargo.toml" -p pinfold-cli --bin pinfold --target-dir "$W/target"
emit() { printf '{"fs":"%s","corpus":"%s","config":"%s","step":"%s","r":%s}\n' "$LABEL" "$1" "$2" "$3" "$4" >> "$OUT"; }
du_kb() { du -sk "$1" | awk '{print $1}'; }

# Whole-tree runs raise the file and new-content caps so a take covers everything it may;
# the per-file 16 MiB cap stays. One run per corpus keeps every default cap.
WHOLE="REWIND_MAX_FILES=10000000 REWIND_MAX_NEW_MIB=1000000"
take() { env $WHOLE $CONFIG "$B" "$1" "$2" "$3"; }

measure() { # <corpus> <dir>
  name=$1 dir=$2 store="$W/store-$name"
  find "$dir" -name .git -prune -o -type f -exec cat {} + > /dev/null # warm the cache
  emit "$name" - tree "{\"kb\":$(du_kb "$dir"),\"git_kb\":$( [ -d "$dir/.git" ] && du_kb "$dir/.git" || echo 0)}"

  for copy in clone plain; do
    for i in 1 2 3; do
      rm -rf "$W/copy" && mkdir "$W/copy"
      before=$(df -Pk "$W" | awk 'NR==2 {print $3}')
      if [ "$copy" = clone ]; then
        if [ "$(uname)" = Darwin ]; then flag=-c; else flag=--reflink=auto; fi
      else flag=; fi
      # shellcheck disable=SC2046
      r=$("$B" time sh -c 'cd "$1" && shift && exec cp -R "$@"' - "$dir" $flag $(cd "$dir" && ls -A | grep -vx .git) "$W/copy/")
      after=$(df -Pk "$W" | awk 'NR==2 {print $3}')
      emit "$name" - "cp_$copy" "{\"t\":$r,\"du_kb\":$(du_kb "$W/copy"),\"df_kb\":$((after - before))}"
    done
  done
  rm -rf "$W/copy"

  for config in adr recommended; do
    if [ $config = adr ]; then
      CONFIG="REWIND_READERS=1 REWIND_COMPRESSION=-1 REWIND_FAST_IMPORT_ARGS=--quiet"
    else
      CONFIG="REWIND_READERS=4 REWIND_COMPRESSION=1 REWIND_FAST_IMPORT_ARGS=--depth=0"
    fi
    for i in 1 2 3; do
      rm -rf "$store"
      emit "$name" $config first "$(take "$dir" "$store" 0)"
    done
    emit "$name" $config du_take "{\"kb\":$(du_kb "$store")}"
    r=$("$B" time env GIT_DIR="$store" GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 git repack -a -d -q)
    emit "$name" $config repack "{\"t\":$r,\"kb\":$(du_kb "$store")}"
    for n in 1 2 3; do emit "$name" $config unchanged "$(take "$dir" "$store" $n)"; done
    for n in 4 5 6; do
      # Append a line to every 100th covered regular file.
      tr '\0' '\n' < "$store/stat/spike-$((n - 1))" |
        awk 'NR % 100 == 0 && $1 != "120000" { sub(/^[^ ]+ [^ ]+ [^ ]+ [^ ]+ [^ ]+ [^ ]+ /, ""); print }' |
        while IFS= read -r p; do echo "// changed" >> "$dir/$p"; done
      emit "$name" $config changed "$(take "$dir" "$store" $n)"
    done
    emit "$name" $config du_end "{\"kb\":$(du_kb "$store")}"
  done
  rm -rf "$store"
  emit "$name" default default_caps "$("$B" "$dir" "$store" 0)"
  rm -rf "$store"

  if [ -d "$dir/.git" ]; then
    # What reading .git/index would replace: an index read, after which a first take costs
    # about as much as an unchanged one.
    r=$("$B" time git -C "$dir" ls-files -s -z)
    emit "$name" - index "{\"t\":$r,\"tracked\":$(git -C "$dir" ls-files | wc -l)}"
  fi
}

# Every take runs under `spike-rewind peaks`, in its own process group, which samples each git
# process in the group every 5 ms (phys_footprint on macOS, RSS on Linux). "bg" adds the
# background priority ADR-0050 asks for: QoS utility on macOS, nice 10 and ioprio idle on Linux.
peaks() { # <corpus> <dir>
  name=$1 dir=$2 store="$W/store-$1"
  find "$dir" -name .git -prune -o -type f -exec cat {} + > /dev/null # warm the cache
  if [ "$(uname)" = Darwin ]; then BG="taskpolicy -c utility"; else BG="nice -n 10 ionice -c 3"; fi
  for run in adr-1-normal recommended-4-normal recommended-2-normal recommended-4-bg recommended-2-bg; do
    cfg=${run%%-*} readers=${run#*-} readers=${readers%-*} prio=${run##*-}
    if [ "$cfg" = adr ]; then
      CONFIG="REWIND_READERS=1 REWIND_COMPRESSION=-1 REWIND_FAST_IMPORT_ARGS=--quiet"
    else
      CONFIG="REWIND_READERS=$readers REWIND_COMPRESSION=1 REWIND_FAST_IMPORT_ARGS=--depth=0"
    fi
    PRIO=
    [ "$prio" = bg ] && PRIO=$BG
    for i in 1 2 3; do
      rm -rf "$store"
      # shellcheck disable=SC2086
      emit "$name" "$run" first "$("$B" peaks $PRIO env $WHOLE $CONFIG "$B" "$dir" "$store" 0)"
    done
    for n in 1 2 3; do
      # shellcheck disable=SC2086
      emit "$name" "$run" unchanged "$("$B" peaks $PRIO env $WHOLE $CONFIG "$B" "$dir" "$store" $n)"
    done
  done
  rm -rf "$store"
}

if [ -z "$PEAKS" ]; then
  rm -rf "$W/pinfold" && git clone -q https://github.com/0xPiranhaCodes/pinfold.sh.git "$W/pinfold" && git -C "$W/pinfold" checkout -q "$PINFOLD_COMMIT"
  measure pinfold "$W/pinfold"
  rm -rf "$W/pinfold"

  rm -rf "$W/generated" && "$B" gen "$W/generated"
  measure generated "$W/generated"
  rm -rf "$W/generated"
fi

if [ -n "${LINUX_SRC:-}" ]; then
  cp -R "$LINUX_SRC" "$W/linux"
else
  git clone -q --depth 1 --branch v6.12 -c advice.detachedHead=false https://github.com/torvalds/linux.git "$W/linux"
fi
# The real take runs at background priority itself (pinfold_sys::background), so the spike's
# gets the same: QoS utility on macOS, nice 10 and ioprio idle on Linux, and ADR-0050's
# amended caps, which cover the Linux tree whole. Each step of each round runs one then the other.
real() { # <corpus> <dir>
  name=$1 dir=$2
  find "$dir" -name .git -prune -o -type f -exec cat {} + > /dev/null # warm the cache
  if [ "$(uname)" = Darwin ]; then BG="taskpolicy -c utility"; else BG="nice -n 10 ionice -c 3"; fi
  CAPS="REWIND_MAX_FILES=200000 REWIND_MAX_NEW_MIB=2048"
  one() { # <take> <step> <n>
    if [ "$1" = real ]; then
      # Its store goes under a state directory of its own, and PINFOLD_MEASURE adds ms and peak.
      emit "$name" real "$2" "$("$B" peaks env HOME="$W/state" XDG_STATE_HOME="$W/state" PINFOLD_MEASURE=1 "$R" checkpoint take r1-0 api -- "$dir")"
    else
      # shellcheck disable=SC2086
      emit "$name" spike "$2" "$("$B" peaks $BG env $CAPS "$B" "$dir" "$W/store-spike" "$3")"
    fi
  }
  # Which goes first alternates, as the first after a store is removed ran slower.
  turn=0
  both() { # <step> <n>
    turn=$((1 - turn))
    if [ $turn = 1 ]; then one real "$1" "$2"; one spike "$1" "$2"; else one spike "$1" "$2"; one real "$1" "$2"; fi
  }
  for i in 1 2 3 4; do rm -rf "$W/state" "$W/store-spike"; sync; both first 0; done
  emit "$name" real du_take "{\"kb\":$(du_kb "$W/state")}"
  for n in 1 2 3; do both unchanged $n; done
  for n in 4 5 6; do
    # Append a line to every 100th covered regular file, from the real take's list.
    tr '\0' '\n' < "$(find "$W/state" -path "*/stat/r1-0-$((n - 1))")" |
      awk 'NR % 100 == 0 && $1 != "120000" { sub(/^[^ ]+ [^ ]+ [^ ]+ [^ ]+ [^ ]+ [^ ]+ /, ""); print }' |
      while IFS= read -r p; do echo "// changed" >> "$dir/$p"; done
    both changed $n
  done
  rm -rf "$W/state" "$W/store-spike"
}

case "$PEAKS" in
  -peaks) peaks linux "$W/linux" ;;
  -real) real linux "$W/linux" ;;
  *) measure linux "$W/linux" ;;
esac
rm -rf "$W/linux"

{ uname -srvmo 2>/dev/null || uname -srvm; } | awk '{printf "{\"fs\":\"'"$LABEL"'\",\"step\":\"host\",\"uname\":\"%s\"", $0}' >> "$OUT"
printf ',"git":"%s","cpus":%s}\n' "$(git --version)" "$(getconf _NPROCESSORS_ONLN)" >> "$OUT"
