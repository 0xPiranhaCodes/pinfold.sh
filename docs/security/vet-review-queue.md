# cargo-vet review queue: Tier 1

The 20 Tier 1 exemptions from #302 that no import and no trusted publisher covers. Tier 1
means direct dependencies, or crates the vendored `alacritty_terminal` uses directly,
that have `unsafe`, FFI or a build script and ship on macOS or Linux. The other 134
exemptions are in tiers 2 to 5; their `notes` in `supply-chain/config.toml` say why
each one remains.

**This page is not an audit.** It is a pre-read to help the maintainer audit each
crate as described in [dependency-review.md](dependency-review.md). Nothing here was
recorded with `cargo vet certify`. "Pre-read" means the build script, the FFI surface and
where the `unsafe` sits were looked at; the `unsafe` blocks were not checked one by one
unless an entry says so. Findings marked **#307** come from that PR's AI-assisted
pass, recorded in the exemption's `notes`.

Before this queue was written, `cargo vet regenerate imports` found nothing new, and
importing the three remaining registry sources (`actix`, `fermyon`, `ariel-os`)
covered no exempt version, so none were added.

## Order

| # | Crate | Version | Review | Size | Runs in |
| - | ----- | ------- | ------ | ---- | ------- |
| 1 | `parking_lot` | 0.12.5 | inspect | 6,208 lines | core, window |
| 2 | `parking_lot_core` | 0.9.12 | inspect | 4,029 lines | core, window |
| 3 | `home` | 0.5.12 | diff from 0.5.11 | 37 lines | core |
| 4 | `bitflags` | 2.13.2 | diff from 2.11.1 | 733 lines | core, window |
| 5 | `polling` | 3.11.0 | inspect | 8,582 lines | core, window |
| 6 | `zeno` | 0.3.3 | inspect | 6,580 lines | window |
| 7 | `yazi` | 0.2.1 | inspect | 3,942 lines | window |
| 8 | `font-types` | 0.12.7 | diff from 0.12.1 | 736 lines | window |
| 9 | `swash` | 0.2.10 | inspect | 27,130 lines | window |
| 10 | `softbuffer` | 0.4.8 | inspect | 6,168 lines | window |
| 11 | `dispatch` | 0.2.0 | inspect | 1,329 lines | window (macOS) |
| 12 | `dispatch2` | 0.3.1 | inspect | 7,618 lines | window (macOS) |
| 13 | `block2` | 0.5.1 | diff from 0.6.2 | 1,566 lines | window (macOS) |
| 14 | `objc2` | 0.5.3 | diff from 0.6.3 | 24,779 lines | window (macOS) |
| 15 | `objc2-quartz-core` | 0.2.2 | inspect | 5,310 lines | iOS only today |
| 16 | `objc2-metal` | 0.2.2 | inspect | 18,749 lines | iOS only today |
| 17 | `objc2-foundation` | 0.2.2 | inspect | 60,961 lines | window (macOS) |
| 18 | `objc2-app-kit` | 0.2.2 | inspect | 87,848 lines | window (macOS) |
| 19 | `objc2-app-kit` | 0.3.2 | inspect | 121,787 lines | window (macOS menu bar) |
| 20 | `winit` | 0.30.13 | inspect | 62,412 lines | window |

The Amanieu crates come first: the maintainer may prefer to trust the publisher rather
than audit them (below). After them, the order is crates the core reaches, then
untrusted-input parsers in the window, then FFI by size. Moving `winit` and `copypasta`
to `objc2` 0.6 would let mozilla's imports cover most of rows 13 to 18, which is cheaper
than reviewing the 0.5 line.

## Entries

### 1-2. `parking_lot` 0.12.5, `parking_lot_core` 0.9.12

- **Command:** `cargo vet inspect parking_lot 0.12.5`, `cargo vet inspect parking_lot_core 0.9.12`
- **Who uses it:** vendored `alacritty_terminal` (default features), so the core.
- **What the unsafe does:** `parking_lot` (88 `unsafe` tokens) implements raw
  mutex, rwlock, condvar and once on atomics; `parking_lot_core` (149) keeps the global
  parking hash table and parks threads with `futex` on Linux and pthread mutex/condvar
  on macOS, through `libc`. Its `build.rs` reads only `CARGO_CFG_SANITIZE` and sets a
  `tsan_enabled` cfg.
- **Trust option:** mozilla, bytecode-alliance and isrg trusted the publisher, Amanieu
  d'Antras (crates.io user 2915), for both crates. Every entry has expired: mozilla's
  `parking_lot` on 2026-04-29, `parking_lot_core` on 2024-05-05, bytecode-alliance's on
  2024-07-06. `dependency-review.md` trusts a publisher only when an import trusts them
  now, so trusting him again needs a policy change by the maintainer. The same applies
  to `lock_api` and `scopeguard` (Tier 3).
- **Pre-read:** nothing found. The build script is benign; the `unsafe` blocks were
  not read.

### 3. `home` 0.5.12

- **Command:** `cargo vet diff home 0.5.11 0.5.12`
- **What changed:** edition 2024, `windows-sys` 0.59 to 0.61, `extern "C"` to `unsafe
  extern "C"` and `unsafe {}` around `env::set_var` in Windows-only code and tests,
  two doc typos. On Unix, `home_dir` still calls `std::env::home_dir`.
- **Pre-read:** the whole diff was read; nothing found. This is the cheapest entry.

### 4. `bitflags` 2.13.2

- **Command:** `cargo vet diff bitflags 2.11.1 2.13.2`
- **What changed:** a `#[bitflags(flag_name = "...")]` rename attribute, a new
  `IterEqualNames` iterator, macro plumbing and lint allows, and tests. No new `unsafe`,
  no build script, no file, process, network or environment access, no new
  dependencies. The crate's only `unsafe` is the `bytemuck` `Pod`/`Zeroable` impls on a
  `repr(transparent)` wrapper, unchanged.
- **Pre-read:** the `src/` diff was read; nothing found.

### 5. `polling` 3.11.0

- **Command:** `cargo vet inspect polling 3.11.0`
- **Who uses it:** vendored `alacritty_terminal` and `calloop`.
- **What the unsafe does:** 134 `unsafe` tokens. On macOS and Linux it calls
  `kqueue` and `epoll` through `rustix` (no `extern` blocks, no build script); about half
  of the `unsafe` is in `src/iocp/`, which builds only for Windows.
- **Pre-read:** nothing found; the Unix backends' `unsafe` was not read.

### 6-9. `zeno` 0.3.3, `yazi` 0.2.1, `font-types` 0.12.7, `swash` 0.2.10

- **Commands:** `cargo vet inspect zeno 0.3.3`, `cargo vet inspect yazi 0.2.1`,
  `cargo vet diff font-types 0.12.1 0.12.7`, `cargo vet inspect swash 0.2.10`
- **What they do:** `swash` (54 `unsafe` tokens) parses and scales font files, `zeno`
  (1) rasterises outlines, `yazi` (0) inflates PNG glyph bitmaps, `font-types` (4)
  defines font scalar types. Fonts are semi-trusted input. No build scripts or FFI.
- **Pre-read (#307):** `zeno`'s `raster.rs` `to_fixed` calls `f32::to_int_unchecked`,
  undefined behaviour on NaN, infinite or out-of-range values; `pinfold-render` rejects
  non-finite outlines and extents past its cells first (#310). `yazi`'s `Vec` sink has
  no output cap. In `font-types`, only the new `unsafe` `TableDataRef::header` was read
  and found sound.

### 10. `softbuffer` 0.4.8

- **Command:** `cargo vet inspect softbuffer 0.4.8`
- **What the unsafe does:** 87 `unsafe` tokens presenting a CPU framebuffer through
  CoreGraphics on macOS and X11 shared memory or Wayland shm on Linux, with the X11 and
  Wayland libraries loaded by `dlopen`. No build script.
- **Pre-read (#307):** not read.

### 11-12. `dispatch` 0.2.0, `dispatch2` 0.3.1

- **Commands:** `cargo vet inspect dispatch 0.2.0`, `cargo vet inspect dispatch2 0.3.1`
- **What the unsafe does:** bindings to libdispatch (Grand Central Dispatch) queues;
  53 and 243 `unsafe` tokens. No build scripts.
- **Pre-read (#307):** not read. google rates `dispatch2` 0.3.0 `ub-risk-3` and does not
  certify it `safe-to-deploy`.

### 13-14. `block2` 0.5.1, `objc2` 0.5.3

- **Commands:** `cargo vet diff block2 0.6.2 0.5.1`, `cargo vet diff objc2 0.6.3 0.5.3`
  (backwards diffs from mozilla's audits).
- **What the unsafe does:** `objc2` (1,082 `unsafe` tokens) sends Objective-C messages
  and declares classes over `objc-sys`, which links `libobjc` and is audited; `block2`
  (85) builds Apple block ABI closures to pass into Objective-C. No build scripts.
- **Pre-read (#307):** not read. `winit` 0.30 and `copypasta` need the 0.5 line.

### 15-19. `objc2-quartz-core`, `objc2-metal`, `objc2-foundation`, `objc2-app-kit` ×2

- **Commands:** `cargo vet inspect CRATE VERSION` for each.
- **What the unsafe does:** generated bindings to Apple frameworks, mostly `unsafe`
  extern methods (728 to 24,419 `unsafe` tokens). No build scripts. `objc2-metal` and
  `objc2-quartz-core` 0.2.2 are reached only through iOS-only `objc2-ui-kit` today.
- **Pre-read:** not read. Generated code is hard to audit by reading; `dependency-review.md`
  says such crates stay exempt rather than get an audit of the parts read.
  `objc2-app-kit` 0.3.2 is the one `pinfold-sys` uses directly, for the menu bar.

### 20. `winit` 0.30.13

- **Command:** `cargo vet inspect winit 0.30.13`
- **What the unsafe does:** 938 `unsafe` tokens of platform FFI across the macOS, X11,
  Wayland, Windows, web, Android and Orbital backends. Its `build.rs` only sets
  `cfg_aliases` from target cfgs.
- **Pre-read:** the build script is benign; the backends were not read.

## How these numbers were made

Sizes are the "Audit Size" column of `cargo vet suggest` with cargo-vet 0.10.3
(`N lines` for an inspect, insertions plus deletions for a diff). `unsafe` counts,
build scripts and `links` come from the crate as published, in the local registry:

```sh
R=$(ls -d ~/.cargo/registry/src/index.crates.io-*/ | head -1)
for c in parking_lot-0.12.5 winit-0.30.13; do  # and so on
  echo "$c build.rs=$([ -f $R/$c/build.rs ] && echo yes || echo no)" \
    "unsafe=$(grep -rhow unsafe $R/$c --include='*.rs' | wc -l)"
done
```

Platforms come from `cargo tree --workspace --all-features -e normal,build --target T`
for `aarch64-apple-darwin` and `x86_64-unknown-linux-gnu`.
