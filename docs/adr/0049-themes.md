# ADR-0049: Themes are data the window resolves and the core is told

- **Status:** Proposed
- **Date:** 2026-11-10

## Context

PinFold's colours are compiled in ([#350]). The pane colours are `BACKGROUND`, `FOREGROUND` and
`ANSI` in `crates/pinfold-core/src/vt.rs`, under a `shortcut:` note that the design has no ANSI
palette. The window's ground is `pinfold_proto::DEFAULT_BACKGROUND`, a copy of the core's, under
its own `shortcut:` note. The chrome is some three dozen constants spread over `pinfold-ui`
(`top_bar.rs`, `palette.rs`, `context_menu.rs`, `search_bar.rs`, `paste.rs`, `approval.rs`,
`panes.rs`, `was_running.rs`).

What the code does today, which this decision builds on:

- The core resolves colours. A cell on the link carries `0x00RRGGBB`, and the grid and the
  scrollback store keep the named, indexed or direct colour a program asked for
  (`AlacrittyEngine::colour`, `scrollback.rs` `put_color`). So a palette change recolours
  history as well as the screen.
- OSC 4, 10 and 11 already override per pane, in alacritty_terminal's table of overrides, and
  104, 110 and 111 clear them. OSC 12 is stored and never drawn: the window draws the cursor by
  inverting a cell. An OSC 11 background is drawn as opaque cells, because the renderer knows
  the default background only by comparing with the constant (`docs/configuration.md`).
- Only the window watches `pinfold.toml`, once a second (`pinfold-ui/src/reload.rs`). The core
  reads it once, at start.
- The link is at version 13, and a window serves a core one version older (ADR-0043).
- The built-in tools draw in the 16 ANSI roles (`pinfold-tools/src/screen.rs`), so a palette
  reaches them with no change.

Config is untrusted input (`docs/design/threat-model.md`), and the core must not panic on it or
gain parsers it can do without. Agent states are always glyph, colour and word, and prompts
PinFold draws sit outside pane text (CLAUDE.md, UI). All UI is cells in one font (ADR-0005,
ADR-0011), so a theme changes colours and nothing else.

`docs/spikes/themes/` holds the script behind every contrast number below, and the commits the
licences were read at.

## Decision

### 1. A theme is 18 required colours and 17 optional ones

One schema, `pinfold_config::theme::Colors`, serves a theme file and the `[colors]` table. Each
value is a string of `#` and six hex digits. There is no alpha, no colour name and no other
kind of key.

| Keys | In a theme file | When absent |
| --- | --- | --- |
| `background`, `foreground` | required | |
| `black`, `red`, `green`, `yellow`, `blue`, `magenta`, `cyan`, `white`, and each with `bright_` before it | required | |
| `cursor` | optional | `foreground` |
| `cursor_text` | optional | `background` |
| `selection_background`, `selection_foreground` | optional | `foreground` and `background`, which is today's inversion |
| `[chrome]`: `accent`, `tab`, `tab_hover`, `tab_active`, `overlay`, `border`, `dim`, `muted` | optional | derived, below |
| `[states]`: `needs_you`, `running`, `done`, `error`, `idle` | optional | Graphite Grid's five colours |

The 256-colour cube and the grey ramp stay computed and are not keys.

**Derived chrome.** A mix is done per sRGB channel and rounded. `tab`, `tab_hover` and `overlay`
are the background moved 6%, 14% and 4% towards the foreground. `tab_active` is the background
moved 22% towards the accent. `border` and `muted` are the background moved 20% and 30% towards
the foreground, and `dim` is the foreground moved 25% towards the background. `accent` is
`cyan`, unless it contrasts under 4.5 with `tab_active`, when it is `foreground`. The shares
follow Graphite Grid, whose hover surface `#2B323B` sits 12% to 19% of the way from its ground
to its text, by channel. So a plain terminal theme is enough on its own.

**Graphite Grid** stays the default and is a theme like any other, built in, with every chrome
key set to today's constants, so the default window does not change by a pixel. Its ANSI
colours are the 16 now in `vt.rs`, recorded in `docs/design/ui/` as the design's palette, which
retires that `shortcut:`. Where a constant has no key among the eight above, the slice adds
the key, and the golden images are the test.

### 2. Config

```toml
theme = "GitHub Dark"

[colors]
background = "#0d1117"

[colors.states]
needs_you = "#f5b544"
```

- **`theme`** names a theme. A name is 1 to 64 characters of ASCII letters, digits, space, `-`,
  `_`, `+` and `.`, and does not start with `.`, so no name is a path. PinFold looks in the
  user's themes directory first, then among the bundled themes. Without the key the theme is
  Graphite Grid.
- **User themes** are `themes/<name>.toml` beside `pinfold.toml`: `~/.config/pinfold/themes/`,
  or under `$XDG_CONFIG_HOME`. A theme file passes the checks `pinfold.toml` does: a regular
  file after links are followed, at most 64 KiB, UTF-8, owned by the user and not writable by
  group or others, with unknown keys refused and named with their line. The sandbox already
  hides the whole config directory from a sandboxed agent (ADR-0041), and that covers `themes/`.
- **`[colors]`** overrides single keys on top of the named theme, or on Graphite Grid.
- **Problems.** A theme that cannot be used is said in the log with the reason, as config
  problems are. At start PinFold then uses Graphite Grid, and on a reload it keeps the theme it
  has. The other keys of `pinfold.toml` still apply.
- **Live reload.** The window's watch also `stat`s the theme file in use, one more `stat` each
  second, so saving either file applies within about a second.
- **Light and dark.** `theme = { light = "GitHub Light", dark = "GitHub Dark" }` follows the
  system's appearance, read from winit (`Window::theme` and `ThemeChanged`). Where the system
  does not say, PinFold uses `dark`.

### 3. The window resolves a theme, and the core is told 18 colours

| Option | Why not |
| --- | --- |
| The core reads the theme | Puts theme files, and later the Ghostty and iTerm2 parsers, in the only long-lived process, and gives it a second file to watch. |
| Cells carry a palette index and the window resolves | Changes every cell and every frame on the link, which has no safe degradation, and the window would need each pane's OSC overrides too. |
| **The window resolves and sends the core a palette (chosen)** | One small request. The core gains no parser and no file access. |

- `pinfold_proto::Palette` is the background, the foreground and the 16 ANSI colours. The
  window loads the theme, applies `[colors]`, runs the checks of sections 4 and 5, and sends
  `Request::Palette` after its hello and after each change.
- The core keeps **one palette**, not one per pane and nothing per cell. `vt.rs` resolves
  named and indexed colours through it, where it reads its constants today. A new palette marks
  every pane changed, and the next frames carry the new colours, history included. Until the
  first request the core uses Graphite Grid, so a pane an agent opens before any window has
  the default, and one opened later has the last palette sent.
- The window keeps **one theme**: the palette, the cursor and selection colours, the chrome and
  the states. Every chrome constant becomes a read from it. The renderer takes the background
  from it for the clear, the padding and the test for the default background, so
  `background_opacity` and `background_blur` work under any theme and the padding matches. A
  cell is default background when its colour equals the theme's, as it equals the constant
  today. A frame already on its way when the theme changes can show the old background opaque
  for one frame.
- **The link goes to 14.** Both messages carry `since = 14`, and ADR-0043 gets this row:

  | Change in 14 | Spoken version 13 |
  | --- | --- |
  | `Request::Palette` and `Notice::Colours` | The window never sends the first and the core never sends the second. The window draws everything in Graphite Grid whatever `theme` says, and says in its log that themes need the core updated. OSC 11 and 12 behave as before. |

  From 14, `OLDEST` is 13, so a version 12 core restarts the session.

### 4. OSC colours stay per pane

- OSC 4, 10 and 11 override in the pane that sent them and never touch the palette, another
  pane or the chrome. OSC 104, 110 and 111 clear the override, so the theme shows again,
  including a theme chosen since.
- OSC 12 and OSC 11 need the window to know one pane's cursor colour and default background.
  `Notice::Colours` carries those two optional colours for a pane, and the core sends it when
  either changes or is reset by OSC 112 or 111. The window then draws that pane's cursor in the
  colour, and treats that pane's OSC 11 colour as its default background, so it gets the glass
  too. That retires the `shortcut:` on `DEFAULT_BACKGROUND`. The padding keeps the theme's
  background, as it belongs to the window and not to a pane.
- Which colour queries the core answers does not change here.

### 5. Agent states: themed, stepped until legible, and checked apart

Fixed colours cannot work. Graphite Grid's five, as they are, fall under 4.5 on a chrome
surface of 10 of the 11 other themes to bundle (1.26 on Catppuccin Latte, 3.05 on Dracula), and
only GitHub Dark passes (`docs/spikes/themes/README.md`). So each state colour starts from
`[states]`, or from Graphite Grid's, and the window runs this when it loads a theme:

1. **Surfaces.** Every ground a state is drawn on: `background`, `tab`, `tab_hover`,
   `tab_active` and `overlay`.
2. **Ground.** The theme is dark when the background's WCAG relative luminance is under 0.179,
   where black and white text contrast equally.
3. **Step.** While a state's WCAG contrast is under **4.5** on any surface, move its OKLab
   lightness by 0.01, up on a dark theme and down on a light one, keeping `a` and `b`, and
   clamp each sRGB channel. Stop at lightness 0 or 1. That is at most 100 steps a state.
4. **Check.** Every state contrasts at least 4.5 on every surface, and every two of the five
   are at least **0.07** apart in OKLab (Euclidean distance).
5. **Fallback.** If the check fails, the chrome and the states are Graphite Grid's, the panes
   keep the theme, and the log says which pair or surface failed with the measured number.
   Graphite Grid passes its own check, at 4.51 and 0.122, so the fallback always holds.

The script runs the first four steps over all 12 themes: all pass, the lowest contrast is 4.50
and the closest pair is 0.076, running and idle on Dracula and on Nord. 0.07 is set just under
that. At 0.08 both would fall back, and Graphite Grid's own closest pair is 0.122. A ground near
mid grey fails and takes the fallback, as the script's last two rows show.

A theme sets colours only. The glyph and the word of a state are not keys, so no theme can
remove them, and they still tell the states apart for someone who cannot tell the colours
apart. The ▲ mark of ADR-0044 takes the same step and joins the distance check in the slice
that measures it.

**Text.** Foreground on background under 4.5 warns in the log with the ratio, and changes
nothing: a terminal theme's text colours are its author's, and programs pick their own anyway.
Two themes to bundle warn, Solarized Light at 4.13 and Tokyo Night at 4.40, and
`docs/configuration.md` will say so beside each.

### 6. Prompts stay PinFold's own under any theme

What makes a prompt PinFold's is structure, and a theme has no key for structure: the window
composes a prompt after pane cells, under a rule that names PinFold ("── PinFold approval"),
and mutes pane text while one is up (`approval.rs`). The colours are held to these rules:

- A prompt reads only the window's theme, never a pane's OSC colours.
- Its ground is `overlay`. Its text is `foreground` if that contrasts at least 4.5 with
  `overlay`, and otherwise black or white, whichever contrasts more. So prompt text is forced
  legible where pane text only warns.
- Its rule and title are in the needs-you colour, which section 5 holds to 4.5 on `overlay`.
- While a prompt is up, pane text is drawn in `muted`, which must contrast under 3 with
  `background`. So the prompt is the most legible text in the window.
- If a theme breaks any of these, it takes section 5's fallback.

A program can still paint any colour in its own pane, as it can today. The defence against a
fake prompt remains that pane output cannot draw over chrome or answer for it.

### 7. Bundled themes: converted from MIT sources, one file each

Vendoring the collection whole is ruled out by its own records. iTerm2-Color-Schemes is MIT
(Copyright (c) 2011 to Present Mark Badolato), and its `licenses.yml` gives each scheme's own
licence: of 916 entries, 593 are `unknown` and 18 `nonfree`. So PinFold bundles a named
subset: each file converted by a committed script from a pinned commit of a source whose
licence is MIT, with a header naming the source, commit, licence and copyright. The licence
texts sit beside the files, as `OFL.txt` sits beside the bundled font, and ship the same way.
The files are `include_str!` text, and only the theme in use is parsed.

The repository is MIT OR Apache-2.0, and `deny.toml` allows only those two. All read on
2026-11-10:

| Theme | Bundled from | Licence, read from the source | Bundled |
| --- | --- | --- | --- |
| GitHub Dark | `GitHub Dark` in iTerm2-Color-Schemes at `bc5531d8ec26`, which gives primer/github-vscode-theme | MIT in `licenses.yml`; MIT, Copyright (c) 2020 Primer | yes |
| GitHub Light | `GitHub Light Default`, same source | the same | yes |
| One Dark | `Atom One Dark`, which gives atom/atom `packages/one-dark-ui` | MIT in `licenses.yml`; MIT, Copyright (c) 2011-2022 GitHub Inc. | yes |
| Nord | `Nord`, which gives nordtheme/iterm2 | MIT in `licenses.yml`; MIT, Copyright (c) 2016-present Sven Greb | yes |
| Catppuccin Mocha, Catppuccin Latte | the schemes of those names, which give catppuccin/palette | MIT in `licenses.yml`; MIT, Copyright (c) 2021 Catppuccin | yes |
| Gruvbox Dark | `Gruvbox Dark`, which gives morhetz/gruvbox | MIT in `licenses.yml`. Upstream has no licence file: its README says "MIT/X11" and its `package.json` says `"license": "MIT"`, author Pavel Pertsev | yes, with the MIT text under the author's name from `package.json`. The coordinator should confirm this one. |
| Dracula | `Dracula.itermcolors` in dracula/iterm at `e5749bcd0776` | MIT, Copyright (c) 2013-present Dracula Theme. The collection's own `Dracula` is `unknown`, so it is not the source | yes |
| Solarized Dark, Solarized Light | `iterm2-colors-solarized/` in altercation/solarized at `62f656a02f93` | MIT, Copyright (c) 2011 Ethan Schoonover. The collection's `iTerm2 Solarized` pair is GPL-2.0-or-later and `Solarized Dark Patched` is `unknown`, so they are not the source | yes |
| Tokyo Night | the terminal colours of `themes/tokyo-night-color-theme.json` in tokyo-night/tokyo-night-vscode-theme at `7c0f11eaef32` | MIT, Copyright (c) 2018-present Enkia. The collection's `TokyoNight` schemes are Apache-2.0, from the folke/tokyonight.nvim port; the MIT original keeps every bundled theme under one licence | yes |
| iTerm2's defaults | `iTerm2 Default`, `iTerm2 Dark Background` | GPL-2.0-or-later in `licenses.yml`, from iTerm2's `ColorPresets.plist` | **no**: not compatible with MIT OR Apache-2.0 |
| Terminal.app's defaults | `Terminal Basic`, `Terminal Basic Dark` | `nonfree` in `licenses.yml`: Apple's, with no licence | **no** |

So 11 themes are bundled beside Graphite Grid, and [#350]'s "Terminal.app/iTerm2 defaults" are
left out. A user who has either app can import its colours from their own copy (section 8).

### 8. Import: two bounded parsers, in the window, with no new dependency

A user theme may also be a Ghostty theme file or an iTerm2 `.itermcolors` file. For a name,
PinFold tries `themes/<name>.toml`, then `<name>.itermcolors`, then `<name>` as a Ghostty file.
Each passes the file checks of section 2, yields the same `Colors`, and is then checked like any
theme. Both parsers live in `pinfold-config`, are called only by the window, take any bytes
without panicking, and each has a fuzz target.

- **Ghostty.** Lines of `key = value`. PinFold reads `background`, `foreground`,
  `cursor-color`, `cursor-text`, `selection-background`, `selection-foreground` and
  `palette = N=#rrggbb` for N from 0 to 15, with or without the `#`. Every other key is
  ignored, and counted in one log line: a Ghostty theme may hold any Ghostty setting, and
  PinFold must act on none of them. A named colour such as `red` is refused.
- **iTerm2.** An XML property list of at most four levels. A hand-written parser reads only
  `plist`, `dict`, `key`, `real`, `integer` and `string`, skips the `DOCTYPE` without reading
  it, knows the five predefined entities and no others, and refuses anything deeper or
  different. It takes `Ansi 0 Color` to `Ansi 15 Color`, `Background Color`, `Foreground
  Color`, `Cursor Color`, `Cursor Text Color`, `Selection Color` and `Selected Text Color`,
  each from its `Red`, `Green` and `Blue Component`, which must be finite and from 0 to 1.
  shortcut: components are read as sRGB whatever `Color Space` says, and the log says so for
  another space. Convert P3 when a user's theme shows the difference.

  | Parser for `.itermcolors` | Checked on crates.io on 2026-11-10 | Verdict |
  | --- | --- | --- |
  | `plist` 1.10.3 | MIT, released 2026-10-21. Needs `quick-xml`, `base64`, `indexmap` and `time`. `time` is not in `Cargo.lock`, and `quick-xml` is there only under `wayland-scanner`. Also reads binary property lists and dates | No: a crate and its tree in the window for six element names |
  | `roxmltree` 0.21.2 | MIT OR Apache-2.0, released 2026-10-24, forbids `unsafe`, needs only `memchr` | No: it refuses any `DOCTYPE` unless `allow_dtd` is set, which every `.itermcolors` file has, and its node limit is off by default |
  | **Our own parser** | No crate, as for the unified-diff parser and the key decoder (ADR-0045) | Chosen, with a fuzz target |

Alacritty and Kitty files are not read. [#350] marks them optional, and a Ghostty or iTerm2
copy exists for nearly every theme.

### 9. Memory

No budget changes, and no row of `cargo xtask mem-gate` should move beyond its run-to-run
spread. By arithmetic from the types above, to be replaced by the measured table in each slice:

- Core: one `Palette` of 18 colours, 72 bytes, whatever the number of panes. "Each idle pane"
  and "Core, 8 idle panes" should not change: a pane's OSC overrides already live in
  alacritty_terminal's table.
- Window: one theme of 35 colours, 140 bytes. The "Window on macOS" and "Window on X11" rows
  should not change. Cells stay 12 bytes.
- Binary: 11 theme files of about 0.5 KB each as read-only text (the Ghostty file for GitHub
  Dark is 475 bytes), and their licence texts are not in the binary.

Slices 1, 3 and 5 include the table.

### 10. Slices

Each is one issue and one PR of about 400 changed lines, in this order.

1. [#397]: Link 14 and the core's palette: `Palette`, `Request::Palette` and `Notice::Colours`
   with the ADR-0043 row; `vt.rs` resolves through the palette, which retires its `shortcut:`.
   Depends on nothing.
2. [#398]: The theme model in `pinfold-config`: `Colors`, `theme`, `[colors]`, the user themes
   directory, derived chrome and the checks of sections 5 and 6; `docs/design/ui/` and
   `docs/configuration.md`. Depends on nothing, and can go beside slice 1.
3. [#399]: The window draws from the theme: chrome, renderer background, cursor and selection,
   the palette sent at start, and the version 13 path; golden images of the default unchanged.
   Depends on 1 and 2.
4. [#400]: Live reload of the theme, and `Notice::Colours` in the window for OSC 11 and 12.
   Depends on 3.
5. [#401]: The 11 bundled themes with their licences, the per-theme legibility and prompt tests,
   and golden images of one light and one dark theme. Depends on 3.
6. [#402]: Ghostty theme import and its fuzz target. Depends on 2.
7. [#403]: iTerm2 `.itermcolors` import and its fuzz target. Depends on 2.
8. [#404]: `theme = { light, dark }` following the system. Depends on 4 and 5.

## Consequences

- A user picks a theme by name, and it reaches agent panes, the padding and the glass, which
  the OSC script could not. Themes written for Ghostty and iTerm2 work as they are.
- The default window is unchanged, and Graphite Grid's ANSI colours are now design, not a
  stand-in.
- The core gains 72 bytes and one request, and no parser. A bad theme file can at worst end
  the window, never a PTY.
- State colours differ a little from theme to theme, since they are stepped to stay legible.
  The glyph and the word do not change. A theme near mid grey gets Graphite Grid's chrome
  around its own panes.
- An update from link 13 to 14 keeps the session, with themes off until the core is updated.
- PinFold keeps a licence record per bundled theme, and adding one means reading its licence
  first. The iTerm2 and Terminal.app defaults are not bundled.
- We own a small XML parser, and its fuzz target.
- Revisit if 0.07 lets two states read alike in a theme people use, if users want the state
  colours fixed whatever the cost in contrast, if a P3 theme looks wrong, or if a second window
  per core arrives, when "one palette per core" must become one per window.

[#350]: https://github.com/0xPiranhaCodes/pinfold.sh/issues/350
[#397]: https://github.com/0xPiranhaCodes/pinfold.sh/issues/397
[#398]: https://github.com/0xPiranhaCodes/pinfold.sh/issues/398
[#399]: https://github.com/0xPiranhaCodes/pinfold.sh/issues/399
[#400]: https://github.com/0xPiranhaCodes/pinfold.sh/issues/400
[#401]: https://github.com/0xPiranhaCodes/pinfold.sh/issues/401
[#402]: https://github.com/0xPiranhaCodes/pinfold.sh/issues/402
[#403]: https://github.com/0xPiranhaCodes/pinfold.sh/issues/403
[#404]: https://github.com/0xPiranhaCodes/pinfold.sh/issues/404
