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
- Growing the storage makes the rows it needs, not at least 1000. With no history in the grid
  that cache only saved work on a resize, and one row of growth cost about 3.8 MB across both
  grids at 80 columns. Freed pages still count on macOS, so the peak stayed in the footprint.
- `CSI 3J` no longer depends on the grid holding history. The guard would otherwise skip
  the clear, and a shell's `clear` would leave its rows in the store.

Upstream's `tests/` is left out because it is 46 MB of recordings, and its Cargo test targets
go with it. The crate has `unsafe`, so it stays out of the workspace, whose crates forbid it.

`smithay-clipboard/` is `smithay-clipboard` 0.7.2 from crates.io, MIT, plus
[`smithay-clipboard.patch`](smithay-clipboard.patch), used the same way. It reads a Wayland
selection offer from another client, and stock 0.7.2 and 0.7.3 append it all to a `Vec`, so a
hostile client could grow the window without bound (#310). The patch stops reading past 1 MiB,
the paste limit, and fails the load with `ErrorKind::FileTooLarge`, which `pinfold-sys` shows
as a paste over the limit; its buffer is made that size once and never grows. Reading the
offer ourselves would mean a second Wayland data device client, many times this patch. Drop
the fork once upstream caps loads ([Smithay/smithay-clipboard#66](https://github.com/Smithay/smithay-clipboard/issues/66)). Its GitHub workflows,
`.gitignore` and `doc_index.html` are left out.

`./check.sh` proves each directory is the published crate plus its patch, and CI runs it. To
change a fork, edit the crate, then run `./check.sh --update`, which rewrites every patch;
keep only the one you meant to change.

`cargo audit` and `cargo deny` cannot see a path crate, and `cargo update` cannot fix one. The
Nightly workflow fails when RustSec lists an advisory for either crate. Bug fixes in later
upstream releases have to be watched for by hand.
