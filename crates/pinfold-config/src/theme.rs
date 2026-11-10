//! Themes (ADR-0049): the colours a theme file or `[colors]` gives, and the theme they make.
//!
//! A theme file is untrusted input, so nothing here panics on what it holds.

use std::path::Path;

use serde::{Deserialize, Deserializer, de};

/// Declares a table of colour keys twice: `$all` holds every colour, and `$some` is the table
/// as a file writes it, with each key optional.
macro_rules! keys {
    ($what:literal $all:ident $some:ident: $($key:ident)* $(; $($table:ident: $tall:ident $tsome:ident)*)?) => {
        #[doc = concat!($what, ", each `0x00RRGGBB`.")]
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub struct $all {
            $(#[doc = concat!("The `", stringify!($key), "` key.")] pub $key: u32,)*
            $($(#[doc = concat!("The `[", stringify!($table), "]` table.")] pub $table: $tall,)*)?
        }

        #[doc = concat!($what, " as a file gives them: each key optional, written `#rrggbb`.")]
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
        #[serde(default, deny_unknown_fields)]
        pub struct $some {
            $(#[doc = concat!("The `", stringify!($key), "` key.")]
            #[serde(deserialize_with = "hex")]
            pub $key: Option<u32>,)*
            $($(#[doc = concat!("The `[", stringify!($table), "]` table.")] pub $table: $tsome,)*)?
        }

        impl $some {
            /// These keys, and `under`'s where one is absent.
            fn or(self, under: Self) -> Self {
                Self {
                    $($key: self.$key.or(under.$key),)*
                    $($($table: self.$table.or(under.$table),)*)?
                }
            }

            /// Every colour, or the first key that is absent.
            fn all(self) -> Result<$all, &'static str> {
                Ok($all {
                    $($key: self.$key.ok_or(stringify!($key))?,)*
                    $($($table: self.$table.all()?,)*)?
                })
            }
        }

        impl From<$all> for $some {
            fn from(all: $all) -> Self {
                Self { $($key: Some(all.$key),)* $($($table: all.$table.into(),)*)? }
            }
        }
    };
}

keys!("A theme's colours" Theme Colors:
    background foreground black red green yellow blue magenta cyan white
    bright_black bright_red bright_green bright_yellow bright_blue bright_magenta bright_cyan
    bright_white cursor cursor_text selection_background selection_foreground;
    chrome: Chrome ChromeColors states: States StateColors);
keys!("The chrome's colours" Chrome ChromeColors:
    accent tab tab_hover tab_active overlay border dim muted);
keys!("The agent states' colours" States StateColors: needs_you running done error idle);

/// Graphite Grid, the default theme and the fallback (`docs/design/ui/`).
pub const GRAPHITE: Theme = Theme {
    background: 0x0012_1212,
    foreground: 0x00e2_e5ea,
    black: 0x0012_151a,
    red: 0x00ff_8a8a,
    green: 0x0063_d29a,
    yellow: 0x00f5_b544,
    blue: 0x007a_a7ff,
    magenta: 0x00d9_a7e0,
    cyan: 0x009c_c0ff,
    white: 0x00c9_cdd4,
    bright_black: 0x0034_3a44,
    bright_red: 0x00ff_8a8a,
    bright_green: 0x0063_d29a,
    bright_yellow: 0x00f5_b544,
    bright_blue: 0x009c_c0ff,
    bright_magenta: 0x00d9_a7e0,
    bright_cyan: 0x00c4_d9ff,
    bright_white: 0x00e2_e5ea,
    cursor: 0x00e2_e5ea,
    cursor_text: 0x0012_1212,
    selection_background: 0x00e2_e5ea,
    selection_foreground: 0x0012_1212,
    chrome: Chrome {
        accent: 0x005c_d6cb,
        tab: 0x001c_2026,
        tab_hover: 0x002b_323b,
        tab_active: 0x0025_3c3b,
        overlay: 0x0012_151a,
        border: 0x0034_3a44,
        dim: 0x009a_a1ac,
        muted: 0x0055_5a63,
    },
    states: States {
        needs_you: 0x00f5_b544,
        running: 0x007a_a7ff,
        done: 0x0063_d29a,
        error: 0x00ff_8a8a,
        idle: 0x009a_a1ac,
    },
};

/// The lowest contrast of text, and of a state on a surface.
const LEGIBLE: f64 = 4.5;
/// The least OKLab distance between two states.
const APART: f64 = 0.07;

/// Why a named theme was not used.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The name could be a path, or is empty or too long.
    #[error(
        "a theme name is 1 to 64 letters, digits, spaces and `-_+.`, and does not start with `.`"
    )]
    Name,
    /// No file in the themes directory and no built-in theme has the name.
    #[error("there is no theme of that name")]
    NotFound,
    /// The theme file was refused, as `pinfold.toml` would be, or lacks a required key.
    #[error(transparent)]
    File(#[from] crate::Error),
}

impl Theme {
    /// The colour of a prompt's text on `chrome.overlay`: the foreground where that contrasts
    /// at least 4.5, and otherwise black or white, whichever contrasts more.
    ///
    /// ```
    /// let graphite = pinfold_config::theme::GRAPHITE;
    /// assert_eq!(graphite.prompt_text(), graphite.foreground);
    /// ```
    pub fn prompt_text(&self) -> u32 {
        let on = |text| contrast(text, self.chrome.overlay);
        if on(self.foreground) >= LEGIBLE {
            self.foreground
        } else if on(0) > on(0x00ff_ffff) {
            0
        } else {
            0x00ff_ffff
        }
    }

    /// Every ground a state is drawn on.
    fn surfaces(&self) -> [(&'static str, u32); 5] {
        let chrome = self.chrome;
        [
            ("background", self.background),
            ("tab", chrome.tab),
            ("tab_hover", chrome.tab_hover),
            ("tab_active", chrome.tab_active),
            ("overlay", chrome.overlay),
        ]
    }

    fn states(&self) -> [(&'static str, u32); 5] {
        let states = self.states;
        [
            ("needs_you", states.needs_you),
            ("running", states.running),
            ("done", states.done),
            ("error", states.error),
            ("idle", states.idle),
        ]
    }

    /// The lowest contrast of a state on a surface, and which they are.
    fn faintest(&self) -> (f64, &'static str, &'static str) {
        let mut faintest = (f64::INFINITY, "", "");
        for (state, colour) in self.states() {
            for (surface, ground) in self.surfaces() {
                let ratio = contrast(colour, ground);
                if ratio < faintest.0 {
                    faintest = (ratio, state, surface);
                }
            }
        }
        faintest
    }

    /// The smallest OKLab distance between two states, and which they are.
    fn closest(&self) -> (f64, &'static str, &'static str) {
        let states = self.states().map(|(name, colour)| (name, oklab(colour)));
        let mut closest = (f64::INFINITY, "", "");
        for (index, (first, a)) in states.iter().enumerate() {
            for (second, b) in &states[index + 1..] {
                let distance = (0..3).map(|i| (a[i] - b[i]).powi(2)).sum::<f64>().sqrt();
                if distance < closest.0 {
                    closest = (distance, first, second);
                }
            }
        }
        closest
    }

    /// Why the chrome and the states cannot be used, with the measured number.
    fn failure(&self) -> Option<String> {
        let (ratio, state, surface) = self.faintest();
        let (distance, first, second) = self.closest();
        let muted = contrast(self.chrome.muted, self.background);
        if ratio < LEGIBLE {
            Some(format!(
                "{state} contrasts {ratio:.2} on {surface}, under {LEGIBLE}"
            ))
        } else if distance < APART {
            Some(format!(
                "{first} and {second} are {distance:.3} apart, under {APART}"
            ))
        } else if muted >= 3.0 {
            Some(format!(
                "muted contrasts {muted:.2} with the background, not under 3"
            ))
        } else {
            None
        }
    }
}

fn hex<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u32>, D::Error> {
    let text = String::deserialize(deserializer)?;
    // `from_str_radix` alone would take a sign.
    let digits = text
        .strip_prefix('#')
        .filter(|digits| digits.len() == 6 && digits.bytes().all(|b| b.is_ascii_hexdigit()));
    match digits.and_then(|digits| u32::from_str_radix(digits, 16).ok()) {
        Some(colour) => Ok(Some(colour)),
        None => Err(de::Error::custom(
            "a colour is # and six hex digits, such as \"#0d1117\"",
        )),
    }
}

// The maths below follows `docs/spikes/themes/contrast.py` step for step, so the two agree.

fn linear(colour: u32) -> [f64; 3] {
    let [_, r, g, b] = colour.to_be_bytes();
    [r, g, b].map(|channel| {
        let c = f64::from(channel);
        if c <= 10.0 {
            c / 3294.6
        } else {
            ((c / 255.0 + 0.055) / 1.055).powf(2.4)
        }
    })
}

/// The WCAG 2 relative luminance.
fn luminance(colour: u32) -> f64 {
    let [r, g, b] = linear(colour);
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

/// The WCAG 2 contrast ratio.
fn contrast(a: u32, b: u32) -> f64 {
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

fn oklab(colour: u32) -> [f64; 3] {
    let [r, g, b] = linear(colour);
    let l = (0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b).cbrt();
    let m = (0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b).cbrt();
    let s = (0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b).cbrt();
    [
        0.2104542553 * l + 0.7936177850 * m - 0.0040720468 * s,
        1.9779984951 * l - 2.4285922050 * m + 0.4505937099 * s,
        0.0259040371 * l + 0.7827717662 * m - 0.8086757660 * s,
    ]
}

/// Three channels from 0 to 255 as one colour, each rounded and clamped.
fn pack(channels: [f64; 3]) -> u32 {
    // A half rounds to even, as Python's `round` does. A float cast saturates, and NaN is 0.
    let byte = |channel: f64| channel.round_ties_even().clamp(0.0, 255.0) as u32;
    channels
        .iter()
        .fold(0, |colour, channel| colour << 8 | byte(*channel))
}

/// The nearest sRGB colour to an OKLab one, each channel clamped.
fn srgb([lightness, a, b]: [f64; 3]) -> u32 {
    let l = (lightness + 0.3963377774 * a + 0.2158037573 * b).powi(3);
    let m = (lightness - 0.1055613458 * a - 0.0638541728 * b).powi(3);
    let s = (lightness - 0.0894841775 * a - 1.2914855480 * b).powi(3);
    let channels = [
        4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
        -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
        -0.0041960863 * l - 0.7034186147 * m + 1.7076147010 * s,
    ];
    pack(channels.map(|channel| {
        let c = channel.clamp(0.0, 1.0);
        let encoded = if c <= 0.0031308 {
            12.92 * c
        } else {
            1.055 * c.powf(1.0 / 2.4) - 0.055
        };
        encoded * 255.0
    }))
}

/// `under` moved `share` of the way to `over`, per sRGB channel.
fn mix(under: u32, over: u32, share: f64) -> u32 {
    let (under, over) = (under.to_be_bytes(), over.to_be_bytes());
    pack([1, 2, 3].map(|i| {
        let from = f64::from(under[i]);
        from + (f64::from(over[i]) - from) * share
    }))
}

/// `state` with its OKLab lightness stepped away from the ground until it reads on every
/// surface, or reaches black or white: at most 100 steps.
fn legible(mut state: u32, surfaces: &[(&str, u32); 5], dark: bool) -> u32 {
    let [mut lightness, a, b] = oklab(state);
    let faint = |state| surfaces.iter().any(|s| contrast(state, s.1) < LEGIBLE);
    while faint(state) && 0.0 < lightness && lightness < 1.0 {
        lightness = (lightness + if dark { 0.01 } else { -0.01 }).clamp(0.0, 1.0);
        state = srgb([lightness, a, b]);
    }
    state
}

/// Fills the keys a theme may leave out, or names a required key that is absent.
fn fill(colors: Colors) -> Result<Theme, crate::Error> {
    let missing = |key| crate::Error::Invalid(format!("a theme needs the key `{key}`"));
    let (Some(ground), Some(text), Some(cyan)) =
        (colors.background, colors.foreground, colors.cyan)
    else {
        return colors.all().map_err(missing);
    };
    let toward_text = |share| Some(mix(ground, text, share));
    // The active tab is mixed from cyan, as in the script, even where the accent ends up the
    // foreground.
    let towards = colors.chrome.accent.unwrap_or(cyan);
    let tab_active = (colors.chrome.tab_active).unwrap_or_else(|| mix(ground, towards, 0.22));
    let accent = if contrast(cyan, tab_active) < LEGIBLE {
        text
    } else {
        cyan
    };
    let derived = Colors {
        cursor: Some(text),
        cursor_text: Some(ground),
        selection_background: Some(text),
        selection_foreground: Some(ground),
        chrome: ChromeColors {
            accent: Some(accent),
            tab: toward_text(0.06),
            tab_hover: toward_text(0.14),
            tab_active: Some(tab_active),
            overlay: toward_text(0.04),
            border: toward_text(0.20),
            dim: Some(mix(text, ground, 0.25)),
            muted: toward_text(0.30),
        },
        states: GRAPHITE.states.into(),
        ..Colors::default()
    };
    colors.or(derived).all().map_err(missing)
}

/// Makes a theme of `colors` with `overrides`, the `[colors]` table, on top, and says what the
/// log should: the state colours are stepped until legible, and a theme whose chrome fails the
/// checks keeps its pane colours and gets Graphite Grid's chrome and states.
pub(crate) fn resolve(
    colors: Colors,
    overrides: Colors,
) -> Result<(Theme, Vec<String>), crate::Error> {
    // A theme holds its required keys itself, whatever `[colors]` adds.
    fill(colors)?;
    let mut theme = fill(overrides.or(colors))?;
    let mut notes = Vec::new();
    let surfaces = theme.surfaces();
    // Under 0.179, white text contrasts more than black.
    let dark = luminance(theme.background) < 0.179;
    let states = &mut theme.states;
    for state in [
        &mut states.needs_you,
        &mut states.running,
        &mut states.done,
        &mut states.error,
        &mut states.idle,
    ] {
        *state = legible(*state, &surfaces, dark);
    }
    if let Some(reason) = theme.failure() {
        notes.push(format!(
            "{reason}, so the chrome and the states are Graphite Grid's"
        ));
        (theme.chrome, theme.states) = (GRAPHITE.chrome, GRAPHITE.states);
    }
    let text = contrast(theme.foreground, theme.background);
    if text < LEGIBLE {
        notes.push(format!(
            "the foreground contrasts {text:.2} with the background, under {LEGIBLE}"
        ));
    }
    Ok((theme, notes))
}

/// Parses a theme file and makes its theme, with `overrides`, the `[colors]` table, on top.
/// The strings are for the log: a fallback to Graphite Grid's chrome, or text that is hard to
/// read.
///
/// # Errors
///
/// Returns an error if `bytes` is not a theme: not UTF-8, not TOML, an unknown key, a colour
/// that is not `#rrggbb`, or a required key that is absent.
///
/// ```
/// use pinfold_config::theme::{self, Colors, GRAPHITE};
/// let text = b"background = \"#101216\"\nforeground = \"#8b949e\"";
/// assert!(theme::parse(text, Colors::default()).is_err());
/// let (theme, notes) = theme::load(std::path::Path::new("/nowhere/pinfold.toml"))?;
/// assert!(theme == GRAPHITE && notes.is_empty());
/// # Ok::<(), theme::Error>(())
/// ```
pub fn parse(bytes: &[u8], overrides: Colors) -> Result<(Theme, Vec<String>), crate::Error> {
    resolve(crate::parse_as(bytes)?, overrides)
}

/// The two keys of `pinfold.toml` that choose a theme. Its other keys are ignored here.
#[derive(Default, Deserialize)]
#[serde(default)]
struct Chosen {
    theme: Option<String>,
    colors: Colors,
}

/// Loads the theme `config`, the path of `pinfold.toml`, chooses: its `theme`, or Graphite Grid
/// without one, with its `[colors]` on top, as [`parse`] does. A named theme is
/// `themes/<name>.toml` beside `config`, then `themes/<name>.itermcolors`
/// ([`crate::itermcolors::parse`]), then `themes/<name>` as a Ghostty theme file
/// ([`crate::ghostty::parse`]), each read with the checks of [`crate::load`], and then a
/// built-in theme.
///
/// This reads `config` itself, and [`crate::Config`] holds neither key, so the core, which
/// never calls this, carries no theme code.
///
/// # Errors
///
/// Returns an error if `config` or the theme file is refused, `theme` or `[colors]` is bad, or
/// no theme has the name. The caller then keeps the theme it has, or Graphite Grid at start.
pub fn load(config: &Path) -> Result<(Theme, Vec<String>), Error> {
    let chosen: Chosen = match crate::read(config)? {
        Some(bytes) => crate::parse_as(&bytes)?,
        None => Chosen::default(),
    };
    named(config, chosen.theme.as_deref(), chosen.colors)
}

fn named(
    config: &Path,
    name: Option<&str>,
    overrides: Colors,
) -> Result<(Theme, Vec<String>), Error> {
    let Some(name) = name else {
        return Ok(resolve(GRAPHITE.into(), overrides)?);
    };
    // No `/`, and no leading `.`, so no name is a path.
    let valid = |byte: u8| byte.is_ascii_alphanumeric() || b" -_+.".contains(&byte);
    if !(1..=64).contains(&name.len()) || name.starts_with('.') || !name.bytes().all(valid) {
        return Err(Error::Name);
    }
    let themes = config.with_file_name("themes");
    if let Some(bytes) = crate::read(&themes.join(format!("{name}.toml")))? {
        return Ok(parse(&bytes, overrides)?);
    }
    if let Some(bytes) = crate::read(&themes.join(format!("{name}.itermcolors")))? {
        return Ok(crate::itermcolors::parse(&bytes, overrides)?);
    }
    if let Some(bytes) = crate::read(&themes.join(name))? {
        return Ok(crate::ghostty::parse(&bytes, overrides)?);
    }
    if name != "Graphite Grid" {
        return Err(Error::NotFound);
    }
    Ok(resolve(GRAPHITE.into(), overrides)?)
}

#[cfg(test)]
mod tests {
    use std::fs::{self, Permissions};
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::PathBuf;

    use proptest::prelude::*;
    use tempfile::{TempDir, tempdir};

    use super::*;

    /// A theme file of the 18 required keys, from the three colours a row of the spike has.
    fn file(background: u32, foreground: u32, cyan: u32) -> String {
        let mut text = format!(
            "background = \"#{background:06x}\"\nforeground = \"#{foreground:06X}\"\ncyan = \"#{cyan:06x}\"\n"
        );
        for key in [
            "black", "red", "green", "yellow", "blue", "magenta", "white",
        ] {
            text += &format!("{key} = \"#808080\"\nbright_{key} = \"#808080\"\n");
        }
        text + "bright_cyan = \"#808080\"\n"
    }

    /// The theme of a spike row, its states as the script prints them, and the log's lines.
    fn row(background: u32, foreground: u32, cyan: u32) -> (Theme, String, String) {
        let text = file(background, foreground, cyan);
        let (theme, notes) = parse(text.as_bytes(), Colors::default()).unwrap();
        let states = theme.states().map(|(_, colour)| format!("{colour:06x}"));
        (theme, states.join(" "), notes.join("\n"))
    }

    /// A themes directory holding one file, and the path of the `pinfold.toml` beside it.
    fn themes(name: &str, contents: &[u8]) -> (TempDir, PathBuf) {
        let dir = tempdir().unwrap();
        fs::create_dir(dir.path().join("themes")).unwrap();
        let path = dir.path().join("themes").join(name);
        fs::write(&path, contents).unwrap();
        fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
        let config = dir.path().join("pinfold.toml");
        fs::write(&config, "theme = \"My Theme-1\"\nfont_size = 20\n").unwrap();
        fs::set_permissions(&config, Permissions::from_mode(0o600)).unwrap();
        (dir, config)
    }

    fn refused(text: &str) -> String {
        parse(text.as_bytes(), Colors::default())
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn graphite_grid_matches_the_spike_and_is_unchanged() {
        let (theme, notes) = resolve(GRAPHITE.into(), Colors::default()).unwrap();
        assert_eq!((theme, notes), (GRAPHITE, Vec::new()));
        let text = contrast(GRAPHITE.foreground, GRAPHITE.background);
        assert_eq!(format!("{text:.2}"), "14.83");
        assert_eq!(format!("{:.2}", GRAPHITE.faintest().0), "4.51");
        assert_eq!(format!("{:.3}", GRAPHITE.closest().0), "0.122");
        assert_eq!(GRAPHITE.prompt_text(), GRAPHITE.foreground);
        assert_eq!(
            (GRAPHITE.faintest().1, GRAPHITE.faintest().2),
            ("idle", "tab_active")
        );
        // The fuzz seed is Graphite Grid written as a theme file, every key set.
        let seed = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fuzz/seeds/config/theme.toml"
        );
        let seed = parse(&fs::read(seed).unwrap(), Colors::default()).unwrap();
        assert_eq!(seed, (GRAPHITE, Vec::new()));
    }

    #[test]
    fn states_are_stepped_until_legible_as_the_spike_steps_them() {
        // Dracula, a dark theme whose closest pair sets the 0.07.
        let (theme, states, notes) = row(0x0028_2a36, 0x00f8_f8f2, 0x008b_e9fd);
        assert_eq!(states, "f8b848 99c7ff 6ad9a0 ffb0af bcc4cf");
        assert_eq!(format!("{:.2}", theme.faintest().0), "4.51");
        assert_eq!(format!("{:.3}", theme.closest().0), "0.076");
        assert_eq!(notes, "");
        // Solarized Light: stepped down, and its text warns and changes nothing.
        let (theme, states, notes) = row(0x00fd_f6e3, 0x0065_7b83, 0x002a_a198);
        assert_eq!(states, "8d5300 355dae 007140 a93d42 5a606a");
        assert!(notes.contains("foreground contrasts 4.13"), "{notes}");
        assert_eq!(theme.foreground, 0x0065_7b83);
    }

    #[test]
    fn every_theme_of_the_spike_gets_the_states_the_script_prints() {
        // Background, foreground, cyan and the States column of `docs/spikes/themes/README.md`.
        let rows = [
            (
                0x10_1216,
                0x8b_949e,
                0x2b_7489,
                "f5b544 7aa7ff 63d29a ff8a8a 9aa1ac",
            ),
            (
                0xff_ffff,
                0x1f_2328,
                0x1b_7c83,
                "8d5300 355dae 007140 a93d42 5c636d",
            ),
            (
                0x00_2b36,
                0x83_9496,
                0x2a_a198,
                "f5b544 7daaff 63d29a ff8a8a a3aab5",
            ),
            (
                0x21_252b,
                0xab_b2bf,
                0x56_b6c2,
                "f5b544 80adff 63d29a ff8d8d a9b1bc",
            ),
            (
                0x2e_3440,
                0xd8_dee9,
                0x88_c0d0,
                "f8b848 99c7ff 6ad9a0 ffb0af bcc4cf",
            ),
            (
                0x28_2828,
                0xeb_dbb2,
                0x68_9d6a,
                "f5b544 80adff 63d29a ff8d8d a6adb9",
            ),
            (
                0x16_161e,
                0x78_7c99,
                0x7d_cfff,
                "f5b544 7aa7ff 63d29a ff8a8a a3aab5",
            ),
            (
                0x1e_1e2e,
                0xcd_d6f4,
                0x94_e2d5,
                "f5b544 89b7ff 63d29a ff9796 b0b7c2",
            ),
            (
                0xef_f1f5,
                0x4c_4f69,
                0x17_9299,
                "8a5000 335aab 006b3a a2363c 575d67",
            ),
        ];
        for (background, foreground, cyan, states) in rows {
            let (theme, stepped, _) = row(background, foreground, cyan);
            assert_eq!(stepped, states, "{background:06x}");
            assert!(theme.failure().is_none() && theme.chrome != GRAPHITE.chrome);
        }
    }

    #[test]
    fn a_mid_grey_theme_takes_graphite_grids_chrome_and_keeps_its_panes() {
        let (theme, _, notes) = row(0x005a_5a5a, 0x00ff_ffff, 0x00ff_ffff);
        assert_eq!(
            (theme.chrome, theme.states),
            (GRAPHITE.chrome, GRAPHITE.states)
        );
        assert_eq!(theme.background, 0x005a_5a5a);
        assert!(notes.contains("contrasts 3.08 on"), "{notes}");
        let (theme, _, notes) = row(0x0080_8080, 0, 0);
        assert_eq!(theme.chrome, GRAPHITE.chrome);
        assert!(notes.contains("contrasts 3.44 on"), "{notes}");
    }

    #[test]
    fn absent_keys_are_derived() {
        // GitHub Dark, whose cyan is too dim on the active tab to be the accent.
        let (theme, _, notes) = row(0x0010_1216, 0x008b_949e, 0x002b_7489);
        let chrome = Chrome {
            accent: 0x008b_949e,
            tab: 0x0017_1a1e,
            tab_hover: 0x0021_2429,
            tab_active: 0x0016_282f,
            overlay: 0x0015_171b,
            border: 0x0029_2c31,
            dim: 0x006c_747c,
            muted: 0x0035_393f,
        };
        assert_eq!((theme.chrome, notes.as_str()), (chrome, ""));
        assert_eq!(
            (theme.cursor, theme.cursor_text),
            (0x008b_949e, 0x0010_1216)
        );
        assert_eq!(theme.selection_background, 0x008b_949e);
        assert_eq!(theme.selection_foreground, 0x0010_1216);
        assert_eq!(theme.states, GRAPHITE.states);
        let accent = row(0x0028_2a36, 0x00f8_f8f2, 0x008b_e9fd).0.chrome.accent;
        assert_eq!(accent, 0x008b_e9fd);
    }

    #[test]
    fn prompt_text_is_forced_legible() {
        let mut theme = GRAPHITE;
        (theme.foreground, theme.chrome.overlay) = (0x0080_8080, 0x0070_7070);
        assert_eq!(theme.prompt_text(), 0x00ff_ffff);
        theme.chrome.overlay = 0x0090_9090;
        assert_eq!(theme.prompt_text(), 0);
        // Pane text may be muted only while it stays under 3 on the background.
        let mut theme = GRAPHITE;
        theme.chrome.muted = 0x00e2_e5ea;
        assert!(theme.failure().unwrap().contains("not under 3"));
    }

    #[test]
    fn colors_go_on_top_and_a_bad_theme_leaves_the_other_keys() {
        let text = "theme = \"../x\"\nfont_size = 20\n[colors]\ncursor = \"#ff0000\"\n[colors.states]\nidle = \"#9aa1ac\"\n";
        let config = crate::parse(text.as_bytes()).unwrap();
        assert_eq!(config.font_size, 20.0);
        let (_dir, path) = themes("unused.toml", b"");
        fs::write(&path, text).unwrap();
        assert!(matches!(load(&path), Err(Error::Name)));
        fs::write(&path, text.replace("../x", "Graphite Grid")).unwrap();
        let (theme, notes) = load(&path).unwrap();
        let expected = Theme {
            cursor: 0x00ff_0000,
            ..GRAPHITE
        };
        assert_eq!((theme, notes), (expected, Vec::new()));
        // A bad `[colors]` is the theme's problem, named with its line, and no other key's.
        fs::write(&path, "font_size = 20\n[colors]\nglow = \"#000000\"\n").unwrap();
        assert_eq!(crate::load(&path).unwrap().font_size, 20.0);
        let text = load(&path).unwrap_err().to_string();
        assert!(text.contains("line 3") && text.contains("glow"), "{text}");
        fs::remove_file(&path).unwrap();
        assert_eq!(load(&path).unwrap(), (GRAPHITE, Vec::new()));
    }

    #[test]
    fn a_user_theme_is_read_from_beside_the_config_file() {
        let text = file(0x0028_2a36, 0x00f8_f8f2, 0x008b_e9fd);
        let (_dir, config) = themes("My Theme-1.toml", text.as_bytes());
        let (theme, _) = load(&config).unwrap();
        assert_eq!(theme.background, 0x0028_2a36);
        let missing = named(&config, Some("Other"), Colors::default());
        assert!(matches!(missing, Err(Error::NotFound)));
    }

    #[test]
    fn a_ghostty_theme_is_found_after_the_toml_file_and_gets_its_checks() {
        let seed = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fuzz/seeds/ghostty_theme/other-keys"
        );
        let (dir, config) = themes("My Theme-1", &fs::read(seed).unwrap());
        let (theme, notes) = load(&config).unwrap();
        assert_eq!((theme.background, theme.cyan), (0x001d_1f21, 0x008a_beb7));
        assert!(
            notes
                .iter()
                .any(|note| note.starts_with("7 other Ghostty keys")),
            "{notes:?}"
        );
        // The same checks as the `.toml` file, and the `.toml` file first.
        let path = dir.path().join("themes/My Theme-1");
        fs::set_permissions(&path, Permissions::from_mode(0o666)).unwrap();
        assert!(matches!(
            load(&config),
            Err(Error::File(crate::Error::Writable))
        ));
        let text = file(0x0028_2a36, 0x00f8_f8f2, 0x008b_e9fd);
        fs::write(path.with_extension("toml"), text).unwrap();
        assert_eq!(load(&config).unwrap().0.background, 0x0028_2a36);
    }

    #[test]
    fn a_name_that_could_be_a_path_is_refused() {
        let long = "a".repeat(65);
        for name in ["", ".", ".hidden", "a/b", "../a", "a\0", "é", "a\n", &long] {
            let refused = named(Path::new("/nowhere/c"), Some(name), Colors::default());
            assert!(matches!(refused, Err(Error::Name)), "{name:?}");
        }
    }

    #[test]
    fn a_theme_file_gets_the_checks_of_the_config_file() {
        let load = |config: &Path| named(config, Some("t"), Colors::default());
        let (_dir, config) = themes("t.toml", &vec![b'\n'; crate::MAX_BYTES as usize + 1]);
        assert!(matches!(
            load(&config),
            Err(Error::File(crate::Error::TooLarge))
        ));
        let (dir, config) = themes("other.toml", b"");
        let path = dir.path().join("themes/t.toml");
        if pinfold_sys::effective_uid() != 0 {
            symlink("/etc/passwd", &path).unwrap();
            assert!(matches!(
                load(&config),
                Err(Error::File(crate::Error::NotOwned))
            ));
            fs::remove_file(&path).unwrap();
        }
        fs::write(&path, b"").unwrap();
        fs::set_permissions(&path, Permissions::from_mode(0o666)).unwrap();
        assert!(matches!(
            load(&config),
            Err(Error::File(crate::Error::Writable))
        ));
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(matches!(
            load(&config),
            Err(Error::File(crate::Error::NotAFile))
        ));
    }

    #[test]
    fn unknown_keys_bad_colours_and_absent_keys_are_refused_with_a_reason() {
        let good = file(0x0010_1216, 0x008b_949e, 0x002b_7489);
        let text = refused(&format!("{good}glow = \"#000000\"\n"));
        assert!(text.contains("line 19") && text.contains("glow"), "{text}");
        let text = refused(&format!("{good}[chrome]\nshadow = \"#000000\"\n"));
        assert!(
            text.contains("line 20") && text.contains("shadow"),
            "{text}"
        );
        for colour in ["#12345", "#1234567", "#12345g", "+12345", "red", "#ffé"] {
            let text = refused(&format!("{good}cursor = \"{colour}\"\n"));
            assert!(text.contains("line 19"), "{colour}: {text}");
            assert!(text.contains("six hex digits"), "{colour}: {text}");
        }
        assert!(refused(&format!("{good}cursor = 0\n")).contains("line 19"));
        let text = refused(&good.replace("\nred = \"#808080\"", ""));
        assert!(text.contains("needs the key `red`"), "{text}");
        assert!(refused("background = \"#000000\"").contains("`foreground`"));
        // `[colors]` cannot stand in for a key the theme itself must hold.
        let overrides = Colors {
            foreground: Some(0),
            ..Colors::default()
        };
        assert!(parse(b"background = \"#000000\"", overrides).is_err());
        assert!(matches!(
            parse(b"\xff", overrides),
            Err(crate::Error::NotUtf8)
        ));
    }

    proptest! {
        #[test]
        fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
            let _ = parse(&bytes, Colors::default());
        }

        #[test]
        fn any_colours_make_a_theme_whose_chrome_passes_or_is_graphite_grids(
            colours in any::<[u32; 8]>(),
        ) {
            let [background, foreground, cyan, a, b, c, d, e] = colours.map(|c| c & 0x00ff_ffff);
            let states = format!(
                "[states]\nneeds_you = \"#{a:06x}\"\nrunning = \"#{b:06x}\"\ndone = \"#{c:06x}\"\nerror = \"#{d:06x}\"\nidle = \"#{e:06x}\"\n"
            );
            let text = file(background, foreground, cyan) + &states;
            let (theme, notes) = parse(text.as_bytes(), Colors::default()).unwrap();
            prop_assert_eq!(theme.background, background);
            prop_assert!(theme.failure().is_none() || theme.chrome == GRAPHITE.chrome, "{:?}", notes);
        }
    }
}
