# M3 spike: a Dock menu while winit owns the app delegate (#114)

ADR-0033 point 3. macOS asks `NSApp.delegate` for `applicationDockMenu:` when the Dock icon
is right-clicked. winit 0.30.13 installs its own `WinitApplicationDelegate` and exposes no
way to answer that selector. Can PinFold answer it in a supported way?

## Result

Yes, with one caveat about what "supported" means. Adding the method to winit's delegate
class with `class_addMethod` works: the menu is built per request, every item posts an event
through winit's `EventLoopProxy`, and nothing in winit has to change. No winit API does this,
and the alternative of replacing the delegate breaks winit.

## Options tried

| Option | Verdict |
| --- | --- |
| winit's own API (0.30.13) | None. `grep` of `platform_impl/macos` finds no Dock menu hook; `ActiveEventLoop` and `WindowExtMacOS` expose none. |
| Replace the delegate with `NSApp.setDelegate` | No. winit later fetches its delegate and `panic!`s ("tried to get a delegate that was not the one Winit has registered", `app_state.rs`). Forwarding every method by hand is fragile. |
| `NSApp.dockTile` | No. It only offers a badge label and a custom content view, not a menu. |
| `NSApp.menu` / main menu | No. That is the menu bar (ADR-0033 point 2), not the Dock menu. |
| **`class_addMethod` on `WinitApplicationDelegate`** | **Works.** `src/main.rs`. |

How the working option behaves:

- `install()` looks up the class by name after `EventLoop::build()` and adds
  `applicationDockMenu:` (encoding `@@:@`). `class_addMethod` returns false instead of
  overwriting if winit ever implements the selector itself, and the spike prints that result.
- AppKit calls the method on the main thread on each right-click, so tab state is read fresh.
  The menu is rebuilt each time; items point at a small `define_class!` target (kept alive in
  a thread-local, because `NSMenuItem` holds its target unretained) whose `dockItem:` maps
  the item tag to an event and sends it with `EventLoopProxy::send_event`.
- Items: New Tab, Split Right, Split Down, Detach, a separator, then one item per tab as
  `<glyph> <name>  <word>` (`◆ docs  needs you`, `● api  running`, `✕ tests  error`).
- Versions: `objc2 =0.6.5`, `objc2-app-kit =0.3.2` (current on crates.io, 2026-11-03).
  winit 0.30.13 uses objc2 0.5 internally; both share the one Objective-C runtime, so the
  mismatch is harmless here, and pinfold-sys already uses objc2 0.6.

## What was verified, and what was not

Run `cargo run -- --self-test` (macOS). It starts winit, asks the delegate for
`applicationDockMenu:` the way AppKit does, prints the items and triggers each one with
`performActionForItemAtIndex:`. Output on this machine:

```
class_addMethod ok: true
item 0: New Tab / 1: Split Right / 2: Split Down / 3: Detach / 4: (separator)
item 5: ● api  running   6: ◆ docs  needs you   7: ✕ tests  error
FIRED NewTab, SplitRight, SplitDown, Detach, GoTo(0), GoTo(1), GoTo(2)
```

**Not verified: the Dock's own right-click.** That cannot be done from a script. Maintainer
hand check:

```
cd docs/spikes/m3-dock-menu && cargo run
```

Right-click the Dock icon of "dock-menu spike". The menu should list the items above; click
one and the terminal prints `FIRED <item>`. Press keys in the window to cycle tab 0's state,
then right-click again to see the glyph and word change. (Unbundled binaries show the Dock
icon of the terminal-launched process; if the Dock shows a generic icon, that is expected.)

## Recommendation for #113 and #114

Ship the Dock menu through `class_addMethod`, in `pinfold-sys` (the only crate allowed
`unsafe`), behind a small safe function such as `install_dock_menu(items: impl Fn() -> Vec<DockItem>)`.
`pinfold-ui` supplies the item list from the shared `prefix::Command` list (ADR-0033 point 1)
and receives clicks as a winit user event, like any other command. Keep it a no-op with a
`tracing::warn!` when the class lookup or `class_addMethod` fails, so a winit change degrades
to "no Dock menu", never a crash.

Is this "supported"? It is public Objective-C runtime API and winit documents nothing against
it, but winit does not promise the class name or that it stays the delegate. It is a guarded
workaround, not a winit feature.

### Proposed ADR-0033 point 3 wording (for the maintainer; the ADR is not edited here)

> 3. **Dock menu, native.** Right-clicking the Dock icon offers New Tab, Split Right, Split
>    Down, Detach, and one item per tab that goes to it, with the tab's agent state as glyph
>    and word (CLAUDE.md, UI). winit owns the app delegate and has no Dock menu API, so
>    `pinfold-sys` adds `applicationDockMenu:` to winit's delegate class at runtime
>    (`class_addMethod`, spike m3-dock-menu). The lookup is guarded: if winit's delegate class
>    is not found or already answers the selector, the Dock menu is skipped and logged, never
>    a crash. A winit upgrade re-runs the hand check below.

## Risks

- **winit upgrades.** The class name `WinitApplicationDelegate` is an internal detail of
  0.30.x. It is pinned (`=0.30.13`) in `pinfold-ui`, so it only changes when someone bumps
  winit: add the hand check to that PR. winit 0.31 beta moves the macOS backend into a
  separate `winit-appkit` crate; this spike did not check that crate (the 0.31.0-beta.4 main
  crate has no Dock code), so re-run the spike on 0.31 before upgrading.
- **If winit adds its own `applicationDockMenu:`.** `class_addMethod` returns false, we log
  and fall back to winit's menu, so no double install and no overwrite.
- **Not testable in CI.** Only the delegate query is automatable (`--self-test`); the Dock
  right-click is a manual check.
- **Threading.** The method must run on the main thread. AppKit guarantees this; the spike
  uses `MainThreadMarker::new_unchecked` with a `// SAFETY:` comment, and `pinfold-sys`
  should assert the same.
- **Upstream alternative.** A small winit PR adding a Dock menu hook would remove the
  workaround; not attempted here.
