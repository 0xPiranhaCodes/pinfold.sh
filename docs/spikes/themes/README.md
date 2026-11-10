# Themes: contrast of text and agent states

Evidence for [ADR-0049](../../adr/0049-themes.md), computed on 2026-11-10 with Python 3 and no
packages. `contrast.py` holds the background, foreground and ANSI 6 of each theme the ADR
proposes to bundle, and applies the ADR's rules to them:

- **Unstepped:** the lowest contrast of the five Graphite Grid state colours, as they are, on
  any chrome surface.
- **Text:** the WCAG 2 contrast ratio of foreground on background, which warns under 4.5.
- **Ground:** dark when the background's relative luminance is under 0.179, where black and
  white text contrast equally.
- **States:** the five Graphite Grid state colours (needs you, running, done, error, idle),
  each with its OKLab lightness moved in steps of 0.01, away from the background, until it
  contrasts 4.5 with every chrome surface: the background, the overlay and the two tab
  surfaces mixed 4%, 6% and 14% towards the foreground, and the active tab mixed 22% towards
  the accent. Graphite Grid sets its own surfaces.
- **Closest two states:** the smallest OKLab distance between any two of the five, which must
  be at least 0.07.

```sh
python3 docs/spikes/themes/contrast.py
```

| Theme | Text | Ground | Unstepped | States | Lowest state contrast | Closest two states | Result |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Graphite Grid | 14.83 | dark | 4.51 | f5b544 7aa7ff 63d29a ff8a8a 9aa1ac | 4.51 | 0.122 | passes |
| GitHub Dark | 6.10 | dark | 5.85 | f5b544 7aa7ff 63d29a ff8a8a 9aa1ac | 5.85 | 0.122 | passes |
| GitHub Light | 15.80 | light | 1.35 | 8d5300 355dae 007140 a93d42 5c636d | 4.51 | 0.101 | passes |
| Dracula | 13.36 | dark | 3.05 | f8b848 99c7ff 6ad9a0 ffb0af bcc4cf | 4.51 | 0.076 | passes |
| Solarized Dark | 4.75 | dark | 4.10 | f5b544 7daaff 63d29a ff8a8a a3aab5 | 4.56 | 0.115 | passes |
| Solarized Light | 4.13 | light | 1.35 | 8d5300 355dae 007140 a93d42 5a606a | 4.53 | 0.101 | passes, text warns |
| One Dark | 7.22 | dark | 3.90 | f5b544 80adff 63d29a ff8d8d a9b1bc | 4.52 | 0.111 | passes |
| Nord | 9.25 | dark | 3.06 | f8b848 99c7ff 6ad9a0 ffb0af bcc4cf | 4.52 | 0.076 | passes |
| Gruvbox Dark | 10.75 | dark | 3.92 | f5b544 80adff 63d29a ff8d8d a6adb9 | 4.52 | 0.110 | passes |
| Tokyo Night | 4.40 | dark | 4.16 | f5b544 7aa7ff 63d29a ff8a8a a3aab5 | 4.54 | 0.120 | passes, text warns |
| Catppuccin Mocha | 11.34 | dark | 3.59 | f5b544 89b7ff 63d29a ff9796 b0b7c2 | 4.51 | 0.097 | passes |
| Catppuccin Latte | 7.06 | light | 1.26 | 8a5000 335aab 006b3a a2363c 575d67 | 4.50 | 0.098 | passes |
| Grey 5a5a5a, white text | 6.90 | dark | 1.56 | fff386 ceffff acffdf ffd7d5 f8ffff | 3.08 | 0.051 | falls back |
| Grey 808080, black text | 5.32 | light | 1.52 | 000700 040019 000000 030000 000000 | 3.44 | 0.000 | falls back |

"Unstepped" is the lowest contrast of the Graphite Grid state colours as they are. It is
under 4.5 for 10 of the 11 other themes to bundle, and only GitHub Dark passes, which is why
the ADR does not fix the state colours. At a distance of 0.08, Dracula and Nord would fall
back: their closest pair is running and idle, at 0.076.

The last two rows are not themes to bundle. A background near mid grey cannot pass: either the
states stay under 4.5 on the lightest surface, or they pass only once all five are nearly the
same colour. Those themes take the fallback.

## Where the colours come from

| Theme | Read from | Commit |
| --- | --- | --- |
| Graphite Grid | `crates/pinfold-core/src/vt.rs`, `crates/pinfold-ui/src/top_bar.rs` | this repository |
| GitHub Dark, GitHub Light, One Dark, Nord, Gruvbox Dark, Catppuccin Mocha and Latte | `ghostty/GitHub Dark`, `GitHub Light Default`, `Atom One Dark`, `Nord`, `Gruvbox Dark`, `Catppuccin Mocha`, `Catppuccin Latte` in mbadolato/iTerm2-Color-Schemes | `bc5531d8ec26` |
| Dracula | `ghostty/Dracula` in the same collection, for these three values only; the bundled file comes from dracula/iterm | `bc5531d8ec26` |
| Solarized Dark and Light | base03, base0, base3, base00 and cyan in the README of altercation/solarized | `62f656a02f93` |
| Tokyo Night | `terminal.background`, `terminal.foreground` and `terminal.ansiCyan` in `themes/tokyo-night-color-theme.json` of tokyo-night/tokyo-night-vscode-theme | `7c0f11eaef32` |
