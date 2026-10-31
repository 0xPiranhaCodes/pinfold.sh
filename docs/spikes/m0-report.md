# M0 report

Date: 2026-10-31. **Awaiting maintainer go-ahead.** Nothing here changes an ADR yet.

M0 tested the four bets the design rests on. Two hold with limits, one holds on one platform,
and one fails. The headline memory goal has to change on macOS.

| Bet | Limit | Measured | Verdict |
| --- | --- | --- | --- |
| [4. Caller identity](m0-caller-identity/README.md) | Pane A cannot act as pane B | Never through the API. Same-user ptrace still works on Linux at `ptrace_scope` 0 | **Pass for the API**, hardening adopted in ADR-0020 |
| [3. Tiered scrollback](m0-tiered-scrollback/README.md) | ≤ 1 MB and < 5 ms | Scroll 2.82 ms at worst, all rows exact. 0.46–0.89 MB on text on Linux. 14.66 MB of heap on random truecolor | **Partial pass** |
| [1. Process split](m0-process-split/README.md) | Within 2 MB of one process | Linux +0.99 to +1.42 MB. macOS +2.33 to +3.51 MB | **Pass on Linux, miss on macOS** |
| [2. Window memory](m0-window-memory/README.md) | Half-screen Retina ≤ 12 MB | CPU 65.23 MB, Metal 47.65 MB, empty window 19.37 MB | **Fail**, so Metal becomes the macOS default |

## What changes

- **macOS can't be small in the way Linux can.** An empty window costs more than the old 12 MB
  limit. Pinfold can still beat GPU terminals there, but no comparison has been measured.
- **Linux supports the original pitch.** A full-HD X11 window totals 12.51 MB and the core 1.99 MB
  with 8 panes, before fonts and scrollback.
- **Two renderers enter M1.** CPU on Linux and Metal on macOS means two paths to test from the
  start, as ADR-0005 anticipated.
- **The grid fork is the largest unmeasured cost.** Search, selection, vi mode, display scrolling
  and reflow all assume history lives in the grid, and none has been prototyped.

## Proposed targets

| Metric | ADR-0008 now | Proposed | Measured |
| --- | --- | --- | --- |
| Core, 8 idle panes, no scrollback | ≤ 4 MB | Keep | 1.99 Linux, 3.10 macOS |
| Each idle pane, no scrollback | ≤ 250 KB | Keep | About 84 KB Linux, 117 KB macOS |
| 10k lines at 120 columns | ≤ 1 MB | ≤ 1 MB for typical text on Linux, ≤ 1.5 MB on macOS | Text: 0.46–0.89 Linux, 0.77–1.20 macOS |
| Cold scrollback per pane | None | New hard byte cap, 2 MB by default | Needed: 19.92 MB on hostile output |
| Window on Linux | 6 MB + 2 × 4 B × pixels | 8 MB + 2 × 4 B × pixels | Wayland full HD 21.83 of 22.59, before fonts |
| Window on macOS | 6 MB + 2 × 4 B × pixels | 26 MB + 2 × 4 B × pixels, using Metal | 47.65 half, 73.11 full, excluding WindowServer |

The 2 MB cap, the extra 2 MB on Linux and the 26 MB on macOS are proposals derived from these
measurements, and none has been tested as a limit. Fonts are unmeasured everywhere.

## Proposed roadmap

The order of M1 to M5 stays. The changes are:

1. **M1 gains** the Metal renderer on macOS, the scrollback byte cap, and the first fork work:
   search and selection through the store.
2. **M2 gains** ADR-0020's core hardening and reflow of cold scrollback.
3. **`mem-gate` starts in M1,** built from the `measure` tools in these spikes.
4. **Allocator relief needs proof before anyone relies on it.** One call had no effect here.
5. **Decide whether the built-in tools stay in v0.1.** The review estimates 40–50 solo weeks, or
   moving the tools to v0.2. M0 added work, so I recommend moving M4 to v0.2. No schedule has been
   measured.

## Limits of this evidence

- Everything ran on one M1 Pro and in arm64 Linux containers. There was no Intel Mac, x86_64 or
  real Linux desktop.
- Nothing included fonts, a real cell renderer or a full API, so product numbers will be higher.
- Scrollback corpora are generated. Memory runs covered idle panes and short bursts, not long use.
- macOS totals exclude WindowServer, and its compositor cost is unknown.
- macOS bet 1's cause and Metal's buffer model are inferred from a small set of measurements.

## Decisions needed

1. Accept the proposed targets, which will amend ADR-0008?
2. Accept Metal as the macOS default, which settles ADR-0005? The bet's rule says yes.
3. Keep the two-process design despite the macOS miss in bet 1? I recommend yes, because crash
   isolation costs a few MB next to a window of about 48 MB.
4. Move the built-in tools to v0.2?
