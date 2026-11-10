# Graphite Grid UI boards

Exported from the design canvas on 2026-10-31. `Foundations.dc.html` defines the
palette, type and agent states. The other boards show the core screens, and
`canvas.json` lists them.

The boards are Design Component source. They load `support.js` from the design tool,
so they do not render when opened from this folder.

## Native window refinement

The native tab strip uses graphite inactive surfaces (`#1C2026`), a lifted hover
surface (`#2B323B`), and a muted teal active surface (`#253C3B`) with a bright
teal label (`#5CD6CB`). Shortcut badges remain recessed; inactive labels and
shortcuts retain contrast. The active tab of an unfocused window keeps a lifted
surface and bold, light label so the selected tab is still identifiable.

The title remains aligned with the macOS traffic lights. Side and bottom insets
are 16 logical pixels, with 16 pixels between title and tabs and 18 pixels before
the terminal. Fullscreen retains its compact single-row header. All dimensions
scale with the display; tabs still compress to fit narrow windows. Search hints
use secondary text so the query remains the primary focus.

These refinements use the existing cell renderer and rounded pill shapes, with
no animation timers, extra buffers, or dependencies. Terminal colours and agent
state glyphs, colours and words remain independent of the chrome palette.

## New tabs and splits

A new tab or split opened from the window, by key or palette, starts in the working
directory of the pane that was focused, as Terminal, iTerm2 and Ghostty do. PinFold
asks the system where that pane's shell is, never the pane's OSC 7 report, which any
program's output can fake (T4). If that directory no longer exists or cannot be
read, the new pane starts where it did before (home, on macOS) and shows no error.

## Themes

A theme changes colours and nothing else: the font, the cells, and each agent state's glyph and
word stay ([ADR-0049](../../adr/0049-themes.md)). Graphite Grid is the default theme, built in
as `pinfold_config::theme::GRAPHITE`, and the chrome and state colours every other theme falls
back to when its own fail the legibility checks in
[`docs/configuration.md`](../../configuration.md#themes). Its palette, ANSI colours included,
is the design's:

| Key | Colour | Key | Colour |
| --- | --- | --- | --- |
| `background` | `#121212` | `foreground` | `#E2E5EA` |
| `black` | `#12151A` | `bright_black` | `#343A44` |
| `red` | `#FF8A8A` | `bright_red` | `#FF8A8A` |
| `green` | `#63D29A` | `bright_green` | `#63D29A` |
| `yellow` | `#F5B544` | `bright_yellow` | `#F5B544` |
| `blue` | `#7AA7FF` | `bright_blue` | `#9CC0FF` |
| `magenta` | `#D9A7E0` | `bright_magenta` | `#D9A7E0` |
| `cyan` | `#9CC0FF` | `bright_cyan` | `#C4D9FF` |
| `white` | `#C9CDD4` | `bright_white` | `#E2E5EA` |
| `cursor` | `#E2E5EA` | `cursor_text` | `#121212` |
| `selection_background` | `#E2E5EA` | `selection_foreground` | `#121212` |
| `chrome.accent` | `#5CD6CB` | `chrome.tab` | `#1C2026` |
| `chrome.tab_hover` | `#2B323B` | `chrome.tab_active` | `#253C3B` |
| `chrome.overlay` | `#12151A` | `chrome.border` | `#343A44` |
| `chrome.dim` | `#9AA1AC` | `chrome.muted` | `#555A63` |
| `states.needs_you` ◆ | `#F5B544` | `states.running` ● | `#7AA7FF` |
| `states.done` ✓ | `#63D29A` | `states.error` ✕ | `#FF8A8A` |
| `states.idle` ○ | `#9AA1AC` | | |

The lowest contrast of a state on a surface is 4.51, idle on the active tab, and the closest two
states are 0.122 apart in OKLab (`docs/spikes/themes/contrast.py`, and the tests of
`pinfold-config`'s `theme` module).
