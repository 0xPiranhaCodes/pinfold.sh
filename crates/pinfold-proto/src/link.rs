//! The binary protocol between the core and a window (ADR-0028), as pure encoding and decoding.
//!
//! Each message is a little-endian `u32` length, a type byte and a body, and the length counts
//! the type byte and the body. Fields are fixed-width and little-endian, and text or bytes run
//! to the end of the body, so no field carries a length of its own.
//!
//! Both sides send [`Request::Hello`] or [`Notice::Hello`] as soon as the link opens, without
//! waiting. [`Decoder`] takes that message first and returns [`Error::Version`] on a mismatch.
//! The two binaries ship together, so nothing is negotiated: the side that sees the error closes
//! the link, and the core keeps its PTYs.
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

use crate::{BOLD, Cell, Frame, ITALIC, MouseEncoding, MouseMode};

/// The protocol's version. Any change to the format below raises it.
pub const VERSION: u16 = 2;

/// Most rows or columns a screen can have, which the core clamps its panes to.
pub const MAX_DIMENSION: u16 = 1024;

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

const FRAME: u8 = 1;
const CWD: u8 = 2;
const SEARCHED: u8 = 3;
const COPIED: u8 = 4;
const EXITED: u8 = 5;
const RAISE: u8 = 6;

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

/// Which way a search runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Toward newer rows.
    Forward,
    /// Toward older rows.
    Backward,
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
        /// a program through that and never as [`Request::Input`].
        // shortcut: a paste over MAX_PASTE fails to encode, send it in parts if a person ever
        // needs to paste more than 1 MiB, as the pane's input queue would refuse it anyway.
        text: &'a str,
    },
    /// A new pane size. Each side must be from 1 to [`MAX_DIMENSION`], so the window clamps its
    /// grid before sending, as a tiny or huge surface can give anything from 0 to `u16::MAX`.
    Resize {
        /// The pane.
        pane: u32,
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
    /// Asks for a full frame, when the window has none to apply changes to.
    FullFrame {
        /// The pane.
        pane: u32,
    },
    /// The window is closing.
    Quit,
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
    /// A pane's program exited and the pane closed.
    Exited {
        /// The pane.
        pane: u32,
    },
    /// Someone asked for a window while this one is open, so it comes to the front (ADR-0028).
    Raise,
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
    /// Appends this message to `out`. On an error `out` is as it was.
    ///
    /// # Errors
    ///
    /// [`Error::TooLarge`] past [`MAX_REQUEST`], and [`Error::OutOfBounds`] for a resize outside
    /// 1 to [`MAX_DIMENSION`].
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        if let Self::Resize { rows, columns, .. } = *self {
            check("rows", rows, 1..=MAX_DIMENSION)?;
            check("columns", columns, 1..=MAX_DIMENSION)?;
        }
        let start = begin(out);
        let kind = match *self {
            Self::Hello { version } => put!(out, HELLO, version),
            Self::Input { pane, bytes } => put_tail(out, start, MAX_REQUEST, INPUT, pane, bytes)?,
            Self::Paste { pane, text } => {
                put_tail(out, start, MAX_PASTE, PASTE, pane, text.as_bytes())?
            }
            Self::Resize {
                pane,
                rows,
                columns,
            } => put!(out, RESIZE, pane, rows, columns),
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
            Self::FullFrame { pane } => put!(out, FULL_FRAME, pane),
            Self::Quit => QUIT,
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
                let (pane, rows, columns) = (body.u32()?, body.u16()?, body.u16()?);
                check("rows", rows, 1..=MAX_DIMENSION)?;
                check("columns", columns, 1..=MAX_DIMENSION)?;
                Request::Resize {
                    pane,
                    rows,
                    columns,
                }
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
            FULL_FRAME => Request::FullFrame { pane: body.u32()? },
            QUIT => Request::Quit,
            kind => return Err(Error::UnknownKind { kind }),
        };
        body.end()?;
        Ok(request)
    }
}

impl Notice<'_> {
    /// Appends this message to `out`. On an error `out` is as it was.
    ///
    /// # Errors
    ///
    /// [`Error::TooLarge`] past [`MAX_MESSAGE`].
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        let start = begin(out);
        let kind = match self {
            Self::Hello { version } => put!(out, HELLO, version),
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
            Self::Exited { pane } => put!(out, EXITED, pane),
            Self::Raise => RAISE,
        };
        finish(out, start, MAX_MESSAGE, kind)
    }

    fn decode(kind: u8, body: &[u8]) -> Result<Notice<'_>, Error> {
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
            EXITED => Notice::Exited { pane: body.u32()? },
            RAISE => Notice::Raise,
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
    greeted: bool,
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
        self.next(input, MAX_REQUEST, Request::decode)
    }

    /// Reads the next message the core sent, as [`Decoder::next_request`] does.
    ///
    /// # Errors
    ///
    /// Any error ends the link, and every later call returns it.
    pub fn next_notice(&mut self, input: &mut &[u8]) -> Result<Option<Notice<'_>>, Error> {
        self.next(input, MAX_MESSAGE, Notice::decode)
    }

    fn next<'a, M>(
        &'a mut self,
        input: &mut &[u8],
        limit: usize,
        decode: fn(u8, &'a [u8]) -> Result<M, Error>,
    ) -> Result<Option<M>, Error> {
        let decoded = match self.fill(input, limit) {
            Ok(false) => return Ok(None),
            Ok(true) => {
                let (kind, body) = message(&self.buffer);
                decode(kind, body)
            }
            Err(error) => Err(error),
        };
        if let Err(error) = decoded {
            self.failed = Some(error);
        }
        decoded.map(Some)
    }

    /// Takes bytes until `buffer` holds a whole message, and returns whether it does. The length
    /// is checked before any of the body is stored, and the version check before anything else.
    fn fill(&mut self, input: &mut &[u8], limit: usize) -> Result<bool, Error> {
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
        match (kind == HELLO, self.greeted) {
            (true, true) => return Err(Error::RepeatedHello),
            (false, false) => return Err(Error::NoHello { kind }),
            (true, false) => {
                let mut body = Reader(body);
                let theirs = body.u16()?;
                body.end()?;
                if theirs != VERSION {
                    return Err(Error::Version {
                        ours: VERSION,
                        theirs,
                    });
                }
                self.greeted = true;
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

    fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.0)
    }

    fn text(&mut self) -> Result<&'a str, Error> {
        std::str::from_utf8(self.rest()).map_err(|_| Error::BadText)
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
    const HELLO_WIRE: [u8; 7] = [3, 0, 0, 0, HELLO, 2, 0];
    /// Where fields sit in a frame message.
    const FLAGS_AT: usize = 13;
    const MOUSE_AT: usize = 14;
    const ENCODING_AT: usize = 15;
    const CURSOR_AT: usize = 16;
    const HELD_AT: usize = 32;
    const COUNT_AT: usize = 48;
    const ROWS_AT: usize = 50;
    const CELLS_AT: usize = 52;

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
        let wire = raw(HELLO, &9_u16.to_le_bytes());
        let mut decoder = Decoder::default();
        let error = Error::Version {
            ours: VERSION,
            theirs: 9,
        };
        assert_eq!(decoder.next_request(&mut wire.as_slice()), Err(error));
        let mut good: &[u8] = &HELLO_WIRE;
        assert_eq!(decoder.next_request(&mut good), Err(error));
        assert_eq!(good, HELLO_WIRE, "a failed link takes no more bytes");
        assert_eq!(decoder.next_notice(&mut good), Err(error));
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

        let small = raw(EXITED, &[0; 4]);
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
            let message = Request::Resize {
                pane: 0,
                rows,
                columns,
            };
            let mut out = Vec::new();
            assert!(message.encode(&mut out).is_err());
            assert!(out.is_empty());
            let mut body = 0_u32.to_le_bytes().to_vec();
            body.extend_from_slice(&rows.to_le_bytes());
            body.extend_from_slice(&columns.to_le_bytes());
            assert!(matches!(
                request_error(&raw(RESIZE, &body)),
                Err(Error::OutOfBounds { .. })
            ));
        }
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
        assert_eq!(out, [1]);
    }

    #[test]
    fn a_message_cut_anywhere_short_is_refused() {
        for (kind, length) in [
            (RESIZE, 8),
            (SCROLL, 8),
            (REVEAL, 12),
            (SEARCH, 15),
            (COPY, 24),
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
        let wire = raw(EXITED, &[0; 6]);
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
            pane in any::<u32>(),
            rows in 1..=MAX_DIMENSION,
            columns in 1..=MAX_DIMENSION,
            chunk in chunks(),
        ) {
            prop_assert!(request_survives(Request::Resize { pane, rows, columns }, chunk), "changed on the link");
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
        fn full_frame_request_round_trips(pane in any::<u32>(), chunk in chunks()) {
            prop_assert!(request_survives(Request::FullFrame { pane }, chunk), "changed on the link");
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
        fn cwd_round_trips(pane in any::<u32>(), path in any::<String>(), chunk in chunks()) {
            prop_assert!(notice_survives(&Notice::Cwd { pane, path: &path }, chunk), "changed on the link");
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
        fn exited_round_trips(pane in any::<u32>(), chunk in chunks()) {
            prop_assert!(notice_survives(&Notice::Exited { pane }, chunk), "changed on the link");
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
