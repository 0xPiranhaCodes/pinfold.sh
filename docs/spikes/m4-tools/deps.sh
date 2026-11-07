#!/bin/sh
# Resolves each candidate crate for the built-in tools (ADR-0045) in a throwaway project
# and prints its crate count, the `unsafe` count and lines of its own source, and what
# `cargo deny` says under this repository's deny.toml. Needs cargo and cargo-deny.
set -eu
repo=$(cd "$(dirname "$0")/../../.." && pwd)
dir=$(mktemp -d)
trap 'rm -rf "$dir"' EXIT
mkdir "$dir/src"
echo 'fn main() {}' >"$dir/src/main.rs"
cp "$repo/deny.toml" "$dir/"
cat >"$dir/Cargo.toml" <<'TOML'
[package]
name = "probe"
version = "0.0.0"
edition = "2021"
license = "MIT"
publish = false

[dependencies]
pulldown-cmark = { version = "=0.13.6", default-features = false }
similar = "=2.7.0"
unicode-width = "=0.2.3"
ratatui = { version = "=0.30.2", default-features = false, features = ["crossterm"] }
crossterm = { version = "=0.29.0", default-features = false, features = ["events"] }
syntect = { version = "=5.3.0", default-features = false, features = ["parsing", "default-syntaxes", "default-themes", "regex-fancy"] }
ignore = "=0.4.34"
notify = { version = "=8.2.1", default-features = false, features = ["macos_kqueue"] }
TOML
cd "$dir"
cargo fetch -q
registry=$(ls -d "${CARGO_HOME:-$HOME/.cargo}"/registry/src/*/ | head -1)
echo "| Crate | Crates in its tree | unsafe in its own src | Lines in its own src |"
echo "| --- | --- | --- | --- |"
for crate in pulldown-cmark similar unicode-width ratatui crossterm syntect ignore notify; do
    count=$(cargo tree -e normal -p "$crate" --prefix none | sed 's/ (\*)//' | sort -u | wc -l)
    version=$(cargo tree -e normal -p "$crate" --depth 0 --prefix none | awk '{print $2}' | tr -d v)
    src="$registry$crate-$version/src"
    unsafe=$(grep -rwo unsafe "$src" | wc -l)
    lines=$(find "$src" -name '*.rs' -exec cat {} + | wc -l)
    echo "| $crate $version | $count | $unsafe | $lines |"
done
echo
echo "Async runtimes or C builds in the tree:"
found=$(cargo tree -e normal -e build --prefix none | grep -E -i '^(tokio|async-|futures|smol|onig|cc |bindgen)' | sort -u || true)
echo "${found:-none}"
echo
echo "cargo deny, licences and advisories:"
cargo deny check licenses advisories 2>&1 | grep -E '^(error|warning)\[(rejected|unmaintained|vulnerability)|license = |ID: RUSTSEC|^ *└── [a-z]' || true
