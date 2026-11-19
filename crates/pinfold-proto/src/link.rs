//! The binary protocol between the core and a window (ADR-0028), as pure encoding and decoding.
//!
//! Each message is a little-endian `u32` length, a type byte and a body, and the length counts
//! the type byte and the body. Fields are fixed-width and little-endian, and text or bytes run
//! to the end of the body, so no field carries a length of its own.
//!
//! The core sends [`Notice::Hello`] as soon as the link opens. A window answers with
//! [`Request::Hello`] at the lower of the two versions, so a window serves a core one version
//! older than itself, as an update leaves running (ADR-0043). [`Decoder`] takes the hello first,
//! returns [`Error::Version`] for a peer further apart, and remembers the version both speak:
//! a message newer than that is [`Error::Newer`]. The side that sees an error closes the link,
//! and the core keeps its PTYs.
//!
//! Any error means the stream can no longer be trusted, so a [`Decoder`] keeps returning it.
//!
//! ```
//! use pinfold_proto::link::{Decoder, Request, VERSION};
//!
//! let mut wire = Vec::new();
//! Request::Hello { version: VERSION }.encode(&mut wire)?;
//! Request::Input { pane: 0, bytes: b"ls\n" }.encode(&mut wire)?;
//!
//! let mut decoder = Decoder::default();
//! let mut input = wire.as_slice();
//! assert_eq!(
//!     decoder.next_request(&mut input)?,
//!     Some(Request::Hello { version: VERSION })
//! );
//! assert_eq!(
//!     decoder.next_request(&mut input)?,
//!     Some(Request::Input { pane: 0, bytes: b"ls\n" })
//! );
//! assert_eq!(decoder.next_request(&mut input)?, None);
//! # Ok::<(), pinfold_proto::link::Error>(())
//! ```

use std::ops::{Range, RangeInclusive};

use crate::{BOLD, Cell, Frame, ITALIC, MouseEncoding, MouseMode, Palette};

/// The protocol's version. Any change to the format below raises it.
pub const VERSION: u16 = 26;

/// The oldest version this side still speaks, one before [`VERSION`] (ADR-0043).
pub const OLDEST: u16 = VERSION - 1;

/// The first version whose [`Notice::Status`] carries `naming` and its times (ADR-0051).
const TIMED_STATUS: u16 = 16;

/// The first version with [`Request::Inbox`] and every pane's tab in a [`Notice::Layout`]
/// (ADR-0051).
const INBOX: u16 = 17;

/// The first version with [`Notice::Webhook`] (ADR-0051).
const WEBHOOK_SINCE: u16 = 18;

/// The first version with [`Request::Answer`], its refusals, and a [`Notice::Status`] that names
/// the pane's agent profile (ADR-0051).
const ANSWER: u16 = 19;

/// The first version with [`Notice::InboxExtra`] (ADR-0051).
const INBOX_EXTRA: u16 = 20;

/// The first version whose [`Notice::InboxExtra`] carries an agent's context use and steps
/// (ADR-0051, slice 9).
pub const AGENT_FILES: u16 = 21;

/// The first version with [`ApprovalAction::Rewind`], [`Notice::Checkpoint`] and
/// [`Notice::Restored`] (ADR-0050).
pub const REWIND: u16 = 23;

/// The first version with [`Tool::Rewind`], the palette's "Rewind agent changes" (ADR-0050,
/// slice 7).
pub const REWIND_VIEW: u16 = 24;

/// Why a run has no checkpoint, by [`Notice::Checkpoint`]'s `missing`: the words the strip shows
/// after `no checkpoint ·`, none for 0, which clears it (ADR-0050 section 4).
pub const MISSING: [&str; 6] = [
    "",
    "took over wait_s",
    "git not found",
    "the take failed",
    "too many runs",
    "busy",
];

/// The longest error a [`Notice::Restored`] carries, written by the core and naming no path.
pub const MAX_RESTORE_ERROR: usize = 200;

/// The first version with [`Notice::Outside`], [`Request::OpenTabIn`] and
/// [`Refusal::Folder`] (ADR-0051, slice 10).
pub const OUTSIDE: u16 = 22;

/// The first version with [`Notice::Toast`] and [`Request::Toasts`] (ADR-0051 §9, slice 11).
/// 23 and 24 are ADR-0050's.
pub const TOAST: u16 = 25;

/// The most bytes of a [`Notice::Toast`]'s text: 120 cells of four-byte characters.
pub const MAX_TOAST: usize = 480;

/// [`Notice::Toast`]'s `kind` for OSC 9 text.
pub const OSC_9: u8 = 0;
/// [`Notice::Toast`]'s `kind` for OSC 99, kitty's notifications.
pub const OSC_99: u8 = 1;
/// [`Notice::Toast`]'s `kind` for OSC 777 `notify`.
pub const OSC_777: u8 = 2;

/// The first version with [`Notice::Progress`] (ADR-0051 §9, slice 12).
pub const PROGRESS: u16 = 26;

/// OSC 9;4 states, in ConEmu's numbering, which [`progress`] keeps in a progress byte's top
/// three bits: no progress.
pub const HIDDEN: u8 = 0;
/// A percent done.
pub const NORMAL: u8 = 1;
/// Failed, at a percent.
pub const FAILED: u8 = 2;
/// Busy with no percent.
pub const INDETERMINATE: u8 = 3;
/// Paused, at a percent.
pub const PAUSED: u8 = 4;

/// A progress byte, as the core keeps it and [`Notice::Progress`] carries it (ADR-0051 §9,
/// amendment of slice 12): `state` in the top three bits and `percent`, clamped to 100, in the
/// low five, in steps of 100/31. Hidden is always 0 and indeterminate has no percent, so a byte
/// changes only when what it shows does.
///
/// ```
/// use pinfold_proto::link::{FAILED, HIDDEN, NORMAL, progress, progress_parts};
///
/// assert_eq!(progress_parts(progress(NORMAL, 100)), (NORMAL, 100));
/// assert_eq!(progress_parts(progress(FAILED, 250)), (FAILED, 100));
/// assert_eq!(progress(HIDDEN, 40), 0);
/// ```
#[must_use]
pub fn progress(state: u8, percent: u8) -> u8 {
    match state {
        NORMAL | FAILED | PAUSED => {
            // Round to nearest, so 0 and 100 come back exact.
            let steps = (u16::from(percent.min(100)) * 31 + 50) / 100;
            state << 5 | steps as u8
        }
        INDETERMINATE => INDETERMINATE << 5,
        _ => 0,
    }
}

/// A progress byte's state and percent. A state above [`PAUSED`] is never sent, and reads as
/// hidden.
#[must_use]
pub fn progress_parts(byte: u8) -> (u8, u8) {
    match byte >> 5 {
        state @ (NORMAL | FAILED | PAUSED) => {
            let percent = (u16::from(byte & 31) * 100 + 15) / 31;
            (state, percent as u8)
        }
        INDETERMINATE => (INDETERMINATE, 0),
        _ => (HIDDEN, 0),
    }
}

/// The most agents outside PinFold an open inbox lists (ADR-0051 §8).
pub const MAX_OUTSIDE: u8 = 16;

/// The longest directory a [`Notice::Outside`] or [`Request::OpenTabIn`] carries.
pub const MAX_DIR: usize = 1024;

/// The most bytes of steps a [`Notice::InboxExtra`] carries: three of 80 cells and two
/// newlines fit, as `pinfold agents peek` prints at most 1 KiB a pane.
pub const MAX_STEPS: usize = 1024;

/// How many steps a [`Notice::InboxExtra`] carries at most.
pub const STEPS: usize = 3;

/// The longest branch a [`Notice::InboxExtra`] carries: 40 bytes of printable ASCII (ADR-0051).
pub const MAX_BRANCH: usize = 40;

/// The longest profile or key name a [`Request::Answer`] carries.
pub const MAX_ANSWER_NAME: usize = 64;

/// What a person answered an agent's prompt with from the inbox (ADR-0051).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    /// The profile's `approve` key.
    Approve,
    /// Ctrl+C.
    Interrupt,
}

/// The bytes Interrupt types: Ctrl+C.
pub const INTERRUPT: &[u8] = b"\x03";

/// The bytes a key from the window's key table writes, for a profile's `approve` key: `Enter`,
/// `Tab`, `Escape`, `Backspace`, `Space`, or one printable ASCII character as itself. Only keys
/// that write the same bytes in every terminal mode, so the core sends what the window would.
///
/// ```
/// use pinfold_proto::link::key_bytes;
///
/// assert_eq!(key_bytes("Enter"), Some(&b"\r"[..]));
/// assert_eq!(key_bytes("1"), Some(&b"1"[..]));
/// assert_eq!(key_bytes("enter"), None);
/// assert_eq!(key_bytes("yes"), None);
/// ```
#[must_use]
pub fn key_bytes(name: &str) -> Option<&[u8]> {
    match name {
        "Enter" => Some(b"\r"),
        "Tab" => Some(b"\t"),
        "Escape" => Some(b"\x1b"),
        "Backspace" => Some(b"\x7f"),
        "Space" => Some(b" "),
        _ => matches!(name.as_bytes(), [b'!'..=b'~']).then_some(name.as_bytes()),
    }
}

/// Most rows or columns a screen can have, which the core clamps its panes to.
pub const MAX_DIMENSION: u16 = 1024;

/// Most tabs in a session. A strip of this many still gives each a box on a full HD screen, and
/// past it the core refuses the command.
pub const MAX_TABS: usize = 32;

/// Most panes in a session, across its tabs. Each holds a PTY's descriptor and a shell, and a
/// pane's cold history alone can reach 2 MB, so this bounds what a window can make the core hold.
/// macOS starts a process with 256 descriptors, which leaves this plenty of room. Past it the core
/// refuses the command.
pub const MAX_PANES: usize = 64;

const PREFIX: usize = 4;
const KIND: usize = 1;
const CELL: usize = 12;
const ROW_NUMBER: usize = 2;
const COUNT: usize = 2;
/// Every field of a frame before its rows, the last of them the count of rows.
const FRAME_HEAD: usize = 43 + COUNT;

/// The largest value of a message's length: a full frame of the largest screen the grid allows,
/// 1024 rows of 1024 cells. Nothing else comes near it, as a selection stops at about 2 MB.
pub const MAX_MESSAGE: usize =
    KIND + FRAME_HEAD + MAX_DIMENSION as usize * (ROW_NUMBER + MAX_DIMENSION as usize * CELL);

/// Longest hyperlink target in a [`Notice::Linked`]. Browsers take more, but no link meant for
/// a person is this long, and the core answers a longer one as no link.
pub const MAX_URI: usize = 2048;

/// Longest command in a [`Notice::WasRunning`], the cap a session snapshot keeps.
pub const MAX_COMMAND: usize = 4096;

/// The most input a pane holds waiting for its program, so the most one paste can usefully be.
pub const MAX_INPUT: usize = 1024 * 1024;

/// The largest value of a length from a window: a paste of [`MAX_INPUT`]. A window is less trusted
/// than the core, and the core's whole budget is 4 MB, so this stays far below [`MAX_MESSAGE`].
pub const MAX_REQUEST: usize = KIND + size_of::<u32>() + MAX_INPUT;

/// Bytes of the markers around a bracketed paste, which share the input queue with it.
const BRACKETS: usize = 12;

/// The longest paste that fits the input queue with its brackets, for any program.
pub const MAX_PASTE: usize = MAX_INPUT - BRACKETS;

/// Room a [`Decoder`] keeps between messages, so one large paste does not stay in the core's
/// budget. This holds a full frame of 200x80 cells.
const KEEP: usize = 256 * 1024;

/// The indexes a tab can have on the link.
const LAST_TAB: RangeInclusive<u16> = 0..=MAX_TABS as u16 - 1;

/// Bytes of a search ahead of its query.
const HEAD: usize = 11;

/// Appends each field in little-endian order. With a `kind`, evaluates to it.
macro_rules! put {
    ($out:ident; $($field:expr),+) => {
        $($out.extend_from_slice(&$field.to_le_bytes());)+
    };
    ($out:ident, $kind:expr, $($field:expr),+) => {{
        put!($out; $($field),+);
        $kind
    }};
}

const HELLO: u8 = 0;

const INPUT: u8 = 1;
const PASTE: u8 = 2;
const RESIZE: u8 = 3;
const SCROLL: u8 = 4;
const REVEAL: u8 = 5;
const SEARCH: u8 = 6;
const COPY: u8 = 7;
const FULL_FRAME: u8 = 8;
const QUIT: u8 = 9;
const NEW_TAB: u8 = 10;
const SWITCH_TAB: u8 = 11;
const STEP_TAB: u8 = 12;
const SPLIT: u8 = 13;
const CLOSE_PANE: u8 = 14;
const FOCUS: u8 = 15;
const MOVE_FOCUS: u8 = 16;
const MOVE_BORDER: u8 = 17;
const ZOOM: u8 = 18;
const APPROVAL_ANSWER: u8 = 19;
const WAS_RUNNING_ANSWER: u8 = 20;
const LINK: u8 = 21;
const OPEN_TOOL: u8 = 22;
const LIST_MARKDOWN: u8 = 23;
const OPEN_MARKDOWN: u8 = 24;
const PALETTE: u8 = 25;
const INBOX_OPEN: u8 = 26;
const ANSWER_KIND: u8 = 27;
const OPEN_TAB_IN: u8 = 28;
const TOASTS: u8 = 29;

const FRAME: u8 = 1;
const CWD: u8 = 2;
const SEARCHED: u8 = 3;
const COPIED: u8 = 4;
const LAYOUT: u8 = 5;
const RAISE: u8 = 6;
const REFUSED: u8 = 7;
const ACTIVITY: u8 = 8;
const STATUS: u8 = 9;
const APPROVAL: u8 = 10;
const WAS_RUNNING: u8 = 11;
const LINKED: u8 = 12;
const UNHARDENED: u8 = 13;
const MARKDOWN: u8 = 14;
const COLOURS: u8 = 15;
const SECRETS: u8 = 16;
const WEBHOOK: u8 = 17;
const INBOX_EXTRA_KIND: u8 = 18;
const OUTSIDE_KIND: u8 = 19;
const CHECKPOINT: u8 = 20;
const RESTORED: u8 = 21;
const TOAST_KIND: u8 = 22;
const PROGRESS_KIND: u8 = 23;
/// [`Notice::InboxExtra`]'s `ctx` byte when it has none.
const NO_CTX: u8 = 255;

/// How many agent states there are. [`Notice::Status`] numbers them in the order of the
/// contributor rule: needs you, running, done, error, idle.
pub const STATES: u8 = 5;

const FLAG_FULL: u8 = 1;
const FLAG_CURSOR: u8 = 1 << 1;
const FLAG_APP_CURSOR: u8 = 1 << 2;
const FLAG_ALTERNATE_SCROLL: u8 = 1 << 3;
const FLAGS: u8 = FLAG_FULL | FLAG_CURSOR | FLAG_APP_CURSOR | FLAG_ALTERNATE_SCROLL;

/// The bits of [`Cell::foreground`] and [`Cell::background`] that hold a colour.
const COLOUR: u32 = 0x00ff_ffff;

const BLANK: Cell = Cell {
    character: ' ',
    foreground: 0,
    background: 0,
};

/// A tab in a layout: its focused pane.
const LAYOUT_TAB: usize = 4;
/// A pane in a layout: its number and the four sides of its rectangle.
const LAYOUT_PANE: usize = 4 + 4 * 2;
/// A pane of the session in a layout: its number and its tab's index.
const LAYOUT_MEMBER: usize = 4 + 1;

/// Why bytes could not be encoded or decoded. This holds sizes and tags, never message contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// A message is over [`MAX_MESSAGE`].
    #[error("a message of {size} bytes is over the limit")]
    TooLarge {
        /// Its size in bytes.
        size: u64,
    },
    /// A message ended before its fields did.
    #[error("a message ended early")]
    Truncated,
    /// A message went on after its fields ended.
    #[error("{bytes} bytes follow the end of a message")]
    Trailing {
        /// How many.
        bytes: usize,
    },
    /// A message's type is not one this side receives.
    #[error("unknown message type {kind}")]
    UnknownKind {
        /// The type byte.
        kind: u8,
    },
    /// An enum or flags field held a value it cannot.
    #[error("bad tag {tag} for {field}")]
    BadTag {
        /// The field's name.
        field: &'static str,
        /// The value.
        tag: u8,
    },
    /// A number is outside what the field allows.
    #[error("{field} of {value} is out of bounds")]
    OutOfBounds {
        /// The field's name.
        field: &'static str,
        /// The value. Only rows, columns and counts get here.
        value: u64,
    },
    /// A cell holds a surrogate or a value past the last character.
    #[error("a cell holds an invalid character")]
    BadCharacter,
    /// A cell has bits set outside its colour and style.
    #[error("a cell holds an invalid colour")]
    BadColour,
    /// Text is not UTF-8.
    #[error("text is not UTF-8")]
    BadText,
    /// The peer speaks another version.
    #[error("the peer speaks version {theirs}, and this is version {ours}")]
    Version {
        /// This side's version.
        ours: u16,
        /// The peer's version.
        theirs: u16,
    },
    /// The first message was not a version check.
    #[error("message type {kind} came before the version check")]
    NoHello {
        /// The type byte.
        kind: u8,
    },
    /// A second version check arrived.
    #[error("a second version check arrived")]
    RepeatedHello,
    /// A message the version both sides speak does not have.
    #[error("message type {kind} needs link version {since}")]
    Newer {
        /// The type byte.
        kind: u8,
        /// The first version that has it.
        since: u16,
    },
}

/// Longest query, in characters, which bounds the work per cell and what a search carries.
pub const MAX_QUERY: usize = 256;

/// A cell, in numbers that survive output and eviction, though not a change of width.
///
/// `column` has no bound on the link, as `u16::MAX` means the end of a row and reading a position
/// clamps it to what is held.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Position {
    /// The row's number at the pane's width. A new width reflows rows and numbers them back from
    /// the same end, so a position taken before it names other text afterwards (ADR-0029).
    pub row: usize,
    /// The column at that width.
    pub column: u16,
}

/// The cells of a match, or of a selection in either order, both ends included.
///
/// This is a plain value: it holds nothing output can invalidate. Reading one clamps it to the
/// rows still held.
// shortcut: a screen row's number names whatever is there now, so text a program overwrites
// after selection is copied as the new text, and the alternate screen shares its numbers with
// the primary. Clear selections on damage when the window draws them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Selection {
    /// Where it started.
    pub anchor: Position,
    /// Where it ends now.
    pub focus: Position,
}

/// A built-in tool a window opens by key (ADR-0045). `md` needs a file, which no window
/// request names yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    /// `pinfold files`, the file manager.
    Files,
    /// `pinfold diff`, the git diff viewer.
    Diff,
    /// The diff viewer on the run in the pane, since its launch checkpoint, which the core picks
    /// (ADR-0050 section 6). Since [`REWIND_VIEW`].
    Rewind,
}

/// Which way a search runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Toward newer rows.
    Forward,
    /// Toward older rows.
    Backward,
}

/// Where a split puts its second pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitWay {
    /// Beside the first.
    Right,
    /// Under the first.
    Down,
}

/// A way across a pane's border.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// Towards column 0.
    Left,
    /// Away from column 0.
    Right,
    /// Towards row 0.
    Up,
    /// Away from row 0.
    Down,
}

/// A cross-pane operation that needs a person's approval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ApprovalAction {
    /// Read screen text or the last marked output.
    Read,
    /// Type exact text into another pane.
    Send,
    /// Trust a workspace's layout file. The first text is the canonical directory and the rest
    /// are every `run` line; `caller` is 0 and `target` is 1 when the directory was trusted with
    /// another layout. Only allowing once answers it.
    Trust,
    /// Run a method only the owner may call, asked from a terminal outside every pane. The
    /// first text names the method and the rest what it touches; `caller` and `target` are 0.
    /// Only allowing once answers it.
    Owner,
    /// Undo an agent run's changes from a checkpoint (ADR-0050 section 6). The texts, every one
    /// escaped by the core, are the directory, when and why the checkpoint was taken, who asks,
    /// the counts, notes, then each path shown, `! ` before one changed later and `  ` before
    /// the rest, and `and N more`; `caller` is 0 and `target` the run's pane. Only allowing once
    /// answers it. Since version 23.
    Rewind,
}

impl ApprovalAction {
    /// Stable API spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Send => "send",
            Self::Trust => "trust",
            Self::Owner => "owner",
            Self::Rewind => "rewind",
        }
    }

    /// Whether only allowing once answers it, so it never becomes a lasting grant and never
    /// batches with another prompt.
    #[must_use]
    pub const fn is_once(self) -> bool {
        matches!(self, Self::Trust | Self::Owner | Self::Rewind)
    }

    /// The first link version that carries it.
    #[must_use]
    pub const fn since(self) -> u16 {
        match self {
            Self::Owner => 10,
            Self::Rewind => REWIND,
            Self::Read | Self::Send | Self::Trust => OLDEST,
        }
    }
}

/// A real key's answer to an approval prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ApprovalAnswer {
    /// Allow the requests already in this batch once.
    AllowOnce,
    /// Allow this caller, target and action for thirty minutes.
    AllowForThirtyMinutes,
    /// Refuse the batch.
    Deny,
}

/// One cross-pane prompt. Every send in a batch remains distinct and exact.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Approval<'a> {
    /// Stable id returned in the answer.
    pub id: u64,
    /// Pane asking.
    pub caller: u32,
    /// Pane being acted on.
    pub target: u32,
    /// Requested action.
    pub action: ApprovalAction,
    /// Exact text of each batched send, empty for reads.
    pub texts: Vec<&'a str>,
}

/// Why the core did not carry out a command. The link stays up.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    /// The session holds [`MAX_TABS`].
    #[error("This session has as many tabs as it can hold")]
    Tabs,
    /// The session holds [`MAX_PANES`].
    #[error("This session has as many panes as it can hold")]
    Panes,
    /// The tab has no room for another pane.
    #[error("No room for another pane here")]
    Room,
    /// The shell or tool would not start.
    #[error("The shell or tool would not start")]
    Spawn,
    /// Input found no room: in the pane's queue, for a program that has stopped reading, or in
    /// the window, for a core that has.
    #[error("Input not sent: the program or the core is behind")]
    Input,
    /// The pane answered from the inbox has gone. Since version 19, as are those below.
    #[error("Not answered: the pane has gone")]
    Gone,
    /// The pane no longer needs you.
    #[error("Not answered: the pane no longer needs you")]
    NotWaiting,
    /// The pane was not launched from an agent profile.
    #[error("Not answered: not an agent profile's pane")]
    NotAgent,
    /// The pane is protected (`api.protect`).
    #[error("Not answered: the pane is protected")]
    Protected,
    /// The profile or key the prompt showed is not the one the core read when it started.
    #[error(
        "Not answered: the agent's profile changed since the core started; restart the session \
         to use it"
    )]
    ProfileChanged,
    /// The folder an outside agent's row named is not one a tab may open in: missing, relative,
    /// a link, or writable by someone else (ADR-0051 §8). Since version 22.
    #[error("Not opened: the folder is missing, relative or not private")]
    Folder,
}

/// A rectangle of cells, from the top left of the space for panes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rect {
    /// Left column.
    pub x: u16,
    /// Top row.
    pub y: u16,
    /// Width.
    pub columns: u16,
    /// Height.
    pub rows: u16,
}

/// A message from a window to the core. Text and bytes borrow from the decoder's buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request<'a> {
    /// The version check. It is first each way, and this format never changes.
    Hello {
        /// The sender's [`VERSION`].
        version: u16,
    },
    /// Bytes to write to a pane.
    Input {
        /// The pane.
        pane: u32,
        /// What to write.
        bytes: &'a [u8],
    },
    /// Text to paste into a pane.
    Paste {
        /// The pane.
        pane: u32,
        /// What to paste, as the window read it. This format keeps any control character, and
        /// `Reactor::paste` is what cleans them (T7 in the threat model), so a paste must reach
        /// a program through that and never as [`Request::Input`]. Past [`MAX_PASTE`] this does
        /// not encode, so the window refuses such a paste before it gets here. Parts would be
        /// separate pastes to a program, each in its own brackets.
        text: &'a str,
    },
    /// The space for panes, which the core lays the active tab out in, and until which it sends
    /// no layout or frame. Each side must be from 1 to [`MAX_DIMENSION`], so the window clamps its
    /// grid before sending, as a tiny or huge surface can give anything from 0 to `u16::MAX`.
    Resize {
        /// Number of rows.
        rows: u16,
        /// Number of columns.
        columns: u16,
    },
    /// Rows to move the view into history, or back if negative.
    Scroll {
        /// The pane.
        pane: u32,
        /// How many rows.
        lines: i32,
    },
    /// Scrolls the view just far enough to show this row.
    Reveal {
        /// The pane.
        pane: u32,
        /// The row's number.
        row: usize,
    },
    /// Replaces any search running.
    Search {
        /// The pane.
        pane: u32,
        /// Where to start.
        from: Position,
        /// Which way to go.
        direction: Direction,
        /// What to find.
        query: &'a str,
    },
    /// Asks for a selection's text, as [`Notice::Copied`].
    Copy {
        /// The pane.
        pane: u32,
        /// What to copy.
        selection: Selection,
    },
    /// Asks for the OSC 8 hyperlink under a cell, as [`Notice::Linked`].
    Link {
        /// The pane.
        pane: u32,
        /// The cell.
        at: Position,
    },
    /// Asks for a full frame of a shown pane, when the window cannot apply a change to the one
    /// it holds. The core sends a pane's frames while [`Notice::Layout`] shows it, and no others.
    FullFrame {
        /// The pane.
        pane: u32,
    },
    /// The window is closing.
    Quit,
    /// Opens a tab with a new shell after the others, and activates it. The core picks the
    /// program, as ADR-0028 lets nothing a window sends name one.
    NewTab,
    /// Activates a tab.
    SwitchTab {
        /// Its index, from 0. Below [`MAX_TABS`], and an index with no tab does nothing.
        index: u16,
    },
    /// Activates the next or previous tab, wrapping at the ends.
    StepTab {
        /// Whether towards the next.
        forward: bool,
    },
    /// Splits a pane in half and focuses a new shell in the second half.
    Split {
        /// The pane.
        pane: u32,
        /// Where the new pane goes.
        way: SplitWay,
    },
    /// Closes a pane and ends its program. The last pane of a tab closes the tab, and the last
    /// tab ends the session, so the window asks the user first.
    ClosePane {
        /// The pane.
        pane: u32,
    },
    /// Focuses a pane of the active tab.
    Focus {
        /// The pane.
        pane: u32,
    },
    /// Moves focus across a border of the focused pane.
    MoveFocus {
        /// Which way.
        side: Side,
    },
    /// Moves a border of the focused pane.
    MoveBorder {
        /// Which way.
        side: Side,
        /// By how many cells, from 1 to [`MAX_DIMENSION`].
        cells: u16,
    },
    /// Toggles whether the focused pane fills its tab.
    Zoom,
    /// Opens a tool in a new pane right of `pane`, in the directory the system says `pane`'s
    /// shell is in, never its OSC 7 report (T4). Like a shell, the window names no program or
    /// path. Since version 12.
    OpenTool {
        /// The pane.
        pane: u32,
        /// Which tool.
        tool: Tool,
    },
    /// Asks for the `*.md` files in the directory the system says `pane`'s shell is in, as
    /// [`Notice::Markdown`]. Since version 13.
    ListMarkdown {
        /// The pane.
        pane: u32,
    },
    /// Opens `pinfold md` on a file a [`Notice::Markdown`] for `pane` listed, right of `pane`.
    /// The core opens only a name it listed, still a regular file in that directory. Since
    /// version 13.
    OpenMarkdown {
        /// The pane.
        pane: u32,
        /// The name as the notice gave it, escaped.
        name: &'a str,
    },
    /// Opens a new tab whose shell starts in `dir`, the folder of an agent outside PinFold that
    /// an open inbox's row named, once the person confirmed PinFold's own prompt. The core
    /// checks `dir` again and refuses with [`Refusal::Folder`] unless it is a private existing
    /// absolute directory (ADR-0051 §8). Since version 22.
    OpenTabIn {
        /// The folder, at most [`MAX_DIR`] bytes.
        dir: &'a str,
    },
    /// Answers [`Notice::WasRunning`] for a pane: with `run`, the core types the command and a
    /// newline into the pane's shell, if it offered that. Either way it forgets the command.
    WasRunningAnswer {
        /// The pane.
        pane: u32,
        /// Whether the person pressed Enter.
        run: bool,
    },
    /// Answers a prompt. Only the inherited window link can carry this message.
    ApprovalAnswer {
        /// Prompt id.
        id: u64,
        /// The key's meaning.
        answer: ApprovalAnswer,
    },
    /// The colours every pane's named and indexed colours resolve through from now on, history
    /// included (ADR-0049). Sent after the hello and after each change. Since version 14.
    Palette(Palette),
    /// The inbox opened or closed in the window (ADR-0051). Since version 17.
    Inbox {
        /// Whether it is open now.
        open: bool,
    },
    /// Whether `[alerts] program_notifications` is on, sent after the hello and on each change.
    /// The core starts with it off and turns it off when the window goes; off, it keeps and sends
    /// no program notification text (ADR-0051 §9). Since version 25.
    Toasts {
        /// Whether it is on now.
        on: bool,
    },
    /// A person confirmed Approve or Interrupt on a ◆ pane of the inbox (ADR-0051). Only the
    /// window's confirmation key sends it; no API method does. Since version 19.
    Answer {
        /// The pane.
        pane: u32,
        /// What to send.
        choice: Choice,
        /// The profile name the prompt showed, at most [`MAX_ANSWER_NAME`] bytes.
        profile: &'a str,
        /// The key name the prompt showed, from [`key_bytes`]'s table; empty for Interrupt.
        key: &'a str,
    },
}

/// A message from the core to a window. Text borrows from the decoder's buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice<'a> {
    /// The version check, the same bytes as [`Request::Hello`].
    Hello {
        /// The sender's [`VERSION`].
        version: u16,
    },
    /// A pane's screen changed. [`encode_frame`] writes this one.
    Frame(FrameUpdate<'a>),
    /// The last directory a pane reported, as program output wrote it. Empty if none.
    Cwd {
        /// The pane.
        pane: u32,
        /// The path, which is only a hint.
        path: &'a str,
    },
    /// The last search ended, and its match is in view if it had one.
    Searched {
        /// The pane.
        pane: u32,
        /// The match.
        found: Option<Selection>,
    },
    /// The text of a [`Request::Copy`].
    Copied {
        /// The pane.
        pane: u32,
        /// The text.
        text: &'a str,
    },
    /// The target of a [`Request::Link`], as the program wrote it, or empty if the cell has no
    /// link. Program output chose it, so it is untrusted.
    Linked {
        /// The pane.
        pane: u32,
        /// The cell asked about.
        at: Position,
        /// The URI.
        uri: &'a str,
    },
    /// The session's tabs and what the active one shows. [`encode_layout`] writes this one. A
    /// pane that left it has closed or is hidden, and no tabs at all means the session ended.
    Layout(Layout<'a>),
    /// Someone asked for a window while this one is open, so it comes to the front (ADR-0028).
    Raise,
    /// Which tabs printed something since they were last shown.
    Activity {
        /// One bit per tab, lowest first, by the order of the last [`Notice::Layout`]. Thirty-two
        /// bits hold [`MAX_TABS`].
        tabs: u32,
        /// The tabs, in the same bits, with a pane that raised a "needs you" alert while hidden
        /// and needs you still.
        alerts: u32,
    },
    /// A command was not carried out.
    Refused {
        /// Why.
        reason: Refusal,
    },
    /// A restored pane was running `command` when the session was saved (ADR-0034). Nothing
    /// runs until the window answers with [`Request::WasRunningAnswer`].
    WasRunning {
        /// The pane.
        pane: u32,
        /// Whether the core would run it, which it will not once redaction changed it.
        runnable: bool,
        /// The redacted command, which is only a hint. At most [`MAX_COMMAND`] bytes.
        command: &'a str,
    },
    /// The state of a pane that names its tab, or, since version 16, of any pane in the inbox
    /// (ADR-0051). Below 16 the wire has no `naming` or times: a window reads them as `true`
    /// and 0, and the core sends only naming panes.
    Status {
        /// The pane.
        pane: u32,
        /// Below [`STATES`].
        state: u8,
        /// Whether the pane's agent reported it, rather than output's marks, which can fake it.
        reported: bool,
        /// Whether the pane has its tab's focus, so the tab shows this state.
        naming: bool,
        /// Whether the pane is in the inbox: an agent profile's, or one that reported since it
        /// opened. Since version 17; below it, a pane that does not name its tab.
        listed: bool,
        /// Whole seconds the pane has been in `state` when sent, saturating.
        since: u32,
        /// Whole seconds since the pane's program started when sent, saturating.
        started: u32,
        /// The agent profile the pane was launched from, as an index into the config's
        /// profiles. Since version 19; nothing below it.
        profile: Option<u8>,
        /// The reported message, which the core has cleaned of control and bidi characters.
        message: &'a str,
    },
    /// A cross-pane action waiting for a real window key.
    Approval(Approval<'a>),
    /// The core lacks the hardened runtime on macOS, or could not tell, so a same-user process
    /// may read its memory (ADR-0044). Sent once after the hello, and never by a hardened core.
    Unhardened {
        /// Whether the core read its own signing flags. False means the check failed.
        checked: bool,
    },
    /// The `*.md` files a [`Request::ListMarkdown`] found, sorted. Since version 13.
    Markdown {
        /// The pane.
        pane: u32,
        /// One name a line, with control and bidi characters escaped, so none holds a line
        /// break. Empty if there are none.
        names: &'a str,
    },
    /// The colours a pane's program set with OSC 12 and OSC 11, sent when either changes or is
    /// reset (ADR-0049). Since version 14.
    Colours {
        /// The pane.
        pane: u32,
        /// The cursor's colour, or nothing for the window's own.
        cursor: Option<u32>,
        /// The pane's default background, or nothing for the palette's.
        background: Option<u32>,
    },
    /// On macOS, a sandboxed agent can read secret-looking variables in the user's other panes
    /// (ADR-0047). Sent at most once a core session, never a value. Since version 15.
    Secrets {
        /// How many names look like secrets.
        count: u32,
        /// The first few, escaped and joined by `, `, ending in `…` if more.
        names: &'a str,
    },
    /// A webhook is configured (ADR-0051). Sent after the hello, and again when `failed`
    /// changes. Since version 18.
    Webhook {
        /// The URL's host only, never its path or query, which can hold a secret.
        host: &'a str,
        /// Whether `curl` was found, without which nothing is posted.
        sender: bool,
        /// How many alerts were given up on.
        failed: u32,
    },
    /// One of the agents outside PinFold an open inbox lists after its own rows, from Claude
    /// Code's session files with `[inbox] agent_files` on, sent at each refresh and kept by
    /// neither side once it closes (ADR-0051 §8). A refresh sends rows `0..count` in order, and
    /// one with `count` 0 and an empty `dir` when there are none. Since version 22.
    Outside {
        /// This row's place, below `count`.
        index: u8,
        /// How many rows this refresh has, at most [`MAX_OUTSIDE`].
        count: u8,
        /// Seconds since its session file changed.
        secs: u32,
        /// Its folder as the session file names it, a hint only: absolute, at most [`MAX_DIR`]
        /// bytes, with no control or bidi character.
        dir: &'a str,
    },
    /// A program's notification, OSC 9, 99 or 777, with `[alerts] program_notifications` on:
    /// cleaned of control and bidi characters and cut to 120 cells by the core, which keeps
    /// none of it once sent. At most one per pane per 10 s and one per 2 s in all. Never a state
    /// (ADR-0051 §9). Since version 25.
    Toast {
        /// The pane that printed it.
        pane: u32,
        /// [`OSC_9`], [`OSC_99`] or [`OSC_777`].
        kind: u8,
        /// The text, never empty, at most [`MAX_TOAST`] bytes.
        text: &'a str,
    },
    /// A pane's OSC 9;4 progress changed, at most once a second (ADR-0051 §9). A pane whose
    /// program exits sends 0. Since version 26.
    Progress {
        /// The pane.
        pane: u32,
        /// A byte from [`progress`]; [`progress_parts`] reads it.
        progress: u8,
    },
    /// What an open inbox's row shows beside the pane's status, read when it opens and every
    /// 5 s while open, and kept by neither side once it closes (ADR-0051 §7). Since version 20.
    InboxExtra {
        /// The listed pane.
        pane: u32,
        /// The git branch of the pane's directory, at most [`MAX_BRANCH`] bytes of printable
        /// ASCII, a detached head's first 7 hex digits, or empty for none.
        branch: &'a str,
        /// The agent's context use as a whole percent, at most 100, from its own session file
        /// with `[inbox] agent_files` on. Since version 21; none below it.
        ctx: Option<u8>,
        /// Its last steps, oldest first, at most [`STEPS`] joined by newlines and
        /// [`MAX_STEPS`] bytes, none empty and none holding a control or bidi character. Since
        /// version 21; empty below it.
        steps: &'a str,
    },
    /// Why the run in a pane has no checkpoint, for the strip under it, sent when that changes:
    /// an index into [`MISSING`], 0 once a later checkpoint succeeded (ADR-0050). Since version
    /// 23.
    Checkpoint {
        /// The run's agent pane.
        pane: u32,
        /// Below the length of [`MISSING`].
        missing: u8,
    },
    /// A `checkpoint.restore` the window allowed has ended, for its flash (ADR-0050). Since
    /// version 23.
    Restored {
        /// Files written back or deleted.
        undid: u32,
        /// Files skipped as changed since shown, or failed.
        skipped: u32,
        /// Empty on success; otherwise the core's own words, at most [`MAX_RESTORE_ERROR`]
        /// bytes and never a path.
        error: &'a str,
    },
}

/// The session as the window draws it.
///
/// Only [`Decoder`] makes one, so its lists are already checked: at most [`MAX_TABS`] tabs and
/// [`MAX_PANES`] panes, each pane once, and each rectangle inside the largest screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout<'a> {
    /// The index of the active tab. It is below the count of tabs, or 0 if there are none.
    pub active: u16,
    /// The pane keys go to. It can be missing from [`Layout::panes`] in a window too small for it.
    pub focused: u32,
    /// How many panes the session holds, shown or not, up to [`MAX_PANES`].
    pub total: u16,
    tabs: &'a [u8],
    panes: &'a [u8],
    members: &'a [u8],
}

impl Layout<'_> {
    /// Returns each tab's focused pane, in order. That pane's directory names the tab.
    pub fn tabs(&self) -> impl Iterator<Item = u32> + '_ {
        self.tabs
            .as_chunks::<LAYOUT_TAB>()
            .0
            .iter()
            .filter_map(|tab| Reader(tab).u32().ok())
    }

    /// Returns each pane the active tab shows, and where.
    pub fn panes(&self) -> impl Iterator<Item = (u32, Rect)> + '_ {
        self.panes
            .as_chunks::<LAYOUT_PANE>()
            .0
            .iter()
            .filter_map(|pane| read_pane(pane).ok())
    }

    /// Returns every pane of the session and its tab's index, tab by tab, each tab's in its
    /// order, since version 17; none below it.
    pub fn members(&self) -> impl Iterator<Item = (u32, u8)> + '_ {
        self.members
            .as_chunks::<LAYOUT_MEMBER>()
            .0
            .iter()
            .filter_map(|member| Some((Reader(member).u32().ok()?, member[4])))
    }

    /// Returns whether no tab is left, which ends the window.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    fn write(&self, version: u16, out: &mut Vec<u8>) {
        let (tabs, panes) = (self.tabs.len() / LAYOUT_TAB, self.panes.len() / LAYOUT_PANE);
        put!(out; self.active, self.focused, self.total, tabs as u16, panes as u16);
        out.extend_from_slice(self.tabs);
        out.extend_from_slice(self.panes);
        if version >= INBOX {
            out.extend_from_slice(self.members);
        }
    }
}

/// A pane's screen as sent: its state, and the rows that changed since the last frame.
///
/// Only [`Decoder`] makes one, so every row in it is already checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameUpdate<'a> {
    /// The pane.
    pub pane: u32,
    /// Whether this holds every row.
    pub full: bool,
    /// The core's number for the first row of the frame.
    pub top: usize,
    /// The rows the view can reach.
    pub held: Range<usize>,
    rows: u16,
    columns: u16,
    cursor: Option<(u16, u16)>,
    app_cursor: bool,
    mouse: MouseMode,
    mouse_encoding: MouseEncoding,
    alternate_scroll: bool,
    clipboard_writes: u32,
    changed: u16,
    /// `changed` rows, each a row number and its cells.
    records: &'a [u8],
}

impl FrameUpdate<'_> {
    /// Writes this onto `frame`, reusing its allocation.
    ///
    /// Returns false and leaves `frame` as it was if this holds changes to a frame of another
    /// size. The window then sends [`Request::FullFrame`].
    ///
    /// A full frame's rows have all arrived by now, so a short message cannot grow `frame`.
    ///
    /// ```
    /// use pinfold_proto::link::{Decoder, Notice, VERSION, encode_frame};
    /// use pinfold_proto::{Cell, Frame};
    ///
    /// let cell = Cell { character: 'a', foreground: 0, background: 0 };
    /// let screen = Frame { rows: 1, columns: 1, cells: vec![cell], ..Frame::default() };
    /// let mut wire = Vec::new();
    /// Notice::Hello { version: VERSION }.encode(&mut wire)?;
    /// encode_frame(&mut wire, 0, &screen, 0, 0..1, None)?;
    ///
    /// let (mut decoder, mut input) = (Decoder::default(), wire.as_slice());
    /// decoder.next_notice(&mut input)?;
    /// let mut drawn = Frame::default();
    /// if let Some(Notice::Frame(update)) = decoder.next_notice(&mut input)? {
    ///     assert!(update.apply(&mut drawn));
    /// }
    /// assert_eq!(drawn, screen);
    /// # Ok::<(), pinfold_proto::link::Error>(())
    /// ```
    #[must_use]
    pub fn apply(&self, frame: &mut Frame) -> bool {
        let columns = usize::from(self.columns);
        let cells = usize::from(self.rows) * columns;
        if self.full {
            frame.cells.resize(cells, BLANK);
        } else if (frame.rows, frame.columns) != (self.rows, self.columns)
            || frame.cells.len() != cells
        {
            return false;
        }
        for record in self.records.chunks_exact(record_size(self.columns)) {
            let Some((number, row)) = record.split_first_chunk::<ROW_NUMBER>() else {
                return false;
            };
            let start = usize::from(u16::from_le_bytes(*number)) * columns;
            let Some(target) = frame.cells.get_mut(start..start + columns) else {
                return false;
            };
            for (cell, bytes) in target.iter_mut().zip(row.as_chunks::<CELL>().0) {
                let Ok(read) = read_cell(bytes) else {
                    return false;
                };
                *cell = read;
            }
        }
        frame.rows = self.rows;
        frame.columns = self.columns;
        frame.cursor = self.cursor;
        frame.app_cursor = self.app_cursor;
        frame.mouse = self.mouse;
        frame.mouse_encoding = self.mouse_encoding;
        frame.alternate_scroll = self.alternate_scroll;
        frame.clipboard_writes = self.clipboard_writes;
        true
    }

    /// The bounds both directions share, so nothing is sent that the peer would refuse.
    fn check(&self) -> Result<(), Error> {
        check("rows", self.rows, 1..=MAX_DIMENSION)?;
        check("columns", self.columns, 1..=MAX_DIMENSION)?;
        if let Some((row, column)) = self.cursor {
            check("cursor row", row, 0..=self.rows - 1)?;
            check("cursor column", column, 0..=self.columns - 1)?;
        }
        if self.held.start > self.held.end {
            return Err(Error::OutOfBounds {
                field: "held rows",
                value: self.held.start as u64,
            });
        }
        Ok(())
    }

    /// Checks the rows, which a peer refuses as a whole.
    fn check_records(&self) -> Result<(), Error> {
        if self.full && self.changed != self.rows {
            return Err(Error::OutOfBounds {
                field: "rows in a full frame",
                value: self.changed.into(),
            });
        }
        let size = record_size(self.columns);
        let expected = usize::from(self.changed)
            .checked_mul(size)
            .ok_or(Error::Truncated)?;
        match self.records.len().checked_sub(expected) {
            None => return Err(Error::Truncated),
            Some(0) => {}
            Some(bytes) => return Err(Error::Trailing { bytes }),
        }
        // Rows rise strictly, so a full frame's are exactly 0..rows and none is sent twice.
        let mut lowest = 0;
        for record in self.records.chunks_exact(size) {
            let (number, cells) = record
                .split_first_chunk::<ROW_NUMBER>()
                .ok_or(Error::Truncated)?;
            let row = u16::from_le_bytes(*number);
            if row < lowest {
                return Err(Error::OutOfBounds {
                    field: "row order",
                    value: row.into(),
                });
            }
            check("row", row, 0..=self.rows - 1)?;
            lowest = row + 1;
            for cell in cells.as_chunks::<CELL>().0 {
                read_cell(cell)?;
            }
        }
        Ok(())
    }

    fn write_head(&self, out: &mut Vec<u8>) {
        let flags = (u8::from(self.full) * FLAG_FULL)
            | (u8::from(self.cursor.is_some()) * FLAG_CURSOR)
            | (u8::from(self.app_cursor) * FLAG_APP_CURSOR)
            | (u8::from(self.alternate_scroll) * FLAG_ALTERNATE_SCROLL);
        let (row, column) = self.cursor.unwrap_or_default();
        put!(out; self.pane, self.rows, self.columns);
        out.extend_from_slice(&[flags, self.mouse as u8, self.mouse_encoding as u8]);
        put!(out; row, column, self.clipboard_writes);
        put!(out; self.top as u64);
        put!(out; self.held.start as u64, self.held.end as u64);
        put!(out; self.changed);
    }
}

impl Request<'_> {
    /// The first link version that carries this message, so a window sends it only to a core
    /// that speaks that version (ADR-0043).
    #[must_use]
    pub fn since(&self) -> u16 {
        match self {
            Self::OpenTool {
                tool: Tool::Rewind, ..
            } => REWIND_VIEW,
            Self::OpenTool { .. } => 12,
            Self::ListMarkdown { .. } | Self::OpenMarkdown { .. } => 13,
            Self::Palette(_) => 14,
            Self::Inbox { .. } => INBOX,
            Self::Answer { .. } => ANSWER,
            Self::OpenTabIn { .. } => OUTSIDE,
            Self::Toasts { .. } => TOAST,
            _ => OLDEST,
        }
    }

    /// Appends this message to `out`. On an error `out` is as it was.
    ///
    /// # Errors
    ///
    /// [`Error::TooLarge`] past [`MAX_REQUEST`], [`Error::OutOfBounds`] for a resize or a
    /// border's move outside 1 to [`MAX_DIMENSION`], or a tab past [`MAX_TABS`], and
    /// [`Error::BadColour`] for a palette colour above `0xFFFFFF`.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        if let Self::Palette(palette) = *self {
            let named = [palette.background, palette.foreground];
            named
                .iter()
                .chain(&palette.ansi)
                .try_for_each(check_colour)?;
        }
        if let Self::Resize { rows, columns } = *self {
            check("rows", rows, 1..=MAX_DIMENSION)?;
            check("columns", columns, 1..=MAX_DIMENSION)?;
        }
        if let Self::SwitchTab { index } = *self {
            check("tab", index, LAST_TAB)?;
        }
        if let Self::MoveBorder { cells, .. } = *self {
            check("cells", cells, 1..=MAX_DIMENSION)?;
        }
        let start = begin(out);
        let kind = match *self {
            Self::Hello { version } => put!(out, HELLO, version),
            Self::Input { pane, bytes } => put_tail(out, start, MAX_REQUEST, INPUT, pane, bytes)?,
            Self::Paste { pane, text } => {
                put_tail(out, start, MAX_PASTE, PASTE, pane, text.as_bytes())?
            }
            Self::Resize { rows, columns } => put!(out, RESIZE, rows, columns),
            Self::Scroll { pane, lines } => put!(out, SCROLL, pane, lines),
            Self::Reveal { pane, row } => put!(out, REVEAL, pane, row as u64),
            Self::Search {
                pane,
                from,
                direction,
                query,
            } => {
                if query.len() > MAX_REQUEST {
                    out.truncate(start);
                    return Err(Error::TooLarge {
                        size: query.len() as u64,
                    });
                }
                let mut head = Vec::with_capacity(HEAD + query.len());
                put_position(&mut head, from);
                head.push(direction as u8);
                head.extend_from_slice(query.as_bytes());
                put_tail(out, start, MAX_REQUEST, SEARCH, pane, &head)?
            }
            Self::Copy { pane, selection } => {
                put!(out; pane);
                put_selection(out, selection);
                COPY
            }
            Self::Link { pane, at } => {
                put!(out; pane);
                put_position(out, at);
                LINK
            }
            Self::FullFrame { pane } => put!(out, FULL_FRAME, pane),
            Self::Quit => QUIT,
            Self::NewTab => NEW_TAB,
            Self::SwitchTab { index } => put!(out, SWITCH_TAB, index),
            Self::StepTab { forward } => {
                out.push(u8::from(forward));
                STEP_TAB
            }
            Self::Split { pane, way } => {
                put!(out; pane);
                out.push(way as u8);
                SPLIT
            }
            Self::ClosePane { pane } => put!(out, CLOSE_PANE, pane),
            Self::Focus { pane } => put!(out, FOCUS, pane),
            Self::MoveFocus { side } => {
                out.push(side as u8);
                MOVE_FOCUS
            }
            Self::MoveBorder { side, cells } => {
                out.push(side as u8);
                put!(out, MOVE_BORDER, cells)
            }
            Self::Zoom => ZOOM,
            Self::OpenTool { pane, tool } => {
                put!(out; pane);
                out.push(tool as u8);
                OPEN_TOOL
            }
            Self::ListMarkdown { pane } => put!(out, LIST_MARKDOWN, pane),
            Self::OpenMarkdown { pane, name } => put_tail(
                out,
                start,
                MAX_REQUEST,
                OPEN_MARKDOWN,
                pane,
                name.as_bytes(),
            )?,
            Self::WasRunningAnswer { pane, run } => {
                put!(out; pane);
                out.push(u8::from(run));
                WAS_RUNNING_ANSWER
            }
            Self::ApprovalAnswer { id, answer } => {
                put!(out; id);
                out.push(answer as u8);
                APPROVAL_ANSWER
            }
            Self::Palette(palette) => {
                put!(out; palette.background, palette.foreground);
                for colour in palette.ansi {
                    put!(out; colour);
                }
                PALETTE
            }
            Self::Inbox { open } => {
                out.push(u8::from(open));
                INBOX_OPEN
            }
            Self::Toasts { on } => {
                out.push(u8::from(on));
                TOASTS
            }
            Self::OpenTabIn { dir } => {
                if dir.len() > MAX_DIR {
                    out.truncate(start);
                    return Err(Error::TooLarge {
                        size: dir.len() as u64,
                    });
                }
                out.extend_from_slice(dir.as_bytes());
                OPEN_TAB_IN
            }
            Self::Answer {
                pane,
                choice,
                profile,
                key,
            } => {
                if profile.len() > MAX_ANSWER_NAME || key.len() > MAX_ANSWER_NAME {
                    out.truncate(start);
                    return Err(Error::TooLarge {
                        size: profile.len().max(key.len()) as u64,
                    });
                }
                put!(out; pane);
                out.extend_from_slice(&[choice as u8, profile.len() as u8]);
                out.extend_from_slice(profile.as_bytes());
                out.extend_from_slice(key.as_bytes());
                ANSWER_KIND
            }
        };
        finish(out, start, MAX_REQUEST, kind)
    }

    fn decode(kind: u8, body: &[u8]) -> Result<Request<'_>, Error> {
        let mut body = Reader(body);
        let request = match kind {
            HELLO => Request::Hello {
                version: body.u16()?,
            },
            INPUT => Request::Input {
                pane: body.u32()?,
                bytes: body.rest(),
            },
            PASTE => {
                let pane = body.u32()?;
                let text = body.text()?;
                if text.len() > MAX_PASTE {
                    return Err(Error::TooLarge {
                        size: text.len() as u64,
                    });
                }
                Request::Paste { pane, text }
            }
            RESIZE => {
                let (rows, columns) = (body.u16()?, body.u16()?);
                check("rows", rows, 1..=MAX_DIMENSION)?;
                check("columns", columns, 1..=MAX_DIMENSION)?;
                Request::Resize { rows, columns }
            }
            SCROLL => Request::Scroll {
                pane: body.u32()?,
                lines: body.i32()?,
            },
            REVEAL => Request::Reveal {
                pane: body.u32()?,
                row: body.row()?,
            },
            SEARCH => Request::Search {
                pane: body.u32()?,
                from: body.position()?,
                direction: body.tag("direction", &[Direction::Forward, Direction::Backward])?,
                query: body.text()?,
            },
            COPY => Request::Copy {
                pane: body.u32()?,
                selection: body.selection()?,
            },
            LINK => Request::Link {
                pane: body.u32()?,
                at: body.position()?,
            },
            FULL_FRAME => Request::FullFrame { pane: body.u32()? },
            QUIT => Request::Quit,
            NEW_TAB => Request::NewTab,
            SWITCH_TAB => {
                let index = body.u16()?;
                check("tab", index, LAST_TAB)?;
                Request::SwitchTab { index }
            }
            STEP_TAB => Request::StepTab {
                forward: body.tag("tab step", &[false, true])?,
            },
            SPLIT => Request::Split {
                pane: body.u32()?,
                way: body.tag("split", &[SplitWay::Right, SplitWay::Down])?,
            },
            CLOSE_PANE => Request::ClosePane { pane: body.u32()? },
            FOCUS => Request::Focus { pane: body.u32()? },
            MOVE_FOCUS => Request::MoveFocus { side: body.side()? },
            MOVE_BORDER => {
                let (side, cells) = (body.side()?, body.u16()?);
                check("cells", cells, 1..=MAX_DIMENSION)?;
                Request::MoveBorder { side, cells }
            }
            ZOOM => Request::Zoom,
            LIST_MARKDOWN => Request::ListMarkdown { pane: body.u32()? },
            OPEN_MARKDOWN => Request::OpenMarkdown {
                pane: body.u32()?,
                name: body.text()?,
            },
            OPEN_TOOL => Request::OpenTool {
                pane: body.u32()?,
                tool: body.tag("tool", &[Tool::Files, Tool::Diff, Tool::Rewind])?,
            },
            WAS_RUNNING_ANSWER => Request::WasRunningAnswer {
                pane: body.u32()?,
                run: body.tag("run", &[false, true])?,
            },
            APPROVAL_ANSWER => Request::ApprovalAnswer {
                id: body.array().map(u64::from_le_bytes)?,
                answer: body.tag(
                    "approval answer",
                    &[
                        ApprovalAnswer::AllowOnce,
                        ApprovalAnswer::AllowForThirtyMinutes,
                        ApprovalAnswer::Deny,
                    ],
                )?,
            },
            PALETTE => {
                let (background, foreground) = (body.colour()?, body.colour()?);
                let mut ansi = [0; 16];
                for colour in &mut ansi {
                    *colour = body.colour()?;
                }
                Request::Palette(Palette {
                    background,
                    foreground,
                    ansi,
                })
            }
            INBOX_OPEN => Request::Inbox {
                open: body.tag("open", &[false, true])?,
            },
            TOASTS => Request::Toasts {
                on: body.tag("on", &[false, true])?,
            },
            OPEN_TAB_IN => {
                let dir = body.text()?;
                if dir.len() > MAX_DIR {
                    return Err(Error::TooLarge {
                        size: dir.len() as u64,
                    });
                }
                Request::OpenTabIn { dir }
            }
            // Bounded and checked against the key table before the core compares them.
            ANSWER_KIND => {
                let pane = body.u32()?;
                let choice = body.tag("choice", &[Choice::Approve, Choice::Interrupt])?;
                let length = usize::from(body.u8()?);
                if length > MAX_ANSWER_NAME {
                    return Err(Error::OutOfBounds {
                        field: "profile",
                        value: length as u64,
                    });
                }
                let profile =
                    std::str::from_utf8(body.take(length)?).map_err(|_| Error::BadText)?;
                let key = body.text()?;
                let known = match choice {
                    Choice::Approve => key_bytes(key).is_some(),
                    Choice::Interrupt => key.is_empty(),
                };
                if !known {
                    return Err(Error::OutOfBounds {
                        field: "key",
                        value: key.len() as u64,
                    });
                }
                Request::Answer {
                    pane,
                    choice,
                    profile,
                    key,
                }
            }
            kind => return Err(Error::UnknownKind { kind }),
        };
        body.end()?;
        Ok(request)
    }
}

impl Notice<'_> {
    /// The first link version that carries this message, so the core sends it only to a window
    /// that speaks that version (ADR-0043).
    #[must_use]
    pub fn since(&self) -> u16 {
        match self {
            Self::Approval(approval) => approval.action.since(),
            Self::Markdown { .. } => 13,
            Self::Colours { .. } => 14,
            Self::Secrets { .. } => 15,
            Self::Webhook { .. } => WEBHOOK_SINCE,
            Self::InboxExtra { .. } => INBOX_EXTRA,
            Self::Toast { .. } => TOAST,
            Self::Progress { .. } => PROGRESS,
            Self::Outside { .. }
            | Self::Refused {
                reason: Refusal::Folder,
            } => OUTSIDE,
            Self::Checkpoint { .. } | Self::Restored { .. } => REWIND,
            Self::Refused {
                reason:
                    Refusal::Gone
                    | Refusal::NotWaiting
                    | Refusal::NotAgent
                    | Refusal::Protected
                    | Refusal::ProfileChanged,
            } => ANSWER,
            Self::Status { naming: false, .. } => TIMED_STATUS,
            _ => OLDEST,
        }
    }

    /// Appends this message to `out`, as [`VERSION`] lays it out. On an error `out` is as it
    /// was.
    ///
    /// # Errors
    ///
    /// [`Error::TooLarge`] past [`MAX_MESSAGE`], and [`Error::BadColour`] for a pane colour above
    /// `0xFFFFFF`.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        self.encode_at(VERSION, out)
    }

    /// Appends this message as a window speaking `version` reads it, which differs only for
    /// [`Notice::Status`] and [`Notice::Layout`] below 17. It never checks [`Notice::since`]:
    /// the sender does.
    ///
    /// ```
    /// use pinfold_proto::link::{Decoder, Notice, OLDEST};
    ///
    /// let mut wire = Vec::new();
    /// Notice::Hello { version: OLDEST }.encode(&mut wire)?;
    /// let status = Notice::Status {
    ///     pane: 1, state: 0, reported: false, naming: true, listed: false, since: 0, started: 0,
    ///     profile: None, message: "",
    /// };
    /// status.encode_at(OLDEST, &mut wire)?;
    /// let (mut decoder, mut input) = (Decoder::default(), wire.as_slice());
    /// decoder.next_notice(&mut input)?;
    /// assert_eq!(decoder.next_notice(&mut input)?, Some(status));
    /// # Ok::<(), pinfold_proto::link::Error>(())
    /// ```
    ///
    /// # Errors
    ///
    /// As [`Notice::encode`].
    pub fn encode_at(&self, version: u16, out: &mut Vec<u8>) -> Result<(), Error> {
        if let Self::Colours {
            cursor, background, ..
        } = self
        {
            cursor.iter().chain(background).try_for_each(check_colour)?;
        }
        let start = begin(out);
        let kind = match self {
            Self::Hello { version } => put!(out, HELLO, version),
            Self::Colours {
                pane,
                cursor,
                background,
            } => {
                put!(out; pane);
                for colour in [cursor, background] {
                    out.push(u8::from(colour.is_some()));
                    if let Some(colour) = colour {
                        put!(out; colour);
                    }
                }
                COLOURS
            }
            Self::Frame(update) => {
                if let Err(error) = update.check().and_then(|()| update.check_records()) {
                    out.truncate(start);
                    return Err(error);
                }
                update.write_head(out);
                out.extend_from_slice(update.records);
                FRAME
            }
            Self::Cwd { pane, path } => {
                put_tail(out, start, MAX_MESSAGE, CWD, *pane, path.as_bytes())?
            }
            Self::Searched { pane, found } => {
                put!(out; pane);
                out.push(u8::from(found.is_some()));
                if let Some(found) = found {
                    put_selection(out, *found);
                }
                SEARCHED
            }
            Self::Copied { pane, text } => {
                put_tail(out, start, MAX_MESSAGE, COPIED, *pane, text.as_bytes())?
            }
            Self::Linked { pane, at, uri } => {
                let mut head = Vec::with_capacity(10 + uri.len());
                put_position(&mut head, *at);
                head.extend_from_slice(uri.as_bytes());
                put_tail(out, start, MAX_MESSAGE, LINKED, *pane, &head)?
            }
            Self::Layout(layout) => {
                layout.write(version, out);
                LAYOUT
            }
            Self::Raise => RAISE,
            Self::Unhardened { checked } => {
                out.push(u8::from(*checked));
                UNHARDENED
            }
            Self::Markdown { pane, names } => {
                put_tail(out, start, MAX_MESSAGE, MARKDOWN, *pane, names.as_bytes())?
            }
            Self::Secrets { count, names } => {
                put_tail(out, start, MAX_MESSAGE, SECRETS, *count, names.as_bytes())?
            }
            Self::Webhook {
                host,
                sender,
                failed,
            } => {
                out.push(u8::from(*sender));
                put_tail(out, start, MAX_MESSAGE, WEBHOOK, *failed, host.as_bytes())?
            }
            Self::InboxExtra {
                pane,
                branch,
                ctx,
                steps,
            } if version >= AGENT_FILES => {
                let size = branch.len().max(steps.len());
                if branch.len() > MAX_BRANCH || steps.len() > MAX_STEPS || ctx > &Some(100) {
                    out.truncate(start);
                    return Err(Error::TooLarge { size: size as u64 });
                }
                let count = u8::try_from(branch.len()).unwrap_or(u8::MAX);
                put!(out; pane, ctx.unwrap_or(NO_CTX), count);
                out.extend_from_slice(branch.as_bytes());
                out.extend_from_slice(steps.as_bytes());
                INBOX_EXTRA_KIND
            }
            Self::InboxExtra { pane, branch, .. } => put_tail(
                out,
                start,
                MAX_BRANCH,
                INBOX_EXTRA_KIND,
                *pane,
                branch.as_bytes(),
            )?,
            Self::Toast { pane, kind, text } => {
                if !toast(*kind, text) {
                    out.truncate(start);
                    return Err(Error::BadText);
                }
                put!(out; *pane, *kind);
                out.extend_from_slice(text.as_bytes());
                TOAST_KIND
            }
            Self::Progress { pane, progress } => {
                put!(out; *pane, *progress);
                PROGRESS_KIND
            }
            Self::Outside {
                index,
                count,
                secs,
                dir,
            } => {
                if !outside_row(*index, *count, dir) {
                    out.truncate(start);
                    return Err(Error::BadText);
                }
                put!(out; *index, *count, *secs);
                out.extend_from_slice(dir.as_bytes());
                OUTSIDE_KIND
            }
            Self::Activity { tabs, alerts } => put!(out, ACTIVITY, tabs, alerts),
            Self::Checkpoint { pane, missing } => {
                if usize::from(*missing) >= MISSING.len() {
                    out.truncate(start);
                    let (field, tag) = ("missing", *missing);
                    return Err(Error::BadTag { field, tag });
                }
                put!(out, CHECKPOINT, pane, missing)
            }
            Self::Restored {
                undid,
                skipped,
                error,
            } => {
                put!(out; undid);
                put_tail(
                    out,
                    start,
                    MAX_RESTORE_ERROR,
                    RESTORED,
                    *skipped,
                    error.as_bytes(),
                )?
            }
            Self::Refused { reason } => {
                out.push(*reason as u8);
                REFUSED
            }
            Self::WasRunning {
                pane,
                runnable,
                command,
            } => {
                // The decoder refuses a longer one, so it must not go out.
                if command.len() > MAX_COMMAND {
                    out.truncate(start);
                    return Err(Error::TooLarge {
                        size: command.len() as u64,
                    });
                }
                put!(out; pane);
                out.push(u8::from(*runnable));
                out.extend_from_slice(command.as_bytes());
                WAS_RUNNING
            }
            Self::Status {
                pane,
                state,
                reported,
                naming,
                listed,
                since,
                started,
                profile,
                message,
            } => {
                if *state >= STATES {
                    out.truncate(start);
                    return Err(Error::BadTag {
                        field: "state",
                        tag: *state,
                    });
                }
                out.extend_from_slice(&[*state, u8::from(*reported)]);
                if version >= TIMED_STATUS {
                    let listed = u8::from(*listed && version >= INBOX) << 1;
                    out.push(u8::from(*naming) | listed);
                    put!(out; since, started);
                }
                if version >= ANSWER {
                    out.push(profile.unwrap_or(u8::MAX));
                }
                put_tail(out, start, MAX_MESSAGE, STATUS, *pane, message.as_bytes())?
            }
            Self::Approval(approval) => {
                put!(out; approval.id, approval.caller, approval.target);
                out.push(approval.action as u8);
                let Ok(count) = u16::try_from(approval.texts.len()) else {
                    out.truncate(start);
                    return Err(Error::TooLarge {
                        size: approval.texts.len() as u64,
                    });
                };
                put!(out; count);
                for text in &approval.texts {
                    let Ok(length) = u32::try_from(text.len()) else {
                        out.truncate(start);
                        return Err(Error::TooLarge {
                            size: text.len() as u64,
                        });
                    };
                    put!(out; length);
                    out.extend_from_slice(text.as_bytes());
                }
                APPROVAL
            }
        };
        finish(out, start, MAX_MESSAGE, kind)
    }

    fn decode(kind: u8, body: &[u8], version: u16) -> Result<Notice<'_>, Error> {
        let mut body = Reader(body);
        let notice = match kind {
            HELLO => Notice::Hello {
                version: body.u16()?,
            },
            FRAME => Notice::Frame(read_frame(&mut body)?),
            CWD => Notice::Cwd {
                pane: body.u32()?,
                path: body.text()?,
            },
            SEARCHED => Notice::Searched {
                pane: body.u32()?,
                found: if body.tag("search result", &[false, true])? {
                    Some(body.selection()?)
                } else {
                    None
                },
            },
            COPIED => Notice::Copied {
                pane: body.u32()?,
                text: body.text()?,
            },
            LINKED => Notice::Linked {
                pane: body.u32()?,
                at: body.position()?,
                uri: body.text()?,
            },
            LAYOUT => Notice::Layout(read_layout(&mut body, version)?),
            RAISE => Notice::Raise,
            MARKDOWN => Notice::Markdown {
                pane: body.u32()?,
                names: body.text()?,
            },
            COLOURS => Notice::Colours {
                pane: body.u32()?,
                cursor: body.optional_colour()?,
                background: body.optional_colour()?,
            },
            SECRETS => Notice::Secrets {
                count: body.u32()?,
                names: body.text()?,
            },
            CHECKPOINT => {
                let pane = body.u32()?;
                let missing = body.u8()?;
                if usize::from(missing) >= MISSING.len() {
                    let (field, tag) = ("missing", missing);
                    return Err(Error::BadTag { field, tag });
                }
                Notice::Checkpoint { pane, missing }
            }
            RESTORED => {
                let (undid, skipped) = (body.u32()?, body.u32()?);
                let error = body.text()?;
                if error.len() > MAX_RESTORE_ERROR {
                    return Err(Error::TooLarge {
                        size: error.len() as u64,
                    });
                }
                Notice::Restored {
                    undid,
                    skipped,
                    error,
                }
            }
            WEBHOOK => Notice::Webhook {
                sender: body.tag("sender", &[false, true])?,
                failed: body.u32()?,
                host: body.text()?,
            },
            TOAST_KIND => {
                let (pane, kind) = (body.u32()?, body.u8()?);
                let text = body.text()?;
                // The window draws it as chrome, so only text the core's cleaning lets through.
                if !toast(kind, text) {
                    return Err(Error::BadText);
                }
                Notice::Toast { pane, kind, text }
            }
            PROGRESS_KIND => Notice::Progress {
                pane: body.u32()?,
                progress: body.u8()?,
            },
            OUTSIDE_KIND => {
                let (index, count, secs) = (body.u8()?, body.u8()?, body.u32()?);
                let dir = body.text()?;
                // The window draws it escaped and offers it for a tab, so only a plain path.
                if !outside_row(index, count, dir) {
                    return Err(Error::BadText);
                }
                Notice::Outside {
                    index,
                    count,
                    secs,
                    dir,
                }
            }
            INBOX_EXTRA_KIND if version >= AGENT_FILES => {
                let pane = body.u32()?;
                let ctx = match body.u8()? {
                    NO_CTX => None,
                    ctx @ 0..=100 => Some(ctx),
                    tag => return Err(Error::BadTag { field: "ctx", tag }),
                };
                let count = usize::from(body.u8()?);
                let branch = std::str::from_utf8(body.take(count)?).map_err(|_| Error::BadText)?;
                let steps = body.text()?;
                if branch.len() > MAX_BRANCH || steps.len() > MAX_STEPS {
                    return Err(Error::TooLarge {
                        size: branch.len().max(steps.len()) as u64,
                    });
                }
                // The window draws both as they are, so only what a core's cleaning lets through.
                let shown = |step: &str| {
                    !step.is_empty() && step.chars().all(|c| !c.is_control() && !bidi(c))
                };
                if !branch.bytes().all(|byte| matches!(byte, b' '..=b'~'))
                    || !(steps.is_empty() || steps.split('\n').all(shown))
                    || steps.split('\n').count() > STEPS
                {
                    return Err(Error::BadText);
                }
                Notice::InboxExtra {
                    pane,
                    branch,
                    ctx,
                    steps,
                }
            }
            INBOX_EXTRA_KIND => {
                let pane = body.u32()?;
                let branch = body.text()?;
                if branch.len() > MAX_BRANCH {
                    return Err(Error::TooLarge {
                        size: branch.len() as u64,
                    });
                }
                // The window draws it as is, so only what a core's cleaning lets through.
                if !branch.bytes().all(|byte| matches!(byte, b' '..=b'~')) {
                    return Err(Error::BadText);
                }
                Notice::InboxExtra {
                    pane,
                    branch,
                    ctx: None,
                    steps: "",
                }
            }
            UNHARDENED => Notice::Unhardened {
                checked: body.tag("checked", &[false, true])?,
            },
            ACTIVITY => Notice::Activity {
                tabs: body.u32()?,
                alerts: body.u32()?,
            },
            // The state, its source and times go ahead of the pane, as the message runs to the end.
            STATUS => {
                let state = body.u8()?;
                if state >= STATES {
                    return Err(Error::BadTag {
                        field: "state",
                        tag: state,
                    });
                }
                let reported = body.tag("reported", &[false, true])?;
                let (flags, since, started) = if version >= TIMED_STATUS {
                    let flags: &[u8] = if version >= INBOX {
                        &[0, 1, 2, 3]
                    } else {
                        &[0, 1]
                    };
                    (body.tag("flags", flags)?, body.u32()?, body.u32()?)
                } else {
                    (1, 0, 0)
                };
                let profile = if version >= ANSWER {
                    Some(body.u8()?).filter(|&profile| profile != u8::MAX)
                } else {
                    None
                };
                let naming = flags & 1 != 0;
                Notice::Status {
                    state,
                    reported,
                    naming,
                    listed: if version >= INBOX {
                        flags & 2 != 0
                    } else {
                        !naming
                    },
                    since,
                    started,
                    profile,
                    pane: body.u32()?,
                    message: body.text()?,
                }
            }
            REFUSED => Notice::Refused {
                reason: body.tag(
                    "refusal",
                    &[
                        Refusal::Tabs,
                        Refusal::Panes,
                        Refusal::Room,
                        Refusal::Spawn,
                        Refusal::Input,
                        Refusal::Gone,
                        Refusal::NotWaiting,
                        Refusal::NotAgent,
                        Refusal::Protected,
                        Refusal::ProfileChanged,
                        Refusal::Folder,
                    ],
                )?,
            },
            WAS_RUNNING => {
                let (pane, runnable) = (body.u32()?, body.tag("runnable", &[false, true])?);
                let command = body.text()?;
                if command.len() > MAX_COMMAND {
                    return Err(Error::TooLarge {
                        size: command.len() as u64,
                    });
                }
                Notice::WasRunning {
                    pane,
                    runnable,
                    command,
                }
            }
            APPROVAL => {
                let id = body.array().map(u64::from_le_bytes)?;
                let caller = body.u32()?;
                let target = body.u32()?;
                let action = body.tag(
                    "approval action",
                    &[
                        ApprovalAction::Read,
                        ApprovalAction::Send,
                        ApprovalAction::Trust,
                        ApprovalAction::Owner,
                        ApprovalAction::Rewind,
                    ],
                )?;
                let count = body.u16()?;
                let mut texts = Vec::with_capacity(usize::from(count));
                for _ in 0..count {
                    texts.push(body.sized_text()?);
                }
                Notice::Approval(Approval {
                    id,
                    caller,
                    target,
                    action,
                    texts,
                })
            }
            kind => return Err(Error::UnknownKind { kind }),
        };
        body.end()?;
        Ok(notice)
    }
}

/// Appends a frame of `frame`, with only the rows that differ from `previous`.
///
/// `previous` is the last frame sent. Without one, or if the size changed, every row goes.
/// `top` and `held` are the core's numbers for the frame's first row and the rows the view can
/// reach. On an error `out` is as it was.
///
/// # Errors
///
/// [`Error::OutOfBounds`] for a size outside 1 to [`MAX_DIMENSION`], a cursor off the screen or
/// reversed `held` or `frame.cells` that do not fit its size, and [`Error::BadColour`] for a
/// changed cell with bits a peer refuses.
// shortcut: one changed cell sends its whole row, split rows into runs if typing costs too much.
pub fn encode_frame(
    out: &mut Vec<u8>,
    pane: u32,
    frame: &Frame,
    top: usize,
    held: Range<usize>,
    previous: Option<&Frame>,
) -> Result<(), Error> {
    let previous = previous.filter(|previous| {
        (previous.rows, previous.columns) == (frame.rows, frame.columns)
            && previous.cells.len() == frame.cells.len()
    });
    let update = FrameUpdate {
        pane,
        full: previous.is_none(),
        top,
        held,
        rows: frame.rows,
        columns: frame.columns,
        cursor: frame.cursor,
        app_cursor: frame.app_cursor,
        mouse: frame.mouse,
        mouse_encoding: frame.mouse_encoding,
        alternate_scroll: frame.alternate_scroll,
        clipboard_writes: frame.clipboard_writes,
        changed: 0,
        records: &[],
    };
    update.check()?;
    let columns = usize::from(frame.columns);
    if frame.cells.len() != usize::from(frame.rows) * columns {
        return Err(Error::OutOfBounds {
            field: "cells",
            value: frame.cells.len() as u64,
        });
    }

    let start = begin(out);
    update.write_head(out);
    let count = out.len().saturating_sub(COUNT);
    let mut changed: u16 = 0;
    for (number, cells) in (0..frame.rows).zip(frame.cells.chunks_exact(columns)) {
        let first = usize::from(number) * columns;
        if previous
            .is_some_and(|previous| previous.cells.get(first..first + columns) == Some(cells))
        {
            continue;
        }
        changed += 1;
        out.extend_from_slice(&number.to_le_bytes());
        for cell in cells {
            if let Err(error) = check_colours(cell.foreground, cell.background) {
                out.truncate(start);
                return Err(error);
            }
            out.extend_from_slice(&u32::from(cell.character).to_le_bytes());
            out.extend_from_slice(&cell.foreground.to_le_bytes());
            out.extend_from_slice(&cell.background.to_le_bytes());
        }
    }
    if let Some(field) = out.get_mut(count..count + COUNT) {
        field.copy_from_slice(&changed.to_le_bytes());
    }
    finish(out, start, MAX_MESSAGE, FRAME)
}

/// Appends a [`Notice::Layout`] as a window speaking `version` reads it. `tabs` holds each
/// tab's focused pane, `panes` what the active tab shows, `total` how many panes the session
/// holds, and `members` each of those with its tab's index, which goes only from version 17.
/// On an error `out` is as it was.
///
/// # Errors
///
/// What a peer would refuse: [`Error::OutOfBounds`] for too many tabs or panes, an `active` tab
/// that does not exist, a pane listed twice, a rectangle off the largest screen, or members
/// that are not `total` panes of existing tabs.
#[expect(clippy::too_many_arguments, reason = "each is one field of the layout")]
pub fn encode_layout(
    out: &mut Vec<u8>,
    version: u16,
    active: u16,
    focused: u32,
    total: u16,
    tabs: &[u32],
    panes: &[(u32, Rect)],
    members: &[(u32, u8)],
) -> Result<(), Error> {
    let too_long = |value: usize| Error::OutOfBounds {
        field: "layout",
        value: value as u64,
    };
    let tabs_count = u16::try_from(tabs.len()).map_err(|_| too_long(tabs.len()))?;
    let panes_count = u16::try_from(panes.len()).map_err(|_| too_long(panes.len()))?;
    let start = begin(out);
    put!(out; active, focused, total, tabs_count, panes_count);
    for tab in tabs {
        put!(out; tab);
    }
    for (pane, rect) in panes {
        put!(out; pane, rect.x, rect.y, rect.columns, rect.rows);
    }
    if version >= INBOX {
        for (pane, tab) in members {
            put!(out; pane, tab);
        }
    }
    let body = out.get(start + PREFIX + KIND..).unwrap_or_default();
    let mut read = Reader(body);
    if let Err(error) = read_layout(&mut read, version).and_then(|_| read.end()) {
        out.truncate(start);
        return Err(error);
    }
    finish(out, start, MAX_MESSAGE, LAYOUT)
}

/// Turns bytes in chunks of any size into messages, one at a time.
///
/// It takes only what the next message needs from each chunk, so it never holds more than one
/// message within its direction's limit, and it keeps its buffer from one message to the next. Bytes
/// that a length only promises allocate nothing. Each side keeps one per link.
///
/// ```
/// use pinfold_proto::link::{Decoder, Request, VERSION};
///
/// let mut wire = Vec::new();
/// Request::Hello { version: VERSION }.encode(&mut wire)?;
/// Request::Quit.encode(&mut wire)?;
///
/// let mut decoder = Decoder::default();
/// let mut count = 0;
/// for chunk in wire.chunks(2) {
///     let mut input = chunk;
///     while decoder.next_request(&mut input)?.is_some() {
///         count += 1;
///     }
/// }
/// assert_eq!(count, 2);
/// # Ok::<(), pinfold_proto::link::Error>(())
/// ```
#[derive(Debug, Default)]
pub struct Decoder {
    buffer: Vec<u8>,
    /// Whether `buffer` holds a whole message, already handed out.
    whole: bool,
    /// The version both sides speak, once the peer's hello has come.
    version: Option<u16>,
    failed: Option<Error>,
}

impl Decoder {
    /// Reads the next message a window sent, advancing `input` past the bytes it used.
    ///
    /// Returns `None` once `input` is empty and the message is not complete yet. Call this until
    /// then, as `input` can hold several messages.
    ///
    /// # Errors
    ///
    /// Any error ends the link, and every later call returns it.
    pub fn next_request(&mut self, input: &mut &[u8]) -> Result<Option<Request<'_>>, Error> {
        // A window answers at the lower version, so never above the core's.
        let decode = |kind, body, _| Request::decode(kind, body);
        self.next(input, MAX_REQUEST, VERSION, decode, Request::since)
    }

    /// Reads the next message the core sent, as [`Decoder::next_request`] does.
    ///
    /// # Errors
    ///
    /// Any error ends the link, and every later call returns it.
    pub fn next_notice(&mut self, input: &mut &[u8]) -> Result<Option<Notice<'_>>, Error> {
        self.next(
            input,
            MAX_MESSAGE,
            VERSION + 1,
            Notice::decode,
            Notice::since,
        )
    }

    /// The version both sides speak: the lower of the two, once the peer's hello has come.
    #[must_use]
    pub const fn version(&self) -> Option<u16> {
        self.version
    }

    fn next<'a, M>(
        &'a mut self,
        input: &mut &[u8],
        limit: usize,
        newest: u16,
        decode: fn(u8, &'a [u8], u16) -> Result<M, Error>,
        since: fn(&M) -> u16,
    ) -> Result<Option<M>, Error> {
        let decoded = match self.fill(input, limit, newest) {
            Ok(false) => return Ok(None),
            Ok(true) => {
                let (kind, body) = message(&self.buffer);
                let version = self.version.unwrap_or(OLDEST);
                decode(kind, body, version).and_then(|message| match since(&message) {
                    since if since > version => Err(Error::Newer { kind, since }),
                    _ => Ok(message),
                })
            }
            Err(error) => Err(error),
        };
        if let Err(error) = decoded {
            self.failed = Some(error);
        }
        decoded.map(Some)
    }

    /// Takes bytes until `buffer` holds a whole message, and returns whether it does. The length
    /// is checked before any of the body is stored, and the version check before anything else:
    /// a hello from [`OLDEST`] to `newest`.
    fn fill(&mut self, input: &mut &[u8], limit: usize, newest: u16) -> Result<bool, Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if self.whole {
            self.whole = false;
            self.buffer.clear();
            self.buffer.shrink_to(KEEP);
        }
        if !self.take_to(PREFIX, input) {
            return Ok(false);
        }
        let length = Reader(&self.buffer).u32()? as usize;
        if length > limit {
            return Err(Error::TooLarge {
                size: length as u64,
            });
        }
        if length < KIND {
            return Err(Error::Truncated);
        }
        if !self.take_to(PREFIX + length, input) {
            return Ok(false);
        }
        self.whole = true;
        let (kind, body) = message(&self.buffer);
        match (kind == HELLO, self.version.is_some()) {
            (true, true) => return Err(Error::RepeatedHello),
            (false, false) => return Err(Error::NoHello { kind }),
            (true, false) => {
                let mut body = Reader(body);
                let theirs = body.u16()?;
                body.end()?;
                if !(OLDEST..=newest).contains(&theirs) {
                    return Err(Error::Version {
                        ours: VERSION,
                        theirs,
                    });
                }
                self.version = Some(theirs.min(VERSION));
            }
            (false, true) => {}
        }
        Ok(true)
    }

    /// Moves bytes from `input` until `buffer` holds `target` of them, and says if it does.
    /// It grows with what has arrived and never past `target`, so it stays one message.
    fn take_to(&mut self, target: usize, input: &mut &[u8]) -> bool {
        let held = self.buffer.len();
        let wanted = target.saturating_sub(held).min(input.len());
        if wanted == 0 {
            return held >= target;
        }
        let (head, rest) = input.split_at(wanted);
        let room = self
            .buffer
            .capacity()
            .saturating_mul(2)
            .max(held + wanted)
            .min(target);
        self.buffer.reserve_exact(room.saturating_sub(held));
        self.buffer.extend_from_slice(head);
        *input = rest;
        self.buffer.len() >= target
    }
}

/// Splits a whole message in `buffer` into its type and body.
fn message(buffer: &[u8]) -> (u8, &[u8]) {
    match buffer.get(PREFIX..).and_then(<[u8]>::split_first) {
        Some((kind, body)) => (*kind, body),
        None => (HELLO, &[]),
    }
}

/// Leaves room for the length and type, which [`finish`] fills in.
fn begin(out: &mut Vec<u8>) -> usize {
    let start = out.len();
    out.extend_from_slice(&[0; PREFIX + KIND]);
    start
}

/// Appends a pane and the rest of a body. The check comes first, so a huge paste is not copied.
fn put_tail(
    out: &mut Vec<u8>,
    start: usize,
    limit: usize,
    kind: u8,
    pane: u32,
    tail: &[u8],
) -> Result<u8, Error> {
    if tail.len() > limit {
        out.truncate(start);
        return Err(Error::TooLarge {
            size: tail.len() as u64,
        });
    }
    put!(out; pane);
    out.extend_from_slice(tail);
    Ok(kind)
}

fn finish(out: &mut Vec<u8>, start: usize, limit: usize, kind: u8) -> Result<(), Error> {
    let length = out.len().saturating_sub(start + PREFIX);
    let Some(prefix) = u32::try_from(length).ok().filter(|_| length <= limit) else {
        out.truncate(start);
        return Err(Error::TooLarge {
            size: length as u64,
        });
    };
    let [a, b, c, d] = prefix.to_le_bytes();
    if let Some(head) = out.get_mut(start..start + PREFIX + KIND) {
        head.copy_from_slice(&[a, b, c, d, kind]);
    }
    Ok(())
}

fn put_position(out: &mut Vec<u8>, position: Position) {
    put!(out; position.row as u64, position.column);
}

fn put_selection(out: &mut Vec<u8>, selection: Selection) {
    put_position(out, selection.anchor);
    put_position(out, selection.focus);
}

fn check(field: &'static str, value: u16, range: RangeInclusive<u16>) -> Result<(), Error> {
    if range.contains(&value) {
        Ok(())
    } else {
        Err(Error::OutOfBounds {
            field,
            value: value.into(),
        })
    }
}

/// The bytes of a changed row. `columns` is a `u16`, so this cannot overflow.
const fn record_size(columns: u16) -> usize {
    ROW_NUMBER + columns as usize * CELL
}

fn check_colours(foreground: u32, background: u32) -> Result<(), Error> {
    if foreground & !(COLOUR | BOLD | ITALIC) != 0 || background & !COLOUR != 0 {
        return Err(Error::BadColour);
    }
    Ok(())
}

fn check_colour(colour: &u32) -> Result<(), Error> {
    check_colours(0, *colour)
}

fn read_cell(bytes: &[u8; CELL]) -> Result<Cell, Error> {
    let mut cell = Reader(bytes);
    let character = char::from_u32(cell.u32()?).ok_or(Error::BadCharacter)?;
    let (foreground, background) = (cell.u32()?, cell.u32()?);
    check_colours(foreground, background)?;
    Ok(Cell {
        character,
        foreground,
        background,
    })
}

fn read_frame<'a>(body: &mut Reader<'a>) -> Result<FrameUpdate<'a>, Error> {
    let (pane, rows, columns) = (body.u32()?, body.u16()?, body.u16()?);
    let flags = body.u8()?;
    if flags & !FLAGS != 0 {
        return Err(Error::BadTag {
            field: "frame flags",
            tag: flags,
        });
    }
    let mouse = body.tag(
        "mouse mode",
        &[
            MouseMode::Off,
            MouseMode::Click,
            MouseMode::Drag,
            MouseMode::Motion,
        ],
    )?;
    let mouse_encoding = body.tag(
        "mouse encoding",
        &[
            MouseEncoding::Legacy,
            MouseEncoding::Utf8,
            MouseEncoding::Sgr,
        ],
    )?;
    let cursor = (body.u16()?, body.u16()?);
    if flags & FLAG_CURSOR == 0 && cursor != (0, 0) {
        return Err(Error::OutOfBounds {
            field: "hidden cursor",
            value: cursor.0.into(),
        });
    }
    let clipboard_writes = body.u32()?;
    let (top, held) = (body.row()?, body.row()?..body.row()?);
    let changed = body.u16()?;
    let update = FrameUpdate {
        pane,
        full: flags & FLAG_FULL != 0,
        top,
        held,
        rows,
        columns,
        cursor: (flags & FLAG_CURSOR != 0).then_some(cursor),
        app_cursor: flags & FLAG_APP_CURSOR != 0,
        mouse,
        mouse_encoding,
        alternate_scroll: flags & FLAG_ALTERNATE_SCROLL != 0,
        clipboard_writes,
        changed,
        records: body.rest(),
    };
    update.check()?;
    update.check_records()?;
    Ok(update)
}

fn read_pane(bytes: &[u8]) -> Result<(u32, Rect), Error> {
    let mut pane = Reader(bytes);
    let number = pane.u32()?;
    let rect = Rect {
        x: pane.u16()?,
        y: pane.u16()?,
        columns: pane.u16()?,
        rows: pane.u16()?,
    };
    check("columns", rect.columns, 1..=MAX_DIMENSION)?;
    check("rows", rect.rows, 1..=MAX_DIMENSION)?;
    check("x", rect.x, 0..=MAX_DIMENSION - rect.columns)?;
    check("y", rect.y, 0..=MAX_DIMENSION - rect.rows)?;
    Ok((number, rect))
}

/// Whether two records of `size` bytes start with the same pane. These lists stop at
/// [`MAX_PANES`], so comparing each pair costs little and allocates nothing.
fn repeats(records: &[u8], size: usize) -> bool {
    let number = |record: &[u8]| record.first_chunk::<LAYOUT_TAB>().copied();
    let mut records = records.chunks_exact(size);
    while let Some(record) = records.next() {
        if records.clone().any(|later| number(later) == number(record)) {
            return true;
        }
    }
    false
}

fn read_layout<'a>(body: &mut Reader<'a>, version: u16) -> Result<Layout<'a>, Error> {
    let (active, focused, total) = (body.u16()?, body.u32()?, body.u16()?);
    let (tabs, panes) = (usize::from(body.u16()?), usize::from(body.u16()?));
    let bound = |field, value: usize, most: usize| {
        if value > most {
            return Err(Error::OutOfBounds {
                field,
                value: value as u64,
            });
        }
        Ok(())
    };
    bound("tabs", tabs, MAX_TABS)?;
    bound("panes", panes, usize::from(total))?;
    bound("session's panes", usize::from(total), MAX_PANES)?;
    bound("active tab", usize::from(active), tabs.saturating_sub(1))?;
    let tabs = body.take(tabs * LAYOUT_TAB)?;
    let panes = body.take(panes * LAYOUT_PANE)?;
    for pane in panes.as_chunks::<LAYOUT_PANE>().0 {
        read_pane(pane)?;
    }
    let members = if version >= INBOX {
        body.take(usize::from(total) * LAYOUT_MEMBER)?
    } else {
        &[]
    };
    let count = tabs.len() / LAYOUT_TAB;
    for member in members.as_chunks::<LAYOUT_MEMBER>().0 {
        if usize::from(member[4]) >= count {
            return Err(Error::OutOfBounds {
                field: "member's tab",
                value: member[4].into(),
            });
        }
    }
    if repeats(tabs, LAYOUT_TAB) || repeats(panes, LAYOUT_PANE) || repeats(members, LAYOUT_MEMBER) {
        return Err(Error::OutOfBounds {
            field: "repeated pane",
            value: u64::from(focused),
        });
    }
    Ok(Layout {
        active,
        focused,
        total,
        tabs,
        panes,
        members,
    })
}

/// Whether a [`Notice::Outside`] is well formed: a row below `count` of at most
/// [`MAX_OUTSIDE`] whose `dir` is absolute, at most [`MAX_DIR`] bytes and free of control and
/// bidi characters, or the empty refresh, `count` 0 with no `dir`.
fn outside_row(index: u8, count: u8, dir: &str) -> bool {
    if count == 0 {
        return index == 0 && dir.is_empty();
    }
    let plain = dir.chars().all(|c| !c.is_control() && !bidi(c));
    index < count && count <= MAX_OUTSIDE && dir.starts_with('/') && dir.len() <= MAX_DIR && plain
}

/// Whether a [`Notice::Toast`] is well formed: a known kind and text that is not empty, at most
/// [`MAX_TOAST`] bytes and free of control and bidi characters.
fn toast(kind: u8, text: &str) -> bool {
    let plain = text.chars().all(|c| !c.is_control() && !bidi(c));
    kind <= OSC_777 && !text.is_empty() && text.len() <= MAX_TOAST && plain
}

/// Whether `character` reorders text: the bidi controls and marks.
fn bidi(character: char) -> bool {
    matches!(
        character,
        '\u{61c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}

/// Reads fields from the front of a body, and never past its end.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        let (head, rest) = self.0.split_first_chunk::<N>().ok_or(Error::Truncated)?;
        self.0 = rest;
        Ok(*head)
    }

    fn u8(&mut self) -> Result<u8, Error> {
        self.array().map(u8::from_le_bytes)
    }

    fn u16(&mut self) -> Result<u16, Error> {
        self.array().map(u16::from_le_bytes)
    }

    fn u32(&mut self) -> Result<u32, Error> {
        self.array().map(u32::from_le_bytes)
    }

    fn i32(&mut self) -> Result<i32, Error> {
        self.array().map(i32::from_le_bytes)
    }

    /// Reads a byte that picks one of `values`.
    fn tag<T: Copy>(&mut self, field: &'static str, values: &[T]) -> Result<T, Error> {
        let tag = self.u8()?;
        let value = values.get(usize::from(tag)).copied();
        value.ok_or(Error::BadTag { field, tag })
    }

    fn row(&mut self) -> Result<usize, Error> {
        let row = self.array().map(u64::from_le_bytes)?;
        usize::try_from(row).map_err(|_| Error::OutOfBounds {
            field: "row",
            value: row,
        })
    }

    fn position(&mut self) -> Result<Position, Error> {
        Ok(Position {
            row: self.row()?,
            column: self.u16()?,
        })
    }

    fn selection(&mut self) -> Result<Selection, Error> {
        Ok(Selection {
            anchor: self.position()?,
            focus: self.position()?,
        })
    }

    fn colour(&mut self) -> Result<u32, Error> {
        let colour = self.u32()?;
        check_colour(&colour).map(|()| colour)
    }

    fn optional_colour(&mut self) -> Result<Option<u32>, Error> {
        match self.tag("colour", &[false, true])? {
            true => self.colour().map(Some),
            false => Ok(None),
        }
    }

    fn side(&mut self) -> Result<Side, Error> {
        self.tag("side", &[Side::Left, Side::Right, Side::Up, Side::Down])
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8], Error> {
        let (head, rest) = self.0.split_at_checked(count).ok_or(Error::Truncated)?;
        self.0 = rest;
        Ok(head)
    }

    fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.0)
    }

    fn text(&mut self) -> Result<&'a str, Error> {
        std::str::from_utf8(self.rest()).map_err(|_| Error::BadText)
    }

    fn sized_text(&mut self) -> Result<&'a str, Error> {
        let count = usize::try_from(self.u32()?).map_err(|_| Error::OutOfBounds {
            field: "text length",
            value: u64::MAX,
        })?;
        std::str::from_utf8(self.take(count)?).map_err(|_| Error::BadText)
    }

    fn end(&self) -> Result<(), Error> {
        match self.0.len() {
            0 => Ok(()),
            bytes => Err(Error::Trailing { bytes }),
        }
    }
}

#[cfg(test)]
mod tests {
    use proptest::collection::vec;
    use proptest::prelude::*;

    use super::*;

    /// Version 1's hello, spelled out because it must never change.
    const HELLO_WIRE: [u8; 7] = [3, 0, 0, 0, HELLO, 26, 0];
    /// Where fields sit in a frame message.
    const FLAGS_AT: usize = 13;
    const MOUSE_AT: usize = 14;
    const ENCODING_AT: usize = 15;
    const CURSOR_AT: usize = 16;
    const HELD_AT: usize = 32;
    const COUNT_AT: usize = 48;
    const ROWS_AT: usize = 50;
    const CELLS_AT: usize = 52;

    /// Any rectangle a peer accepts.
    fn rects() -> impl Strategy<Value = Rect> {
        (1..=MAX_DIMENSION, 1..=MAX_DIMENSION).prop_flat_map(|(columns, rows)| {
            (0..=MAX_DIMENSION - columns, 0..=MAX_DIMENSION - rows).prop_map(move |(x, y)| Rect {
                x,
                y,
                columns,
                rows,
            })
        })
    }

    fn wire_chunks<'a>(input: &mut &'a [u8], chunk: usize) -> std::slice::Chunks<'a, u8> {
        std::mem::take(input).chunks(chunk)
    }

    fn raw(kind: u8, body: &[u8]) -> Vec<u8> {
        let mut wire = ((body.len() + KIND) as u32).to_le_bytes().to_vec();
        wire.push(kind);
        wire.extend_from_slice(body);
        wire
    }

    fn cell(character: char) -> Cell {
        Cell {
            character,
            foreground: 0x00ff_ffff | BOLD,
            background: 0x0012_3456,
        }
    }

    fn screen(rows: u16, columns: u16) -> Frame {
        Frame {
            rows,
            columns,
            cells: vec![cell('a'); usize::from(rows) * usize::from(columns)],
            cursor: Some((rows - 1, columns - 1)),
            ..Frame::default()
        }
    }

    fn frame_wire(frame: &Frame, previous: Option<&Frame>) -> Vec<u8> {
        let mut wire = Vec::new();
        assert_eq!(
            encode_frame(&mut wire, 7, frame, 10, 2..40, previous),
            Ok(())
        );
        wire
    }

    /// What the decoder makes of `wire` as the first bytes of a link.
    fn first(wire: &[u8]) -> Result<(), Error> {
        Decoder::default().next_request(&mut &*wire).map(|_| ())
    }

    /// What the decoder makes of `wire` after a good hello, up to the first error.
    fn request_error(wire: &[u8]) -> Result<(), Error> {
        let mut decoder = Decoder::default();
        let mut input: &[u8] = &HELLO_WIRE;
        decoder.next_request(&mut input)?;
        input = wire;
        while decoder.next_request(&mut input)?.is_some() {}
        Ok(())
    }

    fn notice_error(wire: &[u8]) -> Result<(), Error> {
        let mut decoder = Decoder::default();
        let mut input: &[u8] = &HELLO_WIRE;
        decoder.next_notice(&mut input)?;
        input = wire;
        while decoder.next_notice(&mut input)?.is_some() {}
        Ok(())
    }

    /// Sends a frame through a decoder onto `drawn`, as a window would.
    fn apply(wire: &[u8], drawn: &mut Frame) -> Option<bool> {
        let mut decoder = Decoder::default();
        let mut input: &[u8] = &HELLO_WIRE;
        decoder.next_notice(&mut input).ok()?;
        input = wire;
        match decoder.next_notice(&mut input).ok()?? {
            Notice::Frame(update) => Some(update.apply(drawn)),
            _ => None,
        }
    }

    fn request_survives(message: Request<'_>, chunk: usize) -> bool {
        let mut wire = HELLO_WIRE.to_vec();
        if message.encode(&mut wire).is_err() {
            return false;
        }
        let mut decoder = Decoder::default();
        let mut seen = 0;
        for bytes in wire.chunks(chunk) {
            let mut input = bytes;
            while let Ok(Some(decoded)) = decoder.next_request(&mut input) {
                seen += 1;
                if seen == 2 && decoded != message {
                    return false;
                }
            }
            if !input.is_empty() {
                return false;
            }
        }
        seen == 2
    }

    fn notice_survives(message: &Notice<'_>, chunk: usize) -> bool {
        let mut wire = HELLO_WIRE.to_vec();
        if message.encode(&mut wire).is_err() {
            return false;
        }
        let mut decoder = Decoder::default();
        let mut seen = 0;
        for bytes in wire.chunks(chunk) {
            let mut input = bytes;
            while let Ok(Some(decoded)) = decoder.next_notice(&mut input) {
                seen += 1;
                if seen == 2 && decoded != *message {
                    return false;
                }
            }
            if !input.is_empty() {
                return false;
            }
        }
        seen == 2
    }

    #[test]
    fn hello_has_one_format_both_ways() {
        let (mut request, mut notice) = (Vec::new(), Vec::new());
        let version = VERSION;
        Request::Hello { version }.encode(&mut request).unwrap();
        Notice::Hello { version }.encode(&mut notice).unwrap();
        assert_eq!(request, HELLO_WIRE);
        assert_eq!(notice, HELLO_WIRE);
    }

    #[test]
    fn another_version_is_an_error_that_stays() {
        let wire = raw(HELLO, &(VERSION + 1).to_le_bytes());
        let mut decoder = Decoder::default();
        let error = Error::Version {
            ours: VERSION,
            theirs: VERSION + 1,
        };
        assert_eq!(decoder.next_request(&mut wire.as_slice()), Err(error));
        let mut good: &[u8] = &HELLO_WIRE;
        assert_eq!(decoder.next_request(&mut good), Err(error));
        assert_eq!(good, HELLO_WIRE, "a failed link takes no more bytes");
        assert_eq!(decoder.next_notice(&mut good), Err(error));
    }

    /// ADR-0043: a core takes a window one version older, a window a core one version either
    /// way, and both speak the lower. Two apart is still [`Error::Version`].
    #[test]
    fn a_hello_one_version_apart_speaks_the_lower() {
        let hello = |version: u16| raw(HELLO, &version.to_le_bytes());
        let cases = [
            (OLDEST, true, Some(OLDEST)),
            (VERSION, true, Some(VERSION)),
            (OLDEST, false, Some(OLDEST)),
            (VERSION + 1, false, Some(VERSION)),
            (VERSION + 1, true, None),
            (OLDEST - 1, true, None),
            (OLDEST - 1, false, None),
            (VERSION + 2, false, None),
        ];
        for (theirs, core, spoken) in cases {
            let mut decoder = Decoder::default();
            let wire = hello(theirs);
            let decoded = if core {
                decoder.next_request(&mut wire.as_slice()).map(|_| ())
            } else {
                decoder.next_notice(&mut wire.as_slice()).map(|_| ())
            };
            let error = Error::Version {
                ours: VERSION,
                theirs,
            };
            assert_eq!(decoded, spoken.map(|_| ()).ok_or(error), "{theirs} {core}");
            assert_eq!(decoder.version(), spoken, "{theirs} {core}");
        }
    }

    #[test]
    fn a_message_newer_than_the_spoken_version_ends_the_link() {
        let at_oldest = |notice: Notice<'_>| {
            let mut wire = raw(HELLO, &OLDEST.to_le_bytes());
            notice.encode(&mut wire).unwrap();
            let mut decoder = Decoder::default();
            let mut input = wire.as_slice();
            decoder.next_notice(&mut input).unwrap();
            decoder
                .next_notice(&mut input)
                .map(|notice| notice.is_some())
        };
        let owner = Notice::Approval(Approval {
            id: 1,
            caller: 0,
            target: 0,
            action: ApprovalAction::Owner,
            texts: vec!["session.save"],
        });
        assert_eq!(at_oldest(owner), Ok(true));
        let markdown = Notice::Markdown {
            pane: 1,
            names: "a.md",
        };
        assert_eq!(at_oldest(markdown), Ok(true));
        let refused = Notice::Refused {
            reason: Refusal::Protected,
        };
        // 19, the oldest spoken, has it.
        assert_eq!(at_oldest(refused), Ok(true));
        let extra = Notice::InboxExtra {
            pane: 1,
            branch: "main",
            ctx: Some(64),
            steps: "Bash",
        };
        // The oldest spoken has its context use and steps too.
        assert_eq!(at_oldest(extra), Ok(true));
        let outside = Notice::Outside {
            index: 0,
            count: 1,
            secs: 3,
            dir: "/work",
        };
        // The oldest spoken has outside rows and their refusal.
        assert_eq!(at_oldest(outside), Ok(true));
        let folder = Notice::Refused {
            reason: Refusal::Folder,
        };
        assert_eq!(at_oldest(folder), Ok(true));
        let toast = Notice::Toast {
            pane: 1,
            kind: OSC_9,
            text: "Build done",
        };
        // 25, the oldest spoken, has toasts.
        assert_eq!(at_oldest(toast), Ok(true));
        let progress = Notice::Progress {
            pane: 1,
            progress: progress(NORMAL, 40),
        };
        let newer = Error::Newer {
            kind: PROGRESS_KIND,
            since: PROGRESS,
        };
        assert_eq!(
            at_oldest(progress),
            Err(newer),
            "a window of 25 never gets it"
        );
        assert_eq!(at_oldest(COLOURS_SEED), Ok(true));
        let secrets = Notice::Secrets {
            count: 1,
            names: "GITHUB_TOKEN",
        };
        assert_eq!(at_oldest(secrets), Ok(true));
        let webhook = Notice::Webhook {
            host: "ntfy.sh",
            sender: true,
            failed: 0,
        };
        assert_eq!(at_oldest(webhook), Ok(true));
        // ADR-0050: a window a version older still gets a rewind prompt, a strip and a flash.
        let rewind = Notice::Approval(Approval {
            id: 1,
            caller: 2,
            target: 2,
            action: ApprovalAction::Rewind,
            texts: vec!["/work"],
        });
        let strip = Notice::Checkpoint {
            pane: 2,
            missing: 1,
        };
        let flash = Notice::Restored {
            undid: 1,
            skipped: 0,
            error: "",
        };
        for notice in [rewind, strip, flash] {
            assert_eq!(at_oldest(notice), Ok(true));
        }
        // 24, the oldest spoken, opens the viewer on a run too.
        let mut wire = raw(HELLO, &OLDEST.to_le_bytes());
        let open = Request::OpenTool {
            pane: 2,
            tool: Tool::Rewind,
        };
        open.encode(&mut wire).unwrap();
        let (mut decoder, mut input) = (Decoder::default(), wire.as_slice());
        decoder.next_request(&mut input).unwrap();
        assert_eq!(
            decoder
                .next_request(&mut input)
                .map(|request| request.is_some()),
            Ok(true)
        );
        // Nothing is new in 16 but a layout, so a stand-in `since` checks the gate.
        let mut wire = raw(HELLO, &OLDEST.to_le_bytes());
        Notice::Raise.encode(&mut wire).unwrap();
        let (mut decoder, mut input) = (Decoder::default(), wire.as_slice());
        let newest = |notice: &Notice<'_>| match notice {
            Notice::Raise => VERSION,
            _ => OLDEST,
        };
        let mut next = |input: &mut &[u8]| {
            let next = decoder.next(input, MAX_MESSAGE, VERSION, Notice::decode, newest);
            next.map(|notice| notice.is_some())
        };
        assert_eq!(next(&mut input), Ok(true));
        let newer = Error::Newer {
            kind: RAISE,
            since: VERSION,
        };
        assert_eq!(next(&mut input), Err(newer));
        let mut wire = raw(HELLO, &OLDEST.to_le_bytes());
        PALETTE_SEED.encode(&mut wire).unwrap();
        let mut decoder = Decoder::default();
        let mut input = wire.as_slice();
        decoder.next_request(&mut input).unwrap();
        assert_eq!(decoder.next_request(&mut input), Ok(Some(PALETTE_SEED)));
    }

    /// ADR-0051: every version spoken gets a pane's times, whether it names its tab, and its
    /// profile.
    #[test]
    fn a_status_is_laid_out_for_the_spoken_version() {
        let with = |profile| Notice::Status {
            pane: 7,
            state: 0,
            reported: false,
            naming: false,
            listed: true,
            since: 42,
            started: 600,
            profile,
            message: "ok",
        };
        let status = with(Some(3));
        let read = |version: u16| {
            let mut wire = raw(HELLO, &version.to_le_bytes());
            status.encode_at(version, &mut wire).unwrap();
            let (mut decoder, mut input) = (Decoder::default(), wire.as_slice());
            decoder.next_notice(&mut input).unwrap();
            let notice = decoder
                .next_notice(&mut input)
                .unwrap()
                .map(|n| format!("{n:?}"));
            (wire.len(), notice)
        };
        let (old, older) = read(OLDEST);
        let (new, newer) = read(VERSION);
        assert_eq!(newer, Some(format!("{status:?}")));
        // 19, the oldest spoken, has the profile too.
        assert_eq!((older, old), (newer, new));
        let none = with(None);
        let mut wire = raw(HELLO, &VERSION.to_le_bytes());
        none.encode(&mut wire).unwrap();
        let (mut decoder, mut input) = (Decoder::default(), wire.as_slice());
        decoder.next_notice(&mut input).unwrap();
        assert_eq!(decoder.next_notice(&mut input), Ok(Some(none)));
    }

    /// ADR-0051 §6: an answer's names are bounded, and its key comes from the key table, before
    /// the core compares anything.
    #[test]
    fn an_answer_carries_only_a_bounded_profile_and_a_known_key() {
        let answer = |choice, profile: &'static str, key: &'static str| Request::Answer {
            pane: 7,
            choice,
            profile,
            key,
        };
        for good in [
            answer(Choice::Approve, "claude", "Enter"),
            answer(Choice::Approve, "a", "1"),
            answer(Choice::Interrupt, "claude", ""),
        ] {
            assert!(request_survives(good, 3), "{good:?}");
            assert_eq!(good.since(), 19);
        }
        let long = "a".repeat(MAX_ANSWER_NAME + 1);
        let too_long = Request::Answer {
            pane: 7,
            choice: Choice::Approve,
            profile: &long,
            key: "Enter",
        };
        assert!(too_long.encode(&mut Vec::new()).is_err());
        // Hand-made, as the encoder refuses none of these keys.
        let wire = |choice: u8, profile: &[u8], key: &[u8]| {
            let mut body = 7u32.to_le_bytes().to_vec();
            body.extend_from_slice(&[choice, profile.len() as u8]);
            body.extend_from_slice(profile);
            body.extend_from_slice(key);
            raw(ANSWER_KIND, &body)
        };
        let decode = |wire: Vec<u8>| {
            let mut input = &raw(HELLO, &VERSION.to_le_bytes())[..];
            let mut decoder = Decoder::default();
            decoder.next_request(&mut input).unwrap();
            let mut input = wire.as_slice();
            decoder
                .next_request(&mut input)
                .map(|request| format!("{request:?}"))
        };
        assert!(decode(wire(0, b"claude", b"Enter")).is_ok());
        for bad in [
            wire(0, b"claude", b"yes"),
            wire(0, b"claude", b""),
            wire(0, b"claude", b"\x1b[A"),
            wire(1, b"claude", b"Enter"),
            wire(2, b"claude", b""),
            wire(0, &[b'a'; MAX_ANSWER_NAME + 1], b"Enter"),
        ] {
            assert!(decode(bad.clone()).is_err(), "{bad:?}");
        }
    }

    const PALETTE_SEED: Request<'static> = Request::Palette(Palette::GRAPHITE_GRID);
    const COLOURS_SEED: Notice<'static> = Notice::Colours {
        pane: 1,
        cursor: Some(0x00f5_b544),
        background: None,
    };

    /// The `link` fuzz target reads a chunk size, then what follows a hello.
    #[test]
    fn the_fuzz_seeds_are_the_two_colour_messages() {
        let mut wire = vec![7];
        PALETTE_SEED.encode(&mut wire).unwrap();
        let seed: &[u8] = include_bytes!("../../../fuzz/seeds/link/palette");
        assert_eq!(seed, wire);
        wire.truncate(1);
        COLOURS_SEED.encode(&mut wire).unwrap();
        let seed: &[u8] = include_bytes!("../../../fuzz/seeds/link/colours");
        assert_eq!(seed, wire);
    }

    #[test]
    fn a_colour_above_ffffff_is_refused_both_ways() {
        let mut out = vec![1];
        for at in 0..18 {
            let mut palette = Palette::GRAPHITE_GRID;
            let colours = [&mut palette.background, &mut palette.foreground];
            *colours
                .into_iter()
                .chain(&mut palette.ansi)
                .nth(at)
                .unwrap() = 0x0100_0000;
            assert_eq!(
                Request::Palette(palette).encode(&mut out),
                Err(Error::BadColour)
            );
            let mut wire = Vec::new();
            PALETTE_SEED.encode(&mut wire).unwrap();
            wire[PREFIX + KIND + 4 * at + 3] = 1;
            assert_eq!(request_error(&wire), Err(Error::BadColour), "{at}");
            let short = raw(PALETTE, &wire[PREFIX + KIND..PREFIX + KIND + 4 * at]);
            assert_eq!(request_error(&short), Err(Error::Truncated), "{at}");
        }
        for (cursor, background) in [(Some(u32::MAX), None), (None, Some(0x0100_0000))] {
            let notice = Notice::Colours {
                pane: 0,
                cursor,
                background,
            };
            assert_eq!(notice.encode(&mut out), Err(Error::BadColour));
        }
        assert_eq!(out, [1]);
        let body = [0, 0, 0, 0, 1, 0, 0, 0, 1, 0];
        assert_eq!(notice_error(&raw(COLOURS, &body)), Err(Error::BadColour));
    }

    #[test]
    fn a_hello_with_anything_after_its_version_is_refused() {
        let wire = raw(HELLO, &[1, 0, 0]);
        assert_eq!(first(&wire), Err(Error::Trailing { bytes: 1 }));
    }

    #[test]
    fn the_version_check_comes_first_and_once() {
        let wire = raw(QUIT, &[]);
        assert_eq!(first(&wire), Err(Error::NoHello { kind: QUIT }));
        assert_eq!(request_error(&HELLO_WIRE), Err(Error::RepeatedHello));
        assert_eq!(notice_error(&HELLO_WIRE), Err(Error::RepeatedHello));
    }

    #[test]
    fn a_length_over_the_limit_is_refused_before_any_allocation() {
        let wire = ((MAX_MESSAGE + 1) as u32).to_le_bytes();
        let mut decoder = Decoder::default();
        let error = Error::TooLarge {
            size: (MAX_MESSAGE + 1) as u64,
        };
        assert_eq!(decoder.next_request(&mut wire.as_slice()), Err(error));
        assert!(decoder.buffer.capacity() <= PREFIX);
        let wire = u32::MAX.to_le_bytes();
        assert!(matches!(first(&wire), Err(Error::TooLarge { .. })));
    }

    #[test]
    fn a_length_of_zero_has_no_type() {
        let wire = 0_u32.to_le_bytes();
        assert_eq!(first(&wire), Err(Error::Truncated));
    }

    #[test]
    fn a_promised_length_allocates_only_what_arrived() {
        let mut wire = (MAX_REQUEST as u32).to_le_bytes().to_vec();
        wire.extend_from_slice(&[HELLO, 1]);
        let mut decoder = Decoder::default();
        assert_eq!(decoder.next_request(&mut wire.as_slice()), Ok(None));
        assert!(decoder.buffer.capacity() <= 2 * PREFIX + 2);
    }

    #[test]
    fn the_largest_frame_fits_exactly() {
        let frame = screen(MAX_DIMENSION, MAX_DIMENSION);
        let wire = frame_wire(&frame, None);
        assert_eq!(wire.len(), PREFIX + MAX_MESSAGE);
        let mut drawn = Frame::default();
        assert_eq!(apply(&wire, &mut drawn), Some(true));
        assert_eq!(drawn, frame);
    }

    #[test]
    fn the_buffer_never_passes_one_message_and_is_reused() {
        let wire = frame_wire(&screen(MAX_DIMENSION, MAX_DIMENSION), None);
        let mut decoder = Decoder::default();
        let mut input: &[u8] = &HELLO_WIRE;
        assert!(decoder.next_notice(&mut input).unwrap().is_some());
        for chunk in wire.chunks(4096) {
            let mut input = chunk;
            let _ = decoder.next_notice(&mut input).unwrap();
            assert!(decoder.buffer.capacity() <= PREFIX + MAX_MESSAGE);
        }

        let small = raw(RAISE, &[]);
        assert!(
            decoder
                .next_notice(&mut small.as_slice())
                .unwrap()
                .is_some()
        );
        assert!(decoder.buffer.capacity() <= KEEP);
        let capacity = decoder.buffer.capacity();
        assert!(
            decoder
                .next_notice(&mut small.as_slice())
                .unwrap()
                .is_some()
        );
        assert_eq!(decoder.buffer.capacity(), capacity);
    }

    #[test]
    fn a_chunk_is_taken_one_message_at_a_time() {
        let mut wire = HELLO_WIRE.to_vec();
        wire.extend_from_slice(&raw(QUIT, &[]));
        wire.extend_from_slice(&raw(QUIT, &[]));
        let (mut decoder, mut input) = (Decoder::default(), wire.as_slice());
        assert!(decoder.next_request(&mut input).unwrap().is_some());
        assert_eq!(input.len(), 10);
        assert_eq!(decoder.next_request(&mut input), Ok(Some(Request::Quit)));
        assert_eq!(input.len(), 5);
        assert_eq!(decoder.next_request(&mut input), Ok(Some(Request::Quit)));
        assert_eq!(decoder.next_request(&mut input), Ok(None));
    }

    #[test]
    fn a_frame_sends_only_what_changed() {
        let mut frame = screen(35, 100);
        let previous = frame.clone();
        let full = frame_wire(&frame, None);
        assert_eq!(full.len(), 42_120);

        frame.cells[1000] = cell('b');
        let one_row = frame_wire(&frame, Some(&previous));
        assert_eq!(one_row.len(), 1252);

        let mut drawn = previous.clone();
        assert_eq!(apply(&one_row, &mut drawn), Some(true));
        assert_eq!(drawn, frame);
    }

    #[test]
    fn state_alone_sends_no_rows() {
        let previous = screen(4, 8);
        let mut frame = previous.clone();
        frame.cursor = None;
        frame.mouse = MouseMode::Drag;
        frame.clipboard_writes = 3;
        let wire = frame_wire(&frame, Some(&previous));
        assert_eq!(wire.len(), PREFIX + KIND + FRAME_HEAD);
        let mut drawn = previous;
        assert_eq!(apply(&wire, &mut drawn), Some(true));
        assert_eq!(drawn, frame);
    }

    #[test]
    fn a_new_size_or_no_previous_frame_sends_every_row() {
        let frame = screen(3, 4);
        let wire = frame_wire(&frame, Some(&screen(3, 5)));
        assert_eq!(wire, frame_wire(&frame, None));
        assert_eq!(wire[FLAGS_AT] & FLAG_FULL, FLAG_FULL);
    }

    #[test]
    fn changes_to_a_frame_of_another_size_ask_for_a_full_one() {
        let previous = screen(3, 4);
        let mut frame = previous.clone();
        frame.cells[0] = cell('b');
        let wire = frame_wire(&frame, Some(&previous));
        let mut drawn = screen(3, 5);
        let before = drawn.clone();
        assert_eq!(apply(&wire, &mut drawn), Some(false));
        assert_eq!(drawn, before);
    }

    #[test]
    fn encoding_refuses_a_frame_the_peer_would_refuse() {
        let frame = screen(2, 2);
        let mut out = vec![1];
        let mut encode = |frame: &Frame, held| encode_frame(&mut out, 0, frame, 0, held, None);
        assert!(matches!(
            encode(&Frame::default(), 0..0),
            Err(Error::OutOfBounds { field: "rows", .. })
        ));
        let cells = Frame {
            cells: vec![],
            ..frame.clone()
        };
        assert!(matches!(
            encode(&cells, 0..0),
            Err(Error::OutOfBounds { field: "cells", .. })
        ));
        let cursor = Frame {
            cursor: Some((2, 0)),
            ..frame.clone()
        };
        assert!(encode(&cursor, 0..0).is_err());
        assert!(encode(&frame, Range { start: 2, end: 1 }).is_err());
        assert_eq!(out, [1]);
    }

    #[test]
    fn a_resize_out_of_bounds_is_refused_both_ways() {
        for (rows, columns) in [(0, 80), (24, 0), (MAX_DIMENSION + 1, 80), (24, u16::MAX)] {
            let message = Request::Resize { rows, columns };
            let mut out = Vec::new();
            assert!(message.encode(&mut out).is_err());
            assert!(out.is_empty());
            let mut body = rows.to_le_bytes().to_vec();
            body.extend_from_slice(&columns.to_le_bytes());
            assert!(matches!(
                request_error(&raw(RESIZE, &body)),
                Err(Error::OutOfBounds { .. })
            ));
        }
    }

    fn layout_body(active: u16, tabs: &[u32], panes: &[(u32, Rect)]) -> Vec<u8> {
        let mut body = active.to_le_bytes().to_vec();
        body.extend_from_slice(&7_u32.to_le_bytes());
        body.extend_from_slice(&(MAX_PANES as u16).to_le_bytes());
        body.extend_from_slice(&(tabs.len() as u16).to_le_bytes());
        body.extend_from_slice(&(panes.len() as u16).to_le_bytes());
        for tab in tabs {
            body.extend_from_slice(&tab.to_le_bytes());
        }
        for (pane, rect) in panes {
            body.extend_from_slice(&pane.to_le_bytes());
            for side in [rect.x, rect.y, rect.columns, rect.rows] {
                body.extend_from_slice(&side.to_le_bytes());
            }
        }
        for (pane, tab) in members() {
            body.extend_from_slice(&pane.to_le_bytes());
            body.push(tab);
        }
        body
    }

    /// The session's [`MAX_PANES`] panes, all in the first tab.
    fn members() -> Vec<(u32, u8)> {
        (0..MAX_PANES as u32).map(|pane| (1000 + pane, 0)).collect()
    }

    const CORNER: Rect = Rect {
        x: 0,
        y: 0,
        columns: 80,
        rows: 24,
    };

    #[test]
    fn a_layout_past_its_limits_is_refused_both_ways() {
        type Bad<'a> = (u16, &'a [u32], &'a [(u32, Rect)], &'a str);
        let tabs: Vec<u32> = (0..=MAX_TABS as u32).collect();
        let panes: Vec<(u32, Rect)> = (0..=MAX_PANES as u32).map(|pane| (pane, CORNER)).collect();
        let off = |rect| [(1, rect)];
        let bad: [Bad; 8] = [
            (0, &tabs, &[], "tabs"),
            (0, &[1], &panes, "panes"),
            (1, &[1], &[], "active tab"),
            (1, &[], &[], "active tab"),
            (0, &[1, 1], &[], "repeated pane"),
            (0, &[1], &[(2, CORNER), (2, CORNER)], "repeated pane"),
            (
                0,
                &[1],
                &off(Rect {
                    columns: 0,
                    ..CORNER
                }),
                "columns",
            ),
            (
                0,
                &[1],
                &off(Rect {
                    x: MAX_DIMENSION - 79,
                    ..CORNER
                }),
                "x",
            ),
        ];
        for (active, tabs, panes, field) in bad {
            let mut out = vec![9];
            let total = MAX_PANES as u16;
            let error = encode_layout(&mut out, VERSION, active, 7, total, tabs, panes, &members())
                .unwrap_err();
            assert!(matches!(error, Error::OutOfBounds { field: f, .. } if f == field));
            assert_eq!(out, [9]);
            let wire = raw(LAYOUT, &layout_body(active, tabs, panes));
            assert_eq!(notice_error(&wire), Err(error));
        }
    }

    #[test]
    fn a_layout_that_promises_more_than_it_holds_is_refused() {
        let mut body = layout_body(0, &[1], &[(2, CORNER)]);
        body.truncate(body.len() - 1);
        assert_eq!(notice_error(&raw(LAYOUT, &body)), Err(Error::Truncated));
        let mut body = layout_body(0, &[1], &[(2, CORNER)]);
        body.push(0);
        let wire = raw(LAYOUT, &body);
        assert_eq!(notice_error(&wire), Err(Error::Trailing { bytes: 1 }));
    }

    #[test]
    fn an_empty_session_is_a_layout() {
        let mut wire = HELLO_WIRE.to_vec();
        encode_layout(&mut wire, VERSION, 0, 0, 0, &[], &[], &[]).unwrap();
        let (mut decoder, mut input) = (Decoder::default(), wire.as_slice());
        decoder.next_notice(&mut input).unwrap();
        let notice = decoder.next_notice(&mut input).unwrap();
        assert!(matches!(notice, Some(Notice::Layout(layout)) if layout.is_empty()));
    }

    #[test]
    fn a_layout_names_each_panes_tab_from_17_and_none_before() {
        let members = [(4, 0), (5, 1), (6, 1)];
        let read = |version: u16| {
            let mut wire = raw(HELLO, &version.to_le_bytes());
            encode_layout(&mut wire, version, 1, 5, 3, &[4, 5], &[], &members).unwrap();
            let (mut decoder, mut input) = (Decoder::default(), wire.as_slice());
            decoder.next_notice(&mut input).unwrap();
            match decoder.next_notice(&mut input).unwrap() {
                Some(Notice::Layout(layout)) => layout.members().collect::<Vec<_>>(),
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(read(VERSION), members);
        // 16, which reads no members, is no longer spoken (ADR-0043).
        assert_eq!(read(OLDEST), members);
        let refused = |members: &[(u32, u8)]| {
            let mut out = Vec::new();
            encode_layout(&mut out, VERSION, 0, 4, 2, &[4, 5], &[], members).unwrap_err()
        };
        assert!(matches!(
            refused(&[(4, 0), (5, 2)]),
            Error::OutOfBounds {
                field: "member's tab",
                ..
            }
        ));
        assert!(matches!(
            refused(&[(4, 0), (4, 1)]),
            Error::OutOfBounds {
                field: "repeated pane",
                ..
            }
        ));
        assert_eq!(refused(&[(4, 0)]), Error::Truncated);
    }

    #[test]
    fn commands_out_of_bounds_are_refused_both_ways() {
        let tab = Request::SwitchTab {
            index: MAX_TABS as u16,
        };
        let border = |cells| Request::MoveBorder {
            side: Side::Up,
            cells,
        };
        for message in [tab, border(0), border(MAX_DIMENSION + 1)] {
            assert!(matches!(
                message.encode(&mut Vec::new()),
                Err(Error::OutOfBounds { .. })
            ));
        }
        assert!(matches!(
            request_error(&raw(SWITCH_TAB, &(MAX_TABS as u16).to_le_bytes())),
            Err(Error::OutOfBounds { .. })
        ));
        assert!(matches!(
            request_error(&raw(MOVE_BORDER, &[0, 0, 0])),
            Err(Error::OutOfBounds { .. })
        ));
        for (kind, body, field) in [
            (STEP_TAB, [2].as_slice(), "tab step"),
            (SPLIT, [0, 0, 0, 0, 2].as_slice(), "split"),
            (MOVE_FOCUS, [4].as_slice(), "side"),
            (MOVE_BORDER, [4, 1, 0].as_slice(), "side"),
        ] {
            let error = request_error(&raw(kind, body));
            assert!(matches!(error, Err(Error::BadTag { field: f, .. }) if f == field));
        }
        let error = notice_error(&raw(REFUSED, &[11]));
        assert!(matches!(
            error,
            Err(Error::BadTag {
                field: "refusal",
                ..
            })
        ));
    }

    #[test]
    fn a_window_cannot_send_what_only_the_core_may() {
        let wire = ((MAX_REQUEST + 1) as u32).to_le_bytes();
        let error = Error::TooLarge {
            size: (MAX_REQUEST + 1) as u64,
        };
        assert_eq!(first(&wire), Err(error));
        assert_eq!(
            Decoder::default().next_notice(&mut wire.as_slice()),
            Ok(None)
        );

        let text = "x".repeat(MAX_PASTE);
        let mut wire = HELLO_WIRE.to_vec();
        let paste = Request::Paste {
            pane: 1,
            text: &text,
        };
        paste.encode(&mut wire).unwrap();
        assert_eq!(
            wire.len(),
            HELLO_WIRE.len() + PREFIX + MAX_REQUEST - BRACKETS
        );
        let mut decoder = Decoder::default();
        let mut input = wire.as_slice();
        assert!(decoder.next_request(&mut input).unwrap().is_some());
        assert_eq!(decoder.next_request(&mut input), Ok(Some(paste)));

        let mut body = 1_u32.to_le_bytes().to_vec();
        body.resize(4 + MAX_PASTE + 1, b'x');
        let mut wire = HELLO_WIRE.to_vec();
        wire.extend_from_slice(&raw(PASTE, &body));
        let mut input = wire.as_slice();
        let mut decoder = Decoder::default();
        assert!(decoder.next_request(&mut input).unwrap().is_some());
        assert!(matches!(
            decoder.next_request(&mut input),
            Err(Error::TooLarge { .. })
        ));

        let text = "x".repeat(MAX_PASTE + 1);
        let paste = Request::Paste {
            pane: 1,
            text: &text,
        };
        assert!(matches!(
            paste.encode(&mut wire),
            Err(Error::TooLarge { .. })
        ));
    }

    #[test]
    fn polling_with_nothing_new_allocates_nothing() {
        let mut wire = (MAX_REQUEST as u32).to_le_bytes().to_vec();
        wire.extend_from_slice(&[HELLO, 1]);
        let mut decoder = Decoder::default();
        assert_eq!(decoder.next_request(&mut wire.as_slice()), Ok(None));
        let capacity = decoder.buffer.capacity();
        for _ in 0..64 {
            assert_eq!(decoder.next_request(&mut [].as_slice()), Ok(None));
        }
        assert_eq!(decoder.buffer.capacity(), capacity);
    }

    #[test]
    fn a_search_over_the_limit_is_refused_before_it_is_copied() {
        let query = "x".repeat(MAX_REQUEST + 1);
        let mut out = vec![1];
        let message = Request::Search {
            pane: 0,
            from: Position::default(),
            direction: Direction::Backward,
            query: &query,
        };
        assert!(matches!(
            message.encode(&mut out),
            Err(Error::TooLarge { .. })
        ));
        assert_eq!(out, [1]);
    }

    #[test]
    fn nothing_a_peer_would_refuse_encodes() {
        let mut frame = screen(2, 2);
        frame.cells[3].background = 0xff00_0000;
        let mut out = vec![1];
        let result = encode_frame(&mut out, 0, &frame, 0, 0..2, None);
        assert_eq!(result, Err(Error::BadColour));
        assert_eq!(out, [1]);

        let update = FrameUpdate {
            pane: 0,
            full: true,
            top: 0,
            held: 0..2,
            rows: 2,
            columns: 2,
            cursor: None,
            app_cursor: false,
            mouse: MouseMode::Off,
            mouse_encoding: MouseEncoding::Legacy,
            alternate_scroll: false,
            clipboard_writes: 0,
            changed: 1,
            records: &[],
        };
        assert!(Notice::Frame(update.clone()).encode(&mut out).is_err());
        let held = FrameUpdate {
            full: false,
            changed: 0,
            cursor: Some((9, 0)),
            ..update
        };
        assert!(Notice::Frame(held).encode(&mut out).is_err());
        assert_eq!(out, [1]);
    }

    #[test]
    fn a_message_over_the_limit_does_not_encode() {
        let text = "x".repeat(MAX_MESSAGE);
        let mut out = vec![1];
        let message = Request::Paste {
            pane: 0,
            text: &text,
        };
        assert!(matches!(
            message.encode(&mut out),
            Err(Error::TooLarge { .. })
        ));
        let message = Notice::Copied {
            pane: 0,
            text: &text,
        };
        assert!(matches!(
            message.encode(&mut out),
            Err(Error::TooLarge { .. })
        ));
        // Under the message limit, but over what the decoder takes for a command.
        let command = "x".repeat(MAX_COMMAND + 1);
        let message = Notice::WasRunning {
            pane: 0,
            runnable: true,
            command: &command,
        };
        assert!(matches!(
            message.encode(&mut out),
            Err(Error::TooLarge { .. })
        ));
        assert_eq!(out, [1]);
    }

    #[test]
    fn a_message_cut_anywhere_short_is_refused() {
        for (kind, length) in [
            (RESIZE, 4),
            (SCROLL, 8),
            (REVEAL, 12),
            (SEARCH, 15),
            (COPY, 24),
            (LINK, 14),
            (SWITCH_TAB, 2),
            (STEP_TAB, 1),
            (SPLIT, 5),
            (CLOSE_PANE, 4),
            (FOCUS, 4),
            (MOVE_FOCUS, 1),
            (MOVE_BORDER, 3),
        ] {
            for short in 0..length {
                let wire = raw(kind, &vec![1; short]);
                assert_eq!(
                    request_error(&wire),
                    Err(Error::Truncated),
                    "{kind} {short}"
                );
            }
        }
        let wire = frame_wire(&screen(2, 3), None);
        for short in 0..wire.len() - PREFIX - KIND {
            let body = &wire[PREFIX + KIND..PREFIX + KIND + short];
            assert_eq!(notice_error(&raw(FRAME, body)), Err(Error::Truncated));
        }
    }

    #[test]
    fn a_message_that_goes_on_is_refused() {
        let wire = raw(QUIT, &[0]);
        assert_eq!(request_error(&wire), Err(Error::Trailing { bytes: 1 }));
        let wire = raw(RAISE, &[0; 2]);
        assert_eq!(notice_error(&wire), Err(Error::Trailing { bytes: 2 }));
        let frame = frame_wire(&screen(2, 3), None);
        let mut body = frame[PREFIX + KIND..].to_vec();
        body.push(0);
        assert_eq!(
            notice_error(&raw(FRAME, &body)),
            Err(Error::Trailing { bytes: 1 })
        );
    }

    #[test]
    fn an_unknown_type_is_refused() {
        let wire = raw(200, &[]);
        assert_eq!(request_error(&wire), Err(Error::UnknownKind { kind: 200 }));
        assert_eq!(notice_error(&wire), Err(Error::UnknownKind { kind: 200 }));
    }

    #[test]
    fn enum_tags_are_checked() {
        let mut body = vec![0; 14];
        body.push(2);
        assert_eq!(
            request_error(&raw(SEARCH, &body)),
            Err(Error::BadTag {
                field: "direction",
                tag: 2
            })
        );
        assert_eq!(
            notice_error(&raw(SEARCHED, &[0, 0, 0, 0, 2])),
            Err(Error::BadTag {
                field: "search result",
                tag: 2
            })
        );
        let good = frame_wire(&screen(2, 3), None);
        for (at, tag, field) in [
            (FLAGS_AT, FLAGS + 1, "frame flags"),
            (MOUSE_AT, 4, "mouse mode"),
            (ENCODING_AT, 3, "mouse encoding"),
        ] {
            let mut wire = good.clone();
            wire[at] = tag;
            assert_eq!(notice_error(&wire), Err(Error::BadTag { field, tag }));
        }
    }

    /// ADR-0051 §7: a branch past 40 bytes or with anything but printable ASCII is refused.
    #[test]
    fn an_inbox_branch_is_short_printable_ascii() {
        let wire = |branch: &[u8]| {
            let mut body = 7u32.to_le_bytes().to_vec();
            body.extend([NO_CTX, branch.len() as u8]);
            body.extend_from_slice(branch);
            raw(INBOX_EXTRA_KIND, &body)
        };
        let read = |branch: &[u8]| notice_error(&wire(branch));
        assert_eq!(read(&[b'a'; MAX_BRANCH]), Ok(()));
        let long = Error::TooLarge {
            size: MAX_BRANCH as u64 + 1,
        };
        assert_eq!(read(&[b'a'; MAX_BRANCH + 1]), Err(long));
        for hostile in [&b"a\x1bb"[..], b"a\nb", "a\u{202e}b".as_bytes(), b"\x7f"] {
            assert_eq!(read(hostile), Err(Error::BadText), "{hostile:?}");
        }
    }

    /// ADR-0051 §7: the window draws an agent's steps as they come, so the decoder takes only
    /// what the core's cleaning lets through.
    #[test]
    fn inbox_steps_are_three_short_clean_lines_at_most() {
        let wire = |ctx: u8, steps: &[u8]| {
            let mut body = 7u32.to_le_bytes().to_vec();
            body.extend([ctx, 4]);
            body.extend_from_slice(b"main");
            body.extend_from_slice(steps);
            raw(INBOX_EXTRA_KIND, &body)
        };
        let read = |ctx, steps: &[u8]| notice_error(&wire(ctx, steps));
        assert_eq!(read(64, "Read\nNow the parser.\n編集".as_bytes()), Ok(()));
        assert_eq!(read(NO_CTX, b""), Ok(()));
        assert_eq!(read(100, &[b'a'; MAX_STEPS]), Ok(()));
        let long = Error::TooLarge {
            size: MAX_STEPS as u64 + 1,
        };
        assert_eq!(read(1, &[b'a'; MAX_STEPS + 1]), Err(long));
        let ctx = Error::BadTag {
            field: "ctx",
            tag: 101,
        };
        assert_eq!(read(101, b"a"), Err(ctx));
        let hostile: [&[u8]; 6] = [
            b"a\nb\nc\nd",
            b"a\n\nb",
            b"a\n",
            b"a\x1b]0;x\x07",
            "a\u{202e}b".as_bytes(),
            b"\xff",
        ];
        for steps in hostile {
            assert_eq!(read(1, steps), Err(Error::BadText), "{steps:?}");
        }
    }

    /// ADR-0051 §9, amendment of slice 12: every state and percent fits one byte, 0 and 100 come
    /// back exact, a percent past 100 is clamped, and a byte no core sends reads as hidden.
    #[test]
    fn a_progress_byte_keeps_every_state_and_its_percent_to_a_step() {
        for state in [NORMAL, FAILED, PAUSED] {
            for percent in 0..=u8::MAX {
                let (back, shown) = progress_parts(progress(state, percent));
                assert_eq!(back, state);
                let percent = percent.min(100);
                assert!(shown.abs_diff(percent) <= 2, "{percent} came back {shown}");
            }
            for step in 0..=31 {
                let byte = state << 5 | step;
                assert_eq!(progress(state, progress_parts(byte).1), byte, "stable");
            }
            assert_eq!(progress_parts(progress(state, 0)), (state, 0));
            assert_eq!(progress_parts(progress(state, 100)), (state, 100));
        }
        for percent in [0, 50, 100, 255] {
            assert_eq!(progress(HIDDEN, percent), 0);
            assert_eq!(progress(INDETERMINATE, percent), INDETERMINATE << 5);
            assert_eq!(progress(9, percent), 0, "an unknown state is no progress");
        }
        assert_eq!(progress_parts(INDETERMINATE << 5 | 7), (INDETERMINATE, 0));
        for byte in 5u8 << 5..=u8::MAX {
            assert_eq!(progress_parts(byte), (HIDDEN, 0));
        }
        let mut wire = Vec::new();
        let sent = Notice::Progress {
            pane: 7,
            progress: progress(FAILED, 64),
        };
        sent.encode(&mut wire).unwrap();
        assert_eq!(wire, raw(PROGRESS_KIND, &[7, 0, 0, 0, FAILED << 5 | 20]));
    }

    /// ADR-0051 §9: the window draws a toast's text as chrome, so the decoder takes only a known
    /// kind and plain text within the cap, whatever a core sent.
    #[test]
    fn a_toast_is_plain_text_within_the_cap() {
        let wire = |kind: u8, text: &[u8]| {
            let mut body = 7u32.to_le_bytes().to_vec();
            body.push(kind);
            body.extend_from_slice(text);
            raw(TOAST_KIND, &body)
        };
        assert_eq!(
            notice_error(&wire(OSC_777, "Build 日本 done".as_bytes())),
            Ok(())
        );
        assert_eq!(notice_error(&wire(OSC_9, &[b'a'; MAX_TOAST])), Ok(()));
        let hostile: [(u8, &[u8]); 6] = [
            (3, b"done"),
            (OSC_9, b""),
            (OSC_9, b"a\nb"),
            (OSC_99, b"\x1b]0;x\x07"),
            (OSC_99, "a\u{202e}b".as_bytes()),
            (OSC_9, &[b'a'; MAX_TOAST + 1]),
        ];
        for (kind, text) in hostile {
            let error = notice_error(&wire(kind, text));
            assert_eq!(error, Err(Error::BadText), "{kind} {text:?}");
        }
        let long = "a".repeat(MAX_TOAST + 1);
        let toast = Notice::Toast {
            pane: 1,
            kind: OSC_9,
            text: &long,
        };
        let mut out = vec![1];
        assert_eq!(toast.encode(&mut out), Err(Error::BadText));
        assert_eq!(out, [1], "a refused toast leaves the queue as it was");
    }

    /// ADR-0051 §8: the window draws an outside row's folder and offers it for a tab, so the
    /// decoder takes only a plain absolute path in a row of at most 16.
    #[test]
    fn an_outside_row_is_a_plain_absolute_path_in_at_most_sixteen() {
        let wire = |index: u8, count: u8, dir: &[u8]| {
            let mut body = vec![index, count];
            body.extend(7u32.to_le_bytes());
            body.extend_from_slice(dir);
            raw(OUTSIDE_KIND, &body)
        };
        assert_eq!(
            notice_error(&wire(15, 16, "/Users/me/日本".as_bytes())),
            Ok(())
        );
        assert_eq!(notice_error(&wire(0, 0, b"")), Ok(()));
        assert_eq!(notice_error(&wire(0, 1, &[b'/'; MAX_DIR])), Ok(()));
        let hostile: [(u8, u8, &[u8]); 9] = [
            (0, 17, b"/w"),
            (1, 1, b"/w"),
            (0, 0, b"/w"),
            (0, 1, b""),
            (0, 1, b"work"),
            (0, 1, b"/w\n/x"),
            (0, 1, b"/w\x1b]0;x\x07"),
            (0, 1, "/w\u{202e}x".as_bytes()),
            (0, 1, &[b'/'; MAX_DIR + 1]),
        ];
        for (index, count, dir) in hostile {
            let error = notice_error(&wire(index, count, dir));
            assert_eq!(error, Err(Error::BadText), "{index} {count} {dir:?}");
        }
        let long = "/".repeat(MAX_DIR + 1);
        let error = Request::OpenTabIn { dir: &long }.encode(&mut Vec::new());
        assert!(matches!(error, Err(Error::TooLarge { .. })));
    }

    #[test]
    fn text_must_be_utf8() {
        for kind in [PASTE, SEARCH] {
            let mut body = vec![0; if kind == SEARCH { 15 } else { 4 }];
            body.push(0xff);
            assert_eq!(request_error(&raw(kind, &body)), Err(Error::BadText));
        }
        for kind in [CWD, COPIED] {
            let wire = raw(kind, &[0, 0, 0, 0, 0xff]);
            assert_eq!(notice_error(&wire), Err(Error::BadText));
        }
        let wire = raw(
            LINKED,
            &[0; 14].into_iter().chain([0xff]).collect::<Vec<_>>(),
        );
        assert_eq!(notice_error(&wire), Err(Error::BadText));
        let wire = raw(
            STATUS,
            &[0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0, 0, 0, 0, 0xff],
        );
        assert_eq!(notice_error(&wire), Err(Error::BadText));
    }

    #[test]
    fn a_status_holds_a_known_state_either_way() {
        let state = Error::BadTag {
            field: "state",
            tag: STATES,
        };
        assert_eq!(
            notice_error(&raw(STATUS, &[STATES, 0, 0, 0, 0, 0])),
            Err(state)
        );
        let reported = Error::BadTag {
            field: "reported",
            tag: 2,
        };
        assert_eq!(
            notice_error(&raw(STATUS, &[0, 2, 0, 0, 0, 0])),
            Err(reported)
        );
        let flags = Error::BadTag {
            field: "flags",
            tag: 4,
        };
        assert_eq!(notice_error(&raw(STATUS, &[0, 0, 4, 0])), Err(flags));
        let notice = Notice::Status {
            pane: 0,
            state: STATES,
            reported: false,
            naming: true,
            listed: false,
            since: 0,
            started: 0,
            profile: None,
            message: "",
        };
        let mut out = vec![7];
        assert_eq!(notice.encode(&mut out), Err(state));
        assert_eq!(out, [7]);
    }

    #[test]
    fn cell_values_are_checked() {
        let good = frame_wire(&screen(1, 1), None);
        for bad in [0xd800, 0xdfff, 0x11_0000, u32::MAX] {
            let mut wire = good.clone();
            wire[CELLS_AT..CELLS_AT + 4].copy_from_slice(&bad.to_le_bytes());
            assert_eq!(notice_error(&wire), Err(Error::BadCharacter));
        }
        for (at, bad) in [(4, 1_u32 << 26), (8, BOLD)] {
            let mut wire = good.clone();
            wire[CELLS_AT + at..CELLS_AT + at + 4].copy_from_slice(&bad.to_le_bytes());
            assert_eq!(notice_error(&wire), Err(Error::BadColour));
        }
    }

    #[test]
    fn rows_are_checked() {
        let previous = screen(3, 2);
        let mut frame = previous.clone();
        frame.cells[0] = cell('b');
        frame.cells[4] = cell('b');
        let good = frame_wire(&frame, Some(&previous));
        let second = ROWS_AT + record_size(2);

        let mut past_the_end = good.clone();
        past_the_end[second..second + 2].copy_from_slice(&3_u16.to_le_bytes());
        assert!(matches!(
            notice_error(&past_the_end),
            Err(Error::OutOfBounds { field: "row", .. })
        ));
        let mut repeated = good.clone();
        repeated[second..second + 2].copy_from_slice(&0_u16.to_le_bytes());
        let mut backward = repeated.clone();
        backward[ROWS_AT..ROWS_AT + 2].copy_from_slice(&1_u16.to_le_bytes());
        for wire in [repeated, backward] {
            assert!(matches!(
                notice_error(&wire),
                Err(Error::OutOfBounds {
                    field: "row order",
                    ..
                })
            ));
        }
    }

    #[test]
    fn a_full_frame_must_hold_every_row() {
        let mut wire = frame_wire(&screen(2, 2), None);
        wire[COUNT_AT..COUNT_AT + 2].copy_from_slice(&1_u16.to_le_bytes());
        assert!(matches!(
            notice_error(&wire),
            Err(Error::OutOfBounds {
                field: "rows in a full frame",
                ..
            })
        ));
    }

    #[test]
    fn a_short_message_cannot_grow_a_frame() {
        let mut wire = frame_wire(&screen(1, 1), Some(&screen(1, 1)));
        // Claims the largest screen and sends none of it.
        wire[9..11].copy_from_slice(&MAX_DIMENSION.to_le_bytes());
        wire[11..13].copy_from_slice(&MAX_DIMENSION.to_le_bytes());
        wire[CURSOR_AT..CURSOR_AT + 4].fill(0);
        let mut drawn = Frame::default();
        assert_eq!(apply(&wire, &mut drawn), Some(false));
        assert!(drawn.cells.capacity() == 0);
        wire[FLAGS_AT] |= FLAG_FULL;
        assert!(notice_error(&wire).is_err());
    }

    #[test]
    fn the_cursor_and_held_rows_are_checked() {
        let good = frame_wire(&screen(2, 3), None);
        for (row, column) in [(2_u16, 0_u16), (0, 3)] {
            let mut wire = good.clone();
            wire[CURSOR_AT..CURSOR_AT + 2].copy_from_slice(&row.to_le_bytes());
            wire[CURSOR_AT + 2..CURSOR_AT + 4].copy_from_slice(&column.to_le_bytes());
            assert!(matches!(
                notice_error(&wire),
                Err(Error::OutOfBounds { .. })
            ));
        }
        let mut hidden = good.clone();
        hidden[FLAGS_AT] &= !FLAG_CURSOR;
        assert!(matches!(
            notice_error(&hidden),
            Err(Error::OutOfBounds {
                field: "hidden cursor",
                ..
            })
        ));
        let mut reversed = good;
        reversed[HELD_AT..HELD_AT + 8].copy_from_slice(&41_u64.to_le_bytes());
        assert!(matches!(
            notice_error(&reversed),
            Err(Error::OutOfBounds {
                field: "held rows",
                ..
            })
        ));
    }

    #[test]
    fn errors_never_hold_message_contents() {
        let mut body = 0_u32.to_le_bytes().to_vec();
        body.extend_from_slice(b"hunter2\xff");
        let error = request_error(&raw(PASTE, &body)).unwrap_err();
        let shown = format!("{error} {error:?}");
        assert!(!shown.contains("hunter2"));
    }

    fn colours() -> impl Strategy<Value = (u32, u32)> {
        (any::<u32>(), any::<u32>()).prop_map(|(foreground, background)| {
            (foreground & (COLOUR | BOLD | ITALIC), background & COLOUR)
        })
    }

    fn frames() -> impl Strategy<Value = Frame> {
        (1_u16..6, 1_u16..6).prop_flat_map(|(rows, columns)| {
            let cells = usize::from(rows) * usize::from(columns);
            (
                vec((any::<char>(), colours()), cells),
                proptest::option::of((0..rows, 0..columns)),
                any::<bool>(),
                any::<bool>(),
                0_u8..4,
                0_u8..3,
                any::<u32>(),
            )
                .prop_map(
                    move |(
                        cells,
                        cursor,
                        app_cursor,
                        alternate_scroll,
                        mouse,
                        encoding,
                        writes,
                    )| {
                        Frame {
                            rows,
                            columns,
                            cells: cells
                                .into_iter()
                                .map(|(character, (foreground, background))| Cell {
                                    character,
                                    foreground,
                                    background,
                                })
                                .collect(),
                            cursor,
                            app_cursor,
                            mouse: match mouse {
                                0 => MouseMode::Off,
                                1 => MouseMode::Click,
                                2 => MouseMode::Drag,
                                _ => MouseMode::Motion,
                            },
                            mouse_encoding: match encoding {
                                0 => MouseEncoding::Legacy,
                                1 => MouseEncoding::Utf8,
                                _ => MouseEncoding::Sgr,
                            },
                            alternate_scroll,
                            clipboard_writes: writes,
                        }
                    },
                )
        })
    }

    fn positions() -> impl Strategy<Value = Position> {
        (any::<usize>(), any::<u16>()).prop_map(|(row, column)| Position { row, column })
    }

    fn selections() -> impl Strategy<Value = Selection> {
        (positions(), positions()).prop_map(|(anchor, focus)| Selection { anchor, focus })
    }

    fn chunks() -> impl Strategy<Value = usize> {
        1_usize..64
    }

    proptest! {
        #[test]
        fn hello_round_trips(version in Just(VERSION), chunk in chunks()) {
            let mut wire = Vec::new();
            let hello = Request::Hello { version };
            prop_assert!(hello.encode(&mut wire).is_ok());
            let mut decoder = Decoder::default();
            let mut found = false;
            for bytes in wire.chunks(chunk) {
                let mut input = bytes;
                if decoder.next_request(&mut input) == Ok(Some(hello)) {
                    found = true;
                }
            }
            prop_assert!(found);
        }

        #[test]
        fn input_round_trips(pane in any::<u32>(), bytes in vec(any::<u8>(), 0..256), chunk in chunks()) {
            prop_assert!(request_survives(Request::Input { pane, bytes: &bytes }, chunk), "changed on the link");
        }

        #[test]
        fn paste_round_trips(pane in any::<u32>(), text in any::<String>(), chunk in chunks()) {
            prop_assert!(request_survives(Request::Paste { pane, text: &text }, chunk), "changed on the link");
        }

        #[test]
        fn resize_round_trips(
            rows in 1..=MAX_DIMENSION,
            columns in 1..=MAX_DIMENSION,
            chunk in chunks(),
        ) {
            prop_assert!(request_survives(Request::Resize { rows, columns }, chunk), "changed on the link");
        }

        #[test]
        fn commands_round_trip(
            pane in any::<u32>(),
            index in 0..MAX_TABS as u16,
            forward in any::<bool>(),
            way in prop::sample::select(&[SplitWay::Right, SplitWay::Down][..]),
            side in prop::sample::select(&[Side::Left, Side::Right, Side::Up, Side::Down][..]),
            cells in 1..=MAX_DIMENSION,
            chunk in chunks(),
        ) {
            for command in [
                Request::NewTab,
                Request::SwitchTab { index },
                Request::StepTab { forward },
                Request::Split { pane, way },
                Request::ClosePane { pane },
                Request::Focus { pane },
                Request::MoveFocus { side },
                Request::MoveBorder { side, cells },
                Request::Zoom,
                Request::OpenTool { pane, tool: if forward { Tool::Files } else { Tool::Diff } },
                Request::ListMarkdown { pane },
                Request::WasRunningAnswer { pane, run: forward },
                Request::Inbox { open: forward },
            ] {
                prop_assert!(request_survives(command, chunk), "changed on the link");
            }
        }

        #[test]
        fn a_layout_round_trips(
            mut tabs in prop::collection::btree_set(any::<u32>(), 1..=MAX_TABS),
            panes in prop::collection::btree_map(any::<u32>(), rects(), 0..=MAX_PANES),
            active in any::<u16>(),
            focused in any::<u32>(),
            chunk in chunks(),
        ) {
            let active = active % tabs.len().max(1) as u16;
            let tabs: Vec<u32> = std::mem::take(&mut tabs).into_iter().collect();
            let panes: Vec<(u32, Rect)> = panes.into_iter().collect();
            let mut wire = Vec::new();
            let members: Vec<(u32, u8)> = (members().into_iter().enumerate())
                .map(|(index, (pane, _))| (pane, (index % tabs.len()) as u8))
                .collect();
            let total = MAX_PANES as u16;
            encode_layout(&mut wire, VERSION, active, focused, total, &tabs, &panes, &members)
                .unwrap();
            let mut decoder = Decoder::default();
            let hello = [HELLO_WIRE.as_slice(), wire.as_slice()].concat();
            let mut input = hello.as_slice();
            let mut seen = None;
            for part in wire_chunks(&mut input, chunk) {
                let mut part = part;
                while let Some(notice) = decoder.next_notice(&mut part).unwrap() {
                    if let Notice::Layout(layout) = notice {
                        seen = Some((
                            layout.active,
                            layout.focused,
                            layout.tabs().collect::<Vec<_>>(),
                            layout.panes().collect::<Vec<_>>(),
                            layout.members().collect::<Vec<_>>(),
                        ));
                    }
                }
            }
            prop_assert_eq!(seen, Some((active, focused, tabs, panes, members)));
        }

        #[test]
        fn a_refusal_round_trips(
            reason in prop::sample::select(
                &[
                    Refusal::Tabs,
                    Refusal::Panes,
                    Refusal::Room,
                    Refusal::Spawn,
                    Refusal::Input,
                    Refusal::Gone,
                    Refusal::NotWaiting,
                    Refusal::NotAgent,
                    Refusal::Protected,
                    Refusal::ProfileChanged,
                ][..],
            ),
            chunk in chunks(),
        ) {
            prop_assert!(notice_survives(&Notice::Refused { reason }, chunk), "changed on the link");
        }

        #[test]
        fn scroll_round_trips(pane in any::<u32>(), lines in any::<i32>(), chunk in chunks()) {
            prop_assert!(request_survives(Request::Scroll { pane, lines }, chunk), "changed on the link");
        }

        #[test]
        fn reveal_round_trips(pane in any::<u32>(), row in any::<usize>(), chunk in chunks()) {
            prop_assert!(request_survives(Request::Reveal { pane, row }, chunk), "changed on the link");
        }

        #[test]
        fn search_round_trips(
            pane in any::<u32>(),
            from in positions(),
            backward in any::<bool>(),
            query in any::<String>(),
            chunk in chunks(),
        ) {
            let direction = if backward { Direction::Backward } else { Direction::Forward };
            let message = Request::Search { pane, from, direction, query: &query };
            prop_assert!(request_survives(message, chunk));
        }

        #[test]
        fn copy_round_trips(pane in any::<u32>(), selection in selections(), chunk in chunks()) {
            prop_assert!(request_survives(Request::Copy { pane, selection }, chunk), "changed on the link");
        }

        #[test]
        fn link_round_trips(pane in any::<u32>(), at in positions(), chunk in chunks()) {
            prop_assert!(request_survives(Request::Link { pane, at }, chunk), "changed on the link");
        }

        #[test]
        fn linked_round_trips(
            pane in any::<u32>(),
            at in positions(),
            uri in any::<String>(),
            chunk in chunks(),
        ) {
            prop_assert!(notice_survives(&Notice::Linked { pane, at, uri: &uri }, chunk), "changed on the link");
        }

        #[test]
        fn full_frame_request_round_trips(pane in any::<u32>(), chunk in chunks()) {
            prop_assert!(request_survives(Request::FullFrame { pane }, chunk), "changed on the link");
        }

        #[test]
        fn approval_answers_round_trip(
            id in any::<u64>(),
            answer in prop::sample::select(&[
                ApprovalAnswer::AllowOnce,
                ApprovalAnswer::AllowForThirtyMinutes,
                ApprovalAnswer::Deny,
            ][..]),
            chunk in chunks(),
        ) {
            prop_assert!(
                request_survives(Request::ApprovalAnswer { id, answer }, chunk),
                "changed on the link"
            );
        }

        #[test]
        fn quit_round_trips(chunk in chunks()) {
            prop_assert!(request_survives(Request::Quit, chunk), "changed on the link");
        }

        #[test]
        fn raise_round_trips(chunk in chunks()) {
            prop_assert!(notice_survives(&Notice::Raise, chunk), "changed on the link");
        }

        #[test]
        fn a_checkpoint_strip_round_trips_and_an_unknown_reason_is_refused(
            pane in any::<u32>(),
            missing in any::<u8>(),
            chunk in chunks(),
        ) {
            let notice = Notice::Checkpoint { pane, missing };
            let known = usize::from(missing) < MISSING.len();
            prop_assert_eq!(notice_survives(&notice, chunk), known);
            let mut wire = HELLO_WIRE.to_vec();
            wire.extend(raw(CHECKPOINT, &[&pane.to_le_bytes()[..], &[missing]].concat()));
            let mut input = wire.as_slice();
            let mut decoder = Decoder::default();
            decoder.next_notice(&mut input).unwrap();
            prop_assert_eq!(decoder.next_notice(&mut input).is_ok(), known);
        }

        #[test]
        fn a_restore_result_round_trips_only_with_a_short_error(
            undid in any::<u32>(),
            skipped in any::<u32>(),
            error in ".{0,80}",
            chunk in chunks(),
        ) {
            let short = error.len() <= MAX_RESTORE_ERROR;
            let notice = Notice::Restored { undid, skipped, error: &error };
            prop_assert_eq!(notice_survives(&notice, chunk), short);
        }

        #[test]
        fn webhook_round_trips(host in any::<String>(), sender in any::<bool>(), failed in any::<u32>(), chunk in chunks()) {
            let notice = Notice::Webhook { host: &host, sender, failed };
            prop_assert!(notice_survives(&notice, chunk), "changed on the link");
        }

        #[test]
        fn toasts_round_trip(
            pane in any::<u32>(),
            kind in 0..=OSC_777,
            text in "[^\\p{C}]{1,120}",
            on in any::<bool>(),
            chunk in chunks(),
        ) {
            prop_assume!(!text.chars().any(bidi) && text.len() <= MAX_TOAST);
            let notice = Notice::Toast { pane, kind, text: &text };
            prop_assert!(notice_survives(&notice, chunk), "changed on the link");
            prop_assert!(request_survives(Request::Toasts { on }, chunk), "changed on the link");
        }

        #[test]
        fn progress_round_trips(pane in any::<u32>(), progress in any::<u8>(), chunk in chunks()) {
            let notice = Notice::Progress { pane, progress };
            prop_assert!(notice_survives(&notice, chunk), "changed on the link");
        }

        #[test]
        fn outside_rows_round_trip(
            count in 1..=MAX_OUTSIDE,
            secs in any::<u32>(),
            dir in "/[^\\p{C}]{0,200}",
            chunk in chunks(),
        ) {
            prop_assume!(!dir.chars().any(bidi));
            let notice = Notice::Outside { index: count - 1, count, secs, dir: &dir };
            prop_assert!(notice_survives(&notice, chunk), "changed on the link");
            let request = Request::OpenTabIn { dir: &dir };
            prop_assert!(request_survives(request, chunk), "changed on the link");
        }

        #[test]
        fn inbox_extra_round_trips(
            pane in any::<u32>(),
            branch in "[ -~]{0,40}",
            ctx in proptest::option::of(0..=100_u8),
            steps in "([^\\p{C}\n]{1,80}(\n[^\\p{C}\n]{1,80}){0,2})?",
            chunk in chunks(),
        ) {
            prop_assume!(!steps.chars().any(bidi));
            let notice = Notice::InboxExtra { pane, branch: &branch, ctx, steps: &steps };
            prop_assert!(notice_survives(&notice, chunk), "changed on the link");
        }

        #[test]
        fn secrets_round_trip(count in any::<u32>(), names in any::<String>(), chunk in chunks()) {
            prop_assert!(notice_survives(&Notice::Secrets { count, names: &names }, chunk), "changed on the link");
        }

        #[test]
        fn markdown_round_trips(pane in any::<u32>(), name in any::<String>(), chunk in chunks()) {
            prop_assert!(request_survives(Request::OpenMarkdown { pane, name: &name }, chunk), "changed on the link");
            prop_assert!(notice_survives(&Notice::Markdown { pane, names: &name }, chunk), "changed on the link");
        }

        #[test]
        fn unhardened_round_trips(checked in any::<bool>(), chunk in chunks()) {
            let notice = Notice::Unhardened { checked };
            prop_assert!(notice_survives(&notice, chunk), "changed on the link");
        }

        #[test]
        fn was_running_round_trips(
            pane in any::<u32>(),
            runnable in any::<bool>(),
            command in ".{0,64}",
            chunk in chunks(),
        ) {
            let notice = Notice::WasRunning { pane, runnable, command: &command };
            prop_assert!(notice_survives(&notice, chunk), "changed on the link");
        }

        #[test]
        fn cwd_round_trips(pane in any::<u32>(), path in any::<String>(), chunk in chunks()) {
            prop_assert!(notice_survives(&Notice::Cwd { pane, path: &path }, chunk), "changed on the link");
        }

        #[test]
        fn status_round_trips(
            pane in any::<u32>(),
            state in 0..STATES,
            reported in any::<bool>(),
            naming in any::<bool>(),
            listed in any::<bool>(),
            since in any::<u32>(),
            started in any::<u32>(),
            profile in prop::option::of(0..u8::MAX),
            message in any::<String>(),
            chunk in chunks(),
        ) {
            let notice = Notice::Status { pane, state, reported, naming, listed, since, started, profile, message: &message };
            prop_assert!(notice_survives(&notice, chunk), "changed on the link");
        }

        #[test]
        fn palette_and_colours_round_trip(
            pane in any::<u32>(),
            colours in proptest::array::uniform18(0..=COLOUR),
            cursor in proptest::option::of(0..=COLOUR),
            background in proptest::option::of(0..=COLOUR),
            chunk in chunks(),
        ) {
            let mut ansi = [0; 16];
            ansi.copy_from_slice(&colours[2..]);
            let palette = Palette { background: colours[0], foreground: colours[1], ansi };
            prop_assert!(request_survives(Request::Palette(palette), chunk), "changed on the link");
            let notice = Notice::Colours { pane, cursor, background };
            prop_assert!(notice_survives(&notice, chunk), "changed on the link");
        }

        #[test]
        fn activity_round_trips(tabs in any::<u32>(), alerts in any::<u32>(), chunk in chunks()) {
            prop_assert!(notice_survives(&Notice::Activity { tabs, alerts }, chunk), "changed on the link");
        }

        #[test]
        fn searched_round_trips(
            pane in any::<u32>(),
            found in proptest::option::of(selections()),
            chunk in chunks(),
        ) {
            prop_assert!(notice_survives(&Notice::Searched { pane, found }, chunk), "changed on the link");
        }

        #[test]
        fn copied_round_trips(pane in any::<u32>(), text in any::<String>(), chunk in chunks()) {
            prop_assert!(notice_survives(&Notice::Copied { pane, text: &text }, chunk), "changed on the link");
        }

        #[test]
        fn approvals_round_trip(
            id in any::<u64>(),
            caller in any::<u32>(),
            target in any::<u32>(),
            action in prop::sample::select(
                &[ApprovalAction::Read, ApprovalAction::Send, ApprovalAction::Trust, ApprovalAction::Owner, ApprovalAction::Rewind][..]
            ),
            owned in vec(".{0,32}", 0..4),
            chunk in chunks(),
        ) {
            let texts = owned.iter().map(String::as_str).collect();
            let notice = Notice::Approval(Approval { id, caller, target, action, texts });
            prop_assert!(notice_survives(&notice, chunk), "changed on the link");
        }

        #[test]
        fn a_frame_round_trips_in_any_chunks(
            frame in frames(),
            pane in any::<u32>(),
            top in any::<usize>(),
            held in (any::<usize>(), any::<usize>()).prop_map(|(a, b)| a.min(b)..a.max(b)),
            chunk in chunks(),
        ) {
            let mut wire = HELLO_WIRE.to_vec();
            let result = encode_frame(&mut wire, pane, &frame, top, held.clone(), None);
            prop_assert!(result.is_ok());
            let mut decoder = Decoder::default();
            let mut drawn = Frame::default();
            let mut rebuilt = HELLO_WIRE.to_vec();
            for bytes in wire.chunks(chunk) {
                let mut input = bytes;
                while let Ok(Some(notice)) = decoder.next_notice(&mut input) {
                    if let Notice::Frame(update) = &notice {
                        prop_assert_eq!(update.pane, pane);
                        prop_assert_eq!(update.top, top);
                        prop_assert_eq!(&update.held, &held);
                        prop_assert!(update.full);
                        prop_assert!(update.apply(&mut drawn));
                        prop_assert!(notice.encode(&mut rebuilt).is_ok());
                    }
                }
            }
            prop_assert_eq!(&drawn, &frame);
            prop_assert_eq!(rebuilt, wire);
        }

        #[test]
        fn a_changed_frame_applies_over_the_last((previous, next) in frames().prop_flat_map(|frame| {
            let (rows, columns) = (frame.rows, frame.columns);
            (Just(frame), frames().prop_filter("same size", move |next| {
                (next.rows, next.columns) == (rows, columns)
            }))
        })) {
            let mut wire = HELLO_WIRE.to_vec();
            prop_assert!(encode_frame(&mut wire, 0, &next, 0, 0..0, Some(&previous)).is_ok());
            let mut decoder = Decoder::default();
            let mut input = wire.as_slice();
            prop_assert!(matches!(decoder.next_notice(&mut input), Ok(Some(_))));
            let mut drawn = previous;
            match decoder.next_notice(&mut input) {
                Ok(Some(Notice::Frame(update))) => prop_assert!(update.apply(&mut drawn)),
                other => prop_assert!(false, "{other:?}"),
            }
            prop_assert_eq!(drawn, next);
        }

        #[test]
        fn arbitrary_bytes_never_panic(bytes in vec(any::<u8>(), 0..2048), chunk in chunks()) {
            let mut wire = HELLO_WIRE.to_vec();
            wire.extend_from_slice(&bytes);
            let (mut requests, mut notices) = (Decoder::default(), Decoder::default());
            for part in wire.chunks(chunk) {
                let (mut one, mut two) = (part, part);
                while let Ok(Some(_)) = requests.next_request(&mut one) {}
                while let Ok(Some(notice)) = notices.next_notice(&mut two) {
                    if let Notice::Frame(update) = notice {
                        let _ = update.apply(&mut Frame::default());
                    }
                }
            }
        }

        #[test]
        fn a_framed_body_of_any_bytes_never_panics(kind in 0_u8..12, body in vec(any::<u8>(), 0..128)) {
            let wire = raw(kind, &body);
            let _ = request_error(&wire);
            let _ = notice_error(&wire);
        }
    }
}
