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
