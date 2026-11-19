//! OSC 133 command marks, the OSC 7 working directory, an agent CLI's OSC 7501 states and
//! OSC 9 notifications, and with toasts on the text of OSC 9, 99 and 777 notifications, all of
//! which the parser drops.
//!
//! Program output writes all of this, so any program can fake it (T4 in the threat model).
//! It is a hint to show as reported by output, and it never reaches a reply.
//!
//! A pane holds one string being read, at most `MAX_SEQUENCE` bytes, the latest state, and with
//! toasts on one notification's text of at most 120 cells until it is sent. Other strings are
//! let go at their first bytes, and malformed ones change nothing.

use std::hash::{DefaultHasher, Hash as _, Hasher as _};

use base64::Engine as _;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
use pinfold_proto::link::{MAX_TOAST, OSC_9, OSC_99, OSC_777};
use unicode_width::UnicodeWidthChar as _;

use crate::status::drawable;

/// Longest host name DNS allows. The path is limited by `MAX_SEQUENCE` alone.
const MAX_HOST: usize = 255;

/// The most cells of a program notification's text (ADR-0051 §9).
pub const TOAST_CELLS: usize = 120;

/// The most base64 decoded: enough for [`MAX_TOAST`] bytes, so a long payload costs no more.
const MAX_BASE64: usize = MAX_TOAST.div_ceil(3) * 4;

/// kitty's base64, padded or not.
const BASE64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// How far through a command the shell says it is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ShellPhase {
    /// No mark has arrived, so nothing is known.
    #[default]
    Unknown,
    /// The prompt is being drawn (`A`).
    Prompt,
    /// The user is typing a command (`B`).
    Input,
    /// The command is running (`C`).
    Running,
    /// The command has ended (`D`).
    Finished,
}

/// What the shell last reported through OSC 133.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ShellMarks {
    /// The latest mark. Marks are taken in any order, since a fake can use any order.
    pub phase: ShellPhase,
    /// How many commands have finished, wrapping. Shells send `D` and the next `A`
    /// together, so a change here is how a reader between the two sees a command end.
    pub finished: u32,
    /// The exit status of the last finished command, if it gave one.
    pub exit: Option<u8>,
    /// How many prompts have been drawn, wrapping, so a reader sees a new one at a prompt.
    pub prompts: u32,
}

impl ShellMarks {
    /// Applies the text after `133;`, and returns whether it was a mark. Options after a mark
    /// are ignored.
    fn apply(&mut self, text: &[u8]) -> bool {
        let mut fields = text.split(|&byte| byte == b';');
        self.phase = match fields.next() {
            Some(b"A") => {
                self.prompts = self.prompts.wrapping_add(1);
                ShellPhase::Prompt
            }
            Some(b"B") => ShellPhase::Input,
            Some(b"C") => ShellPhase::Running,
            Some(b"D") => {
                self.exit = match fields.next() {
                    Some(field) if !field.contains(&b'=') => match exit_status(field) {
                        Some(exit) => Some(exit),
                        None => return false,
                    },
                    _ => None,
                };
                self.finished = self.finished.wrapping_add(1);
                ShellPhase::Finished
            }
            _ => return false,
        };
        true
    }
}

/// What an agent CLI last signalled of itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentSignal {
    /// OSC 7501 `state=blocked`: it waits at its own approval prompt.
    Blocked,
    /// OSC 7501 `state=working`.
    Working,
    /// OSC 7501 `state=done`.
    Done,
    /// OSC 7501 `state=idle`.
    Idle,
    /// OSC 7501 `state=clear`: it has stopped saying anything, such as on exit.
    Clear,
    /// An OSC 9 notification. Its text is never kept.
    Notified,
}

/// What an agent CLI in the pane last signalled through output. Any program can print these,
/// so only a pane launched from an agent profile reads them (ADR-0046).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AgentMarks {
    /// The latest signal, if one has arrived.
    pub latest: Option<AgentSignal>,
    /// How many signals have arrived, wrapping, so a reader sees a repeat.
    pub signals: u32,
}

impl AgentMarks {
    fn signal(&mut self, signal: AgentSignal) {
        self.latest = Some(signal);
        self.signals = self.signals.wrapping_add(1);
    }
}

/// Reads the text after `7501;`, as codex-cli sends it: `state=blocked:app=codex:kind=...`.
/// Fields after the state are ignored, and a query such as `?` is no signal.
fn agent_state(text: &[u8]) -> Option<AgentSignal> {
    match text.split(|&byte| byte == b':').next()? {
        b"state=blocked" => Some(AgentSignal::Blocked),
        b"state=working" => Some(AgentSignal::Working),
        b"state=done" => Some(AgentSignal::Done),
        b"state=idle" => Some(AgentSignal::Idle),
        b"state=clear" => Some(AgentSignal::Clear),
        _ => None,
    }
}

/// Parses plain digits, as `u8::from_str` alone would also take a sign.
fn exit_status(field: &[u8]) -> Option<u8> {
    if !field.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(field).ok()?.parse().ok()
}

/// A program notification's text, cleaned and cut when its string ends, kept only until the
/// link takes it (ADR-0051 §9).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Toast {
    /// [`OSC_9`], [`OSC_99`] or [`OSC_777`].
    pub kind: u8,
    /// At most [`TOAST_CELLS`] cells and [`MAX_TOAST`] bytes, never empty once finished, with
    /// no control, bidi or zero-width character and no state glyph.
    pub text: String,
    /// For an OSC 99 chunk that more will follow, its id's hash and whether it was a title.
    held: Option<(u64, bool)>,
}

impl Toast {
    /// A toast of `text`, cleaned and cut, or nothing if none of it is left.
    fn new(kind: u8, text: &[u8]) -> Option<Self> {
        let mut toast = Self {
            kind,
            text: String::new(),
            held: None,
        };
        toast.push(text);
        (!toast.text.is_empty()).then_some(toast)
    }

    /// Appends `text`, a tab or line end as a space, other characters [`drawable`] doesn't
    /// pass dropped, up to the caps.
    fn push(&mut self, text: &[u8]) {
        let mut cells: usize = self.text.chars().filter_map(|c| c.width()).sum();
        for character in String::from_utf8_lossy(text).chars() {
            let character = match character {
                '\t' | '\n' | '\r' => ' ',
                _ if drawable(character) => character,
                _ => continue,
            };
            cells += character.width().unwrap_or(0);
            if cells > TOAST_CELLS || self.text.len() + character.len_utf8() > MAX_TOAST {
                break;
            }
            self.text.push(character);
        }
    }
}

/// Reads the text after `777;notify;`: a title, then the body after the next `;`.
fn rxvt(text: &[u8]) -> Option<Toast> {
    let mut parts = text
        .splitn(2, |&byte| byte == b';')
        .filter(|part| !part.is_empty());
    let mut toast = Toast::new(OSC_777, parts.next()?)?;
    if let Some(body) = parts.next() {
        toast.push(b": ");
        toast.push(body);
    }
    Some(toast)
}

/// Reads the text after `99;`, kitty's `metadata;payload`, given the chunk held before it. Only a
/// title or body is shown, from `i`, `d`, `p` and `e`; other keys are ignored and every other
/// payload type, a query among them, is dropped and never answered. A chunk with `d=0` is held
/// until the rest of the same id; anything else replaces it.
fn kitty(text: &[u8], before: Option<Toast>) -> Option<Toast> {
    let at = text.iter().position(|&byte| byte == b';')?;
    let (mut id, mut done, mut title, mut base64) = (DefaultHasher::new(), true, true, false);
    for field in text[..at].split(|&byte| byte == b':') {
        let Some(equals) = field.iter().position(|&byte| byte == b'=') else {
            continue;
        };
        match field.split_at(equals) {
            (b"i", value) => value.hash(&mut id),
            (b"d", b"=0") => done = false,
            (b"p", b"=title") => title = true,
            (b"p", b"=body") => title = false,
            (b"p", _) => return None,
            (b"e", b"=1") => base64 = true,
            _ => {}
        }
    }
    let id = id.finish();
    let payload = &text[at + 1..];
    let decoded;
    let payload = if base64 {
        let cut = if payload.len() > MAX_BASE64 {
            &payload[..MAX_BASE64]
        } else {
            payload
        };
        decoded = BASE64.decode(cut).ok()?;
        &decoded[..]
    } else {
        payload
    };
    let mut toast = match before {
        Some(mut before) if before.held.is_some_and(|(held, _)| held == id) => {
            if before.held.is_some_and(|(_, was_title)| was_title) && !title {
                before.push(b": ");
            }
            before.push(payload);
            before
        }
        _ => Toast::new(OSC_99, payload)?,
    };
    toast.held = (!done).then_some((id, title));
    Some(toast)
}

/// A working directory from OSC 7.
///
/// The path is a well-formed absolute path and nothing more. Before acting on it a caller
/// must check that it is an existing directory, which is left until then because the I/O
/// thread cannot wait on a path a program chose. It is also text for the screen only
/// after the display rules for hostile names (T6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkingDirectory {
    /// The host named by the URL, which may be empty. Any host but this machine's means
    /// the path is on another machine, such as over SSH, and the caller has to compare.
    pub host: String,
    /// The decoded path, starting with `/`.
    pub path: String,
}

/// Parses a `file://host/path` URL. Anything else is refused, not repaired.
fn working_directory(url: &[u8]) -> Option<WorkingDirectory> {
    let rest = url.strip_prefix(b"file://")?;
    let (host, path) = rest.split_at(rest.iter().position(|&byte| byte == b'/')?);
    let plain = |byte: &u8| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_');
    if host.len() > MAX_HOST || !host.iter().all(plain) {
        return None;
    }
    let path = String::from_utf8(percent_decoded(path)?).ok()?;
    // These checks come after decoding, so `%2e%2e` and `%0a` cannot get past them.
    let unsafe_path =
        path.chars().any(char::is_control) || path.split('/').any(|name| name == "..");
    (!unsafe_path).then(|| WorkingDirectory {
        host: String::from_utf8_lossy(host).into_owned(),
        path,
    })
}

/// Decodes a URL path once. A raw `?` or `#` would start another part of the URL.
fn percent_decoded(path: &[u8]) -> Option<Vec<u8>> {
    let mut decoded = Vec::with_capacity(path.len());
    let mut bytes = path.iter();
    while let Some(&byte) = bytes.next() {
        match byte {
            b'%' => decoded.push(hex(bytes.next())? << 4 | hex(bytes.next())?),
            b'?' | b'#' => return None,
            _ => decoded.push(byte),
        }
    }
    Some(decoded)
}

fn hex(byte: Option<&u8>) -> Option<u8> {
    let digit = char::from(*byte?).to_digit(16)?;
    u8::try_from(digit).ok()
}

/// The marks and working directory of one pane, fed OSC strings by `vt::Scan`.
#[derive(Debug, Default)]
pub(crate) struct Marks {
    /// The string so far, without its terminator. `Scan` stops feeding at `MAX_SEQUENCE`.
    pub(crate) text: Vec<u8>,
    /// Whether the string is still worth keeping.
    open: bool,
    pub(crate) shell: ShellMarks,
    pub(crate) cwd: Option<WorkingDirectory>,
    pub(crate) agent: AgentMarks,
    /// Whether `[alerts] program_notifications` is on, so notification text is kept.
    pub(crate) toasts: bool,
    /// The latest notification's text, until the link takes it. Boxed, so an idle pane pays a
    /// pointer for it.
    pub(crate) toast: Option<Box<Toast>>,
}

impl Marks {
    /// Turns notification text on or off. Off forgets any held.
    pub(crate) fn set_toasts(&mut self, on: bool) {
        self.toasts = on;
        if !on {
            self.toast = None;
        }
    }

    /// Takes the finished notification, if one is in flight. A held OSC 99 chunk stays.
    pub(crate) fn take_toast(&mut self) -> Option<Toast> {
        if self.toast.as_ref()?.held.is_some() {
            return None;
        }
        self.toast.take().map(|toast| *toast)
    }

    /// Starts a string. One cut off at the limit was never finished, so it is forgotten.
    pub(crate) fn start(&mut self) {
        self.text.clear();
        self.open = true;
    }

    /// Takes the next byte of a string, including the one that ends it.
    ///
    /// BEL and ESC finish a string as they do for the parser, which acts at the ESC of an
    /// ST. CAN and SUB cancel one, though the parser would still act on what it has. Returns
    /// whether the byte ended an OSC 133 mark.
    #[inline]
    pub(crate) fn byte(&mut self, byte: u8) -> bool {
        if !self.open {
            return false;
        }
        let marked = match byte {
            0x07 | 0x1b => {
                self.open = false;
                self.finish()
            }
            0x18 | 0x1a => {
                self.open = false;
                false
            }
            _ => {
                self.text.push(byte);
                let matches = |prefix: &&[u8]| prefix.iter().zip(&self.text).all(|(a, b)| a == b);
                let toasts: &[&[u8]] = if self.toasts {
                    &[b"99;", b"777;notify;"]
                } else {
                    &[]
                };
                self.open = [b"133;".as_slice(), b"7;", b"7501;", b"9;"]
                    .iter()
                    .chain(toasts)
                    .any(matches);
                false
            }
        };
        // A string's text is gone once read or let go, a notification's included.
        if !self.open {
            self.text.clear();
        }
        marked
    }

    /// Lets go of a string cut off at the limit, with its text.
    pub(crate) fn cut(&mut self) {
        self.open = false;
        self.text.clear();
    }

    /// Applies a finished string, and returns whether it was an OSC 133 mark.
    fn finish(&mut self) -> bool {
        if let Some(text) = self.text.strip_prefix(b"133;") {
            return self.shell.apply(text);
        }
        if let Some(url) = self.text.strip_prefix(b"7;")
            && let Some(cwd) = working_directory(url)
        {
            self.cwd = Some(cwd);
        }
        if let Some(signal) = self.text.strip_prefix(b"7501;").and_then(agent_state) {
            self.agent.signal(signal);
        }
        // ConEmu's `9;4;...` progress and other numbered OSC 9 commands are not notifications.
        if let Some(body) = self.text.strip_prefix(b"9;")
            && !body.first().is_some_and(u8::is_ascii_digit)
        {
            self.agent.signal(AgentSignal::Notified);
        }
        if self.toasts {
            self.read_toast();
        }
        false
    }

    /// Reads a finished notification's text. Out of line, so a parser with toasts off pays no
    /// stack for it.
    #[cold]
    #[inline(never)]
    fn read_toast(&mut self) {
        let text = &self.text;
        let toast = if let Some(body) = text
            .strip_prefix(b"9;")
            .filter(|body| !body.first().is_some_and(u8::is_ascii_digit))
        {
            Toast::new(OSC_9, body)
        } else if let Some(body) = text.strip_prefix(b"777;notify;") {
            rxvt(body)
        } else if let Some(body) = text.strip_prefix(b"99;") {
            kitty(body, self.toast.take().map(|toast| *toast))
        } else {
            return;
        };
        self.toast = toast.map(Box::new);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn finished(marks: &mut Marks, string: &[u8]) {
        marks.start();
        for &byte in string.iter().chain(b"\x07") {
            marks.byte(byte);
        }
    }

    fn text(toast: Option<Toast>) -> Option<String> {
        toast.map(|toast| toast.text)
    }

    #[test]
    fn off_keeps_no_notification_text() {
        let mut marks = Marks::default();
        for string in [&b"9;secret"[..], b"99;;secret", b"777;notify;secret;body"] {
            finished(&mut marks, string);
            assert_eq!(marks.toast, None);
            assert!(marks.text.is_empty(), "{string:?}");
        }
        assert_eq!(marks.agent.signals, 1, "OSC 9 still only moves the counter");
        marks.set_toasts(true);
        finished(&mut marks, b"99;i=1:d=0;held");
        assert!(marks.toast.is_some());
        marks.set_toasts(false);
        assert_eq!(marks.toast, None, "off forgets a held chunk");
    }

    #[test]
    fn each_kind_gives_its_text_and_progress_and_queries_give_none() {
        let mut marks = Marks::default();
        marks.set_toasts(true);
        type Case<'a> = (&'a [u8], Option<(u8, &'a str)>);
        let cases: [Case<'_>; 11] = [
            (b"9;Build done", Some((OSC_9, "Build done"))),
            (b"9;4;1;50", None),
            (b"777;notify;Build;done", Some((OSC_777, "Build: done"))),
            (b"777;notify;;done", Some((OSC_777, "done"))),
            (b"777;notify;Build", Some((OSC_777, "Build"))),
            (b"99;;Hello", Some((OSC_99, "Hello"))),
            (b"99;i=1:p=body:a=focus;Hi", Some((OSC_99, "Hi"))),
            (b"99;e=1;SGVsbG8=", Some((OSC_99, "Hello"))),
            (b"99;e=1;SGVsbG8", Some((OSC_99, "Hello"))),
            (b"99;e=1;not*base64", None),
            (b"99;i=1:p=?;", None),
        ];
        for (string, want) in cases {
            finished(&mut marks, string);
            let got = marks.take_toast().map(|toast| (toast.kind, toast.text));
            let want = want.map(|(kind, text)| (kind, text.to_owned()));
            assert_eq!(got, want, "{:?}", String::from_utf8_lossy(string));
        }
        for kind in ["close", "alive", "buttons", "icon"] {
            finished(&mut marks, format!("99;i=1:p={kind};x").as_bytes());
            assert_eq!(marks.take_toast(), None, "{kind}");
        }
    }

    #[test]
    fn a_kitty_title_waits_for_its_body_and_another_id_replaces_it() {
        let mut marks = Marks::default();
        marks.set_toasts(true);
        finished(&mut marks, b"99;i=7:d=0;Build");
        assert_eq!(marks.take_toast(), None, "held until the rest");
        finished(&mut marks, b"99;i=7:p=body;done");
        assert_eq!(text(marks.take_toast()).as_deref(), Some("Build: done"));
        finished(&mut marks, b"99;i=7:d=0;Build");
        finished(&mut marks, b"99;i=8:p=body;other");
        assert_eq!(text(marks.take_toast()).as_deref(), Some("other"));
        finished(&mut marks, b"99;i=7:d=0;Build");
        finished(&mut marks, b"99;i=7:p=close;");
        assert_eq!(marks.toast, None, "a dropped chunk drops the held one");
    }

    #[test]
    fn hostile_text_is_cleaned_and_cut_to_120_cells() {
        let mut marks = Marks::default();
        marks.set_toasts(true);
        finished(&mut marks, "9;a\u{202e}b\u{200b}c◆\u{9b}31m\td".as_bytes());
        assert_eq!(text(marks.take_toast()).as_deref(), Some("abc31m d"));
        finished(&mut marks, &[b"9;".as_slice(), &[b'a'; 4000]].concat());
        assert_eq!(text(marks.take_toast()).map(|t| t.len()), Some(TOAST_CELLS));
        finished(&mut marks, format!("9;{}", "日".repeat(200)).as_bytes());
        assert_eq!(text(marks.take_toast()), Some("日".repeat(60)));
        finished(
            &mut marks,
            format!("9;{}", "e\u{301}".repeat(200)).as_bytes(),
        );
        let combined = text(marks.take_toast()).unwrap_or_default();
        assert!(
            combined.len() <= MAX_TOAST,
            "marks without cells stop at the byte cap"
        );
        finished(&mut marks, b"9;\xff\xfe");
        assert_eq!(
            text(marks.take_toast()).as_deref(),
            Some("\u{fffd}\u{fffd}")
        );
        finished(&mut marks, "9;\u{202e}◆".as_bytes());
        assert_eq!(marks.take_toast(), None, "nothing left is no toast");
    }

    /// The window decodes only what `Notice::Toast` allows, so every toast must fit it.
    fn fits(toast: &Toast) -> bool {
        let cells: usize = toast.text.chars().filter_map(|c| c.width()).sum();
        !toast.text.is_empty()
            && cells <= TOAST_CELLS
            && toast.text.len() <= MAX_TOAST
            && toast.text.chars().all(drawable)
    }

    proptest! {
        #[test]
        fn any_kitty_payload_decodes_within_the_cap(
            meta in "[ipde=a-z0-9:]{0,24}",
            payload in proptest::collection::vec(any::<u8>(), 0..4096),
        ) {
            let mut text = meta.into_bytes();
            text.push(b';');
            text.extend_from_slice(&payload);
            if let Some(toast) = kitty(&text, None) {
                prop_assert!(fits(&toast), "{toast:?}");
            }
        }

        #[test]
        fn a_hostile_decoded_payload_is_cleaned(decoded in "[\\x00-\\x1f\\x7f\u{80}-\u{9f}\u{202a}-\u{202e}a◆]{1,2000}") {
            let encoded = base64::engine::general_purpose::STANDARD.encode(&decoded);
            let text = [b"e=1;".as_slice(), encoded.as_bytes()].concat();
            if let Some(toast) = kitty(&text, None) {
                prop_assert!(fits(&toast), "{toast:?}");
            }
        }

        #[test]
        fn any_notification_fits_the_link(body in proptest::collection::vec(any::<u8>(), 0..4096)) {
            for prefix in [&b"9;"[..], b"777;notify;", b"99;;", b"99;i=1:d=0;"] {
                let mut marks = Marks::default();
                marks.set_toasts(true);
                finished(&mut marks, &[prefix, body.as_slice()].concat());
                if let Some(toast) = &marks.toast {
                    prop_assert!(fits(toast), "{toast:?}");
                }
            }
        }
    }
}
