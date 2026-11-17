#!/bin/sh
# Reproduces results.md for one file system. Needs cargo, git, and ~6 GB free on <work>.
# Usage: measure.sh <work-dir> <label>   (writes results/<label>.jsonl beside this script)
#        measure.sh --linux              (Docker: the same run on a loop-mounted ext4 image)
# LINUX_SRC=<clone of torvalds/linux at v6.12> skips the download.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(git -C "$HERE" rev-parse --show-toplevel)
PINFOLD_COMMIT=8f0bd1f4baf33dee3203c16d5213eff4ebc3895d
mkdir -p "$HERE/results"

if [ "${1:-}" = --linux ]; then
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
        ./measure.sh /w linux-ext4'
fi

W=$1
LABEL=$2
OUT="$HERE/results/$LABEL.jsonl"
: > "$OUT"
avail_kb=$(df -Pk "$W" | awk 'NR==2 {print $4}')
[ "$avail_kb" -gt 6000000 ] || { echo "need 6 GB free on $W, have $((avail_kb / 1024)) MB" >&2; exit 1; }

cargo build -q --release --manifest-path "$HERE/Cargo.toml" --target-dir "$W/target"
B="$W/target/release/spike-rewind"
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

rm -rf "$W/pinfold" && git clone -q https://github.com/0xPiranhaCodes/pinfold.sh.git "$W/pinfold" && git -C "$W/pinfold" checkout -q "$PINFOLD_COMMIT"
measure pinfold "$W/pinfold"
rm -rf "$W/pinfold"

rm -rf "$W/generated" && "$B" gen "$W/generated"
measure generated "$W/generated"
rm -rf "$W/generated"

if [ -n "${LINUX_SRC:-}" ]; then
  cp -R "$LINUX_SRC" "$W/linux"
else
  git clone -q --depth 1 --branch v6.12 -c advice.detachedHead=false https://github.com/torvalds/linux.git "$W/linux"
fi
measure linux "$W/linux"
rm -rf "$W/linux"

{ uname -srvmo 2>/dev/null || uname -srvm; } | awk '{printf "{\"fs\":\"'"$LABEL"'\",\"step\":\"host\",\"uname\":\"%s\"", $0}' >> "$OUT"
printf ',"git":"%s","cpus":%s}\n' "$(git --version)" "$(getconf _NPROCESSORS_ONLN)" >> "$OUT"
