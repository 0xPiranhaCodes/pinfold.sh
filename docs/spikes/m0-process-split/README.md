# M0 bet 1: process split

**Result: pass on Linux, miss on macOS.** The core and window together cost 0.99–1.42 MB
more than one process on Linux, within the 2 MB limit. On macOS they cost 2.33–3.51 MB more,
which misses it.

| Median of 5 runs, MB | Linux (PSS) | macOS (phys_footprint) |
| --- | --- | --- |
| Overhead, 1 pane | +0.99 | **+2.33** |
| Overhead, 8 panes | +1.42 | **+3.51** |
| Core alone, 1 pane | 1.40 | 2.28 |
| Core alone, 8 panes | 1.99 | 3.10 |
| Window process, 8 panes | 6.80 | 87.90 |
| Run-to-run spread | up to 0.09 | up to 0.59 |

Full tables are in [`results.md`](results.md), and `./run.sh` reproduces them.

## How it works

Three real binaries are built from one crate:

- `core` owns PTYs and `alacritty_terminal` state, with I/O, mux and API threads. It links no
  window code: `winit` and `softbuffer` do not appear in the binary.
- `window` receives each pane's grid over a Unix socket and draws it with `softbuffer`.
- `single` does both in one process with the same threads, as the baseline.

## Findings

- **The core fits its budget.** It stays under the ADR-0008 limit of 4 MB with 8 idle panes on
  both platforms, though without scrollback, a real API or a mux.
- **Idle panes look cheap.** Between 1 and 8 panes the core grows by about 84 KB per pane on
  Linux and 117 KB on macOS, under the 250 KB target before scrollback.
- **The macOS miss looks like the metric.** The window process stays about the size of the
  single process, so the overhead is close to the whole core. `phys_footprint` does not divide
  shared pages as PSS does. That explanation is inferred and was not tested.
- **The split still looks worth it.** It delivers the crash isolation in ADR-0003 at a cost
  that is small next to the window. That is a judgement for the maintainer, because the bet's
  macOS criterion was not met.
- **The macOS window is the real problem.** At 87.90 MB it dwarfs this overhead. Bet 2 covers it.

## Not tested

- Fonts, scrollback, tools, real API work, or any pane activity. The panes sit idle.
- A real desktop on Linux. Xvfb is a virtual display, and Linux ran in a container.
- Intel macOS and x86_64 Linux.
- A true `pinfold` build. This spike's IPC and mux are minimal.
