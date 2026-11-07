//! Golden images: frames drawn by the CPU renderer's painter with the bundled font alone,
//! compared with the PNG files in `src/golden/`. `PINFOLD_UPDATE_GOLDEN=1 cargo test -p
//! pinfold-ui golden` rewrites them. A failure writes `NAME.actual.png` and `NAME.diff.png`
//! beside the reference, with differing pixels red in the diff.

use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};

use pinfold_proto::{BOLD, Cell, DEFAULT_BACKGROUND, Frame, ITALIC};
use pinfold_render::cpu::cells_with_cursor;
use pinfold_render::font::{Font, Fonts};
use pinfold_render::padding::Padding;
use pinfold_render::paint::{Canvas, Painter};

use crate::snapshots::{approval, bar, borders, pane};

/// A Retina display, so these are the pixels most people see.
const SCALE: f64 = 2.0;
/// The default 13-point font at that scale.
const FONT_PIXELS: f32 = 26.0;
/// Pixels around the grid, enough for the tab row's pills to reach past it.
const MARGIN: usize = 16;
/// How far a channel may stray from the reference. Glyphs come from `swash`, which is pure
/// Rust and gives the same coverage everywhere; only the sRGB tables in `paint.rs` call the
/// system's `powf`, whose last bit can differ by platform and moves a blended byte by one.
/// With no tolerance, the images matched exactly on Linux x86-64 and macOS arm64 and x86-64.
const TOLERANCE: u8 = 2;

const TEXT: u32 = 0x00e2_e5ea;

/// Writes `text` from `(row, column)`, a wide character over two cells as the core sends it.
fn line(frame: &mut Frame, (row, column): (usize, usize), text: &str, colours: (u32, u32)) {
    let mut at = row * usize::from(frame.columns) + column;
    for character in text.chars() {
        let cell = Cell {
            character,
            foreground: colours.0,
            background: colours.1,
        };
        frame.cells[at] = cell;
        if unicode_width::UnicodeWidthChar::width(character) == Some(2) {
            at += 1;
            frame.cells[at] = Cell {
                character: ' ',
                ..cell
            };
        }
        at += 1;
    }
}

/// Plain text, wide characters, and the cursor on a wide one.
fn text() -> Frame {
    let mut frame = pane(' ', 4, 32);
    let plain = (TEXT, DEFAULT_BACKGROUND);
    line(&mut frame, (0, 0), "$ ls -la ~/src | grep 'rs'", plain);
    line(&mut frame, (1, 0), "0123456789 {}[]()<>=+-*/&|!?@#", plain);
    line(&mut frame, (2, 0), "wide: 日本語 ｗｉｄｅ", plain);
    line(&mut frame, (3, 0), "box: ─│┌┐└┘├┤ ▀▄█", plain);
    Frame {
        cursor: Some((2, 6)),
        ..frame
    }
}

/// Bold, italic, both, inverse, and colours on text and background, with the cursor on a
/// narrow character.
fn styles() -> Frame {
    let mut frame = pane(' ', 4, 32);
    let on = DEFAULT_BACKGROUND;
    for (at, text, colours) in [
        ((0, 0), "regular", (TEXT, on)),
        ((1, 0), "bold", (TEXT | BOLD, on)),
        ((2, 0), "italic", (TEXT | ITALIC, on)),
        ((3, 0), "bold italic", (TEXT | BOLD | ITALIC, on)),
        ((0, 16), "inverse", (on, TEXT)),
        ((1, 16), "red", (0x00e0_6c75, on)),
        ((2, 16), "on blue", (on, 0x0061_afef)),
        ((3, 16), "amber bold", (0x00e5_c07b | BOLD, 0x0028_2c34)),
    ] {
        line(&mut frame, at, text, colours);
    }
    Frame {
        cursor: Some((0, 3)),
        ..frame
    }
}

/// Draws every row of `frame` as the CPU renderer does, the bar with a window's gaps around
/// its row of tabs, whose pills reach into them.
fn draw(frame: &Frame, bar: bool) -> (Vec<u32>, usize) {
    let mut painter = Painter::new(Fonts::alone(Font::bundled().unwrap()), FONT_PIXELS);
    let (cell_width, cell_height) = painter.cell_size();
    let window = if bar {
        Padding::scaled(SCALE)
    } else {
        Padding::default()
    };
    let padding = Padding {
        left: MARGIN,
        top: MARGIN,
        right: MARGIN,
        bottom: MARGIN,
        ..window
    };
    let columns = usize::from(frame.columns);
    let width = 2 * MARGIN + columns * cell_width;
    let height = padding.row_top(usize::from(frame.rows), cell_height) + MARGIN;
    let mut pixels = vec![0; width * height];
    let mut canvas = Canvas::new(&mut pixels, width, height)
        .unwrap()
        .inset(padding);
    canvas.clear(DEFAULT_BACKGROUND);
    let mut cells = Vec::new();
    cells_with_cursor(frame, &mut cells);
    for (row, cells) in cells.chunks(columns).enumerate() {
        painter.draw_row(&mut canvas, row, cells);
    }
    (pixels, width)
}

fn rgb(pixels: &[u32]) -> Vec<u8> {
    let bytes = |pixel: &u32| [(pixel >> 16) as u8, (pixel >> 8) as u8, *pixel as u8];
    pixels.iter().flat_map(bytes).collect()
}

fn write(path: &Path, data: &[u8], width: usize) {
    let height = data.len() / 3 / width;
    let file = BufWriter::new(File::create(path).unwrap());
    let mut encoder = png::Encoder::new(file, width as u32, height as u32);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::High);
    encoder
        .write_header()
        .unwrap()
        .write_image_data(data)
        .unwrap();
}

/// Returns the reference's width and RGB bytes, or nothing if there is none.
fn read(path: &Path) -> Option<(usize, Vec<u8>)> {
    let decoder = png::Decoder::new(BufReader::new(File::open(path).ok()?));
    let mut reader = decoder.read_info().unwrap();
    let mut data = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut data).unwrap();
    assert_eq!(info.color_type, png::ColorType::Rgb, "{}", path.display());
    data.truncate(info.buffer_size());
    Some((info.width as usize, data))
}

fn check(name: &str, (pixels, width): (Vec<u32>, usize)) {
    let path = |suffix: &str| -> PathBuf {
        [
            env!("CARGO_MANIFEST_DIR"),
            "src",
            "golden",
            &format!("{name}{suffix}"),
        ]
        .iter()
        .collect()
    };
    let actual = rgb(&pixels);
    if std::env::var_os("PINFOLD_UPDATE_GOLDEN").is_some_and(|value| value == "1") {
        write(&path(".png"), &actual, width);
        return;
    }
    let failure = match read(&path(".png")) {
        None => "has no reference".to_owned(),
        Some((expected_width, _)) if expected_width != width => {
            format!("is {width} pixels wide, not {expected_width}")
        }
        Some((_, expected)) if expected.len() != actual.len() => "changed height".to_owned(),
        Some((_, expected)) => {
            let mut diff = Vec::with_capacity(actual.len());
            let (mut wrong, mut worst) = (0, 0);
            for (got, want) in actual.chunks(3).zip(expected.chunks(3)) {
                let off = got.iter().zip(want).map(|(a, b)| a.abs_diff(*b)).max();
                let off = off.unwrap_or(0);
                worst = worst.max(off);
                if off > TOLERANCE {
                    wrong += 1;
                    diff.extend([0xff, 0, 0]);
                } else {
                    diff.extend(got.iter().map(|byte| byte / 4));
                }
            }
            if wrong == 0 {
                // Images from an earlier failure would mislead whoever looks next.
                let _ = std::fs::remove_file(path(".actual.png"));
                let _ = std::fs::remove_file(path(".diff.png"));
                return;
            }
            write(&path(".diff.png"), &diff, width);
            format!("has {wrong} pixels off by more than {TOLERANCE}, at worst {worst}")
        }
    };
    write(&path(".actual.png"), &actual, width);
    panic!(
        "{name} {failure}; see {} (PINFOLD_UPDATE_GOLDEN=1 accepts it)",
        path(".actual.png").display()
    );
}

#[test]
fn golden_text_wide_characters_and_the_cursor() {
    check("text", draw(&text(), false));
}

#[test]
fn golden_styles_and_colours() {
    check("styles", draw(&styles(), false));
}

#[test]
fn golden_borders() {
    check("borders", draw(&borders(), false));
}

#[test]
fn golden_bar() {
    // A tab's key hint is `⌘1` on macOS and `1` elsewhere, so each system has its own image.
    check(&format!("bar@{}", std::env::consts::OS), draw(&bar(), true));
}

#[test]
fn golden_approval_prompt() {
    check("approval", draw(&approval(), false));
}
