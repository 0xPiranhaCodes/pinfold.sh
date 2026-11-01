//! Rows that left the screen, compressed and capped in bytes (ADR-0021).
//!
//! Rows are encoded into one open block, which is compressed once it is full. Styles are
//! written inline per run, because LZ4 removes their repeats within a block.

use std::collections::VecDeque;

use alacritty_terminal::grid::{HistorySink, Row};
use alacritty_terminal::index::Column;
use alacritty_terminal::term::cell::{Cell, Flags, Hyperlink};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

/// The hard cap on one pane's scrollback.
pub const BYTE_CAP: usize = 2_000_000;
/// Scrollback keeps fewer rows than this, so a quiet pane stays far under the byte cap.
pub const LINE_CAP: usize = 10_000;

const BLOCK_ROWS: usize = 64;
/// An open block is compressed at this many bytes, however few rows it holds.
const BLOCK_BYTES: usize = 64 * 1024;
/// Counted for each block: its queue entry and the allocator's overhead.
const BLOCK_OVERHEAD: usize = 64;
/// Longer links are dropped from history, so one cell cannot hold megabytes.
const MAX_LINK: usize = 2048;
/// Combining marks kept per cell.
const MAX_ZEROWIDTH: usize = 16;
/// The most a block can inflate to. Anything claiming more is not a block.
const MAX_INFLATED: usize = 16 * 1024 * 1024;

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
}

impl Open {
    fn bytes(&self) -> usize {
        self.runs.capacity() + self.text.capacity()
    }

    fn push(&mut self, row: &Row<Cell>) {
        let cells = || (0..row.len()).map(|column| &row[Column(column)]);
        let default = Cell::default();
        let used = cells()
            .rposition(|cell| *cell != default)
            .map_or(0, |at| at + 1);
        put_varint(&mut self.runs, used);
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
        self.rows += 1;
    }

    fn put_run(&mut self, first: &Cell, count: usize) {
        put_varint(&mut self.runs, count);
        // A wide row of long links would otherwise pass the cap before the block is sealed.
        let links = self.runs.len() < 2 * BLOCK_BYTES;
        put_style(&mut self.runs, first, links);
    }

    fn seal(self) -> Block {
        let mut raw = Vec::with_capacity(self.bytes() + 8);
        put_varint(&mut raw, self.runs.len());
        raw.extend_from_slice(&self.runs);
        raw.extend_from_slice(self.text.as_bytes());
        Block {
            rows: self.rows,
            data: lz4_flex::block::compress_prepend_size(&raw).into_boxed_slice(),
        }
    }
}

#[derive(Debug)]
struct Block {
    rows: usize,
    data: Box<[u8]>,
}

/// Decodes `count` rows. Returns nothing if the data is not what `Open` wrote.
fn decode_rows(count: usize, mut runs: &[u8], text: &str) -> Option<Vec<Vec<Cell>>> {
    let mut characters = text.chars();
    let mut rows = Vec::new();
    for _ in 0..count {
        let mut left = get_varint(&mut runs)?;
        let mut row = Vec::new();
        while left > 0 {
            let length = get_varint(&mut runs)?;
            if length == 0 || length > left {
                return None;
            }
            let style = get_style(&mut runs)?;
            for _ in 0..length {
                row.push(style.cell(characters.next()?));
            }
            left -= length;
        }
        rows.push(row);
    }
    Some(rows)
}

fn decode_block(count: usize, data: &[u8]) -> Option<Vec<Vec<Cell>>> {
    let (size, compressed) = lz4_flex::block::uncompressed_size(data).ok()?;
    if size > MAX_INFLATED {
        return None;
    }
    let raw = lz4_flex::block::decompress(compressed, size).ok()?;
    let mut input = raw.as_slice();
    let length = get_varint(&mut input)?;
    let (runs, text) = input.split_at_checked(length)?;
    decode_rows(count, runs, std::str::from_utf8(text).ok()?)
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
    blocks: VecDeque<Block>,
    block_rows: usize,
    block_bytes: usize,
    byte_cap: usize,
    line_cap: usize,
}

impl Default for Scrollback {
    fn default() -> Self {
        Self::with_caps(BYTE_CAP, LINE_CAP)
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
            blocks: VecDeque::new(),
            block_rows: 0,
            block_bytes: 0,
            byte_cap,
            line_cap,
        }
    }

    /// Returns the number of rows held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.block_rows + self.open.rows
    }

    /// Returns whether no rows are held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the bytes held, which never exceed the cap once a row has been pushed.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.block_bytes + self.blocks.len() * BLOCK_OVERHEAD + self.open.bytes()
    }

    /// Returns up to `count` rows from `first`, where 0 is the oldest row held. Rows are
    /// trimmed of trailing default cells.
    #[must_use]
    pub fn rows(&self, first: usize, count: usize) -> Vec<Vec<Cell>> {
        let mut out = Vec::new();
        let mut skip = first;
        for block in &self.blocks {
            if out.len() == count {
                return out;
            }
            if skip >= block.rows {
                skip -= block.rows;
                continue;
            }
            let rows = decode_block(block.rows, &block.data).unwrap_or_default();
            let wanted = count - out.len();
            out.extend(rows.into_iter().skip(skip).take(wanted));
            skip = 0;
        }
        let rows = decode_rows(self.open.rows, &self.open.runs, &self.open.text);
        let wanted = count - out.len();
        out.extend(rows.unwrap_or_default().into_iter().skip(skip).take(wanted));
        out
    }

    fn seal(&mut self) {
        let block = std::mem::take(&mut self.open).seal();
        self.block_rows += block.rows;
        self.block_bytes += block.data.len();
        self.blocks.push_back(block);
    }

    fn drop_oldest(&mut self) -> bool {
        let Some(block) = self.blocks.pop_front() else {
            return false;
        };
        self.block_rows -= block.rows;
        self.block_bytes -= block.data.len();
        true
    }
}

impl HistorySink<Cell> for Scrollback {
    fn push(&mut self, row: &Row<Cell>) {
        self.open.push(row);
        if self.open.rows >= BLOCK_ROWS || self.open.bytes() >= BLOCK_BYTES {
            self.seal();
        }
        while (self.bytes() > self.byte_cap || self.len() > self.line_cap) && self.drop_oldest() {}
    }

    fn clear(&mut self) {
        *self = Self::with_caps(self.byte_cap, self.line_cap);
    }

    fn is_empty(&self) -> bool {
        Scrollback::is_empty(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn clearing_forgets_everything() {
        let mut scrollback = Scrollback::new();
        for number in 0..BLOCK_ROWS + 1 {
            scrollback.push(&row(&format!("secret {number}")));
        }
        scrollback.clear();
        assert_eq!((scrollback.len(), scrollback.bytes()), (0, 0));
        assert!(scrollback.rows(0, 9).is_empty());
    }

    proptest::proptest! {
        #[test]
        fn decoding_arbitrary_bytes_never_panics(
            count in 0..100_usize,
            data in proptest::collection::vec(0..=255_u8, 0..2048),
        ) {
            let _ = decode_block(count, &data);
            let _ = decode_rows(count, &data, "text");
        }
    }
}
