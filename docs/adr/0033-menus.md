# ADR-0033: Menus for mouse users

- **Status:** Accepted
- **Date:** 2026-11-03

## Context

Every action in PinFold needs a key: the prefix (`Ctrl+a`, then a letter) or one of a few
`Cmd` shortcuts handled in `crates/pinfold-ui/src/input.rs` and `prefix.rs`. A person who
does not know them has no way to find them. The maintainer asked for a menu bar, a Dock menu
and a right-click menu.

ADR-0005 says there is no GUI toolkit and the whole UI is cells. The menu bar and the Dock
menu are drawn by macOS outside the window, so cells cannot draw them. The right-click menu
is inside the window, where cells can.

## Decision

1. **One list of actions.** Every menu item runs an existing `prefix::Command`, or a few new
   ones (copy, paste, select all, find, font size, open config, open docs). Keys and every
   menu share the list, so a menu can never do something the keyboard cannot.
2. **macOS menu bar, native.** An `NSMenu` built once at startup through `objc2-app-kit` in
   `pinfold-sys`, where the `unsafe` lives. It holds:
   - **PinFold:** About, Settings (opens the config file), Hide, Quit
   - **Shell:** New Tab `⌘T`, Split Right `⌘D`, Split Down `⇧⌘D`, Close Pane `⌘W`, Detach
   - **Edit:** Copy `⌘C`, Paste `⌘V`, Select All `⌘A`, Find `⌘F`
   - **View:** Zoom Pane, Bigger `⌘+`, Smaller `⌘-`, Actual Size `⌘0`, Full Screen
   - **Window:** Next Tab `⇧⌘]`, Previous Tab `⇧⌘[`, tabs 1 to 9 `⌘1`–`⌘9`, Minimize
   - **Help:** opens the docs
   Each item's action posts its `Command` to the event loop through winit's proxy. The menu's
   key equivalents become the only home of `Cmd` shortcuts on macOS, so the key handler drops
   its own `Cmd` branch there, and no shortcut fires twice. Items that cannot apply, such as
   Copy with nothing selected, are greyed out.
3. **Dock menu, native.** Right-clicking the Dock icon offers New Tab, Split Right, Split
   Down, Detach, and one item per tab that goes to it, with the tab's agent state as glyph and
   word (CLAUDE.md, UI). macOS asks the app delegate for it, and winit owns that delegate, so a
   spike decides how to answer first. If no supported way exists, the Dock menu waits, and the
   ADR is updated.
4. **Right-click menu, in cells.** Right-click in a pane opens a menu drawn by the window over
   the pane, the same on Linux and macOS: Copy, Paste, Select All, Find, Split Right, Split
   Down, New Tab, Zoom, Close Pane, and Open Link and Copy Link on a hyperlink. Arrows, Enter
   and Esc work it as well as the mouse. When the program has turned on mouse reporting, the
   click goes to the program, as in other terminals, and `Shift`+right-click opens the menu.
   Like every prompt PinFold draws, it sits outside pane text, so program output cannot fake it.
5. **Paste from a menu is a paste.** It passes the paste sanitizer and bracketed paste, as
   `⌘V` does.

## Consequences

The maintainer decided on 2026-11-03 that all three menus block the macOS beta, so ADR-0031's
list names them.

The menu bar costs the window memory, and the macOS window has about 1 MB of margin at
1512x1890 (ADR-0030). Each menu PR includes the `mem-gate` table, and a menu that breaks the
budget is cut down before it lands.

`objc2-app-kit` 0.3 joins the tree beside the 0.2 that winit brings, because `pinfold-sys`
uses `objc2` 0.6. It is pure Rust bindings, MIT or Apache-2.0, from the `objc2` project
already in use.

Linux gets the right-click menu only. A global menu bar there would need D-Bus, against
ADR-0007, so it stays out unless a user asks for it.

`⌘D`, `⇧⌘D` and `⌘W` are new shortcuts. They follow Terminal and iTerm2, and the config can
change them later through `keys`.
