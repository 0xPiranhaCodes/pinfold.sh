//! A Ghostty theme file read as a theme (ADR-0049 section 8).
//!
//! The file is untrusted input, so nothing here panics on what it holds, and no key but a
//! colour is acted on: a Ghostty theme may hold any Ghostty setting.

use crate::Error;
use crate::theme::{self, Colors, Theme};

/// `#rrggbb` or `rrggbb` as a colour. A name such as `red` and the short `#rgb` are refused.
fn colour(text: &str) -> Option<u32> {
    let digits = text.strip_prefix('#').unwrap_or(text);
    // `from_str_radix` alone would take a sign.
    let hex = digits.len() == 6 && digits.bytes().all(|b| b.is_ascii_hexdigit());
    u32::from_str_radix(digits, 16).ok().filter(|_| hex)
}

/// The colours a Ghostty file sets, and how many of its other keys were ignored.
fn colors(bytes: &[u8]) -> Result<(Colors, usize), Error> {
    if bytes.len() as u64 > crate::MAX_BYTES {
        return Err(Error::TooLarge);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| Error::NotUtf8)?;
    let (mut colors, mut palette, mut ignored) = (Colors::default(), [None; 16], 0);
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line.split_once('=').unwrap_or((line, ""));
        let (key, value) = (key.trim(), value.trim());
        // Ghostty takes a value in double quotes too.
        let unquoted = value.strip_prefix('"').and_then(|v| v.strip_suffix('"'));
        let mut value = unquoted.unwrap_or(value);
        // The message names a key from the list below and never quotes the file.
        let refuse = |what: &str| Error::Invalid(format!("line {}: {what}", index + 1));
        let mut beyond = None;
        let slot = match key {
            "background" => &mut colors.background,
            "foreground" => &mut colors.foreground,
            "cursor-color" => &mut colors.cursor,
            "cursor-text" => &mut colors.cursor_text,
            "selection-background" => &mut colors.selection_background,
            "selection-foreground" => &mut colors.selection_foreground,
            "palette" => {
                let entry = value
                    .split_once('=')
                    .and_then(|(n, rest)| Some((n.trim().parse::<u8>().ok()?, rest.trim_start())));
                let Some((n, rest)) = entry else {
                    return Err(refuse("`palette` is `N=#rrggbb`, with N from 0 to 255"));
                };
                value = rest;
                // Colours 16 to 255 stay computed (ADR-0049 section 1), so one is checked and
                // then ignored like any other key.
                match palette.get_mut(usize::from(n)) {
                    Some(slot) => slot,
                    None => {
                        ignored += 1;
                        &mut beyond
                    }
                }
            }
            _ => {
                ignored += 1;
                continue;
            }
        };
        let Some(colour) = colour(value) else {
            return Err(refuse(&format!(
                "`{key}` is not six hex digits, such as `#0d1117`"
            )));
        };
        *slot = Some(colour);
    }
    let [
        black,
        red,
        green,
        yellow,
        blue,
        magenta,
        cyan,
        white,
        bright @ ..,
    ] = palette;
    let [
        bright_black,
        bright_red,
        bright_green,
        bright_yellow,
        rest @ ..,
    ] = bright;
    let [bright_blue, bright_magenta, bright_cyan, bright_white] = rest;
    let colors = Colors {
        black,
        red,
        green,
        yellow,
        blue,
        magenta,
        cyan,
        white,
        bright_black,
        bright_red,
        bright_green,
        bright_yellow,
        bright_blue,
        bright_magenta,
        bright_cyan,
        bright_white,
        ..colors
    };
    Ok((colors, ignored))
}

/// Parses a Ghostty theme file and makes its theme, as [`theme::parse`] does for a theme file,
/// with `overrides`, the `[colors]` table, on top. The file is lines of `key = value`. Read are
/// `background`, `foreground`, `cursor-color`, `cursor-text`, `selection-background`,
/// `selection-foreground` and `palette = N=#rrggbb` for N from 0 to 15, each colour six hex
/// digits with or without the `#`. Every other key is ignored, and one of the strings, which
/// are for the log, says how many.
///
/// # Errors
///
/// Returns an error if `bytes` is over [`crate::MAX_BYTES`] or not UTF-8, if a colour is not
/// six hex digits, with its line, or if a required colour is absent.
///
/// ```
/// use pinfold_config::{ghostty, theme::Colors};
/// let refused = ghostty::parse(b"font-size = 14\nbackground = red", Colors::default());
/// assert!(refused.unwrap_err().to_string().starts_with("line 2: `background`"));
/// ```
pub fn parse(bytes: &[u8], overrides: Colors) -> Result<(Theme, Vec<String>), Error> {
    let (colors, ignored) = colors(bytes)?;
    let (theme, mut notes) = theme::resolve(colors, overrides)?;
    if ignored > 0 {
        notes.push(format!(
            "{ignored} other Ghostty keys in the theme file are ignored"
        ));
    }
    Ok((theme, notes))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn seed(name: &str) -> Vec<u8> {
        let dir = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fuzz/seeds/ghostty_theme"
        );
        std::fs::read(format!("{dir}/{name}")).unwrap()
    }

    fn refused(text: &str) -> String {
        parse(text.as_bytes(), Colors::default())
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn real_ghostty_themes_are_read() {
        // Both files are as iTerm2-Color-Schemes ships them (`fuzz/README.md`).
        let (nord, notes) = parse(&seed("Nord"), Colors::default()).unwrap();
        assert_eq!((nord.background, nord.foreground), (0x2e_3440, 0xd8_dee9));
        assert_eq!(
            (nord.black, nord.red, nord.white),
            (0x3b_4252, 0xbf_616a, 0xe5_e9f0)
        );
        assert_eq!(
            (nord.bright_black, nord.bright_white),
            (0x59_6377, 0xec_eff4)
        );
        assert_eq!((nord.cursor, nord.cursor_text), (0xec_eff4, 0x28_2828));
        assert_eq!(nord.selection_background, 0xec_eff4);
        assert_eq!(nord.selection_foreground, 0x4c_566a);
        assert!(
            notes.iter().all(|note| !note.contains("Ghostty")),
            "{notes:?}"
        );
        let (github, _) = parse(&seed("GitHub Dark"), Colors::default()).unwrap();
        assert_eq!((github.cyan, github.bright_cyan), (0x2b_7489, 0x2b_7489));
        assert_eq!(github.selection_background, 0x3b_5070);
        // `[colors]` goes on top, as on any theme.
        let overrides = Colors {
            cursor: Some(0x00ff_0000),
            ..Colors::default()
        };
        let (github, _) = parse(&seed("GitHub Dark"), overrides).unwrap();
        assert_eq!((github.cursor, github.background), (0x00ff_0000, 0x10_1216));
    }

    #[test]
    fn other_ghostty_keys_are_counted_and_not_acted_on() {
        let (theme, notes) = parse(&seed("other-keys"), Colors::default()).unwrap();
        assert_eq!((theme.background, theme.foreground), (0x1d_1f21, 0xc5_c8c6));
        assert_eq!((theme.red, theme.bright_white), (0xcc_6666, 0xff_ffff));
        // Unset here, so derived as for any theme.
        assert_eq!(theme.cursor, theme.foreground);
        let ignored: Vec<_> = notes.iter().filter(|n| n.contains("Ghostty")).collect();
        assert_eq!(
            ignored,
            ["7 other Ghostty keys in the theme file are ignored"]
        );
        // The last of a repeated key wins, as in Ghostty.
        let (colors, ignored) = colors(b"background=000000\n background = \"#FFFFFF\" ").unwrap();
        assert_eq!((colors.background, ignored), (Some(0x00ff_ffff), 0));
    }

    #[test]
    fn bad_values_are_refused_with_their_line() {
        let nord = String::from_utf8(seed("Nord")).unwrap();
        for value in [
            "red", "#fff", "#12345g", "+12345", "#1234567", "", "\"", "#ffé",
        ] {
            let text = refused(&format!("{nord}cursor-color = {value}\n"));
            assert!(
                text.starts_with("line 23: `cursor-color`"),
                "{value}: {text}"
            );
            assert!(!text.contains('é'), "{text}");
        }
        for entry in ["3", "x=#000000", "256=#000000", "-1=#000000", "=#000000"] {
            let text = refused(&format!("# a comment\n\npalette = {entry}\n"));
            assert!(text.starts_with("line 3: `palette` is"), "{entry}: {text}");
        }
        assert!(refused("palette = 3=red").starts_with("line 1: `palette` is not"));
        assert!(refused("palette = 200=red").starts_with("line 1: `palette` is not"));
        assert!(refused("background").starts_with("line 1: `background`"));
        // A file with no colour of its own is not a theme.
        let text = refused(&nord.replace("palette = 1=#bf616a\n", ""));
        assert!(text.contains("needs the key `red`"), "{text}");
        assert!(refused("font-size = 14").contains("needs the key"));
        assert!(matches!(colors(b"\xff"), Err(Error::NotUtf8)));
        let long = vec![b'\n'; crate::MAX_BYTES as usize + 1];
        assert!(matches!(colors(&long), Err(Error::TooLarge)));
    }

    proptest! {
        #[test]
        fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
            let _ = parse(&bytes, Colors::default());
        }

        #[test]
        fn arbitrary_lines_never_panic(text in "((palette|background|cursor-text|[a-z-]{1,8})? ?=? ?[0-9]{0,3}=?#?[0-9a-fA-F\"é]{0,7}\n){0,24}") {
            let _ = parse(text.as_bytes(), Colors::default());
        }
    }
}
