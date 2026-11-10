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
| `theme`               | string or table | none | A [theme](#themes) by name, or [one each](#light-and-dark) for light and dark. Without it, Graphite Grid |
| `colors`              | table  | none    | Single [colours](#themes) on top of the theme            |
| `keys.prefix`         | string | `"C-a"` | Prefix, `C-` and a lowercase letter                      |
| `api.audit_log`       | bool   | `false` | Also write the API audit log to disk                     |
| `api.protect`         | list   | below   | Command-line text that protects a pane from API sends    |
| `agents.<name>`       | table  | none    | An [agent profile](#agent-profiles)                      |

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

`background_opacity` applies only to the default background, which is the [theme](#themes)'s
`background`, and the padding around the grid.
Text and cells a program coloured stay opaque, so a program that paints its own background,
such as an editor with a colour scheme, covers the glass. A program that changes the default
background with OSC 11 changes it for its own pane: that colour is then the pane's default
background, so it is translucent as the theme's is, while the padding, the other panes and the
chrome keep the theme's. OSC 12 sets the colour of that pane's cursor, drawn under the theme's
`cursor_text`. OSC 111 and OSC 112 give the pane back the theme's background and cursor.
Prompts drawn by PinFold read only the theme and stay opaque. With a core older than the
window (link version 13), OSC 11 stays an opaque cell colour and OSC 12 does nothing.
`background_blur` does nothing
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

The core reads the list when it starts. Copy the defaults into your list to keep them, as the
example does.

## Agent profiles

An `[agents.<name>]` table describes one agent. `command` is the program and its arguments,
run with no shell; it must not be empty or start with `-`. `sandbox = true` runs it in the
system's sandbox. `pinfold agent <name>` in a pane opens a split to its right running the
profile; `--down` splits below and `--tab` opens a tab. It calls the API's `agent.launch`, which
takes only a profile's name, never a program ([ADR-0036](adr/0036-launching-agent-profiles.md)).
The agent starts in the directory the pane's shell last reported with OSC 7, or with none in
the shell's own working directory, read from the system
([ADR-0040](adr/0040-pane-directory-falls-back-to-its-shell.md)). With neither, the launch is
refused. A sandboxed profile's directory must also pass the checks below. The core reads profiles once, at start.

A profile's pane reads its CLI's own state from output: OSC 7501 `state=blocked` (codex-cli
sends it at its approval prompt) or an OSC 9 notification shows ◆ needs you, labelled from
output, and `working`, `done`, `idle` and `clear` move it on. Typing into the pane clears a
needs you. When a tab comes to need you while it is hidden or the window is in the background,
macOS bounces PinFold's Dock icon once. Once you allow PinFold's notifications in System
Settings, a banner says "<tab> needs you", never the program's own text, and clicking it goes
to the tab; macOS asks the first time and shows nothing until you allow it, and a build outside
an app bundle, such as `cargo run`, has no banners. At most one comes per tab a minute and one
every 10 seconds in all. Linux marks the tab only. A plain shell pane never reads them, so a program there cannot fake needs you
([ADR-0046](adr/0046-agent-panes-read-needs-you-from-output.md)).

```toml
[agents.claude]
command = ["claude"]
sandbox = true
keep_env = ["ANTHROPIC_API_KEY"]
```

A sandboxed agent does not get variables whose names look like secrets: any name that holds
`TOKEN`, `SECRET`, `PASSWORD`, `PASSWD`, `CREDENTIAL` or `APIKEY`, ends in `_KEY` or `_PAT`, or
starts with `AWS_`, in any case. `keep_env` names the ones it still gets, such as its own API
key: at most 16 exact names of letters, digits and `_`, not starting with a digit, and no
patterns. It does nothing without `sandbox = true`. Other variables reach the agent as before,
so a secret under a name that matches none of these, such as a password inside `DATABASE_URL`,
is not stripped. PinFold never logs a variable's value.

The sandbox hides `~/.ssh`, the directory of this file, PinFold's state directory, the common
credential stores, and every other pane's terminal device, and where any of those, or this
file, lead through a symlink ([ADR-0041](adr/0041-sandbox-hides-writes-not-secrets.md)). The
credential stores are `~/.aws`, `~/.azure`, `~/.docker`, `~/.gnupg`, `~/.kube`, `~/.netrc`,
`~/.Xauthority`, and `gcloud` and `gh` in `~/.config` and in `$XDG_CONFIG_HOME`, and the
browser profiles: on macOS
Arc, Brave, Chromium, Firefox, Google Chrome and Microsoft Edge in `~/Library/Application
Support`, and `~/Library/Safari`; on Linux `~/.mozilla`, `BraveSoftware`, `chromium`,
`google-chrome` and `microsoft-edge` in the config directories, and the Snap and Flatpak
profiles in `~/snap/firefox`, `~/snap/chromium` and `~/.var/app/` for Firefox, Chrome and
Chromium. A store not on this list stays readable, and a tool that needs a hidden one, such as
`aws` or `gh`, fails inside the sandbox. On macOS it is `/usr/bin/sandbox-exec`, which denies
reads and writes there. On Linux it is bubblewrap, which puts an empty directory over each
directory and `/dev/null` over each file, such as `~/.netrc`, gives the agent its own `/dev`
and `/proc`, and needs unprivileged user namespaces. `bwrap` is taken from
`/usr/bin`, `/usr/local/bin` or `/bin`, never `PATH`, and only if root alone can change it and
the directories above it. It must be bubblewrap 0.8 or later, which can stop the agent making
user namespaces of its own (`--disable-userns`); with an older one the launch fails and names
the version found.
If the tool is missing or refuses, the agent does not start: it never runs unsandboxed. A
path that does not exist when the agent starts is not mounted over on Linux, and the agent
cannot make it, as it writes nowhere but its working and temporary directories.

The agent stays in its pane's session with the pane as its controlling terminal, so the API
still knows it as that pane, and its socket stays reachable. On macOS the sandbox denies typing
into that terminal with `TIOCSTI`. On Linux a seccomp filter does, on x86_64 and aarch64; on
other architectures the agent does not start.

A sandboxed agent writes only to its working directory and temporary directories: a fresh,
empty `/tmp` on Linux, and `/private/tmp` and your temporary directory on macOS. Everything else
it can read but not change, so it cannot edit `~/.zshrc` or put programs on your `PATH`. The
working directory is the pane's, as its shell reports it, and the agent does not start unless
that directory is strictly inside your home folder, such as `~/src/app` or `~/Projects/app`.
It must also be yours, writable by no other user or group, under directories only you or root
can change, not a dot-directory of your home such as `~/.config` or `~/.local/bin` or inside
one, not `~/bin`, `~/Applications` or, on macOS, `~/Library` or inside them, not inside a
`.git` directory, not a directory on your `PATH` or above one, not inside the directory that
holds a `PATH` entry in your home, such as `~/go` for `~/go/bin` or `~/miniconda3`, and must not
hold or sit inside a hidden directory. A refusal says why; start the agent from a project directory
inside your home folder instead. A relative `PATH` entry, such as `.` or an empty one, counts
from the working directory, so it refuses every launch, and so does a `PATH` entry that is a
dangling symlink.

The `PATH` checked is your shell's, followed by the core's own: once, at start, the core runs
`$SHELL -i -c` as a pane's shell starts, a login shell on macOS, and reads the `PATH` it prints,
as an app started from the Dock has only `/usr/bin:/bin:/usr/sbin:/sbin`. The shell writes it
into PinFold's state directory, which the sandbox hides. A launch never waits for the shell:
until it has answered, a sandboxed launch is refused with "PinFold is still reading your shell's
PATH; try again in a few seconds", which `pinfold agent` waits out, asking again for up to 12
seconds. A shell that takes over 10 seconds is stopped, and the next launch asks it again. A
`PATH` you change later in a pane, such as with direnv, is not checked.

Git runs what a repository's hooks and config name outside the sandbox, so the agent cannot
change them. On Linux the working directory's whole `.git` is read-only, and the agent commits
into a shadow git directory of its own instead
([ADR-0039](adr/0039-shadow-git-directory.md)). The shadow is
`$XDG_DATA_HOME/pinfold/agents/<profile>/<key>`, or under `~/.local/share` when that variable
is unset, where the key is the first 16 hex digits of the SHA-256 of the working directory's
resolved path. It is made private (mode 0700), and the agent does not start if it exists as a
symlink, as another user's, or open to others. The sandbox does not hide it, so other sandboxed
agents can read but not write it; it holds only the project's data, which they can read anyway. The agent's `GIT_DIR` points at it, with `GIT_WORK_TREE` set to the
working directory. It starts, at the first launch, at your branch's commit and index, and it
reads your objects through `objects/info/alternates`, so nothing is copied. Later launches keep
it as the agent left it: delete it to start again from your current commit. It is not named
`.git`, so git never finds it on its own, and its hooks and config run only inside the sandbox.
To take the agent's commits, fetch them into remote-tracking refs:

```sh
git fetch --no-tags ~/.local/share/pinfold/agents/<profile>/<key> \
  '+refs/heads/*:refs/remotes/agent-<profile>/*'
```

A local fetch runs none of the shadow's hooks or config, and it never touches your branches.
Do not run other git commands with the shadow as their git directory, such as with `--git-dir`
or from inside it: those run the hooks and config the agent wrote.
Review `agent-<profile>/<branch>`, then merge or cherry-pick it. A working directory whose
`.git` is a file, such as a linked worktree, gets no shadow, and there the agent cannot commit.
The `.git` of each submodule that `.gitmodules` lists is read-only too, whether it is
a file pointing into `.git/modules` or a repository of its own. On macOS the agent can commit,
but it cannot write the hooks, config, worktree config, `commondir` or `gitdir` of the
repository or of any submodule or linked worktree, make them where they are missing, rewrite a
submodule's `.git`, or move `.git` or the directories that hold them. The agent does not start
if the repository or a submodule has no hooks directory or config, or links one elsewhere. On
Linux, an agent in a directory with no `.git` yet can still make one, and on both systems a
nested repository that `.gitmodules` does not list is not protected. A
`core.hooksPath` or other config setting that names a file inside the working directory, such as
a `.husky` directory, points at files the agent can change. So do other project files, such as a
`Makefile`, `.envrc` or a `package.json` script. These run with your full rights when you run
them, so review an agent's changes before you run git or the project's tools. The sandbox
keeps git from running the agent's code on its own; it does not try to guard every file in the
working directory that you might later run, and such cases stay limits of this kind (#161).

The agent gets no `SSH_AUTH_SOCK`, `GPG_AGENT_INFO`, `DBUS_SESSION_BUS_ADDRESS`, `DISPLAY`,
`WAYLAND_DISPLAY`, `WAYLAND_SOCKET` or `XAUTHORITY`, and reaches no Unix socket but PinFold's API socket, so
it cannot sign with your ssh-agent without reading `~/.ssh`. Direct access to the clipboard is
blocked: on macOS the sandbox denies looking up the pasteboard service (`com.apple.pasteboard.1`, which
`pboard` serves) and Universal Clipboard's, so `pbcopy` and `pbpaste` fail, and on Linux the X11
and Wayland sockets in `/tmp/.X11-unix` and `$XDG_RUNTIME_DIR` are out of reach, and so is
`~/.Xauthority`, X11's cookie. An X server's abstract socket stays reachable below Landlock ABI
6 (see below), so there an X server that lets your user in without a cookie, as `xhost
+si:localuser:` does, still serves the agent its clipboard. The network
stays open: the agent can connect to any host over TCP or UDP. On macOS the system's name lookup socket also stays reachable. On Linux the agent's
`$XDG_RUNTIME_DIR` and `/tmp` are empty but for the API socket's directory, which is read-only.
The agent keeps the network, so abstract sockets, such as X11's `@/tmp/.X11-unix/X0` or a
session bus on `unix:abstract=`, are not hidden by a mount. On Linux 6.12 and later, with
Landlock ABI 6, the agent cannot connect to one bound outside its sandbox
([ADR-0038](adr/0038-landlock-scopes-abstract-sockets.md)). On older kernels, or with Landlock
turned off, they stay reachable, and the core's log says so once. If the kernel has Landlock but
it fails, the agent does not start. Landlock needs `no_new_privs` before `bwrap` runs, so a
setuid `bwrap` runs without its setuid rights and needs unprivileged user namespaces like any
other. The agent also gets its own IPC, host name and cgroup namespaces, and ends when the core
does.

## Keys

Press the prefix, then one of these. The prefix pressed twice sends it to the program.

| Keys               | Does                                                         |
| ------------------ | ------------------------------------------------------------ |
| `c`                | New tab                                                      |
| `x`                | Close the pane, after Enter. Esc cancels                     |
| `n`, `p`           | Next or previous tab, wrapping                               |
| `1` to `9`, `0`    | Tab 1 to 9, and `0` for tab 10                               |
| <code>&#124;</code> or `%` | Split right                                          |
| `-` or `"`         | Split down                                                   |
| Arrows             | Move focus                                                   |
| Ctrl+arrows        | Move a border 1 cell                                         |
| Alt+arrows         | Move a border 5 cells                                        |
| `z`                | Zoom the pane                                                |
| `d`                | Close the window. The session stays, and `pinfold` reopens it |
| `f`                | Open the file manager, `pinfold files`, right of the pane     |
| `g`                | Open the git diff viewer, `pinfold diff`, right of the pane   |
| `m`                | Pick a `*.md` file, then open it in `pinfold md` right of the pane |

`f` and `g` open the tool in the directory the system says the focused pane's shell is in,
never one a program's output reported, as a new tab or split does. Tmux binds neither key,
and behind the prefix no shell or program sees them.

`m` lists the `*.md` files directly in that directory, not in folders below it, in the
palette's box: type to filter, Enter opens one, Esc closes. It lists regular files only, not
symlinks, and at most 500 names of up to 255 bytes, from the first 10,000 entries the
directory gives; past either cap the rest are not offered, so open them from `pinfold files`.
Control and bidi characters in a name show escaped, as `^J` or `<U+202E>`. PinFold opens only
a name it listed, and checks again that it is still a regular file in that directory. Tmux
marks a pane with `m`, which PinFold does not have. In a tool's pane the prefix and the
window's own keys still work, and every other key goes to the tool.

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

Right-clicking the Dock icon offers New Tab, Split Right, Split Down and Detach, then each
tab with its agent state as glyph and word, such as `● api  running`; choosing a tab goes to
it.

Bigger and Smaller last until the file is next saved. Elsewhere, Ctrl+Shift+C, V and F copy,
paste and find.

Ctrl+Shift+P, on every system, opens the command palette over the panes: type to filter
the window's commands, Up and Down choose, Enter runs one, and Esc, a click or Ctrl+Shift+P
again closes it. It offers Open Markdown, Open Files and Open Diff, which open the tools as `m`, `f` and `g` do,
and every command in the menus above. Labels whose start matches what you typed come first,
then those containing it, then those with its letters in order. While it is open every key
goes to it, and none to the program, which cannot draw over it.

Right-click in a pane opens a menu of the same commands, or Shift+right-click when the
program reports the mouse. On an OSC 8 hyperlink it also offers Open Link and Copy Link.
Plain URLs that are not OSC 8 links get neither. Open Link opens an `http` or `https` link
with a host in your browser, through `/usr/bin/open` on macOS and `xdg-open` on Linux.
PinFold never looks the opener up in `PATH`: it runs the first of `/usr/bin/xdg-open`,
`/usr/local/bin/xdg-open` and `/run/current-system/sw/bin/xdg-open` that is an executable
file, and says which paths it tried when none is. Any other scheme, or a web link with characters outside ASCII, first shows the whole link under a
`◆ needs you` border, with those characters escaped, and opens it on Enter. A link longer
than 2,048 bytes, or one that is not a URI, gets neither item.

Paste, copy, find and vi mode keys always work, and they cancel a pressed prefix. A key
that composes text in an input method never counts as the prefix.

## Themes

The theme colours the panes, history included,
the padding, the cursor, the selection and the chrome: the bar, the borders, the menus, the
palette, the prompts and the agent states. `background_opacity` and `background_blur` act on
the theme's background. A cursor or a selection whose two colours are the theme's `foreground`
and `background` inverts each cell it covers, and any other colours are drawn as they are. The
▲ mark of a core without the hardened runtime keeps its own violet under every theme. A saved
change to `pinfold.toml` or to the theme file in use applies [on save](#reload). A core older than
the window (link version 13) cannot take a palette, so the window draws Graphite Grid and
[reports](#where-problems-are-reported) that themes need the core updated. The rules are those
of [ADR-0049](adr/0049-themes.md).

```toml
theme = "My Theme"

[colors]
background = "#0d1117"

[colors.states]
needs_you = "#f5b544"
```

`theme` names a theme: 1 to 64 ASCII letters, digits, spaces and `-`, `_`, `+`, `.`, not
starting with `.`, so no name is a path. PinFold looks for `themes/<name>.toml` beside
`pinfold.toml`, then for `themes/<name>.itermcolors`, an
[iTerm2 colour preset](#iterm2-colour-presets), then for `themes/<name>` as a
[Ghostty theme](#ghostty-themes), then among the [bundled themes](#bundled-themes) and
`Graphite Grid`, the default. So a file of yours with a bundled theme's name is used in its
place. A theme file
gets the checks `pinfold.toml` gets: a regular file after links are followed, at most 64 KiB,
UTF-8, yours, and not writable by group or others, with an unknown key refused and named with
its line.

`[colors]` sets single keys on top of the named theme, or on top of Graphite Grid. A theme file
and `[colors]` take the same keys, each a string of `#` and six hex digits:

| Keys | In a theme file | When absent |
| --- | --- | --- |
| `background`, `foreground` | required | |
| `black`, `red`, `green`, `yellow`, `blue`, `magenta`, `cyan`, `white` | required | |
| `bright_black`, `bright_red`, `bright_green`, `bright_yellow`, `bright_blue`, `bright_magenta`, `bright_cyan`, `bright_white` | required | |
| `cursor` | optional | `foreground` |
| `cursor_text` | optional | `background` |
| `selection_background` | optional | `foreground` |
| `selection_foreground` | optional | `background` |
| `chrome.accent` | optional | `cyan`, or `foreground` where `cyan` contrasts under 4.5 with `chrome.tab_active` |
| `chrome.tab`, `chrome.tab_hover`, `chrome.overlay` | optional | `background` moved 6%, 14% and 4% towards `foreground` |
| `chrome.tab_active` | optional | `background` moved 22% towards `chrome.accent` if set, else `cyan` |
| `chrome.border`, `chrome.muted` | optional | `background` moved 20% and 30% towards `foreground` |
| `chrome.dim` | optional | `foreground` moved 25% towards `background` |
| `chrome.tab_text`, `chrome.key`, `chrome.title`, `chrome.title_unfocused` | optional | `foreground` moved 20%, 35%, 30% and 40% towards `background` |
| `chrome.rule`, `chrome.palette` | optional | `background` moved 12% and 4% towards `foreground` |
| `states.needs_you`, `states.running`, `states.done`, `states.error`, `states.idle` | optional | Graphite Grid's five colours |

In a theme file the last two groups are the tables `[chrome]` and `[states]`, and in
`pinfold.toml` they are `[colors.chrome]` and `[colors.states]`. A mix is done per sRGB channel
and rounded, a half to even.

A theme sets colours only. The glyph and the word of an agent state are not keys, and PinFold
holds the colours to these rules whatever the theme says:

- Each state colour is made lighter on a dark theme, or darker on a light one, in steps of 0.01
  of OKLab lightness, until it contrasts at least 4.5 with the background, the three tab
  surfaces and the overlay. A theme is dark when its background's relative luminance is under
  0.179.
- If a state still contrasts under 4.5 on one of them, if two states end up under 0.07 apart in
  OKLab, or if `chrome.muted` contrasts 3 or more with the background, the chrome and the states
  are Graphite Grid's and the panes keep the theme. A background near mid grey does this.
- A prompt's text is `foreground` where that contrasts at least 4.5 with `chrome.overlay`, and
  otherwise black or white, whichever contrasts more.
- `foreground` under 4.5 on `background` is only reported, with the ratio.

A theme that cannot be used, for its name, its file, an absent required key, or a bad key or
colour in `[colors]`, is [reported](#where-problems-are-reported) with the reason, and the
other keys of `pinfold.toml` still apply. At start PinFold then draws Graphite Grid, and on a
save it keeps the theme it has.

### Light and dark

`theme` can also be a table of two names, and PinFold then follows the system's appearance:

```toml
theme = { light = "GitHub Light", dark = "GitHub Dark" }
```

The window asks the system as it opens, before its first frame, so it starts in the matching
theme. Each time the appearance changes it applies the matching theme as it applies a
[save](#reload): the chrome and the panes are recoloured, history included. Where the system does not say, as on X11, PinFold uses `dark`. Both keys are
required, each is a name as above, and `[colors]` goes on top of whichever is in use.

A name that cannot be used is [reported](#where-problems-are-reported) for either half, the
one not in use included. If it is the half the system asks for, PinFold keeps the theme it
has, and at start that is Graphite Grid. [Reload](#reload) follows the half in use: saving
`pinfold.toml` or that half's theme file applies within about a second.

### Bundled themes

Eleven themes come with PinFold beside Graphite Grid, each chosen by its name, such as
`theme = "GitHub Dark"`. Each sets the 18 required colours and, where its source has them, the
cursor and selection colours; the chrome and the agent states are derived, and none takes
Graphite Grid's chrome. Two have text under 4.5 on their background, marked ⚠, which PinFold
[reports](#where-problems-are-reported) and leaves as their authors made it.

| Theme | Text contrast | Converted from | Licence and attribution |
| --- | --- | --- | --- |
| `Catppuccin Latte` | 7.06 | `ghostty/Catppuccin Latte` in mbadolato/iTerm2-Color-Schemes at `bc5531d8ec26`, from catppuccin/palette | MIT, Copyright (c) 2021 Catppuccin |
| `Catppuccin Mocha` | 11.34 | `ghostty/Catppuccin Mocha`, same collection, from catppuccin/palette | MIT, Copyright (c) 2021 Catppuccin |
| `Dracula` | 13.11 | `Dracula.itermcolors` in dracula/iterm at `e5749bcd0776` | MIT, Copyright (c) 2013-present Dracula Theme |
| `GitHub Dark` | 6.10 | `ghostty/GitHub Dark`, same collection, from primer/github-vscode-theme | MIT, Copyright (c) 2020 Primer |
| `GitHub Light` | 15.80 | `ghostty/GitHub Light Default`, same collection, from primer/github-vscode-theme | MIT, Copyright (c) 2020 Primer |
| `Gruvbox Dark` | 10.75 | `ghostty/Gruvbox Dark`, same collection, from morhetz/gruvbox | MIT, Copyright (c) Pavel Pertsev. Upstream has no licence file: its README says "MIT/X11" and its `package.json` MIT |
| `Nord` | 9.25 | `ghostty/Nord`, same collection, from nordtheme/iterm2 | MIT, Copyright (c) 2016-present Sven Greb |
| `One Dark` | 7.22 | `ghostty/Atom One Dark`, same collection, from atom/atom `packages/one-dark-ui` | MIT, Copyright (c) 2011-2022 GitHub Inc. |
| `Solarized Dark` | 4.75 | `xresources/solarized` in altercation/solarized at `62f656a02f93` | MIT, Copyright (c) 2011 Ethan Schoonover |
| `Solarized Light` | ⚠ 4.13 | the same file | MIT, Copyright (c) 2011 Ethan Schoonover |
| `Tokyo Night` | ⚠ 4.40 | the terminal colours of `themes/tokyo-night-color-theme.json` in tokyo-night/tokyo-night-vscode-theme at `7c0f11eaef32` | MIT, Copyright (c) 2018-present Enkia |

The seven from the collection are also under its own MIT licence, Copyright (c) 2011 to Present
Mark Badolato. `crates/pinfold-config/themes/convert.py` writes each file from its source at
that commit, with a header naming the source, commit, licence and copyright, and the licence
texts sit beside them as `LICENSE-*.txt`. Every package ships those in `theme-licenses`. The
contrasts are checked by the tests of `crates/pinfold-config/src/theme.rs`.

Dracula's background is `#1e1f29`, as its iTerm2 preset has it. Tokyo Night's selection has
alpha in its source, which a theme cannot hold, so its selection is the default inversion.
iTerm2's and Terminal.app's default colours are not bundled, as their licences do not allow it
(GPL-2.0-or-later, and none). If you have either app, copy your own
[preset](#iterm2-colour-presets) into `themes/`.

### Ghostty themes

A Ghostty theme file works as it is. Copy it into `themes/` beside `pinfold.toml` under its own
name, with no extension, and name it:

```sh
mkdir -p ~/.config/pinfold/themes
cp "/Applications/Ghostty.app/Contents/Resources/ghostty/themes/Nord" ~/.config/pinfold/themes/
```

```toml
theme = "Nord"
```

PinFold reads these keys and no others:

| Ghostty key | Theme key |
| --- | --- |
| `background`, `foreground` | the same |
| `palette = N=#rrggbb`, N from 0 to 15 | `black` to `white` for 0 to 7, `bright_black` to `bright_white` for 8 to 15 |
| `cursor-color`, `cursor-text` | `cursor`, `cursor_text` |
| `selection-background`, `selection-foreground` | `selection_background`, `selection_foreground` |

A colour is six hex digits, with or without the `#`, and may sit in double quotes. A name such
as `red`, or the short `#rgb`, is refused with its line, and so is a `palette` entry that is not
`N=colour` with N from 0 to 255. The file needs `background`, `foreground` and all 16 palette
colours; the chrome and the agent states are derived, as for any theme, and `[colors]` goes on
top.

A Ghostty theme may hold any Ghostty setting, such as `font-size` or `command`. PinFold acts on
none of them: every other key, and a palette colour from 16 to 255, is ignored, and one log
line says how many. A `themes/<name>.toml` or `themes/<name>.itermcolors` of the same name is
used first.

### iTerm2 colour presets

An iTerm2 colour preset, an `.itermcolors` file, works as it is. In iTerm2, export one from
Settings, Profiles, Colors, Color Presets, Export, or take one from a collection such as
[iTerm2-Color-Schemes](https://github.com/mbadolato/iTerm2-Color-Schemes). Copy it into
`themes/` beside `pinfold.toml`, keeping the `.itermcolors` extension, and name it without:

```sh
mkdir -p ~/.config/pinfold/themes
cp ~/Downloads/Nord.itermcolors ~/.config/pinfold/themes/
```

```toml
theme = "Nord"
```

PinFold reads these colours and no others, each from its `Red Component`, `Green Component`
and `Blue Component`, a number from 0 to 1:

| iTerm2 key | Theme key |
| --- | --- |
| `Background Color`, `Foreground Color` | `background`, `foreground` |
| `Ansi 0 Color` to `Ansi 15 Color` | `black` to `white` for 0 to 7, `bright_black` to `bright_white` for 8 to 15 |
| `Cursor Color`, `Cursor Text Color` | `cursor`, `cursor_text` |
| `Selection Color`, `Selected Text Color` | `selection_background`, `selection_foreground` |

The file needs the background, the foreground and all 16 ANSI colours; the chrome and the agent
states are derived, as for any theme, and `[colors]` goes on top. Every other key is ignored,
including any `(Light)` and `(Dark)` variant of a colour. Components are read as
sRGB: for a colour whose `Color Space` is another, such as `P3`, one log line says how many, and
the colour may look slightly different from iTerm2's.

The file must be the XML property list iTerm2 writes, and PinFold reads it with a small parser
of its own: only the elements `plist`, `dict`, `key`, `real`, `integer` and `string`, at most
four levels and 4096 elements, and only the entities `&amp;`, `&lt;`, `&gt;`, `&quot;` and
`&apos;`. The `DOCTYPE` line is skipped and nothing is fetched. Anything else, such as an
`array`, a comment, a `DOCTYPE` that declares entities, a missing component or one outside 0 to
1, is refused with its line. A `themes/<name>.toml` of the same name is used first.

## Reload

Every key applies on save, with no restart. The window keeps its size, so a new font or font
size changes how many rows and columns fit, and programs get a resize. Editors that write the
file in place and editors that save by rename both work, and so does a symlink.

A save gets the same checks as startup. If it has any of the problems above, PinFold says
why in its [log](#where-problems-are-reported), once, and keeps the current settings rather than the defaults, until a later
save or `chmod` fixes it. A bad `font_path` file means the bundled font, as it does at startup.
Deleting the file changes nothing until the next start.

The theme reloads with the rest: saving `pinfold.toml` or the theme file in use recolours the
chrome and the panes, history included, within about a second. A theme file that a save breaks
is reported once, and the theme in use stays until a later save fixes it. A theme file created
after `theme` named it is picked up at the next save of `pinfold.toml`.

PinFold checks `pinfold.toml` and the theme file in use with one `stat` each a second on a
thread of its own, so an idle window wakes once a second for this. Changes to the font file
itself are not noticed.

## Workspace layouts

A project may describe tabs and panes in `.pinfold/layout.toml`. `pinfold up [dir]` opens it,
from the current directory if no `dir` is given, and a second run focuses its first open tab.

```toml
[[tabs]]
[[tabs.panes]]
cwd = "."
run = "cargo test"

[[tabs.panes]]
split = "right"
cwd = "crates/pinfold-core"
run = "cargo test -p pinfold-core"
```

The root contains only `tabs`; each tab contains only `panes`; and a pane may contain only
`split`, `cwd`, and `run`. `split` is `right` or `down` and is omitted for the first pane. A
relative `cwd` starts at the project directory. Every supplied `cwd` must already exist as a
directory and resolve inside the project, including after following directory symlinks.

A layout must have at least one tab and one pane in each tab. It may hold at most 32 tabs and 64
panes in total, and the file may be at most 64 KiB. Unknown keys, invalid values, non-UTF-8,
special files, and a symlink in place of `layout.toml` are refused.

A `run` line is typed into its pane's shell only once you trust the layout. The first time, and
whenever the file changes, the window asks, showing the directory and every `run` line. `y`
trusts it. `n` or Esc opens the layout as plain shells; `pinfold up` then says how many lines did
not run and exits 1. The [API reference](api/README.md) has the details.

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
on macOS. The file is private to you and starts empty each time a core starts. The last core's
log, if it holds anything, moves to `default.log.1` first, so a crashed core's panic line outlives
the next launch. Those two files are the only limit on its size. A failed `pinfold` names both. A core started some other way, as from a terminal,
writes to its own stderr instead. A config problem quotes the bad line of the file, and a panic
prints only its place in the code.
