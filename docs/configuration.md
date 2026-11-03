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

The core reads the list when it starts. Copy the defaults into your list to keep them, as the
example does.

## Agent profiles

An `[agents.<name>]` table describes one agent. `command` is the program and its arguments,
run with no shell; it must not be empty or start with `-`. `sandbox = true` runs it in the
system's sandbox. `pinfold agent <name>` in a pane opens a split to its right running the
profile; `--down` splits below and `--tab` opens a tab. It calls the API's `agent.launch`, which
takes only a profile's name, never a program ([ADR-0036](adr/0036-launching-agent-profiles.md)).
The agent starts in the directory the pane's shell last reported with OSC 7, if any. A
sandboxed profile needs that directory, which must pass the checks below, and is refused
without it. The core reads profiles once, at start.

```toml
[agents.claude]
command = ["claude"]
sandbox = true
```

The sandbox hides `~/.ssh`, the directory of this file, PinFold's state directory, and every
other pane's terminal device, and where any of those directories, or this file, lead
through a symlink. On macOS it is `/usr/bin/sandbox-exec`, which denies reads and
writes there. On Linux it is bubblewrap, which puts an empty directory over each one, gives the
agent its own `/dev` and `/proc`, and needs unprivileged user namespaces. `bwrap` is taken from
`/usr/bin`, `/usr/local/bin` or `/bin`, never `PATH`, and only if root alone can change it and
the directories above it.
If the tool is missing or refuses, the agent does not start: it never runs unsandboxed. A
directory that does not exist when the agent starts is not hidden on Linux.

The agent stays in its pane's session with the pane as its controlling terminal, so the API
still knows it as that pane, and its socket stays reachable. On macOS the sandbox denies typing
into that terminal with `TIOCSTI`. On Linux a seccomp filter does, on x86_64 and aarch64; on
other architectures the agent does not start.

A sandboxed agent writes only to its working directory and temporary directories: a fresh,
empty `/tmp` on Linux, and `/private/tmp` and your temporary directory on macOS. Everything else
it can read but not change, so it cannot edit `~/.zshrc` or put programs on your `PATH`. The
working directory is the pane's, as its shell reports it, and the agent does not start unless
that directory is yours, writable by no other user or group, under directories only you or root
can change, not `/`, your home directory or above it, not a dot-directory of your home such as
`~/.config` or `~/.local/bin` or inside one, not in `~/Library` on macOS, not inside a `.git`
directory, not a directory on your `PATH` or above one, and does not hold or sit inside a hidden
directory. A relative `PATH` entry, such as `.` or an empty one, counts from the working
directory, so it refuses every launch, and so does a `PATH` entry that is a dangling symlink.

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

The agent gets no `SSH_AUTH_SOCK`, `GPG_AGENT_INFO` or `DBUS_SESSION_BUS_ADDRESS`, and reaches no
Unix socket but PinFold's API socket, so it cannot sign with your ssh-agent without reading
`~/.ssh`. On macOS the system's name lookup socket also stays reachable. On Linux the agent's
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

## Workspace layouts

A project may describe tabs and panes in `.pinfold/layout.toml`. PinFold validates this format
now; `pinfold up` and the trust prompt arrive separately, so it does not open the layout yet.

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
special files, and a symlink in place of `layout.toml` are refused. `run` is only data at this
stage; a later trust prompt will show it before anything can run.

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
