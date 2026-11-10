#!/usr/bin/env python3
"""Checks ADR-0049's thresholds against each theme it proposes to bundle.

Prints one markdown row per theme: text contrast, the lowest contrast of the Graphite Grid
state colours as they are, the state colours after the legibility step, the lowest WCAG contrast of any state on any chrome surface, and the lowest
OKLab distance between two states. Run: python3 docs/spikes/themes/contrast.py
"""

from itertools import combinations

# Background, foreground, ANSI 6 (the derived accent). Sources are in the README beside this.
THEMES = {
    "Graphite Grid": ("121212", "e2e5ea", "5cd6cb"),
    "GitHub Dark": ("101216", "8b949e", "2b7489"),
    "GitHub Light": ("ffffff", "1f2328", "1b7c83"),
    "Dracula": ("282a36", "f8f8f2", "8be9fd"),
    "Solarized Dark": ("002b36", "839496", "2aa198"),
    "Solarized Light": ("fdf6e3", "657b83", "2aa198"),
    "One Dark": ("21252b", "abb2bf", "56b6c2"),
    "Nord": ("2e3440", "d8dee9", "88c0d0"),
    "Gruvbox Dark": ("282828", "ebdbb2", "689d6a"),
    "Tokyo Night": ("16161e", "787c99", "7dcfff"),
    "Catppuccin Mocha": ("1e1e2e", "cdd6f4", "94e2d5"),
    "Catppuccin Latte": ("eff1f5", "4c4f69", "179299"),
    # Not themes to bundle: grounds near mid grey, which no state colour can pass.
    "Grey 5a5a5a, white text": ("5a5a5a", "ffffff", "ffffff"),
    "Grey 808080, black text": ("808080", "000000", "000000"),
}
# Needs you, running, done, error, idle: the Graphite Grid colours every theme starts from.
STATES = ["f5b544", "7aa7ff", "63d29a", "ff8a8a", "9aa1ac"]
# Graphite Grid sets its surfaces; every other theme derives them.
GRAPHITE_SURFACES = ["121212", "1c2026", "2b323b", "253c3b", "12151a", "151920"]

TEXT, STATE, APART, STEP = 4.5, 4.5, 0.07, 0.01
# Where black and white text contrast equally: (L + 0.05)^2 = 1.05 * 0.05.
DARK_BELOW = (1.05 * 0.05) ** 0.5 - 0.05


def rgb(colour):
    return [int(colour[i : i + 2], 16) for i in (0, 2, 4)]


def linear(colour):
    return [c / 3294.6 if c <= 10 else ((c / 255 + 0.055) / 1.055) ** 2.4 for c in rgb(colour)]


def luminance(colour):
    r, g, b = linear(colour)
    return 0.2126 * r + 0.7152 * g + 0.0722 * b


def contrast(a, b):
    high, low = sorted((luminance(a), luminance(b)), reverse=True)
    return (high + 0.05) / (low + 0.05)


def oklab(colour):
    r, g, b = linear(colour)
    l = (0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b) ** (1 / 3)
    m = (0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b) ** (1 / 3)
    s = (0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b) ** (1 / 3)
    return (
        0.2104542553 * l + 0.7936177850 * m - 0.0040720468 * s,
        1.9779984951 * l - 2.4285922050 * m + 0.4505937099 * s,
        0.0259040371 * l + 0.7827717662 * m - 0.8086757660 * s,
    )


def srgb(lab):
    """The nearest sRGB colour to an OKLab one, each channel clamped."""
    l = (lab[0] + 0.3963377774 * lab[1] + 0.2158037573 * lab[2]) ** 3
    m = (lab[0] - 0.1055613458 * lab[1] - 0.0638541728 * lab[2]) ** 3
    s = (lab[0] - 0.0894841775 * lab[1] - 1.2914855480 * lab[2]) ** 3
    channels = (
        4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
        -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
        -0.0041960863 * l - 0.7034186147 * m + 1.7076147010 * s,
    )
    out = ""
    for c in channels:
        c = min(max(c, 0.0), 1.0)
        c = 12.92 * c if c <= 0.0031308 else 1.055 * c ** (1 / 2.4) - 0.055
        out += f"{round(c * 255):02x}"
    return out


def legible(state, grounds, dark):
    """`state` with its OKLab lightness stepped away from the grounds until it reads on each."""
    lightness, a, b = oklab(state)
    while min(contrast(state, ground) for ground in grounds) < STATE and 0 < lightness < 1:
        lightness = min(max(lightness + (STEP if dark else -STEP), 0.0), 1.0)
        state = srgb((lightness, a, b))
    return state


def apart(a, b):
    return sum((x - y) ** 2 for x, y in zip(oklab(a), oklab(b))) ** 0.5


def mix(under, over, share):
    """sRGB bytes of `under` moved `share` of the way to `over`, rounded."""
    return "".join(f"{round(u + (o - u) * share):02x}" for u, o in zip(rgb(under), rgb(over)))


def surfaces(background, foreground, accent):
    quiet, hover = mix(background, foreground, 0.06), mix(background, foreground, 0.14)
    overlay = mix(background, foreground, 0.04)
    return [background, quiet, hover, mix(background, accent, 0.22), overlay]


def main():
    print("| Theme | Text | Ground | Unstepped | States | Lowest state contrast | Closest two states | Result |")
    print("| --- | --- | --- | --- | --- | --- | --- | --- |")
    for name, (background, foreground, accent) in THEMES.items():
        dark = luminance(background) < DARK_BELOW
        grounds = GRAPHITE_SURFACES if name == "Graphite Grid" else surfaces(background, foreground, accent)
        text = contrast(foreground, background)
        unstepped = min(contrast(state, ground) for state in STATES for ground in grounds)
        states = [legible(state, grounds, dark) for state in STATES]
        lowest = min(contrast(state, ground) for state in states for ground in grounds)
        closest = min(apart(a, b) for a, b in combinations(states, 2))
        result = "passes" if lowest >= STATE and closest >= APART else "falls back"
        if text < TEXT:
            result += ", text warns"
        ground = "dark" if dark else "light"
        shown = " ".join(states)
        print(f"| {name} | {text:.2f} | {ground} | {unstepped:.2f} | {shown} | {lowest:.2f} | {closest:.3f} | {result} |")


if __name__ == "__main__":
    main()
