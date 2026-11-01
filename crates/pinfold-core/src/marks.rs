//! OSC 133 command marks and the OSC 7 working directory, which the parser drops.
//!
//! Program output writes all of this, so any program can fake it (T4 in the threat model).
//! It is a hint to show as reported by output, and it never reaches a reply.
//!
//! A pane holds one string being read, at most `MAX_SEQUENCE` bytes, and the latest state.
//! Other strings are let go at their first bytes, and malformed ones change nothing.

/// Longest host name DNS allows. The path is limited by `MAX_SEQUENCE` alone.
const MAX_HOST: usize = 255;

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
}

impl ShellMarks {
    /// Applies the text after `133;`. Options after a mark are ignored.
    fn apply(&mut self, text: &[u8]) {
        let mut fields = text.split(|&byte| byte == b';');
        self.phase = match fields.next() {
            Some(b"A") => ShellPhase::Prompt,
            Some(b"B") => ShellPhase::Input,
            Some(b"C") => ShellPhase::Running,
            Some(b"D") => {
                self.exit = match fields.next() {
                    Some(field) if !field.contains(&b'=') => match exit_status(field) {
                        Some(exit) => Some(exit),
                        None => return,
                    },
                    _ => None,
                };
                self.finished = self.finished.wrapping_add(1);
                ShellPhase::Finished
            }
            _ => return,
        };
    }
}

/// Parses plain digits, as `u8::from_str` alone would also take a sign.
fn exit_status(field: &[u8]) -> Option<u8> {
    if !field.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(field).ok()?.parse().ok()
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
}

impl Marks {
    /// Starts a string. One cut off at the limit was never finished, so it is forgotten.
    pub(crate) fn start(&mut self) {
        self.text.clear();
        self.open = true;
    }

    /// Takes the next byte of a string, including the one that ends it.
    ///
    /// BEL and ESC finish a string as they do for the parser, which acts at the ESC of an
    /// ST. CAN and SUB cancel one, though the parser would still act on what it has.
    #[inline]
    pub(crate) fn byte(&mut self, byte: u8) {
        if !self.open {
            return;
        }
        match byte {
            0x07 | 0x1b => {
                self.open = false;
                self.finish();
            }
            0x18 | 0x1a => self.open = false,
            _ => {
                self.text.push(byte);
                let matches = |prefix: &&[u8]| prefix.iter().zip(&self.text).all(|(a, b)| a == b);
                self.open = [b"133;".as_slice(), b"7;"].iter().any(matches);
            }
        }
    }

    fn finish(&mut self) {
        if let Some(text) = self.text.strip_prefix(b"133;") {
            self.shell.apply(text);
        } else if let Some(url) = self.text.strip_prefix(b"7;")
            && let Some(cwd) = working_directory(url)
        {
            self.cwd = Some(cwd);
        }
    }
}
