# Manual checks for the macOS beta

[ADR-0031](../adr/0031-macos-beta-first.md) makes these checks a beta blocker. Run them on the
signed, notarized `PinFold.dmg` from the draft release, after the checks in
[Cutting the beta](../../packaging/README.md#cutting-the-beta), on at least one Apple Silicon Mac
and one Intel Mac. Start each Mac with no earlier PinFold: do [Uninstall](#13-uninstall) first if
one was there. Record each run in the [results](#results) table.

## 1. Install and first launch

1. Open `PinFold.dmg`. Expected: a window with `PinFold.app` and an `Applications` link.
2. Double-click `PinFold.app` on the disk image. Expected: a dialog says PinFold cannot run from a
   disk image or another read-only place and to drag it to Applications, and no `pinfold-core`
   runs (`pgrep -fl pinfold-core` in Terminal.app prints nothing from `/Volumes`)
   ([#279](https://github.com/0xPiranhaCodes/pinfold.sh/issues/279)).
   Drag `PinFold.app` onto `Applications`, then eject the disk image. Note in the results whether
   the first drag moved the icon. `bash tests/macos/dmg-eject.sh PinFold.dmg [old.dmg]` runs the
   rest of #279's matrix without dragging.
3. Open PinFold from Applications. Expected: macOS asks whether to open an app downloaded from
   the Internet, and names no unidentified developer and no damage. Choose Open. A PinFold window
   opens with one shell.
4. Quit with `⌘Q` and open PinFold again. Expected: no Gatekeeper prompt this time.
5. Add the CLI, as the [README](../../README.md#install-the-beta) says, and run `pinfold ctl list`
   in a new Terminal.app window. Expected: the one pane is listed. `pinfold --version` prints
   `pinfold 0.1.0-beta.1`, and `pinfold --help` lists every command.

## 2. Menu bar

1. Check each menu holds what [docs/configuration.md](../configuration.md#keys) lists: PinFold,
   Shell, Edit, View, Window and Help.
2. With nothing selected, open Edit. Expected: Copy is greyed out, and `⌘C` does nothing.
3. Try `⌘T`, `⌘D`, `⇧⌘D`, `⌘W`, `⌘=`, `⌘-`, `⌘0`, `⇧⌘]`, `⇧⌘[` and `⌘1`. Expected: each does
   what its item says, once.
4. PinFold > Settings… Expected: `~/.config/pinfold/pinfold.toml` opens in the default text
   editor, created empty if it was missing.
5. Help > PinFold Help. Expected: `docs/configuration.md` on GitHub opens in the browser.

## 3. Dock menu

The steps from [#170](https://github.com/0xPiranhaCodes/pinfold.sh/pull/170):

1. Open two tabs. In one run `sleep 30` (● running); leave the other at its prompt (○ idle).
2. Right-click PinFold's Dock icon. Expected: New Tab, Split Right, Split Down, Detach, a
   separator, then `● <label>  running` and `○ <label>  idle`.
3. Choose each item. Expected: New Tab opens a tab, Split Right and Split Down split the focused
   pane, a tab item switches to that tab, and Detach closes the window with the session kept.
4. After `sleep` ends, right-click the Dock icon again. Expected: that tab's glyph and word have
   changed.

## 4. Right-click menu and links

The steps from [#171](https://github.com/0xPiranhaCodes/pinfold.sh/pull/171). In a pane:

1. Right-click plain text. Expected: Copy, Paste, Select All, Find, Split Right, Split Down, New
   Tab, Zoom Pane and Close Pane, with no link items. Arrows, Enter and Esc work the menu.
2. `printf '\e]8;;https://pinfold.sh\e\\pinfold site\e]8;;\e\\\n'`, then right-click
   `pinfold site`. Expected: Open Link and Copy Link are the last two items. Copy Link, then `⌘V`,
   pastes `https://pinfold.sh`. Open Link opens it in the browser with no prompt.
3. `printf '\e]8;;x-man-page://ls\e\\man ls\e]8;;\e\\\n'`, then Open Link. Expected: a
   `── PinFold link ──` border and `◆ needs you  Open this link? x-man-page://ls` over muted pane
   text, with `Enter opens · Esc cancels` on the bottom row. Esc opens nothing; Enter opens the
   `ls` man page.
4. `printf '\e]8;;file:///tmp/a\342\200\256b\e\\odd\e]8;;\e\\\n'`, whose `\342\200\256`
   is U+202E, then Open Link. Expected: the prompt shows `file:///tmp/a<U+202E>b`.
5. Run `vim`, `:set mouse=a`, and right-click. Expected: no menu; `Shift`+right-click opens it.

## 5. Tabs, splits and zoom

1. Prefix `Ctrl+a` then `c`, `|`, `-`, the arrows and `z`. Expected: a tab, a split right, a
   split down, focus moves, and the focused pane fills the tab, then returns on `z` again.
2. `Ctrl+a` then `x`, then Enter. Expected: the pane closes. On the last pane of the last tab the
   question says so in red.
3. Resize the window and run `tput cols; tput lines`. Expected: the numbers follow the size.

## 6. `pinfold agent`

1. Add this to `~/.config/pinfold/pinfold.toml`. The core reads profiles when it starts, so
   then close every pane, which ends the session and the core, and open PinFold again:

   ```toml
   [agents.demo]
   command = ["top"]
   ```

2. In a pane, `pinfold agent demo`. Expected: a split to the right runs `top`. Repeat with
   `--down` and `--tab`.
3. `pinfold agent nosuch`. Expected: `no agent profile by that name in the config`; no pane
   opens.

## 7. `pinfold up` and the trust prompt

1. In an empty directory you own, write `.pinfold/layout.toml`:

   ```toml
   [[tabs]]
   [[tabs.panes]]
   run = "echo hello"
   ```

2. `pinfold up`. Expected: a `── PinFold workspace trust` border, `◆ needs you  Trust the layout
   of <dir>`, the line `echo hello`, and `y trust · n/Esc don't trust`. Press `n`. Expected: the
   layout opens as a plain shell, and `pinfold up` exits 1 saying the line did not run.
3. Close that tab, run `pinfold up` again and press `y`. Expected: `hello` prints.
4. Close the tab, run `pinfold up` again. Expected: no prompt; `hello` prints.
5. Edit the `run` line and run `pinfold up`. Expected: the prompt again, saying the layout changed
   since it was trusted.

## 8. Quit and restore

1. Make two tabs, one split, and `cd /tmp` in one pane. In another run `sleep 600`, and in a
   third `pinfold agent demo` from step 6.
2. `⌘Q`. Expected: the window closes, and `pinfold ctl list` from Terminal.app still lists every
   pane.
3. Open PinFold. Expected: the same tabs, splits and running `sleep`.
4. Log out and back in, or restart the Mac, then open PinFold. Expected: the same tabs and splits,
   with fresh shells, the one that was in `/tmp` still there, and under the agent's pane, when
   focused, `○ was running: top`. Nothing ran by itself. Enter runs it; Esc dismisses the strip.
   The `sleep` pane shows no strip, as a shell pane records only its shell.

## 9. Detach and reopen

1. Shell > Detach, or `Ctrl+a` then `d`. Expected: the window closes and every pane keeps running.
2. Run `pinfold` from Terminal.app, or open the app. Expected: the same session, with output the
   panes printed meanwhile.
3. Run `pinfold` again with the window open. Expected: the window comes to the front; no second
   window.

## 10. About panel

PinFold > About PinFold. Expected: the icon, the name, `PinFold 0.1.0-beta.1` above the
copyright, the getting started text with coloured state glyphs, three working links, and the
licenses. Check it in light and dark mode.

## 11. Memory

In a checkout of the release tag on the same Mac, with nothing else heavy running:

```sh
XDG_CONFIG_HOME="$(mktemp -d)" cargo xtask mem-gate
```

Expected: every row passes. Paste the table into the notes, with `uptime`'s load average. This
measures a build from the checkout, not the installed app.

## 12. Crash isolation

1. With a `sleep 600` pane open, find the window's process with `pgrep -x pinfold-window` and
   `kill -9` it. Expected: the window goes, and `pinfold ctl list` still lists the pane.
2. Run `pinfold`. Expected: the window returns with `sleep` still running.

## 13. Uninstall

Follow [Uninstall](../../README.md#uninstall) in the README. Expected: `pgrep -x pinfold-core`
prints nothing, and none of the listed paths are left.

## Results

One row per Mac. For a failure, give the step number and link the issue.

| Date | Tester | macOS | Chip | Steps passed | Failed steps | Notes |
| ---- | ------ | ----- | ---- | ------------ | ------------ | ----- |
|      |        |       |      |              |              |       |
