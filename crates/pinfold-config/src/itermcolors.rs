//! An iTerm2 `.itermcolors` file read as a theme (ADR-0049 section 8).
//!
//! The file is an XML property list and untrusted input, so the parser here is our own and
//! bounded: six element names, four levels, the five predefined entities, and no panic on
//! what the file holds.

use crate::Error;
use crate::theme::{self, Colors, Theme};

/// More elements than a preset with light and dark variants of every colour holds.
const MAX_ELEMENTS: usize = 4096;

const ELEMENTS: &str = "only `plist`, `dict`, `key`, `real`, `integer` and `string` are read";
const PAIRS: &str = "a `dict` holds a `key` and then its value";

struct Reader<'a> {
    text: &'a str,
    rest: &'a str,
    elements: usize,
}

impl<'a> Reader<'a> {
    /// The message names a key from the lists below and never quotes the file.
    fn refuse(&self, what: &str) -> Error {
        let read = &self.text[..self.text.len() - self.rest.len()];
        Error::Invalid(format!("line {}: {what}", read.matches('\n').count() + 1))
    }

    /// What the next tag holds between `<` and `>`, past any white space.
    fn tag(&mut self) -> Result<&'a str, Error> {
        let next = self.rest.trim_start().strip_prefix('<');
        let Some((tag, rest)) = next.and_then(|rest| rest.split_once('>')) else {
            return Err(self.refuse("an element is expected here"));
        };
        self.rest = rest;
        self.elements += 1;
        if self.elements > MAX_ELEMENTS {
            return Err(self.refuse("it has too many elements"));
        }
        Ok(tag)
    }

    /// The text of the element `name`, whose opening tag was just read, with its entities.
    fn text(&mut self, name: &str) -> Result<String, Error> {
        let (text, rest) = self.rest.split_once('<').unwrap_or((self.rest, ""));
        let end = rest
            .strip_prefix('/')
            .and_then(|rest| rest.strip_prefix(name));
        let Some(rest) = end.and_then(|rest| rest.strip_prefix('>')) else {
            return Err(self.refuse(&format!("`{name}` holds text and then ends")));
        };
        self.rest = rest;
        let (mut plain, mut text) = (String::new(), text);
        while let Some((before, after)) = text.split_once('&') {
            let entity = after.split_once(';').and_then(|(entity, rest)| {
                let entity = match entity {
                    "amp" => '&',
                    "lt" => '<',
                    "gt" => '>',
                    "quot" => '"',
                    "apos" => '\'',
                    _ => return None,
                };
                Some((entity, rest))
            });
            let Some((entity, rest)) = entity else {
                return Err(self.refuse("only the five predefined entities are known"));
            };
            plain.push_str(before);
            plain.push(entity);
            text = rest;
        }
        plain.push_str(text);
        Ok(plain)
    }

    /// The `key` that comes next in a `dict`, or nothing at its end.
    fn key(&mut self) -> Result<Option<String>, Error> {
        match self.tag()? {
            "/dict" => Ok(None),
            "key" => self.text("key").map(Some),
            _ => Err(self.refuse(PAIRS)),
        }
    }

    /// The rest of a colour's `dict`: the colour, when it is `wanted` under that name, and
    /// whether it names a colour space other than sRGB.
    fn colour(&mut self, wanted: Option<&str>) -> Result<(Option<u32>, bool), Error> {
        let (mut parts, mut other) = ([None; 3], false);
        while let Some(key) = self.key()? {
            let leaf = match self.tag()? {
                leaf @ ("real" | "integer" | "string") => leaf,
                "dict" => return Err(self.refuse("it is nested deeper than four levels")),
                _ => return Err(self.refuse(ELEMENTS)),
            };
            let text = self.text(leaf)?;
            let part = match key.as_str() {
                "Red Component" => 0,
                "Green Component" => 1,
                "Blue Component" => 2,
                "Color Space" => {
                    other |= text != "sRGB";
                    continue;
                }
                _ => continue,
            };
            let Some(name) = wanted else { continue };
            // A NaN is in no range, and `parse` reads `inf` and `nan` too.
            let number = text.trim().parse::<f64>().ok();
            let Some(share) = number.filter(|n| leaf != "string" && (0.0..=1.0).contains(n)) else {
                let what = format!("`{key}` of `{name}` is not a number from 0 to 1");
                return Err(self.refuse(&what));
            };
            parts[part] = Some((share * 255.0).round() as u32);
        }
        let Some(name) = wanted else {
            return Ok((None, other));
        };
        match parts {
            [Some(red), Some(green), Some(blue)] => {
                Ok((Some(red << 16 | green << 8 | blue), other))
            }
            _ => Err(self.refuse(&format!(
                "`{name}` needs a `Red`, a `Green` and a `Blue Component`"
            ))),
        }
    }
}

/// Where the colour an iTerm2 key names goes, or nothing for a key that is not read.
fn slot<'a>(colors: &'a mut Colors, key: &str) -> Option<&'a mut Option<u32>> {
    let ansi = key
        .strip_prefix("Ansi ")
        .and_then(|n| n.strip_suffix(" Color"));
    Some(match (key, ansi.unwrap_or("")) {
        ("Background Color", _) => &mut colors.background,
        ("Foreground Color", _) => &mut colors.foreground,
        ("Cursor Color", _) => &mut colors.cursor,
        ("Cursor Text Color", _) => &mut colors.cursor_text,
        ("Selection Color", _) => &mut colors.selection_background,
        ("Selected Text Color", _) => &mut colors.selection_foreground,
        (_, "0") => &mut colors.black,
        (_, "1") => &mut colors.red,
        (_, "2") => &mut colors.green,
        (_, "3") => &mut colors.yellow,
        (_, "4") => &mut colors.blue,
        (_, "5") => &mut colors.magenta,
        (_, "6") => &mut colors.cyan,
        (_, "7") => &mut colors.white,
        (_, "8") => &mut colors.bright_black,
        (_, "9") => &mut colors.bright_red,
        (_, "10") => &mut colors.bright_green,
        (_, "11") => &mut colors.bright_yellow,
        (_, "12") => &mut colors.bright_blue,
        (_, "13") => &mut colors.bright_magenta,
        (_, "14") => &mut colors.bright_cyan,
        (_, "15") => &mut colors.bright_white,
        _ => return None,
    })
}

/// The colours an `.itermcolors` file sets, and how many are in another space than sRGB.
fn colors(bytes: &[u8]) -> Result<(Colors, usize), Error> {
    if bytes.len() as u64 > crate::MAX_BYTES {
        return Err(Error::TooLarge);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| Error::NotUtf8)?;
    let (rest, elements) = (text, 0);
    let mut reader = Reader {
        text,
        rest,
        elements,
    };
    let mut tag = reader.tag()?;
    if tag.starts_with("?xml") {
        tag = reader.tag()?;
    }
    if tag.starts_with("!DOCTYPE") {
        // Skipped unread, but for the `[` of declarations of its own, which define entities.
        if tag.contains('[') {
            return Err(reader.refuse("the DOCTYPE may not declare entities or anything else"));
        }
        tag = reader.tag()?;
    }
    // Only `plist` has attributes, its version, which is not read.
    let plist = tag == "plist" || tag.starts_with("plist ");
    if !plist || reader.tag()? != "dict" {
        return Err(reader.refuse("a `plist` holding one `dict` is expected"));
    }
    let (mut colors, mut others) = (Colors::default(), 0);
    while let Some(key) = reader.key()? {
        match reader.tag()? {
            "dict" => {
                let slot = slot(&mut colors, &key);
                let (colour, other) = reader.colour(slot.is_some().then_some(key.as_str()))?;
                if let Some(slot) = slot {
                    (*slot, others) = (colour, others + usize::from(other));
                }
            }
            // A setting that is not a colour, so not one of ours.
            leaf @ ("real" | "integer" | "string") => drop(reader.text(leaf)?),
            _ => return Err(reader.refuse(ELEMENTS)),
        }
    }
    if reader.tag()? != "/plist" || !reader.rest.trim().is_empty() {
        return Err(reader.refuse("the `plist` ends after its `dict`, and the file with it"));
    }
    Ok((colors, others))
}

/// Parses an iTerm2 `.itermcolors` file and makes its theme, as [`theme::parse`] does for a
/// theme file, with `overrides`, the `[colors]` table, on top. Read are `Ansi 0 Color` to
/// `Ansi 15 Color`, `Background Color`, `Foreground Color`, `Cursor Color`, `Cursor Text
/// Color`, `Selection Color` and `Selected Text Color`, each from its `Red`, `Green` and `Blue
/// Component`, a number from 0 to 1. Every other key is ignored. The strings are for the log.
///
/// shortcut: components are read as sRGB whatever `Color Space` says, and one of the strings
/// says so for another space. Convert P3 when a user's theme shows the difference.
///
/// # Errors
///
/// Returns an error, with its line, if `bytes` is not such a property list: an element other
/// than `plist`, `dict`, `key`, `real`, `integer` and `string`, more than four levels, a
/// `DOCTYPE` with declarations, an entity other than the five predefined ones, or a component
/// that is absent or not from 0 to 1. Also if `bytes` is over [`crate::MAX_BYTES`] or not
/// UTF-8, or a required colour is absent.
///
/// ```
/// use pinfold_config::{itermcolors, theme::Colors};
/// let refused = itermcolors::parse(b"<plist>\n<array/>", Colors::default());
/// assert!(refused.unwrap_err().to_string().starts_with("line 2: a `plist` holding"));
/// ```
pub fn parse(bytes: &[u8], overrides: Colors) -> Result<(Theme, Vec<String>), Error> {
    let (colors, others) = colors(bytes)?;
    let (theme, mut notes) = theme::resolve(colors, overrides)?;
    if others > 0 {
        notes.push(format!(
            "{others} colours in the theme file are in another colour space and are read as sRGB"
        ));
    }
    Ok((theme, notes))
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn seed(name: &str) -> String {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../fuzz/seeds/itermcolors");
        std::fs::read_to_string(format!("{dir}/{name}.itermcolors")).unwrap()
    }

    fn refused(text: &str) -> String {
        parse(text.as_bytes(), Colors::default())
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn real_itermcolors_files_are_read() {
        // Both files are as iTerm2-Color-Schemes ships them (`fuzz/README.md`). Its Ghostty
        // file for Nord has a lighter bright black than this, its source.
        let (nord, notes) = parse(seed("Nord").as_bytes(), Colors::default()).unwrap();
        assert_eq!((nord.background, nord.foreground), (0x2e_3440, 0xd8_dee9));
        assert_eq!(
            (nord.black, nord.red, nord.white),
            (0x3b_4252, 0xbf_616a, 0xe5_e9f0)
        );
        assert_eq!(
            (nord.bright_black, nord.bright_white),
            (0x4c_566a, 0xec_eff4)
        );
        assert_eq!((nord.cursor, nord.cursor_text), (0xec_eff4, 0x28_2828));
        assert_eq!(nord.selection_background, 0xec_eff4);
        assert_eq!(nord.selection_foreground, 0x4c_566a);
        assert!(notes.iter().all(|note| !note.contains("sRGB")), "{notes:?}");
        let overrides = Colors {
            cursor: Some(0x00ff_0000),
            ..Colors::default()
        };
        let (github, _) = parse(seed("GitHub Dark").as_bytes(), overrides).unwrap();
        assert_eq!((github.cursor, github.background), (0x00ff_0000, 0x10_1216));
        assert_eq!((github.cyan, github.bright_green), (0x2b_7489, 0x56_d364));
    }

    #[test]
    fn another_colour_space_is_read_as_srgb_and_said() {
        let nord = seed("Nord").replacen("<string>sRGB</string>", "<string>P3</string>", 2);
        let (theme, notes) = parse(nord.as_bytes(), Colors::default()).unwrap();
        assert_eq!(theme.black, 0x3b_4252);
        let said: Vec<_> = notes.iter().filter(|n| n.contains("sRGB")).collect();
        assert_eq!(
            said,
            ["2 colours in the theme file are in another colour space and are read as sRGB"]
        );
        // Entities, an integer, a setting that is no colour and a bad colour that is not read.
        let text = "<?xml version=\"1.0\"?><plist version=\"1.0\"><dict>\
            <key>Name</key><string>a &lt;&amp;&gt; &quot;b&apos;</string>\
            <key>Badge Color</key><dict><key>Red Component</key><real>7</real></dict>\
            <key>Background &#67;olor</key><string/>";
        assert!(refused(text).contains("five predefined"));
        let text = text.replace(
            "&#67;olor</key><string/>",
            "Color</key><dict>\
            <key>Red Component</key><integer>1</integer><key>Green Component</key>\
            <real> 0.5 </real><key>Blue Component</key><real>0</real></dict></dict></plist>\n",
        );
        let (colors, others) = colors(text.as_bytes()).unwrap();
        assert_eq!((colors.background, others), (Some(0x00ff_8000), 0));
    }

    #[test]
    fn what_is_not_a_bounded_property_list_is_refused_with_a_reason() {
        let nord = seed("Nord");
        let real = "<real>0.23137255012989044</real>";
        for (from, to, reason) in [
            // An entity definition, and its use.
            (
                "1.0.dtd\">",
                "1.0.dtd\" [<!ENTITY a \"aaaa\">]>",
                "line 2: the DOCTYPE may not declare",
            ),
            ("sRGB", "&a;", "line 12: only the five predefined entities"),
            ("sRGB", "&amp", "line 12: only the five predefined entities"),
            // Deep nesting.
            (
                real,
                "<dict><key>a</key><real>0</real></dict>",
                "line 16: it is nested deeper than four levels",
            ),
            // A missing component, and components out of range or not numbers.
            (
                "<key>Red Component</key>",
                "<key>Red</key>",
                "line 17: `Ansi 0 Color` needs a `Red`",
            ),
            (
                real,
                "<real>1.01</real>",
                "line 16: `Red Component` of `Ansi 0 Color` is not",
            ),
            (real, "<real>-0.1</real>", "line 16: `Red Component` of"),
            (real, "<real>NaN</real>", "line 16: `Red Component` of"),
            (real, "<real>inf</real>", "line 16: `Red Component` of"),
            (real, "<real></real>", "line 16: `Red Component` of"),
            (real, "<string>0.5</string>", "line 16: `Red Component` of"),
            // Anything different.
            (
                real,
                "<array><real>0</real></array>",
                "line 16: only `plist`",
            ),
            (real, "<real>0<b/></real>", "line 16: `real` holds text"),
            (real, "<real>0", "line 16: `real` holds text"),
            (real, "<!-- 0 -->", "line 16: only `plist`"),
            (
                "<key>Ansi 1 Color</key>",
                "<true/>",
                "line 18: a `dict` holds",
            ),
            (
                "<key>Ansi 1 Color</key>",
                "stray",
                "line 17: an element is expected",
            ),
            (
                "<plist version=\"1.0\">",
                "<plistx>",
                "line 3: a `plist` holding",
            ),
            ("</plist>", "</plist><dict></dict>", "the `plist` ends"),
            ("</plist>", "", "an element is expected"),
        ] {
            assert!(nord.contains(from), "{from}");
            let text = refused(&nord.replacen(from, to, 1));
            assert!(
                text.starts_with(reason) || text.contains(reason),
                "{to}: {text}"
            );
            assert!(!text.contains("aaaa"), "{text}");
        }
        // A file with a colour absent is not a theme.
        let text = refused(&nord.replace("Ansi 1 Color", "Ansi 01 Color"));
        assert!(text.contains("needs the key `red`"), "{text}");
        assert!(refused("").starts_with("line 1: an element is expected"));
        let many = format!("<plist><dict>{}", "<key></key><real></real>".repeat(2048));
        assert!(refused(&many).contains("too many elements"));
        assert!(matches!(colors(b"\xff"), Err(Error::NotUtf8)));
        let long = vec![b'\n'; crate::MAX_BYTES as usize + 1];
        assert!(matches!(colors(&long), Err(Error::TooLarge)));
    }

    #[test]
    fn it_is_found_before_the_ghostty_file_and_gets_the_file_checks() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let themes = dir.path().join("themes");
        std::fs::create_dir(&themes).unwrap();
        let config = dir.path().join("pinfold.toml");
        let file = themes.join("Nord.itermcolors");
        let p3 = seed("Nord").replace(">sRGB<", ">P3<");
        for (path, text) in [
            (&config, "theme = \"Nord\"\n"),
            (&themes.join("Nord"), "background = 000000\n"),
            (&file, &p3),
        ] {
            std::fs::write(path, text).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let (theme, notes) = theme::load(&config).unwrap();
        assert_eq!(theme.background, 0x2e_3440);
        assert!(
            notes.iter().any(|n| n.starts_with("22 colours")),
            "{notes:?}"
        );
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o666)).unwrap();
        let refused = theme::load(&config).unwrap_err();
        assert!(matches!(refused, theme::Error::File(Error::Writable)));
    }

    proptest! {
        #[test]
        fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
            let _ = parse(&bytes, Colors::default());
        }

        #[test]
        fn arbitrary_elements_never_panic(text in "(</?(plist|dict|key|real|integer|string|!DOCTYPE \\[|a)>|Ansi 1 Color|Red Component|0\\.5|&amp;|&|é|\n){0,48}") {
            let _ = parse(text.as_bytes(), Colors::default());
        }
    }
}
