//! Rows that left the screen, compressed and capped in bytes (ADR-0021).
//!
//! Rows are encoded into one open block, which is compressed once it is full. Styles are
//! written inline per run, because LZ4 removes their repeats within a block.
//!
//! Stored rows never change. Each block also keeps how its rows lay out at the pane's width
//! (ADR-0029), and a new width costs nothing until something reads history.

use std::collections::VecDeque;

use alacritty_terminal::grid::{HistorySink, Row};
use alacritty_terminal::index::Column;
use alacritty_terminal::term::cell::{Cell, Flags, Hyperlink};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use lz4_flex::block::{CompressTable, compress_into_with_table, get_maximum_output_size};

use crate::reflow::{Flow, Reader};
use crate::vt::MAX_DIMENSION;

/// The hard cap on one pane's scrollback.
pub const BYTE_CAP: usize = 2_000_000;
/// Kept out of the store's share of the cap, for the heap around it that it cannot count.
/// `cargo xtask mem-gate` holds a pane to the cap. The same allocations cost up to 0.9 MB more
/// from run to run on macOS, whose heap adds regions as the thread moves between cores.
const HEAP_RESERVE: usize = if cfg!(target_os = "macos") {
    1_000_000
} else {
    400_000
};
/// Scrollback keeps fewer rows than this, so a quiet pane stays far under the byte cap.
pub const LINE_CAP: usize = 10_000;

const BLOCK_ROWS: usize = 64;
/// An open block is compressed at this many bytes, however few rows it holds.
const BLOCK_BYTES: usize = 64 * 1024;
/// Counted for each block: its queue entry.
const BLOCK_OVERHEAD: usize = size_of::<Block>();
/// The size of LZ4's hash table, once a block has been compressed.
const TABLE_BYTES: usize = 16 * 1024;
/// The size of each chunk of compressed bytes.
const CHUNK_BYTES: usize = 64 * 1024;
/// Longer links are dropped from history, so one cell cannot hold megabytes.
const MAX_LINK: usize = 2048;
/// Combining marks kept per cell.
pub(crate) const MAX_ZEROWIDTH: usize = 16;
/// The most a block can inflate to. Anything claiming more is not a block.
const MAX_INFLATED: usize = 16 * 1024 * 1024;
/// The most a store's blocks can inflate to in all, which bounds what a new width reads. Bytes
/// that compress this well are hostile, so only they lose history to it.
const INFLATED_CAP: usize = 64 * 1024 * 1024;

const NAMED: [NamedColor; 29] = {
    use NamedColor::*;
    [
        Black,
        Red,
        Green,
        Yellow,
        Blue,
        Magenta,
        Cyan,
        White,
        BrightBlack,
        BrightRed,
        BrightGreen,
        BrightYellow,
        BrightBlue,
        BrightMagenta,
        BrightCyan,
        BrightWhite,
        Foreground,
        Background,
        Cursor,
        DimBlack,
        DimRed,
        DimGreen,
        DimYellow,
        DimBlue,
        DimMagenta,
        DimCyan,
        DimWhite,
        BrightForeground,
        DimForeground,
    ]
};

fn put_varint(out: &mut Vec<u8>, mut value: usize) {
    while value >= 0x80 {
        out.push(value as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn get_varint(input: &mut &[u8]) -> Option<usize> {
    let (mut value, mut shift) = (0_usize, 0_u32);
    loop {
        let (&byte, rest) = input.split_first()?;
        *input = rest;
        value |= usize::from(byte & 0x7f).checked_shl(shift)?;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
    }
}

fn get_byte(input: &mut &[u8]) -> Option<u8> {
    let (&byte, rest) = input.split_first()?;
    *input = rest;
    Some(byte)
}

fn put_str(out: &mut Vec<u8>, text: &str) {
    put_varint(out, text.len());
    out.extend_from_slice(text.as_bytes());
}

fn get_str(input: &mut &[u8]) -> Option<String> {
    let length = get_varint(input)?;
    let (text, rest) = input.split_at_checked(length)?;
    *input = rest;
    String::from_utf8(text.to_vec()).ok()
}

fn put_color(out: &mut Vec<u8>, color: Color) {
    match color {
        Color::Named(named) => {
            let index = NAMED.iter().position(|&item| item == named).unwrap_or(0);
            out.extend_from_slice(&[0, index as u8]);
        }
        Color::Indexed(index) => out.extend_from_slice(&[1, index]),
        Color::Spec(rgb) => out.extend_from_slice(&[2, rgb.r, rgb.g, rgb.b]),
    }
}

fn get_color(input: &mut &[u8]) -> Option<Color> {
    Some(match get_byte(input)? {
        0 => Color::Named(*NAMED.get(usize::from(get_byte(input)?))?),
        1 => Color::Indexed(get_byte(input)?),
        2 => Color::Spec(Rgb {
            r: get_byte(input)?,
            g: get_byte(input)?,
            b: get_byte(input)?,
        }),
        _ => return None,
    })
}

/// Encodes everything in a cell except its character. Links go only if `links` is set.
fn put_style(out: &mut Vec<u8>, cell: &Cell, links: bool) {
    put_color(out, cell.fg);
    put_color(out, cell.bg);
    put_varint(out, usize::from(cell.flags.bits()));
    match cell.underline_color() {
        Some(color) => {
            out.push(1);
            put_color(out, color);
        }
        None => out.push(0),
    }
    let link = cell
        .hyperlink()
        .filter(|link| links && link.id().len() + link.uri().len() <= MAX_LINK);
    match link {
        Some(link) => {
            out.push(1);
            put_str(out, link.id());
            put_str(out, link.uri());
        }
        None => out.push(0),
    }
    let marks: String = cell
        .zerowidth()
        .unwrap_or_default()
        .iter()
        .take(MAX_ZEROWIDTH)
        .collect();
    put_str(out, &marks);
}

/// A decoded style, which becomes a cell once it has a character.
struct Style {
    template: Cell,
}

fn get_style(input: &mut &[u8]) -> Option<Style> {
    let mut template = Cell {
        fg: get_color(input)?,
        bg: get_color(input)?,
        ..Cell::default()
    };
    template.flags = Flags::from_bits_truncate(u16::try_from(get_varint(input)?).ok()?);
    if get_byte(input)? == 1 {
        template.set_underline_color(Some(get_color(input)?));
    }
    if get_byte(input)? == 1 {
        let id = get_str(input)?;
        template.set_hyperlink(Some(Hyperlink::new(Some(id), get_str(input)?)));
    }
    get_str(input)?
        .chars()
        .for_each(|mark| template.push_zerowidth(mark));
    Some(Style { template })
}

fn skip_color(input: &mut &[u8]) -> Option<()> {
    let size = match get_byte(input)? {
        0 | 1 => 1,
        2 => 3,
        _ => return None,
    };
    *input = input.get(size..)?;
    Some(())
}

fn skip_str(input: &mut &[u8]) -> Option<()> {
    let length = get_varint(input)?;
    *input = input.get(length..)?;
    Some(())
}

/// Reads past a style for its flags alone, which is all a layout needs, so that linked history
/// does not cost an allocation per run.
fn skip_style(input: &mut &[u8]) -> Option<Flags> {
    skip_color(input)?;
    skip_color(input)?;
    let flags = Flags::from_bits_truncate(u16::try_from(get_varint(input)?).ok()?);
    if get_byte(input)? == 1 {
        skip_color(input)?;
    }
    if get_byte(input)? == 1 {
        skip_str(input)?;
        skip_str(input)?;
    }
    skip_str(input)?;
    Some(flags)
}

impl Style {
    fn cell(&self, c: char) -> Cell {
        let mut cell = self.template.clone();
        cell.c = c;
        cell
    }
}

fn same_style(a: &Cell, b: &Cell) -> bool {
    a.fg == b.fg && a.bg == b.bg && a.flags == b.flags && a.extra.is_none() && b.extra.is_none()
}

/// The block being written: row lengths and style runs, then the characters.
#[derive(Debug, Default)]
struct Open {
    rows: usize,
    runs: Vec<u8>,
    text: String,
    /// The layout as this block starts.
    entry: Flow,
    /// The layout after the last row.
    flow: Flow,
    /// The shown rows that start in this block.
    shown: usize,
}

impl Open {
    fn bytes(&self) -> usize {
        self.runs.len() + self.text.len()
    }

    fn push(&mut self, row: &Row<Cell>) {
        let cells = || (0..row.len()).map(|column| &row[Column(column)]);
        let default = Cell::default();
        let used = cells()
            .rposition(|cell| *cell != default)
            .map_or(0, |at| at + 1);
        put_varint(&mut self.runs, used);
        self.shown += usize::from(self.flow.enter(used));
        let mut run: Option<(&Cell, usize)> = None;
        for cell in cells().take(used) {
            self.text.push(cell.c);
            run = match run {
                Some((first, count)) if same_style(first, cell) => Some((first, count + 1)),
                Some((first, count)) => {
                    self.put_run(first, count);
                    Some((cell, 1))
                }
                None => Some((cell, 1)),
            };
        }
        if let Some((first, count)) = run {
            self.put_run(first, count);
        }
        let last = used.checked_sub(1).map(|at| row[Column(at)].flags);
        self.flow.leave(last.unwrap_or(Flags::empty()));
        self.rows += 1;
    }

    fn put_run(&mut self, first: &Cell, count: usize) {
        put_varint(&mut self.runs, count);
        // A wide row of long links would otherwise pass the cap before the block is sealed.
        let links = self.runs.len() < 2 * BLOCK_BYTES;
        put_style(&mut self.runs, first, links);
        let shown = &mut self.shown;
        self.flow.place(first.flags, count, |_| *shown += 1);
    }

    /// Compresses this block into `spare.packed` and empties it. These buffers live as long
    /// as the store, because allocating them per seal leaves freed pages behind.
    fn seal(&mut self, spare: &mut Spare) -> Option<(usize, usize)> {
        spare.raw.clear();
        spare.raw.reserve_exact(self.bytes() + 8);
        put_varint(&mut spare.raw, self.runs.len());
        spare.raw.extend_from_slice(&self.runs);
        spare.raw.extend_from_slice(self.text.as_bytes());
        let (raw, packed) = (&spare.raw, &mut spare.packed);
        let table = spare.table.get_or_insert_with(CompressTable::large);
        let packed_size = 4 + get_maximum_output_size(raw.len());
        packed.clear();
        packed.reserve_exact(packed_size);
        packed.extend_from_slice(&(raw.len() as u32).to_le_bytes());
        packed.resize(packed_size, 0);
        let size = compress_into_with_table(raw, &mut packed[4..], table);
        self.runs.clear();
        self.text.clear();
        self.rows = 0;
        self.shown = 0;
        self.entry = self.flow;
        size.ok().map(|size| (4 + size, raw.len()))
    }
}

/// Working buffers kept between seals.
#[derive(Default)]
struct Spare {
    raw: Vec<u8>,
    packed: Vec<u8>,
    table: Option<CompressTable>,
}

impl std::fmt::Debug for Spare {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Spare")
    }
}

#[derive(Debug)]
struct Block {
    rows: usize,
    size: usize,
    /// The shown rows that start here. Rows of a line that began earlier need no look back,
    /// because `entry` says where that line had got to.
    shown: usize,
    entry: Flow,
    /// The bytes this block inflates to.
    inflated: usize,
}

/// Compressed blocks, oldest first, in chunks of one size.
///
/// Blocks of their own sizes, freed oldest first, leave holes that cost half of what they
/// held. A single growing buffer leaves its old copies behind in the same way.
#[derive(Debug, Default)]
struct Ring {
    chunks: VecDeque<Box<[u8]>>,
    /// The last chunk emptied. Reusing it means a full ring never touches the allocator.
    spare: Option<Box<[u8]>>,
    /// Where the oldest byte sits in the first chunk.
    start: usize,
    len: usize,
}

impl Ring {
    fn push(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            let end = self.start + self.len;
            if end == self.chunks.len() * CHUNK_BYTES {
                let chunk = self.spare.take();
                self.chunks
                    .push_back(chunk.unwrap_or_else(|| vec![0; CHUNK_BYTES].into()));
            }
            let Some(chunk) = self.chunks.get_mut(end / CHUNK_BYTES) else {
                return;
            };
            let at = end % CHUNK_BYTES;
            let (now, rest) = data.split_at(data.len().min(CHUNK_BYTES - at));
            chunk[at..at + now.len()].copy_from_slice(now);
            self.len += now.len();
            data = rest;
        }
    }

    fn drop_oldest(&mut self, size: usize) {
        self.start += size;
        self.len -= size;
        while self.start >= CHUNK_BYTES {
            self.spare = self.chunks.pop_front();
            self.start -= CHUNK_BYTES;
        }
    }

    fn read(&self, offset: usize, size: usize, out: &mut Vec<u8>) {
        out.clear();
        let mut at = self.start + offset;
        while out.len() < size {
            let Some(chunk) = self.chunks.get(at / CHUNK_BYTES) else {
                break;
            };
            let begin = at % CHUNK_BYTES;
            let end = CHUNK_BYTES.min(begin + size - out.len());
            out.extend_from_slice(&chunk[begin..end]);
            at += end - begin;
        }
    }

    fn bytes(&self) -> usize {
        (self.chunks.len() + usize::from(self.spare.is_some())) * CHUNK_BYTES
    }
}

/// Gives `reader` the first `count` rows, stopping when it is done. Only rows it wants are built.
/// Returns nothing if the data is not what `Open` wrote.
fn decode_rows(
    count: usize,
    mut runs: &[u8],
    text: &str,
    reader: &mut Reader,
    visit: &mut impl FnMut(&[Cell]),
) -> Option<()> {
    let mut characters = text.chars();
    let mut row = Vec::new();
    for _ in 0..count {
        if reader.done() {
            break;
        }
        let mut left = get_varint(&mut runs)?;
        let keep = reader.wants(left);
        if !keep {
            reader.enter(left);
        }
        row.clear();
        let mut last = Flags::empty();
        while left > 0 {
            let length = get_varint(&mut runs)?;
            if length == 0 || length > left {
                return None;
            }
            if keep {
                let style = get_style(&mut runs)?;
                for _ in 0..length {
                    row.push(style.cell(characters.next()?));
                }
            } else {
                last = skip_style(&mut runs)?;
                characters.nth(length - 1)?;
                reader.skip_cells(last, length);
            }
            left -= length;
        }
        if keep {
            reader.row(&row, visit);
        } else {
            reader.leave(last);
        }
    }
    Some(())
}

/// Inflates a block into `raw` and decodes it as [`decode_rows`] does.
fn decode_block(
    count: usize,
    data: &[u8],
    raw: &mut Vec<u8>,
    reader: &mut Reader,
    visit: &mut impl FnMut(&[Cell]),
) -> Option<()> {
    let (size, compressed) = lz4_flex::block::uncompressed_size(data).ok()?;
    if size > MAX_INFLATED {
        return None;
    }
    raw.clear();
    raw.resize(size, 0);
    let size = lz4_flex::block::decompress_into(compressed, raw).ok()?;
    let mut input = raw.get(..size)?;
    let length = get_varint(&mut input)?;
    let (runs, text) = input.split_at_checked(length)?;
    decode_rows(count, runs, std::str::from_utf8(text).ok()?, reader, visit)
}

/// One pane's scrollback.
///
/// ```
/// let scrollback = pinfold_core::scrollback::Scrollback::new();
/// assert!(scrollback.is_empty());
/// assert!(scrollback.bytes() <= pinfold_core::scrollback::BYTE_CAP);
/// ```
#[derive(Debug)]
pub struct Scrollback {
    open: Open,
    spare: Spare,
    ring: Ring,
    blocks: VecDeque<Block>,
    block_rows: usize,
    /// The shown rows that start in `blocks`.
    block_shown: usize,
    /// The bytes `blocks` inflate to.
    inflated: usize,
    /// How many stored rows have gone, which numbers the ones held.
    evicted: usize,
    /// Whether `Block::shown` and `Block::entry` are for an older width.
    stale: bool,
    byte_cap: usize,
    line_cap: usize,
    /// The number of the oldest shown row held. See [`Scrollback::end`].
    oldest: usize,
    /// Rows numbered below this were cleared, not evicted.
    floor: usize,
}

impl Default for Scrollback {
    fn default() -> Self {
        Self::with_caps(BYTE_CAP - HEAP_RESERVE, LINE_CAP)
    }
}

impl Scrollback {
    /// Creates an empty scrollback with the default caps.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn with_caps(byte_cap: usize, line_cap: usize) -> Self {
        Self {
            open: Open::default(),
            spare: Spare::default(),
            ring: Ring::default(),
            blocks: VecDeque::new(),
            block_rows: 0,
            block_shown: 0,
            inflated: 0,
            evicted: 0,
            stale: false,
            byte_cap,
            line_cap,
            oldest: 0,
            floor: 0,
        }
    }

    /// Lays rows out at `columns` from now on, or as they were written if that is 0, which is
    /// where a new store starts. This reads nothing: [`Self::sync`] does, once it is needed.
    /// Numbers move up as far as they could need to, so that `sync` leaves [`Self::end`] alone.
    ///
    /// ```
    /// let mut scrollback = pinfold_core::scrollback::Scrollback::new();
    /// scrollback.set_width(80);
    /// scrollback.sync();
    /// assert!(scrollback.is_empty());
    /// ```
    pub fn set_width(&mut self, columns: u16) {
        if columns == self.open.flow.width {
            return;
        }
        let stored = self.block_rows + self.open.rows;
        self.open.flow.width = columns;
        self.open.entry.width = columns;
        self.stale = stored > 0;
        let shortest = usize::from(columns).saturating_sub(1).max(1);
        let most = stored.saturating_mul(usize::from(MAX_DIMENSION) / shortest + 2);
        self.oldest += (self.floor + most).saturating_sub(self.end());
    }

    /// Brings the layout up to the width set, inflating each block once. Until this has run,
    /// [`Self::len`] and everything counted from it are an estimate, but [`Self::end`] is not.
    /// `benches/scrollback.rs` measures it.
    pub fn sync(&mut self) {
        if !std::mem::take(&mut self.stale) {
            return;
        }
        let end = self.end();
        let mut spare = std::mem::take(&mut self.spare);
        let flow = Flow::new(self.open.flow.width);
        let mut reader = Reader::counting(flow);
        let (mut offset, mut shown) = (0, 0);
        for block in &mut self.blocks {
            block.entry = reader.flow();
            self.ring.read(offset, block.size, &mut spare.packed);
            offset += block.size;
            let (data, raw) = (&spare.packed, &mut spare.raw);
            let decoded = decode_block(block.rows, data, raw, &mut reader, &mut |_| {});
            block.shown = reader.take_started();
            if decoded.is_none() {
                block.shown = block.rows;
                reader = Reader::counting(flow);
            }
            shown += block.shown;
        }
        self.spare = spare;
        self.block_shown = shown;
        self.open.entry = reader.flow();
        let open = &self.open;
        let decoded = decode_rows(open.rows, &open.runs, &open.text, &mut reader, &mut |_| {});
        self.open.flow = if decoded.is_some() {
            reader.flow()
        } else {
            flow
        };
        self.open.shown = reader.take_started();
        self.oldest = end.saturating_sub(self.len()).max(self.floor);
    }

    /// Returns the number of the stored row that shown row `first` starts in. Unlike a shown
    /// row's number, this holds across widths.
    #[must_use]
    pub(crate) fn stored_row(&self, first: usize) -> Option<usize> {
        self.walk(first, 1, &mut Spare::default(), &mut |_| {})
    }

    /// Returns the index of the shown row that stored row `number` starts in.
    #[must_use]
    pub(crate) fn shown_index(&self, number: usize) -> usize {
        let mut left = number.saturating_sub(self.evicted);
        let (mut shown, mut offset) = (0, 0);
        let (mut packed, mut raw) = (Vec::new(), Vec::new());
        let mut reader = None;
        for block in &self.blocks {
            if left < block.rows {
                self.ring.read(offset, block.size, &mut packed);
                let reader = reader.insert(Reader::counting(block.entry));
                let _ = decode_block(left, &packed, &mut raw, reader, &mut |_| {});
                break;
            }
            left -= block.rows;
            shown += block.shown;
            offset += block.size;
        }
        let reader = reader.unwrap_or_else(|| {
            let open = &self.open;
            let mut reader = Reader::counting(open.entry);
            let _ = decode_rows(left, &open.runs, &open.text, &mut reader, &mut |_| {});
            reader
        });
        (shown + reader.started()).saturating_sub(usize::from(reader.inside_row()))
    }

    /// Returns the number of shown rows held, which is the rows stored until a width is set.
    #[must_use]
    pub fn len(&self) -> usize {
        self.block_shown + self.open.shown
    }

    /// Returns whether no rows are held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the number the next shown row will get. Numbers count every row shown and stay
    /// on their rows through pushes and evictions, so a viewer's place survives both. They hold
    /// for one width: a new width numbers its rows back from this same end.
    #[must_use]
    pub fn end(&self) -> usize {
        self.oldest + self.len()
    }

    /// Returns the index for [`Scrollback::rows`] of row `number`, or of the oldest row held
    /// if that one was evicted. Returns nothing for a row not pushed yet or since cleared.
    #[must_use]
    pub fn index(&self, number: usize) -> Option<usize> {
        (self.floor..self.end())
            .contains(&number)
            .then(|| number.saturating_sub(self.oldest))
    }

    /// Returns the bytes held, which never exceed the cap once a row has been pushed.
    #[must_use]
    pub fn bytes(&self) -> usize {
        let working = self.open.runs.capacity()
            + self.open.text.capacity()
            + self.spare.raw.capacity()
            + self.spare.packed.capacity()
            + self.spare.table.as_ref().map_or(0, |_| TABLE_BYTES);
        self.ring.bytes() + self.blocks.len() * BLOCK_OVERHEAD + working
    }

    /// Returns up to `count` shown rows from `first`, where 0 is the oldest row held. Rows are
    /// trimmed of trailing default cells. After [`Self::set_width`], call [`Self::sync`] first.
    #[must_use]
    pub fn rows(&self, first: usize, count: usize) -> Vec<Vec<Cell>> {
        let mut out = Vec::new();
        let mut spare = Spare::default();
        let mut visit = |row: &[Cell]| out.push(row.to_vec());
        let _ = self.walk(first, count, &mut spare, &mut visit);
        out
    }

    /// Calls `visit` with what [`Scrollback::rows`] returns, one row at a time. This decodes
    /// through the store's own buffers and builds only those rows, so a frame leaves no freed
    /// rows in the heap. `benches/scrollback.rs` measures what scrolling adds to footprint.
    pub fn read_rows(&mut self, first: usize, count: usize, mut visit: impl FnMut(&[Cell])) {
        let mut spare = std::mem::take(&mut self.spare);
        let _ = self.walk(first, count, &mut spare, &mut visit);
        self.spare = spare;
    }

    /// Reads shown rows from the block where the first one starts, on through as many blocks as
    /// its line takes. Returns the number of the stored row that the first one starts in.
    fn walk(
        &self,
        first: usize,
        count: usize,
        spare: &mut Spare,
        visit: &mut impl FnMut(&[Cell]),
    ) -> Option<usize> {
        let (mut skip, mut offset, mut stored) = (first, 0, self.evicted);
        let mut reader: Option<Reader> = None;
        for block in &self.blocks {
            let start = offset;
            offset += block.size;
            let reader = match &mut reader {
                Some(reader) => reader,
                None if skip >= block.shown => {
                    skip -= block.shown;
                    stored += block.rows;
                    continue;
                }
                None => reader.insert(Reader::new(block.entry, skip, count, stored)),
            };
            if reader.done() {
                return reader.first_stored();
            }
            self.ring.read(start, block.size, &mut spare.packed);
            let data = &spare.packed;
            let _ = decode_block(block.rows, data, &mut spare.raw, reader, visit);
        }
        let open = &self.open;
        if reader.is_none() && skip >= open.shown {
            return None;
        }
        let mut reader = reader.unwrap_or_else(|| Reader::new(open.entry, skip, count, stored));
        let _ = decode_rows(open.rows, &open.runs, &open.text, &mut reader, visit);
        reader.finish(true, visit);
        reader.first_stored()
    }

    fn seal(&mut self) {
        let (rows, shown, entry) = (self.open.rows, self.open.shown, self.open.entry);
        let Some((size, inflated)) = self.open.seal(&mut self.spare) else {
            return;
        };
        self.ring.push(&self.spare.packed[..size]);
        self.block_rows += rows;
        self.block_shown += shown;
        self.inflated += inflated;
        self.blocks.push_back(Block {
            rows,
            size,
            shown,
            entry,
            inflated,
        });
    }

    fn drop_oldest(&mut self) -> bool {
        let Some(block) = self.blocks.pop_front() else {
            return false;
        };
        self.block_rows -= block.rows;
        self.block_shown -= block.shown;
        self.inflated -= block.inflated;
        self.evicted += block.rows;
        self.oldest += block.shown;
        self.ring.drop_oldest(block.size);
        true
    }
}

impl HistorySink<Cell> for Scrollback {
    fn push(&mut self, row: &Row<Cell>) {
        self.open.push(row);
        if self.open.rows >= BLOCK_ROWS || self.open.bytes() >= BLOCK_BYTES {
            self.seal();
        }
        let over = |store: &Self| {
            store.bytes() > store.byte_cap
                || store.block_rows + store.open.rows > store.line_cap
                || store.inflated > INFLATED_CAP
        };
        while over(self) && self.drop_oldest() {}
    }

    fn clear(&mut self) {
        let (end, width, stored) = (self.end(), self.open.flow.width, self.evicted);
        let stored = stored + self.block_rows + self.open.rows;
        *self = Self::with_caps(self.byte_cap, self.line_cap);
        self.oldest = end;
        self.floor = end;
        self.evicted = stored;
        self.set_width(width);
    }

    fn is_empty(&self) -> bool {
        Scrollback::is_empty(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_returns_what_it_was_given_across_chunks() {
        let mut ring = Ring::default();
        let block = |seed: usize, size: usize| -> Vec<u8> {
            (0..size).map(|index| (seed + index * 7) as u8).collect()
        };
        let sizes = [10, CHUNK_BYTES, 3, 2 * CHUNK_BYTES + 5, CHUNK_BYTES - 1, 1];
        let (mut offset, mut oldest, mut out) = (0, 0, Vec::new());
        for (seed, &size) in sizes.iter().enumerate() {
            ring.push(&block(seed, size));
            ring.read(offset, size, &mut out);
            assert_eq!(out, block(seed, size));
            offset += size;
        }
        for (seed, &size) in sizes.iter().enumerate() {
            ring.read(oldest, size, &mut out);
            assert_eq!(out, block(seed, size));
            oldest += size;
        }
        let mut live = offset;
        for (seed, &size) in sizes.iter().enumerate() {
            ring.drop_oldest(size);
            live -= size;
            assert_eq!(ring.len, live);
            assert!(ring.bytes() <= live + 2 * CHUNK_BYTES, "after block {seed}");
        }
    }

    #[test]
    fn a_full_ring_is_recycled_rather_than_grown() {
        let mut store = Scrollback::with_caps(256 * 1024, 100_000);
        for number in 0..20_000 {
            store.push(&row(&format!(
                "{number:x} {:x}",
                number * 2_654_435_761_usize
            )));
            assert!(store.bytes() <= 256 * 1024);
        }
        let newest = store.rows(store.len() - 1, 1);
        assert_eq!(newest[0][0].c, '4');
    }

    fn row(text: &str) -> Row<Cell> {
        let mut row: Row<Cell> = Row::new(text.chars().count());
        for (column, c) in text.chars().enumerate() {
            row[Column(column)].c = c;
        }
        row
    }

    fn text(cells: &[Cell]) -> String {
        cells.iter().map(|cell| cell.c).collect()
    }

    fn noise(seed: usize, columns: usize) -> String {
        let mut state = seed as u64 * 2 + 1;
        (0..columns)
            .map(|_| {
                state = state
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1);
                char::from(b'!' + (state >> 58) as u8)
            })
            .collect()
    }

    #[test]
    fn varints_round_trip() {
        for value in [0, 1, 127, 128, 300, 0x10_ffff, usize::MAX] {
            let mut out = vec![];
            put_varint(&mut out, value);
            assert_eq!(get_varint(&mut out.as_slice()), Some(value));
        }
    }

    #[test]
    fn rows_read_back_across_blocks_and_the_open_block() {
        let mut scrollback = Scrollback::new();
        for number in 0..BLOCK_ROWS * 2 + 5 {
            scrollback.push(&row(&format!("row {number}")));
        }
        let rows = scrollback.rows(BLOCK_ROWS - 1, BLOCK_ROWS + 3);
        assert_eq!(rows.len(), BLOCK_ROWS + 3);
        assert_eq!(text(&rows[0]), format!("row {}", BLOCK_ROWS - 1));
        assert_eq!(
            text(&rows[BLOCK_ROWS + 2]),
            format!("row {}", BLOCK_ROWS * 2 + 1)
        );
    }

    #[test]
    fn reading_past_the_end_stops_at_the_end() {
        let mut scrollback = Scrollback::new();
        scrollback.push(&row("only"));
        assert_eq!(scrollback.rows(0, 9).len(), 1);
        assert!(scrollback.rows(1, 9).is_empty());
    }

    #[test]
    fn the_byte_cap_drops_the_oldest_rows() {
        let cap = 200_000;
        let mut scrollback = Scrollback::with_caps(cap, usize::MAX);
        for number in 0..20_000 {
            scrollback.push(&row(&noise(number, 200)));
            assert!(scrollback.bytes() <= cap);
        }
        assert!(scrollback.len() < 20_000);
        assert_eq!(
            text(&scrollback.rows(scrollback.len() - 1, 1)[0]),
            noise(19_999, 200)
        );
    }

    #[test]
    fn the_line_cap_holds_even_for_blank_rows() {
        let mut scrollback = Scrollback::new();
        for _ in 0..LINE_CAP * 3 {
            scrollback.push(&row(" "));
            assert!(scrollback.len() <= LINE_CAP);
        }
        assert!(scrollback.len() > LINE_CAP - BLOCK_ROWS * 2);
    }

    #[test]
    fn links_too_long_to_keep_are_dropped_without_losing_the_row() {
        let mut long = row("x");
        let uri = "u".repeat(MAX_LINK + 1);
        long[Column(0)].set_hyperlink(Some(Hyperlink::new(None::<String>, uri)));
        let mut scrollback = Scrollback::new();
        scrollback.push(&long);
        let rows = scrollback.rows(0, 1);
        assert_eq!(text(&rows[0]), "x");
        assert!(rows[0][0].hyperlink().is_none());
    }

    #[test]
    fn a_row_of_huge_links_stays_under_the_cap() {
        let mut wide = row(&"x".repeat(1024));
        for column in 0..1024 {
            let uri = format!("{column}{}", noise(column, 2000));
            wide[Column(column)].set_hyperlink(Some(Hyperlink::new(None::<String>, uri)));
        }
        let mut scrollback = Scrollback::new();
        for _ in 0..50 {
            scrollback.push(&wide);
            assert!(scrollback.bytes() <= BYTE_CAP);
        }
    }

    #[test]
    fn a_block_costs_what_adr_0029_says() {
        assert_eq!(BLOCK_OVERHEAD, 40);
    }

    #[test]
    fn what_a_new_width_inflates_is_capped() {
        let mut wide = row(&"x".repeat(1024));
        let uri = "u".repeat(MAX_LINK - 1);
        for column in 0..1024 {
            let link = Hyperlink::new(Some("a"), uri.clone());
            wide[Column(column)].set_hyperlink(Some(link));
        }
        let mut store = Scrollback::with_caps(usize::MAX, usize::MAX);
        for _ in 0..INFLATED_CAP / (2 * BLOCK_BYTES) + 2 {
            store.push(&wide);
            assert!(store.inflated <= INFLATED_CAP);
        }
        assert!(store.evicted > 0);
    }

    #[test]
    fn clearing_forgets_everything() {
        let mut scrollback = Scrollback::new();
        for number in 0..BLOCK_ROWS + 1 {
            scrollback.push(&row(&format!("secret {number}")));
        }
        scrollback.clear();
        assert_eq!((scrollback.len(), scrollback.bytes()), (0, 0));
        assert!(scrollback.rows(0, 9).is_empty());
    }

    #[test]
    fn reading_uses_the_stores_buffers_and_matches_rows() {
        let mut store = Scrollback::new();
        for number in 0..BLOCK_ROWS * 3 + 5 {
            store.push(&row(&format!("row {number}")));
        }
        let held = store.bytes();
        let mut read = Vec::new();
        store.read_rows(BLOCK_ROWS - 2, BLOCK_ROWS * 2 + 4, |row| {
            read.push(text(row))
        });
        let rows = store.rows(BLOCK_ROWS - 2, BLOCK_ROWS * 2 + 4);
        assert_eq!(read, rows.iter().map(|row| text(row)).collect::<Vec<_>>());
        assert_eq!(read[0], format!("row {}", BLOCK_ROWS - 2));
        assert_eq!(read.len(), BLOCK_ROWS * 2 + 4);
        assert_eq!(store.bytes(), held);
    }

    #[test]
    fn numbers_outlive_eviction_and_clearing() {
        let mut store = Scrollback::with_caps(usize::MAX, BLOCK_ROWS * 2);
        for number in 0..BLOCK_ROWS * 3 {
            store.push(&row(&number.to_string()));
        }
        assert_eq!(store.end(), BLOCK_ROWS * 3);
        assert_eq!(
            store.index(0),
            Some(0),
            "an evicted row clamps to the oldest"
        );
        let newest = store.end() - 1;
        let index = store.index(newest).unwrap_or(usize::MAX);
        assert_eq!(text(&store.rows(index, 1)[0]), newest.to_string());
        assert_eq!(store.index(store.end()), None);
        store.clear();
        store.push(&row("after"));
        assert_eq!(
            store.index(newest),
            None,
            "a cleared row is gone, not clamped"
        );
        assert_eq!(store.index(store.end() - 1), Some(0));
    }

    proptest::proptest! {
        #[test]
        fn decoding_arbitrary_bytes_never_panics(
            count in 0..100_usize,
            data in proptest::collection::vec(0..=255_u8, 0..2048),
        ) {
            let reader = || Reader::new(Flow::new(7), 0, count, 0);
            let _ = decode_block(count, &data, &mut Vec::new(), &mut reader(), &mut |_| {});
            let _ = decode_rows(count, &data, "text", &mut reader(), &mut |_| {});
        }
    }
}
