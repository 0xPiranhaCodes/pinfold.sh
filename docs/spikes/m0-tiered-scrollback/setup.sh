#!/bin/sh
# Rebuilds vendor/ from the published alacritty_terminal 0.26.0 plus our patch.
set -eu
cd "$(dirname "$0")"
rm -rf vendor
mkdir -p vendor
curl -sSL https://static.crates.io/crates/alacritty_terminal/alacritty_terminal-0.26.0.crate \
  | tar -xz -C vendor
mv vendor/alacritty_terminal-0.26.0 vendor/alacritty_terminal
patch -s -p1 -d vendor/alacritty_terminal < grid-eviction.patch
