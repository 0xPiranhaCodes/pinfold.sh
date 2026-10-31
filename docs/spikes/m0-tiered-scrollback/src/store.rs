//! Tiered scrollback: compact hot rows, then LZ4-compressed cold blocks.

use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use alacritty_terminal::grid::Row;
use alacritty_terminal::index::Column;
use alacritty_terminal::term::cell::{Cell, Flags, Hyperlink};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};

pub const HOT_LINES: usize = 200;
pub const BLOCK_LINES: usize = 64;
const MAX_LINES: usize = 10_000;

const NAMED: [NamedColor; 30] = {
    use NamedColor::*;
    [
        Black, Red, Green, Yellow, Blue, Magenta, Cyan, White, BrightBlack, BrightRed,
        BrightGreen, BrightYellow, BrightBlue, BrightMagenta, BrightCyan, BrightWhite,
        Foreground, Background, Cursor, DimBlack, DimRed, DimGreen, DimYellow, DimBlue,
        DimMagenta, DimCyan, DimWhite, BrightForeground, DimForeground, Black,
    ]
};

/// Everything in a cell except its character.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Style {
    fg: Color,
    bg: Color,
    flags: u16,
    underline: Option<Color>,
    link: Option<(String, String)>,
    zerowidth: Vec<char>,
}

impl Hash for Style {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let mut bytes = Vec::with_capacity(32);
        put_style(&mut bytes, self);
        state.write(&bytes);
    }
}

impl Style {
    fn of(cell: &Cell) -> Self {
        Self {
            fg: cell.fg,
            bg: cell.bg,
            flags: cell.flags.bits(),
            underline: cell.underline_color(),
            link: cell.hyperlink().map(|link| (link.id().to_owned(), link.uri().to_owned())),
            zerowidth: cell.zerowidth().map(<[char]>::to_vec).unwrap_or_default(),
        }
    }

    fn apply(&self, c: char) -> Cell {
        let mut cell = Cell { c, fg: self.fg, bg: self.bg, ..Cell::default() };
        cell.flags = Flags::from_bits_truncate(self.flags);
        cell.set_underline_color(self.underline);
        for &ch in &self.zerowidth {
            cell.push_zerowidth(ch);
        }
        cell.set_hyperlink(self.link.clone().map(|(id, uri)| Hyperlink::new(Some(id), uri)));
        cell
    }
}

/// Eight bytes per cell, which is the compact hot row from review finding B3.
#[derive(Clone, Copy)]
struct Packed {
    c: char,
    style: u32,
}

#[derive(Default)]
struct Styles {
    ids: HashMap<Arc<Style>, u32>,
    list: Vec<Arc<Style>>,
}

impl Styles {
    fn intern(&mut self, style: Style) -> u32 {
        if let Some(&id) = self.ids.get(&style) {
            return id;
        }
        let id = self.list.len() as u32;
        let style = Arc::new(style);
        self.list.push(style.clone());
        self.ids.insert(style, id);
        id
    }
}

#[derive(Default)]
pub struct Store {
    styles: Styles,
    hot: VecDeque<Box<[Packed]>>,
    cold: VecDeque<Box<[u8]>>,
}

fn same_style(a: &Cell, b: &Cell) -> bool {
    a.fg == b.fg && a.bg == b.bg && a.flags == b.flags && a.extra.is_none() && b.extra.is_none()
}

fn put_varint(out: &mut Vec<u8>, mut value: usize) {
    while value >= 0x80 {
        out.push(value as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn get_varint(input: &mut &[u8]) -> usize {
    let (mut value, mut shift) = (0, 0);
    loop {
        let byte = input[0];
        *input = &input[1..];
        value |= usize::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return value;
        }
        shift += 7;
    }
}

fn put_str(out: &mut Vec<u8>, text: &str) {
    put_varint(out, text.len());
    out.extend_from_slice(text.as_bytes());
}

fn get_str(input: &mut &[u8]) -> String {
    let len = get_varint(input);
    let text = String::from_utf8_lossy(&input[..len]).into_owned();
    *input = &input[len..];
    text
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

fn get_color(input: &mut &[u8]) -> Color {
    let (color, used) = match input[0] {
        0 => (Color::Named(NAMED[usize::from(input[1])]), 2),
        1 => (Color::Indexed(input[1]), 2),
        _ => (Color::Spec(Rgb { r: input[1], g: input[2], b: input[3] }), 4),
    };
    *input = &input[used..];
    color
}

fn put_style(out: &mut Vec<u8>, style: &Style) {
    put_color(out, style.fg);
    put_color(out, style.bg);
    put_varint(out, usize::from(style.flags));
    match style.underline {
        Some(color) => {
            out.push(1);
            put_color(out, color);
        }
        None => out.push(0),
    }
    match &style.link {
        Some((id, uri)) => {
            out.push(1);
            put_str(out, id);
            put_str(out, uri);
        }
        None => out.push(0),
    }
    put_varint(out, style.zerowidth.len());
    style.zerowidth.iter().for_each(|&c| put_varint(out, c as usize));
}

fn get_style(input: &mut &[u8]) -> Style {
    let fg = get_color(input);
    let bg = get_color(input);
    let flags = get_varint(input) as u16;
    let has_underline = input[0] == 1;
    *input = &input[1..];
    let underline = has_underline.then(|| get_color(input));
    let has_link = input[0] == 1;
    *input = &input[1..];
    let link = has_link.then(|| {
        let id = get_str(input);
        (id, get_str(input))
    });
    let zerowidth = (0..get_varint(input))
        .map(|_| char::from_u32(get_varint(input) as u32).unwrap_or(' '))
        .collect();
    Style { fg, bg, flags, underline, link, zerowidth }
}

impl Store {
    /// Takes a row leaving the grid. Trailing default cells are dropped.
    pub fn push(&mut self, row: &Row<Cell>) {
        let columns = row.len();
        let cells: Vec<&Cell> = (0..columns).map(|column| &row[Column(column)]).collect();
        let used = cells.iter().rposition(|cell| **cell != Cell::default()).map_or(0, |at| at + 1);
        // Most cells share the previous cell's style, so skip the hash lookup for those.
        let mut last: Option<(&Cell, u32)> = None;
        let mut packed = Vec::with_capacity(used);
        for &cell in &cells[..used] {
            let reused = last.filter(|(prev, _)| same_style(prev, cell)).map(|(_, id)| id);
            let style = reused.unwrap_or_else(|| self.styles.intern(Style::of(cell)));
            last = Some((cell, style));
            packed.push(Packed { c: cell.c, style });
        }
        let packed = packed.into_boxed_slice();
        self.hot.push_back(packed);
        if self.hot.len() >= HOT_LINES + BLOCK_LINES {
            self.seal_block();
        }
    }

    fn seal_block(&mut self) {
        let mut local: HashMap<u32, usize> = HashMap::new();
        let (mut lengths, mut text, mut runs, mut styles) = (vec![], vec![], vec![], vec![]);
        for row in self.hot.drain(..BLOCK_LINES) {
            put_varint(&mut lengths, row.len());
            let mut run: Option<(usize, usize)> = None;
            for cell in &*row {
                put_varint(&mut text, cell.c as usize);
                let next = local.len();
                let id = *local.entry(cell.style).or_insert(next);
                if id == next {
                    put_style(&mut styles, &self.styles.list[cell.style as usize]);
                }
                run = match run {
                    Some((style, count)) if style == id => Some((style, count + 1)),
                    Some((style, count)) => {
                        put_varint(&mut runs, style);
                        put_varint(&mut runs, count);
                        Some((id, 1))
                    }
                    None => Some((id, 1)),
                };
            }
            if let Some((style, count)) = run {
                put_varint(&mut runs, style);
                put_varint(&mut runs, count);
            }
        }
        let mut block = vec![];
        put_varint(&mut block, local.len());
        for part in [&styles, &lengths, &runs, &text] {
            put_varint(&mut block, part.len());
            block.extend_from_slice(part);
        }
        self.cold.push_back(lz4_flex::block::compress_prepend_size(&block).into_boxed_slice());
        if self.cold.len() * BLOCK_LINES + self.hot.len() > MAX_LINES {
            self.cold.pop_front();
        }
        self.collect_styles();
    }

    /// Keeps the hot style table from growing with every style ever seen.
    fn collect_styles(&mut self) {
        if self.styles.list.len() < 1024 {
            return;
        }
        let old = std::mem::take(&mut self.styles);
        for row in &mut self.hot {
            for cell in row.iter_mut() {
                let style = (*old.list[cell.style as usize]).clone();
                cell.style = self.styles.intern(style);
            }
        }
    }

    pub fn len(&self) -> usize {
        self.cold.len() * BLOCK_LINES + self.hot.len()
    }

    fn decode_block(block: &[u8]) -> Vec<Vec<Cell>> {
        let raw = lz4_flex::block::decompress_size_prepended(block).unwrap_or_default();
        let mut input = raw.as_slice();
        let count = get_varint(&mut input);
        let mut parts = [&[][..]; 4];
        for part in &mut parts {
            let len = get_varint(&mut input);
            *part = &input[..len];
            input = &input[len..];
        }
        let [mut styles, mut lengths, mut runs, mut text] = parts;
        let styles: Vec<Style> = (0..count).map(|_| get_style(&mut styles)).collect();
        (0..BLOCK_LINES)
            .map(|_| {
                let mut left = get_varint(&mut lengths);
                let mut row = Vec::with_capacity(left);
                while left > 0 {
                    let style = &styles[get_varint(&mut runs)];
                    let count = get_varint(&mut runs);
                    for _ in 0..count {
                        let c = char::from_u32(get_varint(&mut text) as u32).unwrap_or(' ');
                        row.push(style.apply(c));
                    }
                    left -= count;
                }
                row
            })
            .collect()
    }

    /// Materializes `count` rows from `first`, where 0 is the oldest stored row.
    pub fn rows(&self, first: usize, count: usize) -> Vec<Vec<Cell>> {
        let mut out = Vec::with_capacity(count);
        let mut index = first;
        while out.len() < count && index < self.len() {
            let cold_lines = self.cold.len() * BLOCK_LINES;
            if index < cold_lines {
                let rows = Self::decode_block(&self.cold[index / BLOCK_LINES]);
                let start = index % BLOCK_LINES;
                let take = (BLOCK_LINES - start).min(count - out.len());
                out.extend(rows.into_iter().skip(start).take(take));
                index += take;
            } else {
                let row = &self.hot[index - cold_lines];
                let cells = row.iter().map(|cell| self.styles.list[cell.style as usize].apply(cell.c));
                out.push(cells.collect());
                index += 1;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_round_trip() {
        for value in [0, 1, 127, 128, 300, 0x10_ffff] {
            let mut out = vec![];
            put_varint(&mut out, value);
            assert_eq!(get_varint(&mut out.as_slice()), value);
        }
    }
}
