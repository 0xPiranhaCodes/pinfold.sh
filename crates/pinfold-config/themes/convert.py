#!/usr/bin/env python3
"""Writes the bundled themes and their licence texts beside this script (ADR-0049, section 7).

Each theme is converted from one file of an MIT source at a pinned commit, and each licence
text is the source's own file at a pinned commit. Python 3 and no packages; needs the network.
Run: python3 crates/pinfold-config/themes/convert.py
"""

import plistlib
import re
import urllib.parse
import urllib.request
from pathlib import Path

HERE = Path(__file__).parent
ANSI = ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white"]
ANSI += ["bright_" + name for name in ANSI]

COLLECTION = ("mbadolato/iTerm2-Color-Schemes", "bc5531d8ec26e6031c698c61eb7d82c9a9cd092d")
DRACULA = ("dracula/iterm", "e5749bcd07764ad6e154038892b118ea45f88cc0")
SOLARIZED = ("altercation/solarized", "62f656a02f93c5190a8753159e34b385588d5ff3")
TOKYO = ("tokyo-night/tokyo-night-vscode-theme", "7c0f11eaef322f293621ca7befe462214b7ea468")

# Licence file: where its text is read, or the copyright holder where upstream has no file.
LICENCES = {
    "iTerm2-Color-Schemes": (*COLLECTION, "LICENSE"),
    "GitHub": ("primer/github-vscode-theme", "cd78e5e4e7bcf132a6f428ae0f32264bb1b729cf", "LICENSE"),
    "One-Dark": ("atom/atom", "1c3bd35ce238dc0491def9e1780d04748d8e18af", "LICENSE.md"),
    "Nord": ("nordtheme/iterm2", "233a2462e04e07a9676386a52dad0c2ff6666d72", "license"),
    "Catppuccin": ("catppuccin/palette", "07d02aa110ef9eb7e7427afca5c73ba9cf7f8ebd", "LICENSE"),
    # morhetz/gruvbox has no licence file: its README says "MIT/X11" and its package.json
    # `"license": "MIT"`, so this is the MIT text under the author's name from package.json.
    "Gruvbox": "Pavel Pertsev <morhetz@gmail.com>",
    "Dracula": (*DRACULA, "LICENSE"),
    "Solarized": (*SOLARIZED, "LICENSE"),
    "Tokyo-Night": (*TOKYO, "LICENSE.txt"),
}

# Theme: source, file, licence file, copyright, and for the collection where it has the scheme
# from. A collection scheme is under its upstream's licence and the collection's own.
THEMES = {
    "GitHub Dark": (COLLECTION, "ghostty/GitHub Dark", "GitHub", "Copyright (c) 2020 Primer", "primer/github-vscode-theme"),
    "GitHub Light": (COLLECTION, "ghostty/GitHub Light Default", "GitHub", "Copyright (c) 2020 Primer", "primer/github-vscode-theme"),
    "One Dark": (COLLECTION, "ghostty/Atom One Dark", "One-Dark", "Copyright (c) 2011-2022 GitHub Inc.", "atom/atom packages/one-dark-ui"),
    "Nord": (COLLECTION, "ghostty/Nord", "Nord", "Copyright (c) 2016-present Sven Greb", "nordtheme/iterm2"),
    "Catppuccin Mocha": (COLLECTION, "ghostty/Catppuccin Mocha", "Catppuccin", "Copyright (c) 2021 Catppuccin", "catppuccin/palette"),
    "Catppuccin Latte": (COLLECTION, "ghostty/Catppuccin Latte", "Catppuccin", "Copyright (c) 2021 Catppuccin", "catppuccin/palette"),
    "Gruvbox Dark": (COLLECTION, "ghostty/Gruvbox Dark", "Gruvbox", "Copyright (c) Pavel Pertsev", "morhetz/gruvbox"),
    "Dracula": (DRACULA, "Dracula.itermcolors", "Dracula", "Copyright (c) 2013-present Dracula Theme", None),
    "Solarized Dark": (SOLARIZED, "xresources/solarized", "Solarized", "Copyright (c) 2011 Ethan Schoonover", None),
    "Solarized Light": (SOLARIZED, "xresources/solarized", "Solarized", "Copyright (c) 2011 Ethan Schoonover", None),
    "Tokyo Night": (TOKYO, "themes/tokyo-night-color-theme.json", "Tokyo-Night", "Copyright (c) 2018-present Enkia", None),
}


def fetch(repo, commit, path):
    url = f"https://raw.githubusercontent.com/{repo}/{commit}/{urllib.parse.quote(path)}"
    with urllib.request.urlopen(url) as response:
        return response.read()


def colour(text):
    """`#rrggbb`, lower case, or an error: a theme has no alpha and no colour names."""
    text = text.strip().lower().removeprefix("#")
    if not re.fullmatch("[0-9a-f]{6}", text):
        raise ValueError(f"not a colour: {text}")
    return "#" + text


def ghostty(data):
    keys = {
        "background": "background",
        "foreground": "foreground",
        "cursor-color": "cursor",
        "cursor-text": "cursor_text",
        "selection-background": "selection_background",
        "selection-foreground": "selection_foreground",
    }
    colours = {}
    for line in data.decode().splitlines():
        key, _, value = (part.strip() for part in line.partition("="))
        if key == "palette":
            index, _, value = value.partition("=")
            colours[ANSI[int(index)]] = colour(value)
        elif key:
            colours[keys[key]] = colour(value)
    return colours


def itermcolors(data):
    """Components are read as sRGB, as PinFold reads a user's `.itermcolors` (section 8)."""
    keys = {f"Ansi {index} Color": name for index, name in enumerate(ANSI)} | {
        "Background Color": "background",
        "Foreground Color": "foreground",
        "Cursor Color": "cursor",
        "Cursor Text Color": "cursor_text",
        "Selection Color": "selection_background",
        "Selected Text Color": "selection_foreground",
    }
    colours = {}
    for key, value in plistlib.loads(data).items():
        if key in keys:
            channels = (value[part + " Component"] for part in ("Red", "Green", "Blue"))
            colours[keys[key]] = "#" + "".join(f"{round(c * 255):02x}" for c in channels)
    return colours


def xresources(data, light):
    """Solarized's sRGB values. The repository's `iterm2-colors-solarized/*.itermcolors` are not
    used: they hold calibrated Generic RGB numbers and no `Color Space` key, so read as sRGB
    they give a background of #001e27 where Solarized's is #002b36.

    The 16 ANSI colours are the same in both themes, as in the README's table and in those
    presets. The light theme takes its background, foreground and cursor from the file's
    commented `Light` definitions."""
    text = data.decode()
    dark = dict(re.findall(r"^#define S_(\w+) +(#\w+)", text, re.M))
    ground = dict(re.findall(r"^! #define S_(\w+) +(#\w+)", text, re.M)) if light else dark
    named = dict(re.findall(r"^\*(\w+): +S_(\w+)", text, re.M))
    colours = {name: colour(dark[named[f"color{index}"]]) for index, name in enumerate(ANSI)}
    for key, resource in (("background", "background"), ("foreground", "foreground"), ("cursor", "cursorColor")):
        colours[key] = colour(ground[named[resource]])
    return colours


def vscode(data):
    """The `terminal.*` colours. The selection has alpha, which a theme cannot hold, so it is
    left to PinFold's default."""
    found = dict(re.findall(r'"terminal\.(\w+)": "(#\w+)"', data.decode()))
    colours = {key: colour(found[key]) for key in ("background", "foreground")}
    for name in ANSI:
        bright, _, base = name.rpartition("_")
        colours[name] = colour(found["ansi" + bright.title() + base.title()])
    return colours


def main():
    licences = fetch(*COLLECTION, "licenses.yml").decode()
    mit = fetch(*LICENCES["Solarized"]).decode().split("\n", 1)[1]
    for name, source in LICENCES.items():
        text = f"Copyright (c) {source}\n{mit}".encode() if isinstance(source, str) else fetch(*source)
        (HERE / f"LICENSE-{name}.txt").write_bytes(text)
    for name, ((repo, commit), path, licence, copyright, upstream) in THEMES.items():
        data = fetch(repo, commit, path)
        if path == "xresources/solarized":
            colours = xresources(data, name.endswith("Light"))
        else:
            suffix = Path(path).suffix
            colours = {".itermcolors": itermcolors, ".json": vscode}.get(suffix, ghostty)(data)
        header = [
            f"{name}, a theme bundled with PinFold. Written by convert.py: do not edit.",
            f"Source: `{path}` in {repo}",
            f"Commit: {commit}",
            f"Licence: MIT, in LICENSE-{licence}.txt",
            copyright,
        ]
        if upstream:
            scheme = Path(path).name
            if not re.search(rf"^{re.escape(scheme)}:\n  license: MIT$", licences, re.M):
                raise ValueError(f"{scheme} is not MIT in licenses.yml")
            header[1] += f", which has it from {upstream}"
            header[3] += " and LICENSE-iTerm2-Color-Schemes.txt"
            header[4] += "; Copyright (c) 2011 to Present Mark Badolato"
        keys = ["background", "foreground", *ANSI]
        keys += [key for key in ("cursor", "cursor_text", "selection_background", "selection_foreground") if key in colours]
        lines = [f"# {line}" for line in header] + [f'{key} = "{colours[key]}"' for key in keys]
        (HERE / f"{name}.toml").write_text("\n".join(lines) + "\n")


if __name__ == "__main__":
    main()
