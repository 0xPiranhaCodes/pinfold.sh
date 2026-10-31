#!/bin/sh
# Checks alacritty_terminal/ against the published crate plus the patch.
# With --update, rewrites the patch from alacritty_terminal/ instead.
set -eu
cd "$(dirname "$0")"
sum=bda177466b9524d59f1b12f0dd30b68696788e9992a7e959021c4a0ed96fcf59
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
curl -fsSL -o "$work/crate.tgz" https://static.crates.io/crates/alacritty_terminal/alacritty_terminal-0.26.0.crate
echo "$sum  $work/crate.tgz" | shasum -a 256 -c - >/dev/null
tar -xzf "$work/crate.tgz" -C "$work"
base="$work/alacritty_terminal-0.26.0"
rm -rf "$base/tests" "$base/Cargo.lock" "$base/Cargo.toml.orig" "$base/.cargo_vcs_info.json" "$base/.cargo-ok"
mv "$base" "$work/a"
if [ "${1:-}" = --update ]; then
  cp -R alacritty_terminal "$work/b"
  (cd "$work" && diff -ruN a b) >alacritty_terminal.patch || [ $? = 1 ]
else
  patch -s -p1 -d "$work/a" <alacritty_terminal.patch
  diff -r "$work/a" alacritty_terminal
  echo "alacritty_terminal/ matches 0.26.0 plus the patch"
fi
