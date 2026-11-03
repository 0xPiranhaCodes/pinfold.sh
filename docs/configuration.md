# Configuration

PinFold reads one TOML file at startup, on Linux and macOS alike:
`$XDG_CONFIG_HOME/pinfold/pinfold.toml`, or `~/.config/pinfold/pinfold.toml` when that
variable is unset. Saving the file applies it to the running window within about a second.

| Key                   | Type   | Default | Meaning                                                  |
| --------------------- | ------ | ------- | -------------------------------------------------------- |
| `font_size`           | number | `13`    | In points, from 6 to 72                                  |
| `font_path`           | string | none    | Absolute path to a monospace font file to use            |
| `macos_option_as_alt` | bool   | `false` | Option sends Alt instead of composing text. macOS only   |
| `background_opacity`  | number | `1`     | Default background opacity, from 0 to 1. macOS only      |
| `background_blur`     | bool   | `false` | Blur what shows through the background. macOS only       |
| `keys.prefix`         | string | `"C-a"` | Prefix, `C-` and a lowercase letter                      |
| `api.audit_log`       | bool   | `false` | Also write the API audit log to disk                     |
| `api.protect`         | list   | below   | Command-line text that protects a pane from API sends    |

```toml
font_size = 14
font_path = "/System/Library/Fonts/Menlo.ttc"
macos_option_as_alt = true
background_opacity = 0.85
background_blur = true

[keys]
prefix = "C-b"
```

The core reads `api.audit_log` when it starts. Without it the [audit log](api/README.md) is
kept only in memory. With it the log is also written, mode 0600, to `audit.log` in
`~/Library/Application Support/PinFold` on macOS, and elsewhere in `$XDG_STATE_HOME/pinfold`,
or `~/.local/state/pinfold` when that variable is unset. At 5 MiB it moves to `audit.log.1`,
replacing the older one. If the file cannot be opened, the core says why in its log and keeps
the log in memory. `pinfold ctl log --verify <file>` checks it.

`font_path` takes a TrueType or OpenType file, or the first font in a collection. PinFold
draws bold and italic by thickening and slanting that one font, not from separate files, and
without `font_path` it uses the bundled JetBrains Mono. A font file is treated as untrusted:
if it cannot be read, is damaged, or cannot draw all of ASCII, PinFold says why in its
[log](#where-problems-are-reported) and uses the bundled font. `~` is not expanded.

Missing characters use the bundled font, the standard CJK fallbacks, then fonts installed
in the usual user and system font directories. Installed Nerd Fonts and symbol fonts are
tried first within that last group, so you can keep a text font such as Consolas and still
use file and status icons. Fallback glyphs fit the selected font's cells; they do not change
the terminal's spacing. Install a font containing the symbols your shell uses if none of
your fonts provides them. Restart the window after installing a new fallback font.

Discovery is lazy, bounded to 4,096 directory entries and eight nested levels, and skips
directory symlinks and special files. It uses `~/Library/Fonts`, `/Library/Fonts`, and
`/System/Library/Fonts` on macOS; on Linux it uses `~/.fonts`,
`$XDG_DATA_HOME/fonts` (default `~/.local/share/fonts`), `/usr/local/share/fonts`, and
`/usr/share/fonts`. Rasterization remains outline-based; this does not add colour emoji.

`background_opacity` applies only to the default background and the padding around the grid.
Text and cells a program coloured stay opaque, so a program that paints its own background,
such as an editor with a colour scheme, covers the glass. A program that changes the default
background with OSC 11 gets that colour as an opaque cell colour. `background_blur` does nothing
at opacity 1. Its radius is fixed at 80, because winit sets it through a private CoreGraphics
call that takes no radius from callers, and macOS may change that call.

On Linux, and on a Mac with no Metal device, the window draws with the CPU and stays opaque:
PinFold accepts both keys and ignores them.

Every key is optional. PinFold always opens: if the file has a problem, it says why in
its [log](#where-problems-are-reported) and uses the defaults for every key. That covers an unknown key or a bad value
(named with its line), a file that is not UTF-8 or is over 64 KiB, a directory in the
file's place, a file it cannot read, and a file that another user owns or that group or
others can write (the rule from the threat model). A symlink is followed, and those
checks apply to its target.

## Protected panes

A pane refuses `pane.send` from the API, even with a grant, while any process in its
foreground process group is `ssh`, `mosh`, `mosh-client`, `sudo`, `su` or `doas`, runs as
root, cannot be read, or matches an `api.protect` pattern, or while the terminal reads a
password (echo off with input in lines).

A pattern is words separated by spaces. The first must be the program's file name, the last
part of its path, and each next word must be one of the program's later arguments, in order,
with others allowed between. So `"kubectl exec"` matches `kubectl -n x exec pod` and
`/usr/local/bin/kubectl exec pod`, but not `kubectl get exec-logs` or `/tmp/notkubectl exec`.
The default list is `["kubectl exec", "docker exec", "psql", "aws ssm"]`, and a list you set
replaces it. An empty pattern is refused as a bad value.

```toml
[api]
protect = ["kubectl exec", "docker exec", "psql", "aws ssm", "mysql -h prod-db"]
```

The window checks `api.protect` now, but the core does not read this file yet, so until it
does the default list applies.

## Keys

Press the prefix, then one of these. The prefix pressed twice sends it to the program.

| Keys               | Does                                                         |
| ------------------ | ------------------------------------------------------------ |
| `c`                | New tab                                                      |
| `x`                | Close the pane, after Enter. Esc cancels                     |
| `n`, `p`           | Next or previous tab, wrapping                               |
| `0` to `9`         | Tab at that index, from `0`                                  |
| <code>&#124;</code> or `%` | Split right                                          |
| `-` or `"`         | Split down                                                   |
| Arrows             | Move focus                                                   |
| Ctrl+arrows        | Move a border 1 cell                                         |
| Alt+arrows         | Move a border 5 cells                                        |
| `z`                | Zoom the pane                                                |
| `d`                | Close the window. The session stays, and `pinfold` reopens it |

A tab closes with its last pane, and the session ends with its last tab.

On macOS the menu bar runs the same commands, and its keys are the Command shortcuts. Items
that cannot apply, such as Copy with nothing selected, are greyed out, and their keys do
nothing.

| Menu    | Items                                                                       |
| ------- | --------------------------------------------------------------------------- |
| PinFold | About, Settings `⌘,` (opens this file), Hide `⌘H`, Quit `⌘Q` (closes the window, as `d` does) |
| Shell   | New Tab `⌘T`, Split Right `⌘D`, Split Down `⇧⌘D`, Close Pane `⌘W`, Detach   |
| Edit    | Copy `⌘C`, Paste `⌘V`, Select All `⌘A`, Find `⌘F`                           |
| View    | Zoom Pane, Bigger `⌘=`, Smaller `⌘-`, Actual Size `⌘0`, Toggle Full Screen  |
| Window  | Minimize `⌘M`, Next Tab `⇧⌘]`, Previous Tab `⇧⌘[`, Tab 1 to 9 `⌘1`–`⌘9`      |
| Help    | PinFold Help `⌘?` (opens this page)                                         |

Bigger and Smaller last until the file is next saved. Elsewhere, Ctrl+Shift+C, V and F copy,
paste and find.

Paste, copy, find and vi mode keys always work, and they cancel a pressed prefix. A key
that composes text in an input method never counts as the prefix.

## Reload

Every key applies on save, with no restart. The window keeps its size, so a new font or font
size changes how many rows and columns fit, and programs get a resize. Editors that write the
file in place and editors that save by rename both work, and so does a symlink.

A save gets the same checks as startup. If it has any of the problems above, PinFold says
why in its [log](#where-problems-are-reported), once, and keeps the current settings rather than the defaults, until a later
save or `chmod` fixes it. A bad `font_path` file means the bundled font, as it does at startup.
Deleting the file changes nothing until the next start.

PinFold checks the file with one `stat` a second on a thread of its own, so an idle window
wakes once a second for this. Changes to the font file itself are not noticed.

## Vi mode

Command+X on macOS, or Ctrl+Shift+X elsewhere, turns vi mode on at the program's cursor, and
off again. While it is on, nothing you do reaches the program: no keys, pastes, input method
text or mouse events. The program keeps running, and the row under the pane says `VI` or
`VISUAL`. Vi mode has no config keys.

| Keys                        | Action                                          |
| --------------------------- | ----------------------------------------------- |
| `h` `j` `k` `l`, arrows     | Move a cell                                     |
| `0` `^` `$`, Home, End      | Start of the row, its first text, its last text |
| `w` `e` `b`                 | Next word, end of the word, previous word       |
| `H` `M` `L`                 | Top, middle and bottom of the view              |
| `gg` `G`                    | Oldest row held, newest row                     |
| Ctrl+`u`, Ctrl+`d`          | Half a view up, down                            |
| Ctrl+`b`, Ctrl+`f`, paging  | A view up, down                                 |
| `v` `V`                     | Select cells, select whole rows. Again to stop  |
| `y`                         | Copy the selection and drop it                  |
| `/`                         | Open the search bar. A match moves the cursor   |
| Esc, `q`                    | Leave for the live screen. Esc drops a selection first |

The cursor stays on its text as output scrolls it, and the next motion brings it back into view.
Word motions read the rows in view, so at the view's edge they stop on the next row. On the
alternate screen, or under a program that reports the mouse, the view cannot leave the screen,
so neither can the cursor. A copy stops at 2 MB, as any selection does.

## Where problems are reported

A core that `pinfold` starts, and each window it opens, writes problems to `default.log` in the
runtime directory: `$XDG_RUNTIME_DIR/pinfold`, or `$TMPDIR/pinfold-<uid>` where that is unset, as
on macOS. The file is private to you and starts empty each time a core starts, which is the only
limit on its size. A failed `pinfold` names it. A core started some other way, as from a terminal,
writes to its own stderr instead. A config problem quotes the bad line of the file, and a panic
prints only its place in the code.
