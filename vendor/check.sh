#!/bin/sh
# Checks each vendored crate against the published crate plus its patch.
# With --update, rewrites the patches from the vendored directories instead.
set -eu
cd "$(dirname "$0")"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
# name, version, sha256 of the .crate, and what is left out of the directory.
check() {
  name=$1 version=$2 sum=$3 left_out=$4
  mkdir "$work/$name"
  curl -fsSL -o "$work/$name/crate.tgz" "https://static.crates.io/crates/$name/$name-$version.crate"
  echo "$sum  $work/$name/crate.tgz" | shasum -a 256 -c - >/dev/null
  tar -xzf "$work/$name/crate.tgz" -C "$work/$name"
  base="$work/$name/$name-$version"
  for path in $left_out Cargo.lock Cargo.toml.orig .cargo_vcs_info.json .cargo-ok; do
    rm -rf "${base:?}/$path"
  done
  mv "$base" "$work/$name/a"
  if [ "${UPDATE:-}" = 1 ]; then
    cp -R "$name" "$work/$name/b"
    (cd "$work/$name" && diff -ruN a b) >"$name.patch" || [ $? = 1 ]
  else
    patch -s -p1 -d "$work/$name/a" <"$name.patch"
    diff -r "$work/$name/a" "$name"
    echo "$name/ matches $version plus the patch"
  fi
}
[ "${1:-}" = --update ] && UPDATE=1
check alacritty_terminal 0.26.0 bda177466b9524d59f1b12f0dd30b68696788e9992a7e959021c4a0ed96fcf59 tests
check smithay-clipboard 0.7.2 cc8216eec463674a0e90f29e0ae41a4db573ec5b56b1c6c1c71615d249b6d846 \
  ".github .gitignore doc_index.html"
