//! Scrolling a pane through its history, from the program's output to the frame.

use pinfold_core::scrollback::LINE_CAP;
use pinfold_core::vt::{AlacrittyEngine, VtEngine};
use pinfold_proto::Frame;
use proptest::prelude::*;

const ROWS: u16 = 4;
const COLUMNS: u16 = 20;

fn feed(engine: &mut AlacrittyEngine, bytes: &[u8]) {
    engine.feed(bytes, &mut Vec::new());
}

fn numbers(first: usize, last: usize) -> String {
    (first..=last)
        .map(|number| format!("{number}\r\n"))
        .collect()
}

/// A pane that has printed `1` to `lines`, each on its own row, with the cursor below them.
fn pane(lines: usize) -> AlacrittyEngine {
    let mut engine = AlacrittyEngine::new(ROWS, COLUMNS);
    feed(&mut engine, numbers(1, lines).as_bytes());
    engine
}

fn frame(engine: &AlacrittyEngine) -> Frame {
    let mut frame = Frame::default();
    engine.frame(&mut frame);
    frame
}

fn screen(engine: &AlacrittyEngine) -> Vec<String> {
    let frame = frame(engine);
    frame
        .cells
        .chunks(usize::from(frame.columns))
        .map(|row| {
            let text: String = row.iter().map(|cell| cell.character).collect();
            text.trim_end().to_owned()
        })
        .collect()
}

#[test]
fn the_wheel_shows_history_above_the_screen_and_returns() {
    let mut engine = pane(9);
    assert_eq!(screen(&engine), ["7", "8", "9", ""]);
    assert!(engine.scroll(1));
    assert_eq!(screen(&engine), ["6", "7", "8", "9"]);
    assert!(engine.scroll(5));
    assert_eq!(screen(&engine), ["1", "2", "3", "4"]);
    assert!(engine.scroll(-3));
    assert_eq!(screen(&engine), ["4", "5", "6", "7"]);
    assert!(engine.scroll(-3));
    assert_eq!(screen(&engine), ["7", "8", "9", ""]);
}

#[test]
fn the_cursor_is_hidden_only_over_history() {
    let mut engine = pane(9);
    assert_eq!(frame(&engine).cursor, Some((3, 0)));
    engine.scroll(1);
    assert_eq!(frame(&engine).cursor, None);
    engine.scroll(-1);
    assert_eq!(frame(&engine).cursor, Some((3, 0)));
}

#[test]
fn scrolling_stops_at_both_ends() {
    let mut engine = pane(9);
    assert!(!engine.scroll(-1));
    assert!(!engine.scroll(i32::MIN));
    assert!(engine.scroll(i32::MAX));
    assert_eq!(screen(&engine)[0], "1");
    assert!(!engine.scroll(1));
    assert!(engine.scroll(i32::MIN));
    assert_eq!(screen(&engine), ["7", "8", "9", ""]);
}

#[test]
fn an_empty_store_cannot_scroll() {
    let mut engine = AlacrittyEngine::new(ROWS, COLUMNS);
    assert!(!engine.scroll(1));
    assert_eq!(frame(&engine).cursor, Some((0, 0)));
}

#[test]
fn output_does_not_move_a_view_of_history() {
    let mut engine = pane(40);
    engine.scroll(10);
    let reading = screen(&engine);
    assert_eq!(reading[0], "28");
    feed(&mut engine, numbers(41, 300).as_bytes());
    assert_eq!(screen(&engine), reading);
    engine.scroll(-1);
    assert_eq!(screen(&engine)[0], "29");
}

#[test]
fn output_does_not_move_a_view_that_still_shows_the_screen() {
    let mut engine = pane(9);
    engine.scroll(2);
    assert_eq!(screen(&engine), ["5", "6", "7", "8"]);
    feed(&mut engine, numbers(10, 11).as_bytes());
    assert_eq!(screen(&engine), ["5", "6", "7", "8"]);
}

#[test]
fn eviction_under_the_view_clamps_to_the_oldest_row() {
    let mut engine = pane(LINE_CAP);
    assert!(engine.scroll(i32::MAX));
    let oldest: usize = screen(&engine)[0].parse().unwrap();
    feed(
        &mut engine,
        numbers(LINE_CAP + 1, LINE_CAP + 500).as_bytes(),
    );
    let top: usize = screen(&engine)[0].parse().unwrap();
    assert!(top > oldest, "{top} after {oldest}");
    assert!(!engine.scroll(1), "the clamped view is at the oldest row");
    assert!(engine.scroll(-1));
    assert_eq!(screen(&engine)[0], (top + 1).to_string());
}

#[test]
fn clearing_history_ends_the_view_for_good() {
    let mut engine = pane(40);
    engine.scroll(10);
    feed(&mut engine, b"\x1b[3J");
    assert_eq!(screen(&engine), ["38", "39", "40", ""]);
    feed(&mut engine, numbers(41, 60).as_bytes());
    assert_eq!(screen(&engine), ["58", "59", "60", ""]);
    assert_eq!(frame(&engine).cursor, Some((3, 0)));
}

#[test]
fn the_alternate_screen_does_not_scroll() {
    let mut engine = pane(9);
    feed(&mut engine, b"\x1b[?1049h");
    assert!(!engine.scroll(1));
    assert_eq!(screen(&engine), ["", "", "", ""]);
}

#[test]
fn a_program_with_mouse_reporting_keeps_the_wheel() {
    let mut engine = pane(9);
    feed(&mut engine, b"\x1b[?1000h");
    assert!(!engine.scroll(1));
    assert_eq!(screen(&engine), ["7", "8", "9", ""]);
}

#[test]
fn a_program_taking_the_screen_shows_through_a_scrolled_view() {
    let mut engine = pane(9);
    engine.scroll(3);
    feed(&mut engine, b"\x1b[?1049h\x1b[Hfull screen");
    assert_eq!(screen(&engine)[0], "full screen");
    assert!(frame(&engine).cursor.is_some());
}

#[test]
fn snapping_returns_to_the_screen_once() {
    let mut engine = pane(9);
    assert!(!engine.snap_to_live());
    engine.scroll(3);
    assert!(engine.snap_to_live());
    assert!(!engine.snap_to_live());
    assert_eq!(screen(&engine), ["7", "8", "9", ""]);
}

#[test]
fn history_fits_a_window_of_another_width() {
    let mut engine = AlacrittyEngine::new(ROWS, COLUMNS);
    let wide = "x".repeat(usize::from(COLUMNS));
    let lines = format!("{wide}\r\n{}", numbers(1, 9));
    feed(&mut engine, lines.as_bytes());
    engine.resize(ROWS, 8);
    engine.scroll(i32::MAX);
    assert_eq!(screen(&engine)[0], "x".repeat(8));
    engine.resize(ROWS, 40);
    assert_eq!(screen(&engine)[0], wide);
    assert_eq!(frame(&engine).cells.len(), usize::from(ROWS) * 40);
}

#[test]
fn a_taller_window_shows_more_of_the_view() {
    let mut engine = pane(40);
    engine.scroll(10);
    engine.resize(ROWS + 2, COLUMNS);
    assert_eq!(screen(&engine)[0], "28");
    assert_eq!(screen(&engine).len(), usize::from(ROWS) + 2);
}

#[derive(Clone, Debug)]
enum Step {
    Output(usize),
    Scroll(i32),
    Snap,
    Resize(u16, u16),
    Bytes(&'static str),
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        (0..300_usize).prop_map(Step::Output),
        any::<i32>().prop_map(Step::Scroll),
        (-70..70_i32).prop_map(Step::Scroll),
        Just(Step::Snap),
        (0..30_u16, 0..50_u16).prop_map(|(rows, columns)| Step::Resize(rows, columns)),
        prop::sample::select(vec![
            "\x1b[3J",
            "\x1bc",
            "\x1b[?1049h",
            "\x1b[?1049l",
            "\x1b[?1000h",
            "\x1b[?1000l",
            "\x1b[2J",
            "漢字\r\n",
        ])
        .prop_map(Step::Bytes),
    ]
}

proptest! {
    #[test]
    fn no_sequence_of_steps_breaks_a_frame(steps in prop::collection::vec(step(), 1..40)) {
        let mut engine = pane(100);
        for step in steps {
            match step {
                Step::Output(lines) => feed(&mut engine, numbers(1, lines).as_bytes()),
                Step::Scroll(lines) => {
                    engine.scroll(lines);
                }
                Step::Snap => {
                    engine.snap_to_live();
                }
                Step::Resize(rows, columns) => engine.resize(rows, columns),
                Step::Bytes(bytes) => feed(&mut engine, bytes.as_bytes()),
            }
            let frame = frame(&engine);
            let (rows, columns) = engine.size();
            prop_assert_eq!((frame.rows, frame.columns), (rows, columns));
            prop_assert_eq!(frame.cells.len(), usize::from(rows) * usize::from(columns));
        }
    }
}
