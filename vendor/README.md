# Vendored crates

`alacritty_terminal/` is `alacritty_terminal` 0.26.0 from crates.io, plus
[`alacritty_terminal.patch`](alacritty_terminal.patch). It stays Apache-2.0 only and keeps
its notices (ADR-0012). The workspace uses it through `[patch.crates-io]`, so code imports
the crate by its usual name.

The patch does what history outside the grid needs (ADR-0006, #24):

- `HistorySink` receives rows as they leave the primary screen, and hears when history is
  cleared. It is called directly, so hostile output cannot pile rows up in a buffer.
- Rows that reflow off the top when the window narrows go to the sink.
- A blank screen clears without scrolling once the sink holds rows, as stock does with history.
- `CSI 3J` no longer depends on the grid holding history. The guard would otherwise skip
  the clear, and a shell's `clear` would leave its rows in the store.

Upstream's `tests/` is left out because it is 46 MB of recordings, and its Cargo test targets
go with it. The crate has `unsafe`, so it stays out of the workspace, whose crates forbid it.

`./check.sh` proves the directory is the published crate plus the patch, and CI runs it. To
change the fork, edit the crate, then run `./check.sh --update`.

`cargo audit` and `cargo deny` cannot see a path crate, and `cargo update` cannot fix one. The
Nightly workflow fails when RustSec lists an advisory for this crate. Bug fixes in later
upstream releases have to be watched for by hand.
