# Configuration

Pinfold reads one TOML file at startup, on Linux and macOS alike:
`$XDG_CONFIG_HOME/pinfold/config.toml`, or `~/.config/pinfold/config.toml` when that
variable is unset. There is no reload yet, so restart to apply a change.

| Key                   | Type   | Default | Meaning                                                  |
| --------------------- | ------ | ------- | -------------------------------------------------------- |
| `font_size`           | number | `13`    | In points, from 6 to 72                                  |
| `macos_option_as_alt` | bool   | `false` | Option sends Alt instead of composing text. macOS only   |

```toml
font_size = 14
macos_option_as_alt = true
```

Every key is optional. Pinfold always opens: if the file has a problem, it prints why to
stderr and uses the defaults for every key. That covers an unknown key or a bad value
(named with its line), a file that is not UTF-8 or is over 64 KiB, a directory in the
file's place, a file it cannot read, and a file that another user owns or that group or
others can write (the rule from the threat model). A symlink is followed, and those
checks apply to its target.
